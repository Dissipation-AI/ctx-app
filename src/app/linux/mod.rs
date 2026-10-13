//! Linux (and other non-macOS systems): windows from `winit`, each showing one CPU buffer through
//! `softbuffer`. Everything the app draws is in that buffer; this module only turns the window
//! system's events into calls on [`App`], with points (pixels ÷ scale) from the top-left.
//!
//! The ⌘ shortcuts are Ctrl+Shift here (Ctrl+Shift+T opens a tab, as in GNOME Terminal and
//! Kitty); a shortcut that already has Shift uses Ctrl+Shift+Alt. Plain Ctrl keys always go to the
//! program.

pub mod install;
pub mod watch;

use super::input::Cursor;
use super::{App, later, with_app};
use crate::keys::{self, Press};
use crate::render::Surface;
use crate::shortcuts::Action;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{CursorIcon, Window, WindowId};

/// Dialogs are drawn by the app (there's no native alert to borrow).
pub const NATIVE_DIALOGS: bool = false;

// ---------------------------------------------------------------------------------------------
// The loop, and running things on it
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
pub enum UserEvent {
    /// The server connection has events.
    Drain,
    /// Work posted from another thread.
    Posted,
    /// Work posted on the main thread.
    Flush,
}

type SendJob = Box<dyn FnOnce() + Send>;

static PROXY: OnceLock<Mutex<EventLoopProxy<UserEvent>>> = OnceLock::new();
static FROM_THREADS: Mutex<Vec<SendJob>> = Mutex::new(Vec::new());

struct Timer {
    at: Instant,
    id: u64,
    job: Box<dyn FnOnce()>,
}

thread_local! {
    /// The active event loop, while a callback runs (windows are made with it).
    static LOOP: Cell<*const ActiveEventLoop> = const { Cell::new(std::ptr::null()) };
    static POSTED: RefCell<VecDeque<Box<dyn FnOnce()>>> = RefCell::new(VecDeque::new());
    static TIMERS: RefCell<Vec<Timer>> = const { RefCell::new(Vec::new()) };
    static TICKERS: RefCell<HashMap<u64, (Duration, Instant)>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
    static WINDOWS: RefCell<HashMap<WindowId, u64>> = RefCell::new(HashMap::new());
    static INPUT: RefCell<HashMap<WindowId, InputState>> = RefCell::new(HashMap::new());
    static CLIPBOARD: RefCell<Option<arboard::Clipboard>> = const { RefCell::new(None) };
    static QUIT: Cell<bool> = const { Cell::new(false) };
}

fn send(e: UserEvent) {
    if let Some(p) = PROXY.get() {
        let _ = p.lock().unwrap().send_event(e);
    }
}

/// From the server reader thread: there are events to handle.
pub fn wake() {
    send(UserEvent::Drain);
}

/// Run `f` on the main thread soon.
pub fn post(f: Box<dyn FnOnce()>) {
    POSTED.with(|p| p.borrow_mut().push_back(f));
    send(UserEvent::Flush);
}

/// Run `f` on the main thread soon, from any thread.
pub fn post_from_any_thread(f: SendJob) {
    FROM_THREADS.lock().unwrap().push(f);
    send(UserEvent::Posted);
}

/// Run `f` on the main thread after `ms` milliseconds.
pub fn after(ms: u64, f: Box<dyn FnOnce()>) {
    let id = NEXT_ID.with(|n| {
        let v = n.get();
        n.set(v + 1);
        v
    });
    TIMERS.with(|t| t.borrow_mut().push(Timer { at: Instant::now() + Duration::from_millis(ms), id, job: f }));
    send(UserEvent::Flush);
}

/// A repeating timer calling [`App::tick`].
pub struct Ticker {
    id: u64,
}

impl Ticker {
    pub fn stop(&self) {
        TICKERS.with(|t| t.borrow_mut().remove(&self.id));
    }
}

pub fn start_ticker(ms: u64) -> Ticker {
    let id = NEXT_ID.with(|n| {
        let v = n.get();
        n.set(v + 1);
        v
    });
    let every = Duration::from_millis(ms);
    TICKERS.with(|t| t.borrow_mut().insert(id, (every, Instant::now() + every)));
    send(UserEvent::Flush);
    Ticker { id }
}

fn run_posted() {
    loop {
        let job = POSTED.with(|p| p.borrow_mut().pop_front());
        match job {
            Some(j) => j(),
            None => break,
        }
    }
    let jobs: Vec<SendJob> = std::mem::take(&mut *FROM_THREADS.lock().unwrap());
    for j in jobs {
        j();
    }
}

/// Run due timers; returns when the next one is due.
fn run_timers() -> Option<Instant> {
    let now = Instant::now();
    let due: Vec<Timer> = TIMERS.with(|t| {
        let mut t = t.borrow_mut();
        let (due, keep): (Vec<Timer>, Vec<Timer>) = t.drain(..).partition(|x| x.at <= now);
        *t = keep;
        due
    });
    let mut due = due;
    due.sort_by_key(|t| (t.at, t.id));
    for t in due {
        (t.job)();
    }
    let tick = TICKERS.with(|t| {
        let mut fire = false;
        for (every, next) in t.borrow_mut().values_mut() {
            if *next <= now {
                *next = now + *every;
                fire = true;
            }
        }
        fire
    });
    if tick && with_app(|a| a.tick()).is_none() {
        later(|a| a.tick());
    }
    let t1 = TIMERS.with(|t| t.borrow().iter().map(|x| x.at).min());
    let t2 = TICKERS.with(|t| t.borrow().values().map(|x| x.1).min());
    match (t1, t2) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

struct Handler {
    started: bool,
}

/// Make the active loop available to window creation for the length of a callback.
fn with_loop<R>(el: &ActiveEventLoop, f: impl FnOnce() -> R) -> R {
    let prev = LOOP.with(|l| l.replace(el as *const _));
    let r = f();
    LOOP.with(|l| l.set(prev));
    r
}

impl ApplicationHandler<UserEvent> for Handler {
    fn new_events(&mut self, el: &ActiveEventLoop, _cause: StartCause) {
        with_loop(el, || {});
    }

    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.started {
            return;
        }
        self.started = true;
        with_loop(el, super::launched);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, e: UserEvent) {
        with_loop(el, || match e {
            UserEvent::Drain => super::drain(),
            UserEvent::Posted | UserEvent::Flush => run_posted(),
        });
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        with_loop(el, || window_event(id, event));
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        with_loop(el, || {
            run_posted();
            let next = run_timers();
            run_posted();
            if QUIT.with(|q| q.get()) {
                el.exit();
                return;
            }
            el.set_control_flow(match next {
                Some(at) => ControlFlow::WaitUntil(at),
                None => ControlFlow::Wait,
            });
        });
    }
}

pub fn run() {
    let el = EventLoop::<UserEvent>::with_user_event().build().expect("an event loop (is DISPLAY set?)");
    let _ = PROXY.set(Mutex::new(el.create_proxy()));
    let mut h = Handler { started: false };
    let _ = el.run_app(&mut h);
}

pub fn terminate() {
    QUIT.with(|q| q.set(true));
    send(UserEvent::Flush);
}

/// Quit as ⌘Q does: ask if commands are running, close the tabs (unless kept), then exit.
pub fn request_quit() {
    later(|a| {
        let summary = if a.quitting { None } else { a.quit_summary() };
        match summary {
            Some(s) => a.ask("Quit CTX?", &format!("These are still running and will be stopped:\n{s}"), "Quit", "Cancel", |a, yes| {
                if yes {
                    a.quit_now();
                    terminate();
                }
            }),
            None => {
                a.quit_now();
                terminate();
            }
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------------------------

type Soft = softbuffer::Surface<Rc<Window>, Rc<Window>>;

/// The platform objects of one window.
pub struct Views {
    pub window: Rc<Window>,
    soft: RefCell<Option<Soft>>,
}

impl Drop for Views {
    fn drop(&mut self) {
        let id = self.window.id();
        WINDOWS.with(|w| w.borrow_mut().remove(&id));
        INPUT.with(|i| i.borrow_mut().remove(&id));
    }
}

pub fn make_window(n: u64, frame: Option<[f64; 4]>, size: (f64, f64), index: usize, _titlebar_tabs: bool) -> Views {
    let el = LOOP.with(|l| l.get());
    assert!(!el.is_null(), "windows are made inside an event loop callback");
    // SAFETY: set by `with_loop` for the duration of the callback we're in.
    let el = unsafe { &*el };
    let mut attrs = Window::default_attributes().with_title("CTX");
    match frame {
        Some([x, y, w, h]) => {
            attrs = attrs.with_inner_size(LogicalSize::new(w, h)).with_position(PhysicalPosition::new(x as i32, y as i32));
        }
        None => {
            attrs = attrs.with_inner_size(LogicalSize::new(size.0, size.1));
            if index > 0 {
                let off = 24 * index as i32;
                attrs = attrs.with_position(PhysicalPosition::new(80 + off, 80 + off));
            }
        }
    }
    let window = Rc::new(el.create_window(attrs).expect("a window"));
    window.set_ime_allowed(true);
    WINDOWS.with(|w| w.borrow_mut().insert(window.id(), n));
    ALL.with(|a| a.borrow_mut().push(Rc::downgrade(&window)));
    let soft = softbuffer::Context::new(window.clone()).ok().and_then(|ctx| Soft::new(&ctx, window.clone()).ok());
    Views { window, soft: RefCell::new(soft) }
}

pub fn backing_scale(v: &Views) -> f64 {
    std::env::var("CTX_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or_else(|| v.window.scale_factor())
}

pub fn content_size(v: &Views) -> (f64, f64) {
    let s = v.window.inner_size();
    let k = backing_scale(v);
    (s.width as f64 / k, s.height as f64 / k)
}

pub fn show_window(v: &Views, activate: bool) {
    v.window.set_visible(true);
    if activate {
        v.window.focus_window();
    }
}

pub fn order_front_without_focus(v: &Views) {
    v.window.set_visible(true);
}

pub fn close_window(v: &Views) {
    v.window.set_visible(false);
}

pub fn is_key(v: &Views) -> bool {
    v.window.has_focus()
}

pub fn move_window_top_left(v: &Views, x: f64, y: f64) {
    if x.is_finite() && y.is_finite() {
        let k = backing_scale(v);
        v.window.set_outer_position(PhysicalPosition::new((x - 40.0 * k) as i32, (y - 10.0 * k) as i32));
    }
}

pub fn window_frame(v: &Views) -> Option<[f64; 4]> {
    let p = v.window.outer_position().ok()?;
    let (w, h) = content_size(v);
    Some([p.x as f64, p.y as f64, w, h])
}

pub fn set_title(v: &Views, title: &str) {
    v.window.set_title(title);
}

pub fn toggle_full_screen(v: &Views) {
    let full = v.window.fullscreen().is_some();
    v.window.set_fullscreen(if full { None } else { Some(winit::window::Fullscreen::Borderless(None)) });
}

pub fn minimise(v: &Views) {
    v.window.set_minimized(true);
}

pub fn zoom(v: &Views) {
    v.window.set_maximized(!v.window.is_maximized());
}

pub fn title_double_click(v: &Views) {
    zoom(v);
}

pub fn drag_window(v: &Views) {
    let _ = v.window.drag_window();
}

/// No window buttons share the bar here.
pub fn titlebar_inset(_v: &Views) -> f64 {
    8.0
}

pub fn new_surface(_v: &Views, w: usize, h: usize) -> Option<Surface> {
    Some(Surface::new_heap(w, h))
}

/// Show what was drawn.
pub fn present(v: &Views, s: &Surface, _scale: f64) {
    let mut soft = v.soft.borrow_mut();
    let Some(soft) = soft.as_mut() else { return };
    let (Some(w), Some(h)) = (NonZeroU32::new(s.width as u32), NonZeroU32::new(s.height as u32)) else { return };
    if soft.resize(w, h).is_err() {
        return;
    }
    if let Ok(mut buf) = soft.buffer_mut() {
        if buf.len() == s.pixels.len() {
            for (d, p) in buf.iter_mut().zip(&s.pixels) {
                *d = p & 0x00FF_FFFF;
            }
        }
        let _ = buf.present();
    }
}

pub fn release(_v: &Views) {}

/// A point in a window's content (points) on the screen (pixels).
pub fn to_screen(v: &Views, x: f64, y: f64) -> (f64, f64) {
    let k = backing_scale(v);
    let p = v.window.inner_position().unwrap_or(PhysicalPosition::new(0, 0));
    (p.x as f64 + x * k, p.y as f64 + y * k)
}

/// The window under a screen point, and the point in its content (points). The key window is
/// tried first (the window system doesn't tell us the stacking order).
pub fn window_at(app: &App, sx: f64, sy: f64) -> Option<(u64, f64, f64)> {
    let mut order: Vec<&super::Win> = app.wins.values().filter(|w| w.visible).collect();
    order.sort_by_key(|w| !w.views.window.has_focus());
    for w in order {
        let Ok(p) = w.views.window.inner_position() else { continue };
        let s = w.views.window.inner_size();
        let (x0, y0) = (p.x as f64, p.y as f64);
        if sx >= x0 && sy >= y0 && sx < x0 + s.width as f64 && sy < y0 + s.height as f64 {
            let k = backing_scale(&w.views);
            return Some((w.number, (sx - x0) / k, (sy - y0) / k));
        }
    }
    None
}

pub fn set_cursor(c: Cursor) {
    let icon = match c {
        Cursor::Arrow => CursorIcon::Default,
        Cursor::Text => CursorIcon::Text,
        Cursor::Pointer => CursorIcon::Pointer,
        Cursor::Grab => CursorIcon::Grabbing,
    };
    // Called from inside the app (which is borrowed): reach the windows through our own list.
    // The pointer shape only shows over the window it's in.
    ALL.with(|all| {
        all.borrow_mut().retain(|w| w.strong_count() > 0);
        for w in all.borrow().iter().filter_map(|w| w.upgrade()) {
            w.set_cursor(icon);
        }
    });
}

thread_local! {
    static ALL: RefCell<Vec<std::rc::Weak<Window>>> = const { RefCell::new(Vec::new()) };
}

pub fn set_ime_area(v: &Views, x: f64, y: f64, w: f64, h: f64) {
    let k = backing_scale(v);
    v.window.set_ime_cursor_area(PhysicalPosition::new(x * k, y * k), PhysicalSize::new(w * k, h * k));
}

// ---------------------------------------------------------------------------------------------
// Things the desktop does
// ---------------------------------------------------------------------------------------------

pub fn set_clipboard(text: &str) {
    CLIPBOARD.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c = arboard::Clipboard::new().ok();
        }
        if let Some(cb) = c.as_mut() {
            let _ = cb.set_text(text.to_string());
        }
    });
}

pub fn get_clipboard() -> Option<String> {
    CLIPBOARD.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c = arboard::Clipboard::new().ok();
        }
        c.as_mut()?.get_text().ok()
    })
}

pub fn open_url(url: &str) {
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

pub fn open_path(p: &std::path::Path) {
    let _ = std::process::Command::new("xdg-open").arg(p).spawn();
}

pub fn about() {
    later(|a| a.alert("CTX", &format!("CTX {}\nThe window for the ctx server.", env!("CARGO_PKG_VERSION"))));
}

pub fn hide() {}
pub fn hide_others() {}
pub fn show_all() {}
pub fn bring_all_to_front() {}

pub fn system_is_dark() -> bool {
    true
}

/// Unused here: dialogs are drawn.
pub fn dialog(_title: &str, _text: &str, _buttons: &[String], _field: Option<&str>) -> (Option<usize>, String) {
    (None, String::new())
}

/// There's no menu bar: shortcuts are matched in [`key_event`]. The actions in table order.
pub fn build_menus(_app: &App) -> Vec<Action> {
    crate::shortcuts::entries().iter().map(|e| e.action).collect()
}

pub fn rebuild_window_menu(_app: &App) {}

/// A shortcut as it's pressed here: `cmd-shift-t` → `Ctrl+Shift+Alt+T`.
pub fn shortcut_label(keys: &str) -> String {
    let Some((key, flags)) = keys::parse_shortcut(keys) else { return String::new() };
    let mut out = String::new();
    if flags & keys::CONTROL != 0 {
        out.push_str("Ctrl+");
    }
    if flags & keys::COMMAND != 0 {
        out.push_str("Ctrl+Shift+");
    }
    if flags & keys::SHIFT != 0 {
        out.push_str("Alt+");
    }
    if flags & keys::OPTION != 0 {
        out.push_str("Alt+");
    }
    let k = match key.as_str() {
        "\u{f702}" => "Left".to_string(),
        "\u{f703}" => "Right".to_string(),
        "\u{f700}" => "Up".to_string(),
        "\u{f701}" => "Down".to_string(),
        k => k.to_uppercase(),
    };
    out + &k
}

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct InputState {
    mods: ModifiersState,
    left_alt: bool,
    right_alt: bool,
    pos: (f64, f64),
    buttons: Vec<u8>,
    last_click: Option<(Instant, (f64, f64), u8, isize)>,
}

fn flags_of(st: &InputState) -> u64 {
    let mut f = 0;
    if st.mods.shift_key() {
        f |= keys::SHIFT;
    }
    if st.mods.control_key() {
        f |= keys::CONTROL;
    }
    if st.mods.alt_key() {
        f |= keys::OPTION;
        if st.left_alt {
            f |= keys::LEFT_OPTION;
        }
        if st.right_alt {
            f |= keys::RIGHT_OPTION;
        }
    }
    if st.mods.super_key() {
        f |= keys::COMMAND;
    }
    f
}

/// The macOS virtual key code for a named key (the key mapper speaks those).
fn keycode(k: &NamedKey) -> Option<u16> {
    Some(match k {
        NamedKey::Enter => 36,
        NamedKey::Tab => 48,
        NamedKey::Space => 49,
        NamedKey::Backspace => 51,
        NamedKey::Escape => 53,
        NamedKey::Delete => 117,
        NamedKey::Insert => 114,
        NamedKey::Home => 115,
        NamedKey::End => 119,
        NamedKey::PageUp => 116,
        NamedKey::PageDown => 121,
        NamedKey::ArrowLeft => 123,
        NamedKey::ArrowRight => 124,
        NamedKey::ArrowDown => 125,
        NamedKey::ArrowUp => 126,
        NamedKey::F1 => 122,
        NamedKey::F2 => 120,
        NamedKey::F3 => 99,
        NamedKey::F4 => 118,
        NamedKey::F5 => 96,
        NamedKey::F6 => 97,
        NamedKey::F7 => 98,
        NamedKey::F8 => 100,
        NamedKey::F9 => 101,
        NamedKey::F10 => 109,
        NamedKey::F11 => 103,
        NamedKey::F12 => 111,
        _ => return None,
    })
}

/// The shortcut name of a key, as `[app.keys]` writes it.
fn shortcut_key(k: &Key) -> Option<String> {
    Some(match k {
        Key::Character(c) => c.to_lowercase(),
        Key::Named(NamedKey::ArrowLeft) => "\u{f702}".into(),
        Key::Named(NamedKey::ArrowRight) => "\u{f703}".into(),
        Key::Named(NamedKey::ArrowUp) => "\u{f700}".into(),
        Key::Named(NamedKey::ArrowDown) => "\u{f701}".into(),
        Key::Named(NamedKey::Tab) => "\t".into(),
        _ => return None,
    })
}

fn window_event(id: WindowId, event: WindowEvent) {
    let Some(w) = WINDOWS.with(|m| m.borrow().get(&id).copied()) else { return };
    INPUT.with(|i| {
        i.borrow_mut().entry(id).or_default();
    });
    match event {
        WindowEvent::CloseRequested => later(move |a| a.close_window(w, false)),
        WindowEvent::Resized(_) => call(move |a| a.window_resized(w)),
        WindowEvent::ScaleFactorChanged { .. } => call(move |a| a.scale_changed(w)),
        WindowEvent::Focused(f) => call(move |a| {
            a.window_key(w, f);
            a.app_active(f || a.wins.values().any(|x| x.views.window.has_focus()));
        }),
        WindowEvent::Occluded(o) => call(move |a| a.window_visibility(w, !o)),
        WindowEvent::RedrawRequested => call(move |a| {
            a.dirty_win(w);
            a.redraw_dirty();
        }),
        WindowEvent::ModifiersChanged(m) => INPUT.with(|i| {
            if let Some(st) = i.borrow_mut().get_mut(&id) {
                st.mods = m.state();
                st.left_alt = m.lalt_state() == winit::keyboard::ModifiersKeyState::Pressed;
                st.right_alt = m.ralt_state() == winit::keyboard::ModifiersKeyState::Pressed;
            }
        }),
        WindowEvent::KeyboardInput { event, .. } => {
            if event.state != ElementState::Pressed {
                return;
            }
            let (flags, mods) = INPUT.with(|i| i.borrow().get(&id).map(|st| (flags_of(st), st.mods)).unwrap_or_default());
            let base = event.key_without_modifiers();
            // Ctrl+Shift(+Alt) is the ⌘ of this platform.
            if mods.control_key() && mods.shift_key() {
                if let Some(k) = shortcut_key(&base) {
                    let want = if mods.alt_key() { keys::COMMAND | keys::SHIFT } else { keys::COMMAND };
                    let action = with_app(|a| {
                        a.bindings.iter().find_map(|(act, s)| match keys::parse_shortcut(s) {
                            Some((sk, sf)) if sf == want && (sk == k || shifted(&sk) == k) => Some(*act),
                            _ => None,
                        })
                    })
                    .flatten();
                    if let Some(act) = action {
                        later(move |a| a.perform(act));
                        return;
                    }
                }
            }
            let code = match &event.logical_key {
                Key::Named(n) => keycode(n),
                _ => None,
            };
            let chars_ignoring = match &base {
                Key::Character(c) => {
                    if mods.shift_key() {
                        match &event.logical_key {
                            Key::Character(l) => l.to_string(),
                            _ => c.to_uppercase(),
                        }
                    } else {
                        c.to_string()
                    }
                }
                _ => String::new(),
            };
            let text = event.text.as_ref().map(|t| t.to_string()).unwrap_or_default();
            let p = Press { keycode: code.unwrap_or(0xFFFF), chars: text.clone(), chars_ignoring, flags };
            let input = with_app(|a| a.key_down(w, &p));
            let input = match input {
                Some(i) => i,
                None => {
                    let p2 = p.clone();
                    later(move |a| {
                        if a.key_down(w, &p2) == keys::Input::TextInput && !p2.chars.is_empty() && !p2.chars.chars().any(|c| c.is_control()) {
                            a.insert_text(w, &p2.chars);
                        }
                    });
                    return;
                }
            };
            if input == keys::Input::TextInput && !text.is_empty() && !text.chars().any(|c| c.is_control()) {
                call(move |a| a.insert_text(w, &text));
            }
        }
        WindowEvent::Ime(ime) => match ime {
            Ime::Preedit(text, _) => call(move |a| a.set_marked(w, (!text.is_empty()).then_some(text))),
            Ime::Commit(text) => call(move |a| a.insert_text(w, &text)),
            Ime::Enabled | Ime::Disabled => {}
        },
        WindowEvent::CursorMoved { position, .. } => {
            let (x, y, flags, button) = INPUT.with(|i| {
                let mut i = i.borrow_mut();
                let st = i.get_mut(&id).unwrap();
                let k = scale_of(w);
                st.pos = (position.x / k, position.y / k);
                (st.pos.0, st.pos.1, flags_of(st), st.buttons.last().copied())
            });
            match button {
                Some(b) => call(move |a| a.mouse_dragged(w, x, y, flags, b)),
                None => call(move |a| a.mouse_moved(w, x, y, flags)),
            }
        }
        WindowEvent::CursorLeft { .. } => call(move |a| a.mouse_exited(w)),
        WindowEvent::MouseInput { state, button, .. } => {
            let b = match button {
                MouseButton::Left => 1,
                MouseButton::Right => 2,
                MouseButton::Middle => 3,
                _ => return,
            };
            let (x, y, flags, clicks) = INPUT.with(|i| {
                let mut i = i.borrow_mut();
                let st = i.get_mut(&id).unwrap();
                let mut clicks = 1;
                if state == ElementState::Pressed {
                    st.buttons.push(b);
                    if let Some((at, p, lb, n)) = st.last_click
                        && lb == b
                        && at.elapsed() < Duration::from_millis(450)
                        && (p.0 - st.pos.0).abs() < 4.0
                        && (p.1 - st.pos.1).abs() < 4.0
                    {
                        clicks = n + 1;
                    }
                    st.last_click = Some((Instant::now(), st.pos, b, clicks));
                } else {
                    st.buttons.retain(|x| *x != b);
                }
                (st.pos.0, st.pos.1, flags_of(st), clicks)
            });
            if state == ElementState::Pressed {
                call(move |a| a.mouse_down(w, x, y, clicks, flags, b));
            } else {
                call(move |a| a.mouse_up(w, x, y, flags, b));
            }
        }
        WindowEvent::MouseWheel { delta, .. } => {
            let (x, y, flags) = INPUT.with(|i| i.borrow().get(&id).map(|st| (st.pos.0, st.pos.1, flags_of(st))).unwrap_or_default());
            let k = scale_of(w);
            let (dx, dy, precise) = match delta {
                MouseScrollDelta::LineDelta(dx, dy) => (dx as f64, dy as f64, false),
                MouseScrollDelta::PixelDelta(p) => (p.x / k, p.y / k, true),
            };
            call(move |a| a.scroll_event(w, x, y, dx, dy, precise, flags));
        }
        WindowEvent::DroppedFile(path) => {
            let p = path.display().to_string();
            call(move |a| a.drop_paths(w, vec![p]));
        }
        _ => {}
    }
}

/// `+` for `=`, as Shift gives on most layouts (so `cmd-+` matches Ctrl+Shift+=).
fn shifted(k: &str) -> String {
    match k {
        "+" => "=".into(),
        "_" => "-".into(),
        "{" => "[".into(),
        "}" => "]".into(),
        k => k.to_string(),
    }
}

fn scale_of(w: u64) -> f64 {
    with_app(|a| a.wins.get(&w).map(|x| backing_scale(&x.views))).flatten().unwrap_or(1.0)
}

/// Call into the app now if it's free, else as soon as it is.
fn call(f: impl FnOnce(&mut App) + 'static) {
    let cell = RefCell::new(Some(f));
    if with_app(|a| (cell.borrow_mut().take().unwrap())(a)).is_none()
        && let Some(f) = cell.borrow_mut().take()
    {
        later(f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_labels() {
        assert_eq!(shortcut_label("cmd-t"), "Ctrl+Shift+T");
        assert_eq!(shortcut_label("cmd-shift-t"), "Ctrl+Shift+Alt+T");
        assert_eq!(shifted("+"), "=");
    }
}
