//! macOS: the application delegate, windows, one content view per window (keyboard, text
//! input, mouse, drops) whose layer shows the window's IOSurface, the menu bar, and alerts.
//! Everything inside a window (tab bar, terminal, panels) is drawn by the shared app; these
//! classes hold almost no state and forward to the [`App`].
//!
//! With `tabs_in_titlebar` (the default) the window has no title bar at all: the tab bar is the
//! top of the window, with red/yellow/green buttons we draw ourselves (the system's cost ~4 MB).

pub mod install;
pub mod watch;

use super::input::Cursor;
use super::{App, later, with_app};
use crate::keys::{self, Press};
use crate::render::Surface;
use crate::shortcuts::{self, Action};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::*;
use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::time::Duration;

/// Alerts are NSAlert.
pub const NATIVE_DIALOGS: bool = true;

/// ⌘-click opens a link, as in Terminal.app.
pub const LINK_MODIFIER: u64 = keys::COMMAND;

fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("main thread")
}

// ---------------------------------------------------------------------------------------------
// Running things on the main thread
// ---------------------------------------------------------------------------------------------

struct SendBox(Box<dyn FnOnce()>);
// SAFETY: only ever run on the main queue, the thread it was made on.
unsafe impl Send for SendBox {}

/// From the server reader thread: there are events to handle.
pub fn wake() {
    dispatch2::DispatchQueue::main().exec_async(super::drain);
}

/// Run `f` on the main thread soon (outside whatever is running now).
pub fn post(f: Box<dyn FnOnce()>) {
    let b = SendBox(f);
    dispatch2::DispatchQueue::main().exec_async(move || {
        let b = b;
        (b.0)()
    });
}

pub fn post_from_any_thread(f: Box<dyn FnOnce() + Send>) {
    dispatch2::DispatchQueue::main().exec_async(f);
}

pub fn after(ms: u64, f: Box<dyn FnOnce()>) {
    let b = SendBox(f);
    let when = dispatch2::DispatchTime::NOW.time(Duration::from_millis(ms).as_nanos() as i64);
    let _ = dispatch2::DispatchQueue::main().after(when, move || {
        let b = b;
        (b.0)()
    });
}

/// A repeating timer calling [`App::tick`].
pub struct Ticker(Retained<NSTimer>);

impl Ticker {
    pub fn stop(&self) {
        self.0.invalidate();
    }
}

pub fn start_ticker(ms: u64) -> Ticker {
    let block = RcBlock::new(|_t: NonNull<NSTimer>| {
        if with_app(|a| a.tick()).is_none() {
            later(|a| a.tick());
        }
    });
    Ticker(unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(ms as f64 / 1000.0, true, &block) })
}

/// Call into the app now if it's free and nothing is queued before it, else in order later.
fn call(f: impl FnOnce(&mut App) + 'static) {
    super::call(f);
}

// ---------------------------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------------------------

thread_local! {
    static DELEGATE: RefCell<Option<Retained<AppDelegate>>> = const { RefCell::new(None) };
}

fn delegate() -> Option<Retained<AppDelegate>> {
    DELEGATE.with(|d| d.borrow().clone())
}

fn delegate_obj() -> Option<Retained<AnyObject>> {
    delegate().map(|d| Retained::into_super(Retained::into_super(d)))
}

pub fn run() {
    let mtm = mtm();
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let d = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*d)));
    DELEGATE.with(|x| *x.borrow_mut() = Some(d));
    app.run();
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CTXAppDelegate"]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _n: &NSNotification) {
            super::launched();
            #[allow(deprecated)]
            NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            let summary = with_app(|a| if a.quitting { None } else { a.quit_summary() }).flatten();
            if let Some(s) = summary
                && !ask_native("Quit CTX?", &format!("These are still running and will be stopped:\n{s}"), "Quit", "Cancel")
            {
                return NSApplicationTerminateReply::TerminateCancel;
            }
            with_app(|a| a.quit_now());
            NSApplicationTerminateReply::TerminateNow
        }

        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_reopen(&self, _app: &NSApplication, visible: bool) -> bool {
            if !visible {
                later(|a| {
                    if a.wins.is_empty() {
                        a.new_window(None);
                    } else if let Some(w) = a.wins.values().next() {
                        show_window(&w.views, true);
                    }
                });
            }
            true
        }

        #[unsafe(method(applicationDidBecomeActive:))]
        fn did_become_active(&self, _n: &NSNotification) {
            later(|a| a.app_active(true));
        }

        #[unsafe(method(applicationDidResignActive:))]
        fn did_resign_active(&self, _n: &NSNotification) {
            later(|a| a.app_active(false));
        }
    }

    impl AppDelegate {
        #[unsafe(method(menuAction:))]
        fn menu_action(&self, sender: &NSMenuItem) {
            let i = sender.tag() as usize;
            later(move |a| {
                if let Some(action) = a.menu_actions.get(i).copied() {
                    a.perform(action);
                }
            });
        }

        #[unsafe(method(showCtxWindow:))]
        fn show_ctx_window(&self, sender: &NSMenuItem) {
            let w = sender.tag() as u64;
            later(move |a| {
                a.open_win(w);
                if let Some(win) = a.wins.get(&w) {
                    show_window(&win.views, true);
                }
            });
        }

        #[unsafe(method(setTheme:))]
        fn set_theme(&self, sender: &NSMenuItem) {
            let name = sender.representedObject().and_then(|o| o.downcast::<NSString>().ok()).map(|s| s.to_string());
            later(move |a| {
                if let Some(t) = a.key_window().and_then(|w| a.focused_tab(w)) {
                    a.set_tab_theme(t, name);
                }
            });
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

pub fn system_is_dark() -> bool {
    let app = NSApplication::sharedApplication(mtm());
    let appearance = app.effectiveAppearance();
    let names = unsafe { NSArray::from_slice(&[NSAppearanceNameAqua, NSAppearanceNameDarkAqua]) };
    appearance.bestMatchFromAppearancesWithNames(&names).is_some_and(|n| &*n == unsafe { NSAppearanceNameDarkAqua })
}

pub fn terminate() {
    NSApplication::sharedApplication(mtm()).terminate(None);
}

/// ⌘Q: through `applicationShouldTerminate`, which asks if commands are running.
pub fn request_quit() {
    terminate();
}

pub fn about() {
    NSApplication::sharedApplication(mtm()).orderFrontStandardAboutPanel(None);
}

pub fn hide() {
    NSApplication::sharedApplication(mtm()).hide(None);
}

pub fn hide_others() {
    NSApplication::sharedApplication(mtm()).hideOtherApplications(None);
}

pub fn show_all() {
    NSApplication::sharedApplication(mtm()).unhideAllApplications(None);
}

pub fn bring_all_to_front() {
    NSApplication::sharedApplication(mtm()).arrangeInFront(None);
}

// ---------------------------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------------------------

/// An alert with buttons and maybe a text field: (button chosen, field text). Runs modally.
pub fn dialog(title: &str, text: &str, buttons: &[String], field: Option<&str>) -> (Option<usize>, String) {
    let mtm = mtm();
    let a = NSAlert::new(mtm);
    a.setMessageText(&NSString::from_str(title));
    a.setInformativeText(&NSString::from_str(text));
    for b in buttons {
        a.addButtonWithTitle(&NSString::from_str(b));
    }
    let tf = field.map(|f| {
        let tf = NSTextField::textFieldWithString(&NSString::from_str(f), mtm);
        tf.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(280.0, 24.0)));
        a.setAccessoryView(Some(&tf));
        a.window().setInitialFirstResponder(Some(&tf));
        tf
    });
    let r = a.runModal();
    let chosen = (r - NSAlertFirstButtonReturn) as usize;
    let text = tf.map(|t| t.stringValue().to_string()).unwrap_or_default();
    ((chosen < buttons.len()).then_some(chosen), text)
}

pub fn alert(title: &str, text: &str) {
    dialog(title, text, &["OK".into()], None);
}

pub fn ask_native(title: &str, text: &str, yes: &str, no: &str) -> bool {
    dialog(title, text, &[yes.into(), no.into()], None).0 == Some(0)
}

// ---------------------------------------------------------------------------------------------
// Clipboard, links, cursors
// ---------------------------------------------------------------------------------------------

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn EnableSecureEventInput() -> i32;
    fn DisableSecureEventInput() -> i32;
    fn IsSecureEventInputEnabled() -> u8;
}

/// Secure Keyboard Entry: while on, event taps in other processes don't get keystrokes. Calls
/// are counted by the system, so `on` must alternate.
pub fn set_secure_input(on: bool) {
    // SAFETY: plain Carbon calls with no arguments.
    unsafe {
        if on {
            EnableSecureEventInput();
        } else {
            DisableSecureEventInput();
        }
    }
}

/// Whether any process has Secure Keyboard Entry on.
pub fn secure_input_enabled() -> bool {
    // SAFETY: a plain Carbon query.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

pub fn set_clipboard(text: &str) {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    pb.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString });
}

pub fn get_clipboard() -> Option<String> {
    let pb = NSPasteboard::generalPasteboard();
    pb.stringForType(unsafe { NSPasteboardTypeString }).map(|s| s.to_string())
}

pub fn open_url(url: &str) {
    if let Some(u) = NSURL::URLWithString(&NSString::from_str(url)) {
        NSWorkspace::sharedWorkspace().openURL(&u);
    }
}

pub fn open_path(p: &std::path::Path) {
    let _ = std::process::Command::new("/usr/bin/open").arg("-t").arg(p).spawn();
}

pub fn set_cursor(c: Cursor) {
    match c {
        Cursor::Arrow => NSCursor::arrowCursor().set(),
        Cursor::Text => NSCursor::IBeamCursor().set(),
        Cursor::Pointer => NSCursor::pointingHandCursor().set(),
        Cursor::Grab => NSCursor::closedHandCursor().set(),
    }
}

/// Input methods ask the view for the caret (`firstRectForCharacterRange`).
pub fn set_ime_area(_v: &Views, _x: f64, _y: f64, _w: f64, _h: f64) {}

// ---------------------------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------------------------

/// The AppKit objects of one window.
pub struct Views {
    pub window: Retained<NSWindow>,
    pub delegate: Retained<WindowDelegate>,
    pub view: Retained<ContentView>,
    pub titlebar_tabs: bool,
}

pub struct WinIvars {
    pub window: Cell<u64>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CTXWindowDelegate"]
    #[ivars = WinIvars]
    pub struct WindowDelegate;

    unsafe impl NSObjectProtocol for WindowDelegate {}

    unsafe impl NSWindowDelegate for WindowDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn should_close(&self, _w: &NSWindow) -> bool {
            let n = self.ivars().window.get();
            later(move |a| a.close_window(n, false));
            false
        }

        #[unsafe(method(windowDidResize:))]
        fn did_resize(&self, _n: &NSNotification) {
            let w = self.ivars().window.get();
            call(move |a| a.window_resized(w));
        }

        #[unsafe(method(windowDidMove:))]
        fn did_move(&self, _n: &NSNotification) {
            later(|a| a.save_state());
        }

        #[unsafe(method(windowDidChangeOcclusionState:))]
        fn occlusion(&self, n: &NSNotification) {
            let w = self.ivars().window.get();
            let visible = n
                .object()
                .and_then(|o| o.downcast::<NSWindow>().ok())
                .is_some_and(|win| win.occlusionState().contains(NSWindowOcclusionState::Visible));
            later(move |a| a.window_visibility(w, visible));
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn did_become_key(&self, _n: &NSNotification) {
            let w = self.ivars().window.get();
            later(move |a| a.window_key(w, true));
        }

        #[unsafe(method(windowDidResignKey:))]
        fn did_resign_key(&self, _n: &NSNotification) {
            let w = self.ivars().window.get();
            later(move |a| a.window_key(w, false));
        }

        #[unsafe(method(windowDidChangeBackingProperties:))]
        fn backing_changed(&self, _n: &NSNotification) {
            let w = self.ivars().window.get();
            later(move |a| a.scale_changed(w));
        }

        #[unsafe(method(windowDidEnterFullScreen:))]
        fn entered_full_screen(&self, n: &NSNotification) {
            let w = self.ivars().window.get();
            if let Some(win) = n.object().and_then(|o| o.downcast::<NSWindow>().ok()) {
                round_corners(&win);
            }
            later(move |a| a.window_full_screen(w, true));
        }

        #[unsafe(method(windowDidExitFullScreen:))]
        fn exited_full_screen(&self, n: &NSNotification) {
            let w = self.ivars().window.get();
            if let Some(win) = n.object().and_then(|o| o.downcast::<NSWindow>().ok()) {
                round_corners(&win);
            }
            later(move |a| a.window_full_screen(w, false));
        }
    }
);

/// Corner radius of a window without a title bar, in points.
const CORNER_RADIUS: f64 = 12.0;

/// Round a title-bar-less window's corners (square in full screen), and reshape its shadow.
fn round_corners(window: &NSWindow) {
    if window.styleMask().contains(NSWindowStyleMask::Titled) {
        return;
    }
    let full = window.styleMask().contains(NSWindowStyleMask::FullScreen);
    if let Some(layer) = window.contentView().and_then(|v| v.layer()) {
        layer.setCornerRadius(if full { 0.0 } else { CORNER_RADIUS });
        layer.setMasksToBounds(true);
    }
    window.invalidateShadow();
}

define_class!(
    /// A window without a title bar that still takes keyboard focus. With the tabs in the title
    /// bar (the default) the bar is ours: we draw the red/yellow/green buttons, which saves the
    /// ~4 MB the system's title bar costs (its buttons are SwiftUI views).
    #[unsafe(super(NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CTXWindow"]
    pub struct BarWindow;

    unsafe impl NSObjectProtocol for BarWindow {}

    impl BarWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            true
        }
    }
);

pub fn make_window(n: u64, frame: Option<[f64; 4]>, size: (f64, f64), index: usize, titlebar_tabs: bool) -> Views {
    let mtm = mtm();
    let content = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size.0, size.1));
    let window: Retained<NSWindow> = if titlebar_tabs {
        let style = NSWindowStyleMask::Borderless
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let w: Retained<BarWindow> = unsafe {
            msg_send![BarWindow::alloc(mtm), initWithContentRect: content, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false]
        };
        let w = Retained::into_super(w);
        // Clear corners around the rounded content; the shadow follows them.
        w.setOpaque(false);
        w.setBackgroundColor(Some(&NSColor::clearColor()));
        w.setHasShadow(true);
        w.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
        w
    } else {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                content,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        }
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTabbingMode(NSWindowTabbingMode::Disallowed);
    window.setRestorable(false);
    window.setContentMinSize(NSSize::new(360.0, 160.0));
    window.setColorSpace(Some(&NSColorSpace::sRGBColorSpace()));
    let wdelegate = {
        let this = WindowDelegate::alloc(mtm).set_ivars(WinIvars { window: Cell::new(n) });
        let d: Retained<WindowDelegate> = unsafe { msg_send![super(this), init] };
        d
    };
    window.setDelegate(Some(ProtocolObject::from_ref(&*wdelegate)));
    match frame {
        Some([x, y, w, h]) => window.setFrame_display(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)), false),
        None => {
            window.center();
            if index > 0 {
                let f = window.frame();
                let off = 24.0 * index as f64;
                window.setFrameTopLeftPoint(NSPoint::new(f.origin.x + off, f.origin.y + f.size.height - off));
            }
        }
    }
    let view = ContentView::new(mtm, n);
    window.setContentView(Some(&view));
    window.makeFirstResponder(Some(&view));
    round_corners(&window);
    Views { window, delegate: wdelegate, view, titlebar_tabs }
}

pub fn backing_scale(v: &Views) -> f64 {
    v.window.backingScaleFactor()
}

pub fn content_size(v: &Views) -> (f64, f64) {
    let b = v.view.bounds();
    (b.size.width, b.size.height)
}

pub fn show_window(v: &Views, activate: bool) {
    v.window.makeKeyAndOrderFront(None);
    if activate {
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm()).activateIgnoringOtherApps(true);
    }
}

/// Bring a window forward without taking keyboard focus from another app (`ctx show`).
pub fn order_front_without_focus(v: &Views) {
    v.window.orderFrontRegardless();
}

pub fn close_window(v: &Views) {
    v.window.setDelegate(None);
    v.window.orderOut(None);
    v.window.close();
}

pub fn is_key(v: &Views) -> bool {
    v.window.isKeyWindow()
}

pub fn move_window_top_left(v: &Views, x: f64, y: f64) {
    if x.is_finite() && y.is_finite() {
        v.window.setFrameTopLeftPoint(NSPoint::new(x - 40.0, y + 10.0));
    }
}

pub fn window_frame(v: &Views) -> Option<[f64; 4]> {
    let f = v.window.frame();
    Some([f.origin.x, f.origin.y, f.size.width, f.size.height])
}

pub fn set_title(v: &Views, title: &str) {
    v.window.setTitle(&NSString::from_str(title));
}

pub fn toggle_full_screen(v: &Views) {
    v.window.toggleFullScreen(None);
}

pub fn minimise(v: &Views) {
    v.window.miniaturize(None);
}

pub fn zoom(v: &Views) {
    v.window.zoom(None);
}

/// A double-click on the empty bar does what the person chose in System Settings (zoom by
/// default, or minimise, or nothing).
pub fn title_double_click(v: &Views) {
    let pref = NSUserDefaults::standardUserDefaults()
        .stringForKey(&NSString::from_str("AppleActionOnDoubleClick"))
        .map(|s| s.to_string());
    match pref.as_deref() {
        Some("Minimize") => v.window.miniaturize(None),
        Some("None") => {}
        _ => v.window.zoom(None),
    }
}

/// Move the window with the mouse (a press on the empty part of the bar).
pub fn drag_window(v: &Views) {
    if let Some(e) = NSApplication::sharedApplication(mtm()).currentEvent()
        && e.r#type() == NSEventType::LeftMouseDown
    {
        v.window.performWindowDragWithEvent(&e);
    }
}

/// Room for the red/yellow/green buttons at the left of the bar.
pub fn titlebar_inset(v: &Views) -> f64 {
    if draws_window_buttons(v) { super::chrome::TRAFFIC_LIGHTS } else { 8.0 }
}

/// The bar shows our red/yellow/green buttons: the window has no title bar. They stay in full
/// screen too (the bar is always shown there), so green leaves it the way it came in.
pub fn draws_window_buttons(v: &Views) -> bool {
    v.titlebar_tabs
}

pub fn new_surface(_v: &Views, w: usize, h: usize) -> Option<Surface> {
    Surface::new_io(w, h)
}

/// Show the window's IOSurface (the layer reads it directly: no copy).
pub fn present(v: &Views, s: &Surface, scale: f64) {
    if let Some(layer) = v.view.layer() {
        let current = unsafe { layer.contents() };
        let io = s.io_surface();
        let same = match (&current, io) {
            (Some(c), Some(io)) => {
                std::ptr::eq(Retained::as_ptr(c).cast::<u8>(), (io as *const objc2_io_surface::IOSurface).cast())
            }
            _ => false,
        };
        if !same && let Some(io) = io {
            layer.setContentsScale(scale);
            unsafe {
                layer.setContentsGravity(objc2_quartz_core::kCAGravityTopLeft);
                layer.setContents(Some(io));
            }
        }
        let _: () = unsafe { msg_send![&*layer, setContentsChanged] };
    }
}

/// Give the window's pixels back (covered, minimised, another Space).
pub fn release(v: &Views) {
    if let Some(layer) = v.view.layer() {
        unsafe { layer.setContents(None) };
    }
}

/// A point in a window's content (points, top-left origin) on the screen (Cocoa coordinates).
pub fn to_screen(v: &Views, x: f64, y: f64) -> (f64, f64) {
    let p = v.view.convertPoint_toView(NSPoint::new(x, y), None);
    let s = v.window.convertPointToScreen(p);
    (s.x, s.y)
}

/// The frontmost shown window under a screen point, and the point in its content.
pub fn window_at(app: &App, sx: f64, sy: f64) -> Option<(u64, f64, f64)> {
    let ordered = NSApplication::sharedApplication(mtm()).orderedWindows();
    for nsw in ordered.iter() {
        let Some((n, win)) = app.wins.iter().find(|(_, w)| Retained::as_ptr(&w.views.window) == Retained::as_ptr(&nsw))
        else {
            continue;
        };
        if !nsw.isVisible() {
            continue;
        }
        let f = nsw.frame();
        if !(sx >= f.origin.x && sx < f.origin.x + f.size.width && sy >= f.origin.y && sy < f.origin.y + f.size.height)
        {
            continue;
        }
        let p = nsw.convertPointFromScreen(NSPoint::new(sx, sy));
        let lp = win.views.view.convertPoint_fromView(p, None);
        return Some((*n, lp.x, lp.y));
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------------------------

const SHIFTED: &[(char, char)] = &[
    ('[', '{'),
    (']', '}'),
    ('=', '+'),
    ('-', '_'),
    ('`', '~'),
    (',', '<'),
    ('.', '>'),
    ('/', '?'),
    (';', ':'),
    ('\'', '"'),
    ('\\', '|'),
];

fn key_equivalent(keys: &str) -> (String, u64) {
    let Some((mut k, mut flags)) = keys::parse_shortcut(keys) else { return (String::new(), 0) };
    if flags & keys::SHIFT != 0
        && let Some(c) = k.chars().next()
        && let Some((_, s)) = SHIFTED.iter().find(|(a, _)| *a == c)
    {
        k = s.to_string();
        flags &= !keys::SHIFT;
    }
    (k, flags)
}

fn menu_item(title: &str, action: Sel, keys: &str, tag: isize) -> Retained<NSMenuItem> {
    let mtm = mtm();
    let (k, flags) = key_equivalent(keys);
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(&k),
        )
    };
    item.setKeyEquivalentModifierMask(NSEventModifierFlags(flags as usize));
    item.setTag(tag);
    if let Some(d) = delegate_obj() {
        unsafe { item.setTarget(Some(&d)) };
    }
    item
}

/// A shortcut as macOS writes it (⇧⌘T).
pub fn shortcut_label(keys: &str) -> String {
    keys::display(keys)
}

/// Build the menu bar from the shortcut table. Returns the action for each menu item tag.
pub fn build_menus(app: &App) -> Vec<Action> {
    let mtm = mtm();
    let main = NSMenu::new(mtm);
    let mut actions = Vec::new();
    for (title, items) in shortcuts::menus() {
        let top = NSMenuItem::new(mtm);
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        for e in items {
            match e {
                None => menu.addItem(&NSMenuItem::separatorItem(mtm)),
                Some(e) => {
                    let keys = app.bindings.get(&e.action).cloned().unwrap_or_default();
                    let item = menu_item(e.title, sel!(menuAction:), &keys, actions.len() as isize);
                    if e.hidden {
                        item.setHidden(true);
                        item.setAllowsKeyEquivalentWhenHidden(true);
                    }
                    actions.push(e.action);
                    menu.addItem(&item);
                }
            }
        }
        if title == "View" {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            let themes = NSMenuItem::new(mtm);
            themes.setTitle(&NSString::from_str("Tab Theme"));
            let sub = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Tab Theme"));
            themes.setSubmenu(Some(&sub));
            menu.addItem(&themes);
        }
        top.setSubmenu(Some(&menu));
        main.addItem(&top);
        if title == "Window" {
            NSApplication::sharedApplication(mtm).setWindowsMenu(Some(&menu));
        }
    }
    NSApplication::sharedApplication(mtm).setMainMenu(Some(&main));
    actions
}

/// Marks the Window menu's items for `ctx` windows that aren't shown.
const WINDOW_ITEM: &str = "ctx-window";

/// Refresh the dynamic parts: `ctx` windows that aren't shown (Window menu), and themes (View).
pub fn rebuild_window_menu(app: &App) {
    let mtm = mtm();
    let Some(main) = NSApplication::sharedApplication(mtm).mainMenu() else { return };
    if let Some(wm) = main.itemWithTitle(&NSString::from_str("Window")).and_then(|i| i.submenu()) {
        // Remove every item this function added before (they carry the marker), then add the
        // current ones: rebuilding never duplicates them.
        let mut i = wm.numberOfItems() - 1;
        while i >= 0 {
            if let Some(item) = wm.itemAtIndex(i)
                && item
                    .representedObject()
                    .and_then(|o| o.downcast::<NSString>().ok())
                    .is_some_and(|s| s.to_string() == WINDOW_ITEM)
            {
                wm.removeItem(&item);
            }
            i -= 1;
        }
        let hidden: Vec<&ctx_protocol::WindowInfo> =
            app.list.windows.iter().filter(|w| !app.wins.contains_key(&w.number)).collect();
        if !hidden.is_empty() {
            let sep = NSMenuItem::separatorItem(mtm);
            unsafe { sep.setRepresentedObject(Some(&NSString::from_str(WINDOW_ITEM))) };
            wm.addItem(&sep);
            for w in hidden {
                let name = match &w.name {
                    Some(n) => format!("Show Window {} \u{201c}{n}\u{201d}", w.number),
                    None => format!("Show Window {}", w.number),
                };
                let title = format!("{name} ({} {})", w.tabs.len(), if w.tabs.len() == 1 { "tab" } else { "tabs" });
                let item = menu_item(&title, sel!(showCtxWindow:), "", w.number as isize);
                unsafe { item.setRepresentedObject(Some(&NSString::from_str(WINDOW_ITEM))) };
                wm.addItem(&item);
            }
        }
    }
    // Theme submenu, with the current tab's theme checked.
    if let Some(vm) = main.itemWithTitle(&NSString::from_str("View")).and_then(|i| i.submenu())
        && let Some(sub) = vm.itemWithTitle(&NSString::from_str("Tab Theme")).and_then(|i| i.submenu())
    {
        sub.removeAllItems();
        let current = app
            .key_window()
            .and_then(|w| app.focused_tab(w))
            .and_then(|t| app.state.tab_themes.get(&t).cloned())
            .unwrap_or_else(|| app.loaded.app.theme.clone());
        for name in crate::theme::available(&app.themes_dir) {
            let title = if name == "system" { "System (Basic, light or dark)".to_string() } else { name.clone() };
            let item = menu_item(&title, sel!(setTheme:), "", 0);
            unsafe { item.setRepresentedObject(Some(&NSString::from_str(&name))) };
            item.setState(if name == current { NSControlStateValueOn } else { NSControlStateValueOff });
            sub.addItem(&item);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The content view
// ---------------------------------------------------------------------------------------------

pub struct ViewIvars {
    pub window: Cell<u64>,
    tracking: RefCell<Option<Retained<NSTrackingArea>>>,
}

fn press_of(e: &NSEvent) -> Press {
    Press {
        keycode: e.keyCode(),
        chars: e.characters().map(|s| s.to_string()).unwrap_or_default(),
        chars_ignoring: e.charactersIgnoringModifiers().map(|s| s.to_string()).unwrap_or_default(),
        flags: e.modifierFlags().0 as u64,
    }
}

fn string_of(obj: &AnyObject) -> String {
    if let Some(a) = obj.downcast_ref::<NSAttributedString>() {
        return a.string().to_string();
    }
    if let Some(s) = obj.downcast_ref::<NSString>() {
        return s.to_string();
    }
    String::new()
}

fn button_of(e: &NSEvent) -> u8 {
    match e.buttonNumber() {
        0 => 1,
        1 => 2,
        2 => 3,
        _ => 0,
    }
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CTXContentView"]
    #[ivars = ViewIvars]
    pub struct ContentView;

    unsafe impl NSObjectProtocol for ContentView {}

    impl ContentView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            true
        }

        /// The bar decides where a press moves the window (empty space only).
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn can_move_window(&self) -> bool {
            false
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {}

        #[unsafe(method(keyDown:))]
        fn key_down(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = press_of(e);
            if p.flags & keys::COMMAND != 0 {
                // A ⌘ shortcut the menus didn't take: not for the program.
                return;
            }
            NSCursor::setHiddenUntilMouseMoves(true);
            let input = with_app(|a| a.key_down(w, &p)).unwrap_or(keys::Input::TextInput);
            if input == keys::Input::TextInput {
                let events = NSArray::from_slice(&[e]);
                self.interpretKeyEvents(&events);
            }
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let flags = e.modifierFlags().0 as u64;
            let loc = self.convertPoint_fromView(self.window().map(|win| win.mouseLocationOutsideOfEventStream()).unwrap_or(NSPoint::ZERO), None);
            call(move |a| a.mouse_moved(w, loc.x, loc.y, flags));
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            if let Some(win) = self.window() {
                win.makeFirstResponder(Some(self));
            }
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let clicks = e.clickCount();
            let flags = e.modifierFlags().0 as u64;
            call(move |a| a.mouse_down(w, p.x, p.y, clicks, flags, 1));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            call(move |a| a.mouse_dragged(w, p.x, p.y, flags, 1));
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            call(move |a| a.mouse_up(w, p.x, p.y, flags, 1));
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            call(move |a| a.mouse_down(w, p.x, p.y, 1, flags, 2));
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            call(move |a| a.mouse_up(w, p.x, p.y, flags, 2));
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            let b = button_of(e);
            call(move |a| a.mouse_down(w, p.x, p.y, 1, flags, b));
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            let b = button_of(e);
            call(move |a| a.mouse_up(w, p.x, p.y, flags, b));
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            call(move |a| a.mouse_moved(w, p.x, p.y, flags));
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _e: &NSEvent) {
            let w = self.ivars().window.get();
            call(move |a| a.mouse_exited(w));
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            let precise = e.hasPreciseScrollingDeltas();
            let (dx, dy) = (e.scrollingDeltaX(), e.scrollingDeltaY());
            call(move |a| a.scroll_event(w, p.x, p.y, dx, dy, precise, flags));
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            if let Some(old) = self.ivars().tracking.borrow_mut().take() {
                self.removeTrackingArea(&old);
            }
            let opts = NSTrackingAreaOptions::MouseMoved
                | NSTrackingAreaOptions::MouseEnteredAndExited
                | NSTrackingAreaOptions::ActiveInKeyWindow
                | NSTrackingAreaOptions::InVisibleRect;
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(NSTrackingArea::alloc(), NSRect::ZERO, opts, Some(self), None)
            };
            self.addTrackingArea(&area);
            *self.ivars().tracking.borrow_mut() = Some(area);
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
        }

        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn appearance_changed(&self) {
            let dark = system_is_dark();
            later(move |a| a.appearance_changed(dark));
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            true
        }
    }

    unsafe impl NSTextInputClient for ContentView {
        #[unsafe(method(insertText:replacementRange:))]
        fn insert_text(&self, string: &AnyObject, _r: NSRange) {
            let w = self.ivars().window.get();
            let s = string_of(string);
            call(move |a| a.insert_text(w, &s));
        }

        #[unsafe(method(doCommandBySelector:))]
        fn do_command(&self, sel: Sel) {
            let w = self.ivars().window.get();
            // Keys text input didn't turn into text: as presses, so they reach whatever has the
            // keyboard (the terminal, a field).
            let code = if sel == sel!(insertNewline:) {
                36
            } else if sel == sel!(insertTab:) {
                48
            } else if sel == sel!(deleteBackward:) {
                51
            } else if sel == sel!(cancelOperation:) {
                53
            } else {
                return;
            };
            let p = Press { keycode: code, chars: String::new(), chars_ignoring: String::new(), flags: 0 };
            call(move |a| {
                a.key_down(w, &p);
            });
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        fn set_marked_text(&self, string: &AnyObject, _sel: NSRange, _rep: NSRange) {
            let w = self.ivars().window.get();
            let s = string_of(string);
            call(move |a| a.set_marked(w, Some(s)));
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            let w = self.ivars().window.get();
            call(move |a| a.set_marked(w, None));
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            NSRange::new(0, 0)
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            let w = self.ivars().window.get();
            let len = with_app(|a| {
                a.focused_tab(w).and_then(|t| a.views.get(&t)).and_then(|v| v.marked.as_ref().map(|m| m.encode_utf16().count()))
            })
            .flatten();
            match len {
                Some(n) => NSRange::new(0, n),
                None => NSRange::new(NSNotFound as usize, 0),
            }
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            let w = self.ivars().window.get();
            with_app(|a| a.has_marked(w)).unwrap_or(false)
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        fn attributed_substring(&self, _r: NSRange, _actual: *mut NSRange) -> Option<Retained<NSAttributedString>> {
            None
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes(&self) -> Retained<NSArray<NSAttributedStringKey>> {
            NSArray::new()
        }

        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        fn first_rect(&self, _r: NSRange, _actual: *mut NSRange) -> NSRect {
            let w = self.ivars().window.get();
            let r = with_app(|a| a.cursor_rect(w)).flatten();
            let local = match r {
                Some(r) => NSRect::new(NSPoint::new(r.x, r.y), NSSize::new(r.w, r.h)),
                None => NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 16.0)),
            };
            let in_window = self.convertRect_toView(local, None);
            match self.window() {
                Some(win) => win.convertRectToScreen(in_window),
                None => in_window,
            }
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index(&self, _p: NSPoint) -> NSUInteger {
            0
        }
    }

    unsafe impl NSDraggingDestination for ContentView {
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, _s: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag(&self, s: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            dropped_files(self.ivars().window.get(), s)
        }
    }
);

fn dropped_files(w: u64, s: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
    let pb = s.draggingPasteboard();
    let classes = NSArray::from_slice(&[NSURL::class()]);
    let Some(items) = (unsafe { pb.readObjectsForClasses_options(&classes, None) }) else { return false };
    let paths: Vec<String> = items
        .iter()
        .filter_map(|o| o.downcast::<NSURL>().ok())
        .filter_map(|u| u.path().map(|p| p.to_string()))
        .collect();
    if paths.is_empty() {
        return false;
    }
    later(move |a| a.drop_paths(w, paths));
    true
}

impl ContentView {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars { window: Cell::new(window), tracking: RefCell::new(None) });
        let v: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        v.setWantsLayer(true);
        v.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::Never);
        let types = NSArray::from_slice(&[unsafe { NSPasteboardTypeFileURL }]);
        v.registerForDraggedTypes(&types);
        v
    }
}
