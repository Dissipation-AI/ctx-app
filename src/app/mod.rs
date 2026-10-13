//! The app: windows that mirror `ctx` windows, each with one pixel buffer holding its tab bar,
//! its terminal tiles and any panel, and the glue between the window system's events and the
//! server. All state lives in one [`App`] on the main thread; platform callbacks reach it
//! through [`with_app`] (or [`later`] when it's busy, since some platform calls call back into
//! us synchronously).
//!
//! Everything here is the same on every platform. The platform layer (`mac`, `linux`) only makes
//! windows, shows their buffers, and turns the window system's events into calls on [`App`].

pub mod actions;
pub mod chrome;
pub mod draw;
pub mod input;
pub mod panels;
pub mod selftest;
pub mod ui;

#[cfg(target_os = "macos")]
pub mod mac;
#[cfg(target_os = "macos")]
pub use mac as plat;

#[cfg(not(target_os = "macos"))]
pub mod linux;
#[cfg(not(target_os = "macos"))]
pub use linux as plat;

use crate::client::{Client, ConnectError, Event};
use crate::config::{self, Loaded};
use crate::layout::{Edge, Node, Rect};
use crate::model::{History, Place, Screen, ViewPos};
use crate::render::{self, FontCache, Fonts, Surface};
use crate::selection::{self, Point, Selection};
use crate::shortcuts::Action;
use crate::state::AppState;
use crate::text::Row;
use crate::theme::{self, Theme};
use chrome::{BarLayout, Hit, Status};
use ctx_protocol::*;
use plat::Views;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// How long a tab shows that another client touched it.
pub const BADGE_SECONDS: u64 = 60;

// ---------------------------------------------------------------------------------------------
// The one App, and getting at it from callbacks
// ---------------------------------------------------------------------------------------------

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static DEFERRED: RefCell<VecDeque<Box<dyn FnOnce(&mut App)>>> = RefCell::new(VecDeque::new());
}

/// Run `f` with the app, or `None` if it's already in use further up the stack.
pub fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| match a.try_borrow_mut() {
        Ok(mut g) => {
            let r = g.as_mut().map(f);
            drop(g);
            flush_deferred();
            r
        }
        Err(_) => None,
    })
}

/// Run `f` with the app as soon as it's free (on the main thread).
pub fn later(f: impl FnOnce(&mut App) + 'static) {
    DEFERRED.with(|d| d.borrow_mut().push_back(Box::new(f)));
    plat::post(Box::new(flush_deferred));
}

/// Call into the app for an event from the window system: now, if the app is free and nothing
/// is queued before this, else after what's queued. Every event goes through here, so a drag is
/// never handled before the press that started it.
pub fn call(f: impl FnOnce(&mut App) + 'static) {
    let queued = DEFERRED.with(|d| !d.borrow().is_empty());
    if queued {
        later(f);
        return;
    }
    let cell = RefCell::new(Some(f));
    if with_app(|a| {
        if let Some(f) = cell.borrow_mut().take() {
            f(a)
        }
    })
    .is_none()
        && let Some(f) = cell.into_inner()
    {
        later(f);
    }
}

/// Run every deferred job the app is free for.
pub fn flush_deferred() {
    loop {
        let busy = APP.with(|a| a.try_borrow_mut().is_err());
        if busy {
            // Still in use (a modal loop further up): try again shortly.
            plat::after(15, Box::new(flush_deferred));
            return;
        }
        let Some(job) = DEFERRED.with(|d| d.borrow_mut().pop_front()) else { return };
        APP.with(|a| {
            if let Ok(mut g) = a.try_borrow_mut()
                && let Some(app) = g.as_mut()
            {
                job(app);
            }
        });
    }
}

/// Run platform work (native alerts, sheets) outside the app's borrow.
pub fn outside(f: impl FnOnce() + 'static) {
    plat::post(Box::new(f));
}

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

/// A find in one tab.
#[derive(Debug, Default)]
pub struct Find {
    pub text: String,
    pub matches: Vec<TabMatch>,
    pub current: Option<usize>,
    pub more: bool,
}

/// What the app holds for a tab it shows.
pub struct TabView {
    pub screen: Screen,
    pub history: History,
    pub pos: ViewPos,
    pub selection: Option<Selection>,
    pub find: Option<Find>,
    /// Text being composed by an input method.
    pub marked: Option<String>,
    /// The size last asked of the server.
    pub asked_size: Option<(u16, u16)>,
    pub look: Option<Look>,
    /// Places drawn last time (to map screen rows to drawn rows).
    pub drawn: Vec<Place>,
    /// Columns the view wraps scrollback lines to.
    pub columns: usize,
    /// Fractional rows from trackpad scrolling.
    pub scroll_rest: f64,
}

impl TabView {
    fn new(tab: u64) -> TabView {
        TabView {
            screen: Screen::new(tab),
            history: History::default(),
            pos: ViewPos::Live,
            selection: None,
            find: None,
            marked: None,
            asked_size: None,
            look: None,
            drawn: Vec::new(),
            columns: 80,
            scroll_rest: 0.0,
        }
    }
}

/// Rows of a tab, for selections (scrollback rows wrapped to the view's width).
pub struct TabSource<'a> {
    pub view: &'a TabView,
    pub history: &'a History,
}

impl TabSource<'_> {
    fn history_rows(&self, line: u64) -> Option<Vec<Row>> {
        self.history.get(line).map(|r| r.wrap(self.view.columns))
    }
}

impl selection::Source for TabSource<'_> {
    fn row(&self, p: Place) -> Option<Row> {
        match p {
            Place::Screen(y) => self.view.screen.lines.get(y as usize).cloned(),
            Place::History { line, sub } => self.history_rows(line)?.get(sub).cloned(),
        }
    }
    fn next(&self, p: Place) -> Option<Place> {
        match p {
            Place::Screen(y) => ((y + 1) < self.view.screen.rows).then_some(Place::Screen(y + 1)),
            Place::History { line, sub } => {
                let n = self.history_rows(line)?.len();
                if sub + 1 < n {
                    Some(Place::History { line, sub: sub + 1 })
                } else if line + 1 < self.view.screen.history_end {
                    Some(Place::History { line: line + 1, sub: 0 })
                } else {
                    Some(Place::Screen(0))
                }
            }
        }
    }
    fn prev(&self, p: Place) -> Option<Place> {
        match p {
            Place::Screen(0) => {
                let end = self.view.screen.history_end;
                if end > self.view.screen.history_first {
                    let n = self.history_rows(end - 1)?.len();
                    Some(Place::History { line: end - 1, sub: n - 1 })
                } else {
                    None
                }
            }
            Place::Screen(y) => Some(Place::Screen(y - 1)),
            Place::History { line, sub } if sub > 0 => Some(Place::History { line, sub: sub - 1 }),
            Place::History { line, .. } => {
                if line > self.view.screen.history_first {
                    let n = self.history_rows(line - 1)?.len();
                    Some(Place::History { line: line - 1, sub: n - 1 })
                } else {
                    None
                }
            }
        }
    }
}

/// Pending redraw of a window's terminal area.
#[derive(Debug, Clone, PartialEq)]
pub enum Dirty {
    All,
    Rows(BTreeSet<(u64, usize)>),
}

/// Where a window's keyboard goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Term,
    Find,
    Panel,
}

/// The find bar of a window.
#[derive(Debug, Clone, Default)]
pub struct FindBar {
    pub field: ui::Field,
    pub status: String,
}

/// A press on a tab in the bar: a click, or the start of a drag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabPress {
    pub tab: u64,
    pub index: usize,
    pub x: f64,
    pub y: f64,
    pub dragging: bool,
}

/// A shown `ctx` window.
pub struct Win {
    pub number: u64,
    pub views: Views,
    pub surface: Option<Surface>,
    pub scale: f64,
    pub visible: bool,
    pub key: bool,
    /// The tab shown when there are no tiles.
    pub selected: Option<u64>,
    /// The tile with keyboard focus.
    pub focused: Option<u64>,
    pub tiles: Option<Node>,
    /// The window's default theme (each tab may choose its own).
    pub theme_name: Option<String>,
    pub dirty: Option<Dirty>,
    /// The tab bar (and the strips) need drawing.
    pub chrome_dirty: bool,
    /// The terminal area, in points within the window's content.
    pub term_rect: Rect,
    /// Tile regions, in points within the window's content.
    pub tile_rects: Vec<(u64, Rect)>,
    pub bar: BarLayout,
    pub bar_scroll: f64,
    pub hover: Option<Hit>,
    /// Where a dragged tab would land.
    pub drop_hint: Option<Rect>,
    pub find: Option<FindBar>,
    pub focus: Focus,
    pub reopen_offer: Option<Vec<u64>>,
    /// A divider being dragged: its path in the tile tree.
    pub dragging_divider: Option<Vec<bool>>,
    /// A selection being dragged: which tab.
    pub selecting: Option<u64>,
    pub hover_link: Option<(u64, usize, usize, usize)>,
    pub tab_press: Option<TabPress>,
    pub panel: Option<panels::Panel>,
    pub menu: Option<ui::Menu>,
    pub dialog: Option<ui::Dialog>,
    pub full_screen: bool,
}

/// Something to start in a new tab, learnt from what you usually run there.
#[derive(Debug, Clone, PartialEq)]
pub struct Starter {
    pub folder: String,
    pub command: String,
    pub count: usize,
}

pub struct App {
    pub client: Option<Client>,
    events: Option<Receiver<Event>>,
    pub loaded: Loaded,
    pub config_path: PathBuf,
    pub themes_dir: PathBuf,
    pub list: TabList,
    pub wins: BTreeMap<u64, Win>,
    pub views: HashMap<u64, TabView>,
    pub state: AppState,
    state_path: PathBuf,
    pub fonts: FontCache,
    themes: HashMap<String, Theme>,
    /// Tab → when another client last touched it, and whether it typed (vs read).
    pub touched: HashMap<u64, (Instant, bool)>,
    pub activity: VecDeque<ActivityEntry>,
    callbacks: HashMap<u64, Box<dyn FnOnce(&mut App, Response)>>,
    subscribed: Vec<u64>,
    known_windows: BTreeSet<u64>,
    first_list: bool,
    /// Windows the app asked for (shown when they appear).
    want_shown: BTreeSet<u64>,
    /// A tab dropped outside every window: where its new window goes.
    pub drop_at: Option<(u64, f64, f64)>,
    pub blink_on: bool,
    pub ticker: Option<plat::Ticker>,
    pub system_dark: bool,
    pub active: bool,
    pub quitting: bool,
    pub bindings: BTreeMap<Action, String>,
    pub menu_actions: Vec<Action>,
    pub reconnecting: bool,
    pub watcher: Option<plat::watch::Watcher>,
    /// Where a plain click landed (a drag from it starts a selection).
    pub pending_anchor: Option<(u64, Point)>,
    pub offer_reopen_later: bool,
    /// After a move to another window: select the tab there.
    pub want_select_after_move: Option<(u64, u64)>,
    /// After a move to another window: tile it there (window, tab, next to, edge).
    pub pending_tile: Option<(u64, u64, u64, Edge)>,
    pub selftest_started: bool,
    pub select_when_listed: Vec<(u64, u64)>,
    /// Text to type into a tab once it appears (a starter from the + menu).
    pub type_when_listed: Vec<(u64, String)>,
    /// Keystroke-to-pixels timing: when the last key went to the server, and measured times.
    pub key_sent: Option<(u64, Instant)>,
    pub key_echo: Option<(u64, Instant)>,
    pub latencies: Vec<Duration>,
    /// What each tab is doing (for the bar and the overview).
    pub statuses: HashMap<u64, Status>,
    /// When each tab last showed new output.
    pub output_at: HashMap<u64, Instant>,
    unread: HashMap<u64, u64>,
    /// Starters for the + menu, and when they were worked out.
    pub starters: Vec<Starter>,
    pub starters_at: Option<Instant>,
}

// ---------------------------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------------------------

pub fn run() {
    plat::run();
}

/// Called once the platform has finished launching.
pub fn launched() {
    let paths = crate::client::paths();
    let loaded = config::load(&paths.config);
    let themes_dir = paths.config.parent().map(|p| p.join("themes")).unwrap_or_default();
    let state_path = crate::state::path(&paths.data);
    let state = AppState::load(&state_path);
    let (bindings, problems) = crate::shortcuts::bindings(&loaded.app.keys);
    let app = App {
        client: None,
        events: None,
        loaded,
        config_path: paths.config.clone(),
        themes_dir,
        list: TabList { windows: Vec::new() },
        wins: BTreeMap::new(),
        views: HashMap::new(),
        state,
        state_path,
        fonts: FontCache::default(),
        themes: HashMap::new(),
        touched: HashMap::new(),
        activity: VecDeque::new(),
        callbacks: HashMap::new(),
        subscribed: Vec::new(),
        known_windows: BTreeSet::new(),
        first_list: true,
        want_shown: BTreeSet::new(),
        drop_at: None,
        blink_on: true,
        ticker: None,
        system_dark: plat::system_is_dark(),
        active: true,
        quitting: false,
        bindings,
        menu_actions: Vec::new(),
        reconnecting: false,
        watcher: None,
        pending_anchor: None,
        offer_reopen_later: false,
        want_select_after_move: None,
        pending_tile: None,
        selftest_started: false,
        select_when_listed: Vec::new(),
        type_when_listed: Vec::new(),
        key_sent: None,
        key_echo: None,
        latencies: Vec::new(),
        statuses: HashMap::new(),
        output_at: HashMap::new(),
        unread: HashMap::new(),
        starters: Vec::new(),
        starters_at: None,
    };
    APP.with(|a| *a.borrow_mut() = Some(app));
    with_app(|app| {
        app.rebuild_menus();
        app.watcher = plat::watch::Watcher::start(&app.config_path);
        let mut problems = problems;
        if let Some(p) = app.loaded.problem.clone() {
            problems.insert(0, p);
        }
        if !problems.is_empty() {
            app.alert("There's a problem in the config file", &problems.join("\n"));
        }
    });
    plat::install::first_run_then(|| {
        connect();
    });
}

/// Connect to the server (starting it if needed) and subscribe.
pub fn connect() {
    let attempt = Client::connect(plat::wake);
    let result = match attempt {
        Err(ConnectError::NoServer(_)) => {
            plat::install::start_server();
            if crate::client::wait_for_server(Duration::from_secs(8)) { Client::connect(plat::wake) } else { attempt }
        }
        other => other,
    };
    match result {
        Ok((client, events)) => {
            with_app(|app| {
                app.client = Some(client);
                app.events = Some(events);
                app.reconnecting = false;
                // A new server (or a restarted one) knows nothing of our sizes and colours.
                app.subscribed.clear();
                for v in app.views.values_mut() {
                    v.asked_size = None;
                    v.look = None;
                    v.drawn.clear();
                }
                // Subscribing with nothing still sends the tab list.
                app.tell(Request::Subscribe { tabs: Vec::new() });
                app.offer_reopen_later = true;
            });
        }
        Err(e) => {
            let msg = match e {
                ConnectError::NoServer(m) | ConnectError::Mismatch(m) => m,
            };
            let ask = with_app(|app| {
                app.ask("CTX can't reach the ctx server", &msg, "Try Again", "Quit", |_, again| {
                    if again {
                        later(|_| connect());
                    } else {
                        plat::terminate();
                    }
                })
            });
            if ask.is_none() {
                plat::terminate();
            }
        }
    }
}

/// Handle everything the reader thread queued (called on the main thread after [`plat::wake`]).
pub fn drain() {
    if with_app(|app| app.drain_events()).is_none() {
        later(|app| app.drain_events());
    }
}

/// `~/code/site` for a folder under home.
pub fn short_folder(folder: &str) -> String {
    let home = crate::client::home();
    match folder.strip_prefix(&home) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => folder.to_string(),
    }
}

/// `Dev server — ~/code/site`, or just the folder.
pub fn tab_label(t: &TabInfo) -> String {
    let folder = match &t.remote_host {
        Some(h) => h.to_string(),
        None => short_folder(&t.folder),
    };
    match &t.name {
        Some(n) => format!("{n} — {folder}"),
        None => folder,
    }
}

/// A tab's two lines: what to call it, and where it is.
pub fn tab_lines(t: &TabInfo) -> (String, String) {
    let folder = match &t.remote_host {
        Some(h) => h.to_string(),
        None => short_folder(&t.folder),
    };
    let label = match (&t.name, &t.running) {
        (Some(n), _) => n.clone(),
        (None, Some(r)) => program_of(r).to_string(),
        (None, None) => folder.rsplit('/').find(|p| !p.is_empty()).unwrap_or("~").to_string(),
    };
    (label, folder)
}

/// `claude (pid 5512)` → `claude`.
pub fn agent_name(opened_by: &str) -> String {
    opened_by.split(" (").next().unwrap_or(opened_by).to_string()
}

/// The program a command line runs: `npx claude --resume` → `claude`, `/usr/bin/vim x` → `vim`.
pub fn program_of(command: &str) -> &str {
    let mut words = command.split_whitespace().filter(|w| !w.contains('='));
    let mut first = words.next().unwrap_or("");
    if matches!(first, "npx" | "bunx" | "pnpx" | "uvx" | "sudo" | "exec" | "time" | "nice" | "env") {
        first = words.find(|w| !w.starts_with('-')).unwrap_or(first);
    }
    first.rsplit('/').next().unwrap_or(first)
}

/// Programs that are AI agents, whose tabs get "working" / "waiting for you" status.
pub const AGENTS: &[&str] =
    &["claude", "codex", "gemini", "aider", "goose", "amp", "opencode", "cursor-agent", "copilot", "qwen", "crush", "bro"];

/// What a tab is doing: idle, running a command, or an agent working or waiting for you.
/// An agent counts as working while its title shows a spinner or it printed in the last few
/// seconds.
pub fn status_of(t: &TabInfo, last_output: Option<Instant>) -> Status {
    let Some(run) = &t.running else {
        return if t.state == "busy" { Status::Busy } else { Status::Idle };
    };
    let prog = program_of(run);
    if !AGENTS.contains(&prog) {
        return Status::Busy;
    }
    let spinner = t.title.as_deref().and_then(|s| s.trim_start().chars().next()).is_some_and(|c| {
        ('\u{2800}'..='\u{28ff}').contains(&c) || "◐◓◑◒⣾⣽⣻⢿⡿⣟⣯⣷✢✳✶✻✽·".contains(c) && c != '✳'
    });
    let recent = last_output.is_some_and(|at| at.elapsed() < Duration::from_millis(2500));
    if spinner || recent { Status::AgentWorking } else { Status::AgentWaiting }
}

/// A path as a shell word.
pub fn shell_quote(p: &str) -> String {
    if !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-~".contains(c)) {
        p.to_string()
    } else {
        format!("'{}'", p.replace('\'', "'\\''"))
    }
}

impl App {
    // -----------------------------------------------------------------------------------------
    // Talking to the server
    // -----------------------------------------------------------------------------------------

    pub fn tell(&mut self, r: Request) {
        if let Some(c) = self.client.as_mut() {
            c.tell(r);
        }
    }

    pub fn input(&mut self, tab: u64, i: AppInput) {
        if let Some(c) = self.client.as_mut() {
            c.input(tab, i);
        }
    }

    /// Send a request and handle its reply later, without waiting for it.
    pub fn request(&mut self, sentence: &str, r: Request, then: impl FnOnce(&mut App, Response) + 'static) {
        if let Some(c) = self.client.as_mut() {
            let id = c.send(sentence, r);
            self.callbacks.insert(id, Box::new(then));
        }
    }

    /// Send a request and wait for its reply (scrollback lines while scrolling: well under a
    /// millisecond on a local socket, and capped).
    pub fn ask_server(&mut self, sentence: &str, r: Request) -> Option<Response> {
        self.client.as_mut()?.ask(sentence, r)
    }

    fn drain_events(&mut self) {
        let mut events = Vec::new();
        if let Some(rx) = &self.events {
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
        }
        for e in events {
            match e {
                Event::Stream(s) => self.on_stream(s),
                Event::Reply(id, r) => {
                    if let Some(cb) = self.callbacks.remove(&id) {
                        cb(self, r);
                    }
                }
                Event::Disconnected(why) => self.disconnected(why),
            }
        }
        self.redraw_dirty();
    }

    fn disconnected(&mut self, why: String) {
        self.client = None;
        self.events = None;
        self.callbacks.clear();
        if self.quitting || self.reconnecting {
            return;
        }
        self.reconnecting = true;
        // launchd/systemd restarts the server (or `ctx restart server` is in progress): wait for
        // it off the main thread, so the windows keep drawing.
        std::thread::spawn(move || {
            let back = crate::client::wait_for_server(Duration::from_secs(10));
            plat::post_from_any_thread(Box::new(move || {
                if back {
                    connect();
                } else {
                    with_app(|app| {
                        app.reconnecting = false;
                        app.ask("Lost the ctx server", &why, "Reconnect", "Quit", |_, again| {
                            if again {
                                later(|_| connect());
                            } else {
                                plat::terminate();
                            }
                        });
                    });
                }
            }));
        });
    }

    fn on_stream(&mut self, s: StreamOut) {
        match s {
            StreamOut::Screen(u) => self.on_screen(u),
            StreamOut::TabsChanged(list) => self.on_tabs(list),
            StreamOut::Show { tab } => self.show_tab(tab, false),
            StreamOut::Clipboard { text, .. } => {
                // Programs may write the clipboard (OSC 52); reading it is refused by the server.
                plat::set_clipboard(&text);
            }
            StreamOut::Activity(entry) => self.on_activity(entry),
            StreamOut::Ended { .. } | StreamOut::Bytes(_) | StreamOut::Reply { .. } => {}
        }
    }

    fn on_screen(&mut self, u: ScreenUpdate) {
        let tab = u.tab;
        if let Some((t, at)) = self.key_sent
            && t == tab
        {
            self.key_sent = None;
            self.key_echo = Some((t, at));
        }
        if !u.changed.is_empty() {
            self.output_at.insert(tab, Instant::now());
        }
        let Some(view) = self.views.get_mut(&tab) else { return };
        let ch = view.screen.apply(u);
        if ch.history_moved {
            view.history.moved(view.screen.history_first, view.screen.history_end);
            view.pos = crate::model::clamp(&view.screen, view.pos);
            // Output that moves screen rows into scrollback invalidates a selection on them.
            if let Some(sel) = view.selection
                && (matches!(sel.anchor.place, Place::Screen(_)) || matches!(sel.head.place, Place::Screen(_)))
                && view.pos == ViewPos::Live
            {
                let moved = !ch.rows.is_empty();
                if moved && self.wins.values().all(|w| w.selecting != Some(tab)) {
                    view.selection = None;
                }
            }
        } else if let Some(sel) = view.selection {
            let touched = ch.rows.iter().any(|r| {
                let p = Place::Screen(*r);
                (sel.anchor.place.min(sel.head.place)..=sel.anchor.place.max(sel.head.place)).contains(&p)
            });
            if touched && self.wins.values().all(|w| w.selecting != Some(tab)) {
                view.selection = None;
            }
        }
        let scrolled = view.pos != ViewPos::Live;
        let rows: Vec<usize> =
            ch.rows.iter().filter_map(|r| view.drawn.iter().position(|p| *p == Place::Screen(*r))).collect();
        let all = ch.resized || scrolled && ch.history_moved || view.drawn.is_empty() || ch.history_moved;
        if ch.title {
            self.update_window_titles();
            self.update_statuses();
        }
        for w in self.wins.values_mut() {
            if !w.shows(tab) {
                continue;
            }
            if all {
                w.dirty = Some(Dirty::All);
            } else {
                w.add_rows(tab, &rows);
            }
        }
    }

    fn on_activity(&mut self, e: ActivityEntry) {
        if let Some(t) = e.tab {
            let typed = e.typed.is_some();
            let prev = self.touched.get(&t).copied();
            // "Typed" wins over "read" within the badge period.
            let typed = typed || prev.is_some_and(|(at, ty)| ty && at.elapsed().as_secs() < BADGE_SECONDS);
            self.touched.insert(t, (Instant::now(), typed));
            self.chrome_changed();
            self.ensure_ticker();
        }
        self.activity.push_back(e);
        while self.activity.len() > 500 {
            self.activity.pop_front();
        }
        self.panels_changed(panels::Kind::Activity);
    }

    // -----------------------------------------------------------------------------------------
    // Windows and tabs
    // -----------------------------------------------------------------------------------------

    pub fn tab_info(&self, tab: u64) -> Option<&TabInfo> {
        self.list.windows.iter().flat_map(|w| w.tabs.iter()).find(|t| t.number == tab)
    }

    pub fn window_info(&self, n: u64) -> Option<&WindowInfo> {
        self.list.windows.iter().find(|w| w.number == n)
    }

    pub fn window_of(&self, tab: u64) -> Option<u64> {
        self.tab_info(tab).map(|t| t.window)
    }

    fn is_agent_window(&self, w: &WindowInfo) -> bool {
        w.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&self.loaded.agent_window_name))
            || (!w.tabs.is_empty() && w.tabs.iter().all(|t| t.opened_by != "you"))
    }

    pub fn agent_window(&self) -> Option<u64> {
        self.list.windows.iter().find(|w| w.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&self.loaded.agent_window_name))).map(|w| w.number)
    }

    fn on_tabs(&mut self, list: TabList) {
        // The tab order before this change, to pick a neighbour when the selected tab closes.
        let before: HashMap<u64, Vec<u64>> =
            self.list.windows.iter().map(|w| (w.number, w.tabs.iter().map(|t| t.number).collect())).collect();
        for t in list.windows.iter().flat_map(|w| w.tabs.iter()) {
            if self.unread.insert(t.number, t.unread_lines).is_some_and(|u| u != t.unread_lines) {
                self.output_at.insert(t.number, Instant::now());
            }
        }
        self.list = list;
        let windows: Vec<u64> = self.list.windows.iter().map(|w| w.number).collect();
        let tabs: Vec<u64> = self.list.windows.iter().flat_map(|w| w.tabs.iter().map(|t| t.number)).collect();
        // Which windows to show.
        let mut show: Vec<u64> = Vec::new();
        if self.first_list {
            self.first_list = false;
            let saved: Vec<u64> = self.state.windows.keys().copied().filter(|w| windows.contains(w)).collect();
            if !saved.is_empty() {
                show = saved;
            } else {
                show = self.list.windows.iter().filter(|w| !self.is_agent_window(w)).map(|w| w.number).collect();
            }
            if show.is_empty() {
                self.new_window(None);
            }
            self.known_windows = windows.iter().copied().collect();
            self.maybe_offer_reopen();
        } else {
            for w in &self.list.windows {
                if !self.known_windows.contains(&w.number) {
                    let ours = self.want_shown.remove(&w.number);
                    let dropped = self.drop_at.is_some_and(|(t, _, _)| w.tabs.iter().any(|x| x.number == t));
                    if ours || dropped || !self.is_agent_window(w) {
                        show.push(w.number);
                    }
                }
            }
            self.known_windows = windows.iter().copied().collect();
        }
        for w in show {
            self.open_win(w);
        }
        if !self.wins.is_empty() && !self.selftest_started && std::env::var_os("CTX_APP_SELFTEST").is_some() {
            self.selftest_started = true;
            plat::after(1500, Box::new(selftest::start));
        }
        // Windows that went away.
        let gone: Vec<u64> = self.wins.keys().copied().filter(|w| !windows.contains(w)).collect();
        for w in gone {
            if let Some(win) = self.wins.remove(&w) {
                plat::close_window(&win.views);
            }
            self.state.windows.remove(&w);
        }
        // Keep each window's selection, focus and tiles valid.
        for (n, win) in self.wins.iter_mut() {
            let info = self.list.windows.iter().find(|w| w.number == *n);
            let wtabs: Vec<u64> = info.map(|i| i.tabs.iter().map(|t| t.number).collect()).unwrap_or_default();
            if let Some(t) = win.tiles.take() {
                let mut node = Some(t);
                for x in node.as_ref().map(|n| n.tabs()).unwrap_or_default() {
                    if !wtabs.contains(&x) {
                        node = node.and_then(|n| n.remove(x));
                    }
                }
                win.tiles = node.filter(|n| matches!(n, Node::Split { .. }));
            }
            if win.selected.is_none_or(|s| !wtabs.contains(&s)) {
                // The tab that took its place (the one to its right, else its left), as in
                // Terminal.app and browsers.
                let old = before.get(n).cloned().unwrap_or_default();
                win.selected = neighbour_after_close(&old, &wtabs, win.selected);
            }
            if win.focused.is_none_or(|s| !wtabs.contains(&s)) {
                win.focused = win.selected;
            }
            win.dirty = Some(Dirty::All);
            win.chrome_dirty = true;
        }
        // A dropped tab's new window goes where it was dropped.
        if let Some((t, x, y)) = self.drop_at
            && let Some(w) = self.window_of(t)
            && let Some(win) = self.wins.get(&w)
        {
            plat::move_window_top_left(&win.views, x, y);
            self.drop_at = None;
        }
        self.views.retain(|t, _| tabs.contains(t));
        self.touched.retain(|t, _| tabs.contains(t));
        self.output_at.retain(|t, _| tabs.contains(t));
        self.unread.retain(|t, _| tabs.contains(t));
        self.state.prune(&windows, &tabs);
        // Tabs the app opened, now listed: select them.
        for (w, t) in std::mem::take(&mut self.select_when_listed) {
            if self.window_of(t) == Some(w) {
                self.open_win(w);
                self.select_tab(w, t);
                if let Some(win) = self.wins.get(&w) {
                    plat::show_window(&win.views, true);
                }
            } else if self.window_of(t).is_none() {
                self.select_when_listed.push((w, t));
            }
        }
        // Finish drags that moved a tab to another window.
        if let Some((w, t)) = self.want_select_after_move
            && self.window_of(t) == Some(w)
        {
            self.want_select_after_move = None;
            self.select_tab(w, t);
        }
        if let Some((w, t, target, edge)) = self.pending_tile
            && self.window_of(t) == Some(w)
        {
            self.pending_tile = None;
            self.tile(w, t, Some(target), edge);
        }
        // Starters waiting for their tab's shell.
        for (t, text) in std::mem::take(&mut self.type_when_listed) {
            match self.tab_info(t) {
                Some(info) if info.shell_integration || info.state == "idle" => {
                    let r = Request::Type { tab: TabRef::Number(t), text: text.clone(), then_press: None };
                    self.request(&format!("ctx type into tab {t}"), r, |_, _| {});
                }
                Some(_) => self.type_when_listed.push((t, text)),
                None => {}
            }
        }
        self.update_statuses();
        self.update_window_titles();
        self.layout_all();
        self.update_subscriptions();
        self.save_state();
        plat::rebuild_window_menu(self);
        self.panels_changed(panels::Kind::History);
        self.panels_changed(panels::Kind::Overview);
    }

    /// Work out each tab's status; redraw the bars when one changes.
    pub fn update_statuses(&mut self) {
        let mut next = HashMap::new();
        for t in self.list.windows.iter().flat_map(|w| w.tabs.iter()) {
            next.insert(t.number, status_of(t, self.output_at.get(&t.number).copied()));
        }
        if next != self.statuses {
            self.statuses = next;
            self.chrome_changed();
            self.panels_changed(panels::Kind::Overview);
        }
        if self.statuses.values().any(|s| *s == Status::AgentWorking) {
            self.ensure_ticker();
        }
    }

    /// Show a `ctx` window in a window of ours.
    pub fn open_win(&mut self, n: u64) {
        if self.wins.contains_key(&n) {
            return;
        }
        let saved = self.state.windows.get(&n).cloned().unwrap_or_default();
        let theme_name = saved.theme.clone();
        let info_tabs: Vec<u64> =
            self.window_info(n).map(|w| w.tabs.iter().map(|t| t.number).collect()).unwrap_or_default();
        let selected = saved.selected.filter(|t| info_tabs.contains(t)).or(info_tabs.first().copied());
        let fonts = self.fonts.get(&self.loaded.app.font, self.loaded.app.font_size, 2.0);
        let (cw, chh) = (fonts.cell_w / 2.0, fonts.cell_h / 2.0);
        let default_size =
            (80.0 * cw + 2.0 * render::PAD_X, 24.0 * chh + 2.0 * render::PAD_Y + chrome::BAR_H);
        let views = plat::make_window(n, saved.frame, default_size, self.wins.len(), self.loaded.app.tabs_in_titlebar);
        let scale = plat::backing_scale(&views);
        let win = Win {
            number: n,
            views,
            surface: None,
            scale,
            visible: true,
            key: false,
            selected,
            focused: selected,
            tiles: saved.tiles.clone(),
            theme_name,
            dirty: Some(Dirty::All),
            chrome_dirty: true,
            term_rect: Rect::new(0.0, chrome::BAR_H, default_size.0, default_size.1 - chrome::BAR_H),
            tile_rects: Vec::new(),
            bar: chrome::bar_layout(&[], default_size.0, 8.0, 0.0),
            bar_scroll: 0.0,
            hover: None,
            drop_hint: None,
            find: None,
            focus: Focus::Term,
            reopen_offer: None,
            dragging_divider: None,
            selecting: None,
            hover_link: None,
            tab_press: None,
            panel: None,
            menu: None,
            dialog: None,
            full_screen: false,
        };
        self.wins.insert(n, win);
        self.state.windows.entry(n).or_insert(saved);
        if let Some(w) = self.wins.get(&n) {
            plat::show_window(&w.views, true);
        }
        self.layout(n);
        self.update_subscriptions();
        self.save_state();
    }

    /// Bring a tab forward: open its window if needed and select it. `activate`: also make the
    /// app active (user actions); `ctx show` from an agent doesn't steal focus from other apps.
    pub fn show_tab(&mut self, tab: u64, activate: bool) {
        let Some(w) = self.window_of(tab) else { return };
        self.open_win(w);
        self.select_tab(w, tab);
        if let Some(win) = self.wins.get(&w) {
            if activate {
                plat::show_window(&win.views, true);
            } else {
                plat::order_front_without_focus(&win.views);
            }
        }
    }

    pub fn select_tab(&mut self, w: u64, tab: u64) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        match &mut win.tiles {
            Some(tiles) if tiles.contains(tab) => win.focused = Some(tab),
            Some(tiles) => {
                // Selecting a tab that isn't tiled puts it in the focused tile.
                if let Some(f) = win.focused {
                    tiles.replace(f, tab);
                }
                win.focused = Some(tab);
                win.selected = Some(tab);
            }
            None => {
                win.selected = Some(tab);
                win.focused = Some(tab);
            }
        }
        win.dirty = Some(Dirty::All);
        win.chrome_dirty = true;
        win.bar_scroll = win.bar.scroll_to(tab, win.bar_scroll);
        if let Some(st) = self.state.windows.get_mut(&w) {
            st.selected = Some(tab);
            st.tiles = win.tiles.clone();
        }
        self.views.entry(tab).or_insert_with(|| TabView::new(tab));
        self.update_window_titles();
        self.layout(w);
        self.update_subscriptions();
        self.save_state();
        self.redraw_dirty();
    }

    pub fn new_window(&mut self, folder: Option<String>) {
        self.request("ctx open window", Request::OpenWindow { folder, name: None }, |app, r| match r {
            Response::Ok(Output::Opened(o)) => {
                app.want_shown.insert(o.window);
                if app.window_info(o.window).is_some() {
                    app.open_win(o.window);
                }
                if let Some(t) = o.tab {
                    app.pending_select(o.window, t);
                }
            }
            Response::Error(e) => app.error(&e.message),
            _ => {}
        });
    }

    fn pending_select(&mut self, w: u64, tab: u64) {
        if self.wins.contains_key(&w) && self.window_of(tab) == Some(w) {
            self.select_tab(w, tab);
            if let Some(win) = self.wins.get(&w) {
                plat::show_window(&win.views, true);
            }
        } else {
            // The tab list with it hasn't arrived yet: select it when it does.
            self.want_shown.insert(w);
            self.select_when_listed.push((w, tab));
        }
    }

    /// A new tab in window `w`, in `folder` (default: the current tab's), typing `command` at its
    /// prompt once its shell is ready (the + menu's starters).
    pub fn new_tab_with(&mut self, w: u64, folder: Option<String>, command: Option<String>) {
        let folder = folder.or_else(|| self.focused_tab(w).and_then(|t| self.tab_info(t)).map(|t| t.folder.clone()));
        let r = Request::OpenTab { window: Some(WindowRef::Number(w)), folder, name: None };
        self.request("ctx open tab", r, move |app, r| match r {
            Response::Ok(Output::Opened(o)) => {
                if let Some(t) = o.tab {
                    app.pending_select(o.window, t);
                    if let Some(c) = command {
                        app.type_when_listed.push((t, c));
                    }
                }
            }
            Response::Error(e) => app.error(&e.message),
            _ => {}
        });
    }

    pub fn new_tab(&mut self, w: u64) {
        self.new_tab_with(w, None, None);
    }

    pub fn close_tab(&mut self, tab: u64, without_asking: bool) {
        let r = Request::CloseTab { tab: TabRef::Number(tab), without_asking };
        self.request(&format!("ctx close tab {tab}"), r, move |app, r| {
            if let Response::Error(e) = r {
                if e.error == ErrorKind::NeedsConfirmation && !without_asking {
                    app.ask("Close this tab?", &e.message, "Close", "Cancel", move |app, yes| {
                        if yes {
                            app.close_tab(tab, true);
                        }
                    });
                } else {
                    app.error(&e.message);
                }
            }
        });
    }

    pub fn close_window(&mut self, w: u64, without_asking: bool) {
        let r = Request::CloseWindow { window: WindowRef::Number(w), without_asking };
        self.request(&format!("ctx close window {w}"), r, move |app, r| {
            if let Response::Error(e) = r {
                if e.error == ErrorKind::NeedsConfirmation && !without_asking {
                    app.ask("Close this window?", &e.message, "Close", "Cancel", move |app, yes| {
                        if yes {
                            app.close_window(w, true);
                        }
                    });
                } else if e.error == ErrorKind::NoSuchWindow {
                    // Already gone on the server: just close ours.
                    if let Some(win) = app.wins.remove(&w) {
                        plat::close_window(&win.views);
                    }
                } else {
                    app.error(&e.message);
                }
            }
        });
    }

    pub fn move_tab(&mut self, tab: u64, to: WindowRef, position: Option<u32>) {
        let sentence = match &to {
            WindowRef::Number(n) => format!("ctx move tab {tab} to window {n}"),
            WindowRef::Name(n) => format!("ctx move tab {tab} to window \"{n}\""),
            WindowRef::New => format!("ctx move tab {tab} to new window"),
        };
        if matches!(to, WindowRef::New) {
            // The new window appears in the next tab list; show it.
            self.drop_at.get_or_insert((tab, f64::NAN, f64::NAN));
        }
        self.request(&sentence, Request::Move { tab: TabRef::Number(tab), to, position }, |app, r| {
            if let Response::Error(e) = r {
                app.drop_at = None;
                app.error(&e.message);
            }
        });
    }

    pub fn reopen_last(&mut self) {
        self.request("ctx list closed tabs", Request::ListClosedTabs { limit: 1 }, |app, r| match r {
            Response::Ok(Output::ClosedTabs(c)) => match c.tabs.first() {
                Some(t) => app.reopen(t.number),
                None => app.error("No closed tabs are saved."),
            },
            Response::Error(e) => app.error(&e.message),
            _ => {}
        });
    }

    pub fn reopen(&mut self, tab: u64) {
        self.request(&format!("ctx reopen tab {tab}"), Request::Reopen { tab }, |app, r| match r {
            Response::Ok(Output::Opened(o)) => {
                if let Some(t) = o.tab {
                    app.want_shown.insert(o.window);
                    app.pending_select(o.window, t);
                }
            }
            Response::Error(e) => app.error(&e.message),
            _ => {}
        });
    }

    pub fn rename_tab(&mut self, tab: u64, to: String) {
        let r = Request::RenameTab { tab: TabRef::Number(tab), to: to.clone() };
        self.request(&format!("ctx rename tab {tab} to \"{to}\""), r, |app, r| {
            if let Response::Error(e) = r {
                app.error(&e.message);
            }
        });
    }

    pub fn error(&mut self, message: &str) {
        self.alert("ctx", message);
    }

    // -----------------------------------------------------------------------------------------
    // Layout, subscriptions, looks
    // -----------------------------------------------------------------------------------------

    pub fn focused_tab(&self, w: u64) -> Option<u64> {
        let win = self.wins.get(&w)?;
        win.focused.or(win.selected)
    }

    pub fn key_window(&self) -> Option<u64> {
        self.wins
            .iter()
            .find(|(_, w)| plat::is_key(&w.views))
            .map(|(n, _)| *n)
            .or_else(|| self.wins.values().find(|w| w.key).map(|w| w.number))
            .or_else(|| self.wins.keys().next().copied())
    }

    pub fn tab_fonts(&mut self, tab: u64, scale: f64) -> Rc<Fonts> {
        let size = self.state.font_sizes.get(&tab).copied().unwrap_or(self.loaded.app.font_size);
        let family = self.loaded.app.font.clone();
        self.fonts.get(&family, size, scale)
    }

    /// A theme by name (`None`: the config's), cached.
    pub fn theme(&mut self, name: Option<&str>) -> Theme {
        let name = name.unwrap_or(&self.loaded.app.theme).to_string();
        let key = format!("{name}/{}", self.system_dark);
        if let Some(t) = self.themes.get(&key) {
            return t.clone();
        }
        let t = theme::load(&name, &self.themes_dir, self.system_dark).unwrap_or_else(|_| theme::ghostty());
        self.themes.insert(key, t.clone());
        t
    }

    /// The theme a tab is drawn in: its own, else its window's, else the config's.
    pub fn tab_theme(&mut self, tab: u64) -> Theme {
        let name = self
            .state
            .tab_themes
            .get(&tab)
            .cloned()
            .or_else(|| self.window_of(tab).and_then(|w| self.wins.get(&w)).and_then(|w| w.theme_name.clone()));
        self.theme(name.as_deref())
    }

    pub fn layout_all(&mut self) {
        let ws: Vec<u64> = self.wins.keys().copied().collect();
        for w in ws {
            self.layout(w);
        }
    }

    /// Lay out the bar, strips and terminal area, compute tiles, and size each visible tab.
    pub fn layout(&mut self, w: u64) {
        let Some(win) = self.wins.get(&w) else { return };
        let (cw, ch) = plat::content_size(&win.views);
        let ids: Vec<u64> = self.window_info(w).map(|i| i.tabs.iter().map(|t| t.number).collect()).unwrap_or_default();
        let inset = plat::titlebar_inset(&win.views);
        let bar = chrome::bar_layout(&ids, cw, inset, win.bar_scroll);
        let mut top = chrome::BAR_H;
        if win.reopen_offer.is_some() {
            top += chrome::STRIP_H;
        }
        let bottom = if win.find.is_some() { chrome::STRIP_H } else { 0.0 };
        let area = Rect::new(0.0, top, cw, (ch - top - bottom).max(1.0));
        let rects = match &win.tiles {
            Some(n) => n.rects(area),
            None => win.selected.map(|t| vec![(t, area)]).unwrap_or_default(),
        };
        let scale = plat::backing_scale(&win.views);
        let mut sizes = Vec::new();
        for (t, r) in &rects {
            let fonts = self.tab_fonts(*t, scale);
            sizes.push((*t, render::grid_size(&fonts, r.w * scale, r.h * scale)));
        }
        let win = self.wins.get_mut(&w).unwrap();
        if win.tile_rects != rects || (win.scale - scale).abs() > f64::EPSILON || win.term_rect != area {
            win.dirty = Some(Dirty::All);
        }
        if win.bar != bar {
            win.chrome_dirty = true;
        }
        win.bar = bar;
        win.term_rect = area;
        win.tile_rects = rects;
        win.scale = scale;
        for (t, (c, r)) in sizes {
            let view = self.views.entry(t).or_insert_with(|| TabView::new(t));
            view.columns = c as usize;
            if view.asked_size != Some((c, r)) {
                view.asked_size = Some((c, r));
                if let Some(cl) = self.client.as_mut() {
                    cl.input(t, AppInput::Resize { columns: c, rows: r });
                }
            }
        }
    }

    /// Subscribe to exactly the tabs on screen in visible windows, and give each its colours.
    pub fn update_subscriptions(&mut self) {
        let mut tabs: Vec<u64> = Vec::new();
        for win in self.wins.values() {
            if !win.visible || win.panel.is_some() {
                continue;
            }
            for (t, _) in &win.tile_rects {
                if !tabs.contains(t) {
                    tabs.push(*t);
                }
            }
        }
        let shape = self.loaded.app.cursor_shape();
        let blink = self.loaded.app.cursor_blink;
        for t in tabs.clone() {
            let look = self.tab_theme(t).look(shape, blink);
            let view = self.views.entry(t).or_insert_with(|| TabView::new(t));
            if view.look.as_ref() != Some(&look) {
                view.look = Some(look.clone());
                if let Some(c) = self.client.as_mut() {
                    c.tell(Request::SetLook { tab: t, look });
                }
            }
        }
        tabs.sort();
        if tabs != self.subscribed {
            for t in &tabs {
                if !self.subscribed.contains(t)
                    && let Some(v) = self.views.get_mut(t)
                {
                    // A fresh subscription starts with the whole screen.
                    v.drawn.clear();
                }
            }
            self.subscribed = tabs.clone();
            self.tell(Request::Subscribe { tabs });
        }
    }

    pub fn save_state(&mut self) {
        for (n, win) in &self.wins {
            let st = self.state.windows.entry(*n).or_default();
            st.selected = win.selected;
            st.tiles = win.tiles.clone();
            st.theme = win.theme_name.clone();
            if !win.full_screen
                && let Some(f) = plat::window_frame(&win.views)
            {
                st.frame = Some(f);
            }
        }
        let _ = self.state.save(&self.state_path);
    }

    /// The bars need drawing (tabs, badges, statuses changed).
    pub fn chrome_changed(&mut self) {
        for w in self.wins.values_mut() {
            w.chrome_dirty = true;
        }
    }

    pub fn update_window_titles(&mut self) {
        for (n, win) in &self.wins {
            let t = win.focused.or(win.selected);
            let info = t.and_then(|t| self.tab_info(t));
            let title = match info {
                Some(i) => {
                    let label = tab_label(i);
                    match &i.title {
                        Some(x) if !x.is_empty() => format!("{label} — {x}"),
                        _ => label,
                    }
                }
                None => match self.window_info(*n).and_then(|w| w.name.clone()) {
                    Some(name) => name,
                    None => "CTX".into(),
                },
            };
            plat::set_title(&win.views, &title);
        }
    }

    pub fn dirty_tab(&mut self, tab: u64) {
        for w in self.wins.values_mut() {
            if w.shows(tab) {
                w.dirty = Some(Dirty::All);
            }
        }
    }

    pub fn dirty_win(&mut self, w: u64) {
        if let Some(win) = self.wins.get_mut(&w) {
            win.dirty = Some(Dirty::All);
            win.chrome_dirty = true;
        }
    }

    // -----------------------------------------------------------------------------------------
    // Alerts and questions
    // -----------------------------------------------------------------------------------------

    /// Tell the person something (a native alert on macOS, a drawn one elsewhere).
    pub fn alert(&mut self, title: &str, text: &str) {
        self.ask_with(title, text, vec!["OK".into()], None, |_, _, _| {});
    }

    /// A question with two buttons; `then` gets true for the first.
    pub fn ask(&mut self, title: &str, text: &str, yes: &str, no: &str, then: impl FnOnce(&mut App, bool) + 'static) {
        self.ask_with(title, text, vec![yes.into(), no.into()], None, move |app, b, _| then(app, b == Some(0)));
    }

    /// Ask for a line of text.
    pub fn prompt(&mut self, title: &str, info: &str, default: &str, then: impl FnOnce(&mut App, Option<String>) + 'static) {
        self.ask_with(title, info, vec!["OK".into(), "Cancel".into()], Some(default.to_string()), move |app, b, text| {
            let t = text.trim().to_string();
            then(app, (b == Some(0) && !t.is_empty()).then_some(t));
        });
    }

    fn ask_with(
        &mut self,
        title: &str,
        text: &str,
        buttons: Vec<String>,
        field: Option<String>,
        then: impl FnOnce(&mut App, Option<usize>, String) + 'static,
    ) {
        if plat::NATIVE_DIALOGS {
            let (title, text) = (title.to_string(), text.to_string());
            outside(move || {
                let (b, t) = plat::dialog(&title, &text, &buttons, field.as_deref());
                later(move |app| then(app, b, t));
            });
            return;
        }
        let Some(w) = self.key_window() else { return };
        let Some(win) = self.wins.get_mut(&w) else { return };
        if win.dialog.is_some() {
            // One at a time: answer this one after the one showing.
            let (title, text) = (title.to_string(), text.to_string());
            plat::after(300, Box::new(move || later(move |app| app.ask_with(&title, &text, buttons, field, then))));
            return;
        }
        win.dialog = Some(ui::Dialog {
            title: title.to_string(),
            text: text.to_string(),
            buttons,
            field: field.map(|f| ui::Field::with_text("", &f)),
            done: Some(Box::new(then)),
            hover: None,
        });
        win.dirty = Some(Dirty::All);
        plat::show_window(&win.views, true);
        self.ensure_ticker();
        self.redraw_dirty();
    }

    /// Answer the dialog in window `w`.
    pub fn dialog_done(&mut self, w: u64, button: Option<usize>) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        let Some(mut d) = win.dialog.take() else { return };
        win.dirty = Some(Dirty::All);
        let text = d.field.as_ref().map(|f| f.text.clone()).unwrap_or_default();
        if let Some(done) = d.done.take() {
            done(self, button, text);
        }
        self.redraw_dirty();
    }
}

/// When the selected tab closes, the tab that took its place: the one after it, else the one
/// before it, else the first.
pub fn neighbour_after_close(old: &[u64], now: &[u64], selected: Option<u64>) -> Option<u64> {
    if let Some(s) = selected
        && let Some(i) = old.iter().position(|t| *t == s)
    {
        if let Some(t) = old[i + 1..].iter().find(|t| now.contains(t)) {
            return Some(*t);
        }
        if let Some(t) = old[..i].iter().rev().find(|t| now.contains(t)) {
            return Some(*t);
        }
    }
    now.first().copied()
}

impl Win {
    pub fn shows(&self, tab: u64) -> bool {
        self.tile_rects.iter().any(|(t, _)| *t == tab)
    }

    fn add_rows(&mut self, tab: u64, rows: &[usize]) {
        if rows.is_empty() {
            return;
        }
        match &mut self.dirty {
            Some(Dirty::All) => {}
            Some(Dirty::Rows(set)) => set.extend(rows.iter().map(|r| (tab, *r))),
            None => self.dirty = Some(Dirty::Rows(rows.iter().map(|r| (tab, *r)).collect())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_paths() {
        assert_eq!(shell_quote("/tmp/a.txt"), "/tmp/a.txt");
        assert_eq!(shell_quote("/tmp/My File.txt"), "'/tmp/My File.txt'");
        assert_eq!(shell_quote("/tmp/it's"), "'/tmp/it'\\''s'");
    }

    fn info(n: u64) -> TabInfo {
        TabInfo {
            number: n,
            name: Some("Dev server".into()),
            folder: format!("{}/code/site", crate::client::home()),
            window: 1,
            opened_by: "claude (pid 5512)".into(),
            title: None,
            state: "idle".into(),
            running: None,
            pid: None,
            started: None,
            unread_lines: 0,
            columns: 80,
            rows: 24,
            shell_integration: true,
            permissions_of: None,
            remote_host: None,
        }
    }

    #[test]
    fn labels() {
        let mut t = info(3);
        assert_eq!(tab_label(&t), "Dev server — ~/code/site");
        assert_eq!(tab_lines(&t), ("Dev server".into(), "~/code/site".into()));
        assert_eq!(agent_name(&t.opened_by), "claude");
        t.name = None;
        assert_eq!(tab_lines(&t).0, "site");
        t.running = Some("npx claude --resume".into());
        assert_eq!(tab_lines(&t).0, "claude");
    }

    #[test]
    fn statuses() {
        let mut t = info(1);
        assert_eq!(status_of(&t, None), Status::Idle);
        t.running = Some("npm test".into());
        t.state = "busy".into();
        assert_eq!(status_of(&t, None), Status::Busy);
        t.running = Some("claude".into());
        assert_eq!(status_of(&t, None), Status::AgentWaiting);
        assert_eq!(status_of(&t, Some(Instant::now())), Status::AgentWorking);
        t.title = Some("⠐ Fixing the auth test".into());
        assert_eq!(status_of(&t, None), Status::AgentWorking);
        t.title = Some("✳ Fixing the auth test".into());
        assert_eq!(status_of(&t, None), Status::AgentWaiting);
        assert_eq!(program_of("FOO=1 /usr/local/bin/codex exec"), "codex");
    }

    #[test]
    fn closing_selects_the_neighbour() {
        assert_eq!(neighbour_after_close(&[1, 2, 3], &[1, 3], Some(2)), Some(3));
        assert_eq!(neighbour_after_close(&[1, 2, 3], &[1, 2], Some(3)), Some(2));
        assert_eq!(neighbour_after_close(&[1, 2, 3], &[2, 3], Some(1)), Some(2));
        assert_eq!(neighbour_after_close(&[], &[5], None), Some(5));
        assert_eq!(neighbour_after_close(&[1], &[], Some(1)), None);
    }
}
