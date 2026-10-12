//! The app: windows that mirror `ctx` windows, a tab bar, one terminal surface per window, and
//! the glue between AppKit events and the server. All state lives in one [`App`] on the main
//! thread; AppKit callbacks reach it through [`with_app`] (or [`later`] when it's busy, since
//! some AppKit calls call back into us synchronously).

pub mod cocoa;
pub mod install;
pub mod panels;
pub mod selftest;
pub mod watch;

use crate::client::{Client, ConnectError, Event};
use crate::config::{self, Loaded};
use crate::keys::{self, MetaKeys, Press};
use crate::layout::{self, Edge, Node, Rect};
use crate::model::{self, Composed, History, Place, Screen, ViewPos};
use crate::render::{self, CursorFrame, Damage, FontCache, Fonts, Surface, TileFrame};
use crate::selection::{self, Point, Selection, Unit};
use crate::shortcuts::Action;
use crate::state::AppState;
use crate::text::Row;
use crate::theme::{self, Theme};
use cocoa::{TabItem, Views};
use ctx_protocol::*;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// Height of the tab bar, in points.
pub const TAB_BAR_HEIGHT: f64 = 28.0;
/// Height of the find bar and the reopen bar.
pub const BAR_HEIGHT: f64 = 30.0;
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
    dispatch2::DispatchQueue::main().exec_async(flush_deferred_soon);
}

fn flush_deferred_soon() {
    flush_deferred();
}

fn flush_deferred() {
    loop {
        let busy = APP.with(|a| a.try_borrow_mut().is_err());
        if busy {
            // Still in use (a modal loop further up): try again shortly.
            let when = dispatch2::DispatchTime::NOW.time(Duration::from_millis(15).as_nanos() as i64);
            let _ = dispatch2::DispatchQueue::main().after(when, flush_deferred_soon);
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

/// Run AppKit work (alerts, sheets) outside the app's borrow.
pub fn outside(f: impl FnOnce() + 'static) {
    struct SendBox(Box<dyn FnOnce()>);
    // SAFETY: only ever run on the main queue, the thread it was made on.
    unsafe impl Send for SendBox {}
    let b = SendBox(Box::new(f));
    dispatch2::DispatchQueue::main().exec_async(move || {
        let b = b;
        (b.0)()
    });
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

/// Pending redraw of a window.
#[derive(Debug, Clone, PartialEq)]
pub enum Dirty {
    All,
    Rows(BTreeSet<(u64, usize)>),
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
    pub theme_name: Option<String>,
    pub theme: Theme,
    pub dirty: Option<Dirty>,
    /// Tile regions, in points within the terminal view.
    pub tile_rects: Vec<(u64, Rect)>,
    /// Where a dragged tab would land.
    pub drop_hint: Option<Rect>,
    pub find_open: bool,
    pub reopen_offer: Option<Vec<u64>>,
    /// A divider being dragged: its path in the tile tree.
    pub dragging_divider: Option<Vec<bool>>,
    /// A selection being dragged: which tab.
    pub selecting: Option<u64>,
    pub hover_link: Option<(u64, usize, usize, usize)>,
}

/// What a drag of a tab is doing.
#[derive(Debug, Clone)]
pub struct TabDrag {
    pub tab: u64,
    pub from_window: u64,
    pub started: bool,
}

pub struct App {
    pub mtm: MainThreadMarker,
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
    pub drag: Option<TabDrag>,
    pub blink_on: bool,
    pub ticker: Option<Retained<objc2_foundation::NSTimer>>,
    pub system_dark: bool,
    pub active: bool,
    pub quitting: bool,
    pub panels: panels::Panels,
    pub bindings: BTreeMap<Action, String>,
    pub menu_actions: Vec<Action>,
    pub reconnecting: bool,
    pub watcher: Option<watch::Watcher>,
    /// Where a plain click landed (a drag from it starts a selection).
    pub pending_anchor: Option<(u64, Point)>,
    pub offer_reopen_later: bool,
    /// After a move to another window: select the tab there.
    pub want_select_after_move: Option<(u64, u64)>,
    /// After a move to another window: tile it there (window, tab, next to, edge).
    pub pending_tile: Option<(u64, u64, u64, Edge)>,
    pub selftest_started: bool,
    pub select_when_listed: Vec<(u64, u64)>,
    /// Keystroke-to-pixels timing: when the last key went to the server, and measured times.
    pub key_sent: Option<(u64, Instant)>,
    pub key_echo: Option<(u64, Instant)>,
    pub latencies: Vec<Duration>,
}

// ---------------------------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------------------------

pub fn run() {
    let mtm = MainThreadMarker::new().expect("CTX.app must start on the main thread");
    cocoa::init_application(mtm);
}

/// Called once AppKit has finished launching.
pub fn launched(mtm: MainThreadMarker) {
    let paths = crate::client::paths();
    let loaded = config::load(&paths.config);
    let themes_dir = paths.config.parent().map(|p| p.join("themes")).unwrap_or_default();
    let state_path = crate::state::path(&paths.data);
    let state = AppState::load(&state_path);
    let (bindings, problems) = crate::shortcuts::bindings(&loaded.app.keys);
    let app = App {
        mtm,
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
        touched: HashMap::new(),
        activity: VecDeque::new(),
        callbacks: HashMap::new(),
        subscribed: Vec::new(),
        known_windows: BTreeSet::new(),
        first_list: true,
        want_shown: BTreeSet::new(),
        drop_at: None,
        drag: None,
        blink_on: true,
        ticker: None,
        system_dark: cocoa::system_is_dark(mtm),
        active: true,
        quitting: false,
        panels: panels::Panels::default(),
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
        key_sent: None,
        key_echo: None,
        latencies: Vec::new(),
    };
    APP.with(|a| *a.borrow_mut() = Some(app));
    with_app(|app| {
        app.rebuild_menus();
        app.watcher = watch::Watcher::start(&app.config_path);
        let mut problems = problems;
        if let Some(p) = app.loaded.problem.clone() {
            problems.insert(0, p);
        }
        if !problems.is_empty() {
            let text = problems.join("\n");
            outside(move || cocoa::alert("There's a problem in the config file", &text));
        }
    });
    install::first_run_then(|| {
        connect();
    });
}

/// Connect to the server (starting it through launchd if needed) and subscribe.
pub fn connect() {
    let attempt = Client::connect(|| {
        dispatch2::DispatchQueue::main().exec_async(drain);
    });
    let result = match attempt {
        Err(ConnectError::NoServer(_)) => {
            install::start_server();
            if crate::client::wait_for_server(Duration::from_secs(8)) {
                Client::connect(|| dispatch2::DispatchQueue::main().exec_async(drain))
            } else {
                attempt
            }
        }
        other => other,
    };
    match result {
        Ok((client, events)) => {
            with_app(|app| {
                app.client = Some(client);
                app.events = Some(events);
                app.reconnecting = false;
                app.subscribed.clear();
                // Subscribing with nothing still sends the tab list.
                app.tell(Request::Subscribe { tabs: Vec::new() });
                app.offer_reopen_later = true;
            });
        }
        Err(e) => {
            let msg = match e {
                ConnectError::NoServer(m) | ConnectError::Mismatch(m) => m,
            };
            outside(move || {
                if cocoa::ask("CTX can't reach the ctx server", &msg, "Try Again", "Quit") {
                    connect();
                } else {
                    cocoa::terminate();
                }
            });
        }
    }
}

/// Handle everything the reader thread queued.
pub fn drain() {
    let r = with_app(|app| app.drain_events());
    if r.is_none() {
        later(|app| app.drain_events());
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

    /// Send a request and handle its reply later.
    pub fn request(&mut self, sentence: &str, r: Request, then: impl FnOnce(&mut App, Response) + 'static) {
        if let Some(c) = self.client.as_mut() {
            let id = c.send(sentence, r);
            self.callbacks.insert(id, Box::new(then));
        }
    }

    pub fn ask(&mut self, sentence: &str, r: Request) -> Option<Response> {
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
        if self.quitting || self.reconnecting {
            return;
        }
        self.reconnecting = true;
        // launchd restarts the server (or `ctx restart server` is in progress): reconnect.
        outside(move || {
            if crate::client::wait_for_server(Duration::from_secs(10))
                || cocoa::ask("Lost the ctx server", &why, "Reconnect", "Quit")
            {
                connect();
            } else {
                cocoa::terminate();
            }
        });
    }

    fn on_stream(&mut self, s: StreamOut) {
        match s {
            StreamOut::Screen(u) => self.on_screen(u),
            StreamOut::TabsChanged(list) => self.on_tabs(list),
            StreamOut::Show { tab } => self.show_tab(tab, false),
            StreamOut::Clipboard { text, .. } => {
                // Programs may write the clipboard (OSC 52); reading it is refused by the server.
                cocoa::set_clipboard(&text);
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
        let Some(view) = self.views.get_mut(&tab) else { return };
        let ch = view.screen.apply(u);
        if ch.history_moved {
            view.history.moved(view.screen.history_first, view.screen.history_end);
            view.pos = model::clamp(&view.screen, view.pos);
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
        let all = ch.resized || scrolled && ch.history_moved || view.drawn.is_empty();
        if ch.title {
            self.update_window_titles();
        }
        for w in self.wins.values_mut() {
            if !w.shows(tab) {
                continue;
            }
            if all || ch.history_moved && view_is_live_and_resized(&ch) {
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
            self.refresh_tab_bars();
            self.ensure_ticker();
        }
        self.activity.push_back(e);
        while self.activity.len() > 500 {
            self.activity.pop_front();
        }
        panels::activity_refresh(self);
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

    fn window_of(&self, tab: u64) -> Option<u64> {
        self.tab_info(tab).map(|t| t.window)
    }

    fn is_agent_window(&self, w: &WindowInfo) -> bool {
        w.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&self.loaded.agent_window_name))
            || (!w.tabs.is_empty() && w.tabs.iter().all(|t| t.opened_by != "you"))
    }

    fn on_tabs(&mut self, list: TabList) {
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
            outside(|| {
                let when = dispatch2::DispatchTime::NOW.time(1_500_000_000);
                let _ = dispatch2::DispatchQueue::main().after(when, selftest::start);
            });
        }
        // Windows that went away.
        let gone: Vec<u64> = self.wins.keys().copied().filter(|w| !windows.contains(w)).collect();
        for w in gone {
            if let Some(win) = self.wins.remove(&w) {
                cocoa::close_window(&win.views);
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
                win.selected = wtabs.last().copied().filter(|_| win.selected.is_some()).or(wtabs.first().copied());
            }
            if win.focused.is_none_or(|s| !wtabs.contains(&s)) {
                win.focused = win.selected;
            }
            win.dirty = Some(Dirty::All);
        }
        // A dropped tab's new window goes where it was dropped.
        if let Some((t, x, y)) = self.drop_at
            && let Some(w) = self.window_of(t)
            && let Some(win) = self.wins.get(&w)
        {
            cocoa::move_window_top_left(&win.views, x, y);
            self.drop_at = None;
        }
        self.views.retain(|t, _| tabs.contains(t));
        self.touched.retain(|t, _| tabs.contains(t));
        self.state.prune(&windows, &tabs);
        // Tabs the app opened, now listed: select them.
        for (w, t) in std::mem::take(&mut self.select_when_listed) {
            if self.window_of(t) == Some(w) {
                self.open_win(w);
                self.select_tab(w, t);
                if let Some(win) = self.wins.get(&w) {
                    cocoa::show_window(&win.views, true);
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
        self.refresh_tab_bars();
        self.update_window_titles();
        self.layout_all();
        self.update_subscriptions();
        self.save_state();
        cocoa::rebuild_window_menu(self);
        panels::history_refresh_if_open(self);
    }

    /// Show a `ctx` window in an app window.
    pub fn open_win(&mut self, n: u64) {
        if self.wins.contains_key(&n) {
            return;
        }
        let saved = self.state.windows.get(&n).cloned().unwrap_or_default();
        let theme_name = saved.theme.clone();
        let theme = self.load_theme(theme_name.as_deref());
        let info_tabs: Vec<u64> =
            self.window_info(n).map(|w| w.tabs.iter().map(|t| t.number).collect()).unwrap_or_default();
        let selected = saved.selected.filter(|t| info_tabs.contains(t)).or(info_tabs.first().copied());
        let fonts = self.fonts.get(&self.loaded.app.font, self.loaded.app.font_size, 2.0);
        let (cw, chh) = (fonts.cell_w / 2.0, fonts.cell_h / 2.0);
        let default_size = (80.0 * cw + 2.0 * render::PAD_X, 24.0 * chh + 2.0 * render::PAD_Y + TAB_BAR_HEIGHT);
        let views = cocoa::make_window(self.mtm, n, saved.frame, default_size, self.wins.len());
        let scale = cocoa::backing_scale(&views);
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
            theme,
            dirty: Some(Dirty::All),
            tile_rects: Vec::new(),
            drop_hint: None,
            find_open: false,
            reopen_offer: None,
            dragging_divider: None,
            selecting: None,
            hover_link: None,
        };
        self.wins.insert(n, win);
        self.state.windows.entry(n).or_insert(saved);
        if let Some(w) = self.wins.get(&n) {
            cocoa::show_window(&w.views, true);
        }
        self.refresh_tab_bars();
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
                cocoa::show_window(&win.views, true);
            } else {
                cocoa::order_front_without_focus(&win.views);
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
        if let Some(st) = self.state.windows.get_mut(&w) {
            st.selected = Some(tab);
            st.tiles = win.tiles.clone();
        }
        self.views.entry(tab).or_insert_with(|| TabView::new(tab)).screen.received |= false;
        self.refresh_tab_bars();
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
                cocoa::show_window(&win.views, true);
            }
        } else {
            // The tab list with it hasn't arrived yet: select it when it does.
            self.want_shown.insert(w);
            self.select_when_listed.push((w, tab));
        }
    }

    pub fn new_tab(&mut self, w: u64) {
        let folder = self.focused_tab(w).and_then(|t| self.tab_info(t)).map(|t| t.folder.clone());
        let r = Request::OpenTab { window: Some(WindowRef::Number(w)), folder, name: None };
        self.request("ctx open tab", r, move |app, r| match r {
            Response::Ok(Output::Opened(o)) => {
                if let Some(t) = o.tab {
                    app.pending_select(o.window, t);
                }
            }
            Response::Error(e) => app.error(&e.message),
            _ => {}
        });
    }

    pub fn close_tab(&mut self, tab: u64, without_asking: bool) {
        let r = Request::CloseTab { tab: TabRef::Number(tab), without_asking };
        self.request(&format!("ctx close tab {tab}"), r, move |app, r| {
            if let Response::Error(e) = r {
                if e.error == ErrorKind::NeedsConfirmation && !without_asking {
                    let msg = e.message.clone();
                    outside(move || {
                        if cocoa::ask("Close this tab?", &msg, "Close", "Cancel") {
                            later(move |app| app.close_tab(tab, true));
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
                    let msg = e.message.clone();
                    outside(move || {
                        if cocoa::ask("Close this window?", &msg, "Close", "Cancel") {
                            later(move |app| app.close_window(w, true));
                        }
                    });
                } else if e.error == ErrorKind::NoSuchWindow {
                    // Already gone on the server: just close ours.
                    if let Some(win) = app.wins.remove(&w) {
                        cocoa::close_window(&win.views);
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
        let m = message.to_string();
        outside(move || cocoa::alert("ctx", &m));
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
            .find(|(_, w)| cocoa::is_key(&w.views))
            .map(|(n, _)| *n)
            .or_else(|| self.wins.values().find(|w| w.key).map(|w| w.number))
            .or_else(|| self.wins.keys().next().copied())
    }

    fn tab_fonts(&mut self, tab: u64, scale: f64) -> Rc<Fonts> {
        let size = self.state.font_sizes.get(&tab).copied().unwrap_or(self.loaded.app.font_size);
        let family = self.loaded.app.font.clone();
        self.fonts.get(&family, size, scale)
    }

    pub fn load_theme(&self, name: Option<&str>) -> Theme {
        let name = name.unwrap_or(&self.loaded.app.theme).to_string();
        theme::load(&name, &self.themes_dir, self.system_dark)
            .unwrap_or_else(|_| if self.system_dark { theme::basic_dark() } else { theme::basic_light() })
    }

    pub fn layout_all(&mut self) {
        let ws: Vec<u64> = self.wins.keys().copied().collect();
        for w in ws {
            self.layout(w);
        }
    }

    /// Place the bars and the terminal view, compute tiles, and size each visible tab.
    pub fn layout(&mut self, w: u64) {
        let Some(win) = self.wins.get(&w) else { return };
        let tab_count = self.window_info(w).map(|i| i.tabs.len()).unwrap_or(0);
        let show_bar = tab_count > 1 || win.tiles.is_some() || {
            let t = win.selected;
            t.and_then(|t| self.tab_info(t)).is_some_and(|i| i.opened_by != "you" || i.permissions_of.is_some())
        };
        let (tw, th) = cocoa::layout_bars(&win.views, show_bar, win.reopen_offer.is_some(), win.find_open);
        let area = Rect::new(0.0, 0.0, tw, th);
        let rects = match &win.tiles {
            Some(n) => n.rects(area),
            None => win.selected.map(|t| vec![(t, area)]).unwrap_or_default(),
        };
        let scale = cocoa::backing_scale(&win.views);
        let mut sizes = Vec::new();
        for (t, r) in &rects {
            let fonts = self.tab_fonts(*t, scale);
            sizes.push((*t, render::grid_size(&fonts, r.w * scale, r.h * scale)));
        }
        let win = self.wins.get_mut(&w).unwrap();
        if win.tile_rects != rects || (win.scale - scale).abs() > f64::EPSILON {
            win.dirty = Some(Dirty::All);
        }
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
        let mut looks: Vec<(u64, Look)> = Vec::new();
        let shape = self.loaded.app.cursor_shape();
        let blink = self.loaded.app.cursor_blink;
        for win in self.wins.values() {
            if !win.visible {
                continue;
            }
            for (t, _) in &win.tile_rects {
                if !tabs.contains(t) {
                    tabs.push(*t);
                    looks.push((*t, win.theme.look(shape, blink)));
                }
            }
        }
        for (t, look) in looks {
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
            if let Some(f) = cocoa::window_frame(&win.views) {
                st.frame = Some(f);
            }
        }
        let _ = self.state.save(&self.state_path);
    }

    pub fn refresh_tab_bars(&mut self) {
        let now = Instant::now();
        let ws: Vec<u64> = self.wins.keys().copied().collect();
        for w in ws {
            let Some(info) = self.window_info(w) else { continue };
            let win = &self.wins[&w];
            let tiled: Vec<u64> = win.tiles.as_ref().map(|n| n.tabs()).unwrap_or_default();
            let items: Vec<TabItem> =
                info.tabs
                    .iter()
                    .map(|t| {
                        let badge = self.touched.get(&t.number).and_then(|(at, typed)| {
                            (now.duration_since(*at).as_secs() < BADGE_SECONDS).then_some(*typed)
                        });
                        TabItem {
                            tab: t.number,
                            label: tab_label(t),
                            title: t.title.clone(),
                            agent: (t.opened_by != "you").then(|| agent_name(&t.opened_by)),
                            launcher: t.permissions_of.clone(),
                            badge,
                            selected: win.focused == Some(t.number)
                                || (win.tiles.is_none() && win.selected == Some(t.number)),
                            tiled: tiled.contains(&t.number),
                            busy: t.state == "busy",
                        }
                    })
                    .collect();
            cocoa::set_tab_items(&win.views, items, win.theme.dark);
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
            cocoa::set_title(&win.views, &title);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------------------------------

    pub fn redraw_dirty(&mut self) {
        let ws: Vec<u64> = self.wins.iter().filter(|(_, w)| w.dirty.is_some() && w.visible).map(|(n, _)| *n).collect();
        for w in ws {
            self.draw_window(w);
        }
    }

    /// Make sure scrollback lines `line` and around are cached.
    fn fetch_history(&mut self, tab: u64, line: u64) {
        let from = line.saturating_sub(300);
        let r = self.ask("", Request::History { tab, from: Some(from), count: 600 });
        if let Some(Response::Ok(Output::History(h))) = r
            && let Some(v) = self.views.get_mut(&tab)
        {
            v.history.insert(h);
            v.history.trim(line);
        }
    }

    fn compose_tab(&mut self, tab: u64, height: usize) -> Vec<Place> {
        for _ in 0..8 {
            let Some(v) = self.views.get(&tab) else { return Vec::new() };
            match model::compose(&v.screen, &v.history, v.pos, height, v.columns) {
                Composed::Rows(r) => return r,
                Composed::Need(n) => {
                    let before = v.history.len();
                    self.fetch_history(tab, n);
                    if self.views.get(&tab).is_none_or(|v| v.history.len() == before) {
                        // The server has no such line any more: go back to live.
                        if let Some(v) = self.views.get_mut(&tab) {
                            v.pos = ViewPos::Live;
                        }
                    }
                }
            }
        }
        Vec::new()
    }

    pub fn draw_window(&mut self, w: u64) {
        let Some(win) = self.wins.get(&w) else { return };
        if !win.visible {
            return;
        }
        let scale = win.scale;
        let (vw, vh) = cocoa::term_size(&win.views);
        let (pw, ph) = ((vw * scale).round().max(1.0) as usize, (vh * scale).round().max(1.0) as usize);
        let rects = win.tile_rects.clone();
        let focused = win.focused;
        let key = win.key && self.active;
        let theme = win.theme.clone();
        let hint = win.drop_hint;
        let hover = win.hover_link;
        let mut dirty = win.dirty.clone().unwrap_or(Dirty::All);
        // Surface of the right size.
        let need_new = win.surface.as_ref().is_none_or(|s| s.width != pw || s.height != ph);
        if need_new {
            let Some(s) = Surface::new_io(pw, ph) else { return };
            let win = self.wins.get_mut(&w).unwrap();
            win.surface = Some(s);
            dirty = Dirty::All;
            cocoa::attach_surface(&win.views, win.surface.as_ref().unwrap(), scale);
        }
        if hint.is_some() {
            dirty = Dirty::All;
        }
        // Gather rows per tile.
        struct Prepared {
            tab: u64,
            rect: Rect,
            fonts: Rc<Fonts>,
            rows: Vec<(Row, bool)>,
            places: Vec<Place>,
        }
        let mut prepared = Vec::new();
        for (t, r) in &rects {
            let fonts = self.tab_fonts(*t, scale);
            let height = render::grid_size(&fonts, r.w * scale, r.h * scale).1 as usize;
            let places = self.compose_tab(*t, height);
            let Some(v) = self.views.get(t) else { continue };
            let mut rows = Vec::with_capacity(places.len());
            let mut wrapped: HashMap<u64, Vec<Row>> = HashMap::new();
            for p in &places {
                match p {
                    Place::Screen(y) => {
                        rows.push((v.screen.lines.get(*y as usize).cloned().unwrap_or_default(), false))
                    }
                    Place::History { line, sub } => {
                        let rs = wrapped
                            .entry(*line)
                            .or_insert_with(|| v.history.get(*line).map(|r| r.wrap(v.columns)).unwrap_or_default());
                        rows.push((rs.get(*sub).cloned().unwrap_or_default(), true));
                    }
                }
            }
            prepared.push(Prepared { tab: *t, rect: *r, fonts, rows, places });
        }
        // Frames.
        let mut frames: Vec<TileFrame> = Vec::new();
        for p in &prepared {
            let v = &self.views[&p.tab];
            let src = TabSource { view: v, history: &v.history };
            let mut selection = Vec::new();
            if let Some(sel) = v.selection {
                let (a, b) = sel.range(&src);
                for (i, place) in p.places.iter().enumerate() {
                    if *place < a.place || *place > b.place {
                        continue;
                    }
                    let from = if *place == a.place { a.column } else { 0 };
                    let to = if *place == b.place { b.column } else { usize::MAX / 2 };
                    selection.push((i, from, to.min(4000)));
                }
            }
            let mut matches = Vec::new();
            if let Some(f) = &v.find {
                for (mi, m) in f.matches.iter().enumerate() {
                    for (place, col_a, col_b) in locate_match(v, m) {
                        if let Some(row) = p.places.iter().position(|x| *x == place) {
                            matches.push((row, col_a, col_b, f.current == Some(mi)));
                        }
                    }
                }
            }
            let live = v.pos == ViewPos::Live;
            let cursor = if live && v.screen.cursor_visible && (self.blink_on || !v.screen.cursor_blink || !key) {
                v.screen.cursor.map(|(x, y)| CursorFrame {
                    column: x as usize,
                    row: y as usize,
                    shape: v.screen.cursor_shape,
                    focused: key && focused == Some(p.tab),
                    color: theme.cursor,
                })
            } else {
                None
            };
            let link = hover.filter(|h| h.0 == p.tab).map(|h| (h.1, h.2, h.3));
            frames.push(TileFrame {
                x: (p.rect.x * scale).round(),
                y: (p.rect.y * scale).round(),
                w: (p.rect.w * scale).round(),
                h: (p.rect.h * scale).round(),
                fonts: p.fonts.clone(),
                rows: p.rows.iter().map(|(r, m)| (r, *m)).collect(),
                foreground: v.screen.foreground,
                background: if v.screen.received { v.screen.background } else { theme.background },
                selection_color: theme.selection,
                cursor,
                selection,
                matches,
                link,
                marked: if focused == Some(p.tab) { v.marked.clone() } else { None },
                scrolled: (!live).then(|| model::scroll_fraction(&v.screen, v.pos)),
            });
        }
        let damage = match &dirty {
            Dirty::All => Damage::All,
            Dirty::Rows(set) => Damage::Rows(
                set.iter()
                    .filter_map(|(tab, row)| prepared.iter().position(|p| p.tab == *tab).map(|i| (i, *row)))
                    .collect(),
            ),
        };
        let divider = mix3(theme.background, theme.foreground, 0.18);
        let win = self.wins.get_mut(&w).unwrap();
        if let Some(s) = win.surface.as_mut() {
            render::draw(s, &frames, &damage, divider);
            if let Some(h) = hint {
                render::overlay(s, (h.x * scale, h.y * scale, h.w * scale, h.h * scale), theme.selection);
            }
        }
        win.dirty = None;
        cocoa::surface_changed(&win.views);
        // CTX_APP_DUMP=<folder>: save each window's pixels after drawing (debugging, tests).
        if let Some(dir) = std::env::var_os("CTX_APP_DUMP")
            && let Some(s) = win.surface.as_mut()
        {
            let _ = s.write_png(&std::path::Path::new(&dir).join(format!("window-{w}.png")));
            if !win.views.tabbar.isHidden() {
                cocoa::dump_view(&win.views.tabbar, &std::path::Path::new(&dir).join(format!("tabbar-{w}.png")));
            }
        }
        if let Some((t, at)) = self.key_echo
            && frames_tab_contains(&prepared_tabs(&rects), t)
        {
            self.key_echo = None;
            self.latencies.push(at.elapsed());
        }
        // Remember what's drawn where, for the next partial update.
        for p in prepared {
            if let Some(v) = self.views.get_mut(&p.tab) {
                v.drawn = p.places;
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------------------------

    /// A key press in a window: what to do with it.
    pub fn key_down(&mut self, w: u64, p: &Press) -> keys::Input {
        let Some(tab) = self.focused_tab(w) else { return keys::Input::TextInput };
        let meta = MetaKeys { left: self.loaded.app.left_option_is_meta, right: self.loaded.app.right_option_is_meta };
        let composing = self.views.get(&tab).is_some_and(|v| v.marked.is_some());
        let input = keys::map(p, meta, composing);
        if let keys::Input::Key { key, text } = &input {
            self.go_live(tab);
            self.input(tab, AppInput::Key { key: key.clone(), text: text.clone() });
        }
        input
    }

    fn go_live(&mut self, tab: u64) {
        if let Some(v) = self.views.get_mut(&tab) {
            let mut changed = false;
            if v.pos != ViewPos::Live {
                v.pos = ViewPos::Live;
                changed = true;
            }
            if v.selection.is_some() {
                v.selection = None;
                changed = true;
            }
            if changed {
                self.dirty_tab(tab);
            }
        }
    }

    pub fn insert_text(&mut self, w: u64, text: &str) {
        let Some(tab) = self.focused_tab(w) else { return };
        if let Some(v) = self.views.get_mut(&tab) {
            v.marked = None;
        }
        self.go_live(tab);
        // Enter from an input method arrives as text.
        let t = text.replace('\n', "\r");
        self.key_sent = Some((tab, Instant::now()));
        self.input(tab, AppInput::Text(t));
        self.dirty_tab(tab);
        self.redraw_dirty();
    }

    pub fn set_marked(&mut self, w: u64, text: Option<String>) {
        let Some(tab) = self.focused_tab(w) else { return };
        if let Some(v) = self.views.get_mut(&tab) {
            v.marked = text.filter(|t| !t.is_empty());
        }
        self.dirty_tab(tab);
        self.redraw_dirty();
    }

    pub fn has_marked(&self, w: u64) -> bool {
        self.focused_tab(w).and_then(|t| self.views.get(&t)).is_some_and(|v| v.marked.is_some())
    }

    /// The cursor's rectangle in the terminal view (points, flipped), for input method windows.
    pub fn cursor_rect(&mut self, w: u64) -> Option<Rect> {
        let tab = self.focused_tab(w)?;
        let win = self.wins.get(&w)?;
        let scale = win.scale;
        let (_, r) = *win.tile_rects.iter().find(|(t, _)| *t == tab)?;
        let fonts = self.tab_fonts(tab, scale);
        let v = self.views.get(&tab)?;
        let (x, y) = v.screen.cursor?;
        let row = v.drawn.iter().position(|p| *p == Place::Screen(y)).unwrap_or(y as usize);
        Some(Rect::new(
            r.x + render::PAD_X + x as f64 * fonts.cell_w / scale,
            r.y + render::PAD_Y + row as f64 * fonts.cell_h / scale,
            fonts.cell_w / scale,
            fonts.cell_h / scale,
        ))
    }

    pub fn dirty_tab(&mut self, tab: u64) {
        for w in self.wins.values_mut() {
            if w.shows(tab) {
                w.dirty = Some(Dirty::All);
            }
        }
    }

    /// The tab, place and column under a point in the terminal view (points, flipped).
    pub fn hit(&mut self, w: u64, x: f64, y: f64) -> Option<(u64, Place, usize, Rect)> {
        let win = self.wins.get(&w)?;
        let scale = win.scale;
        let (tab, r) = *win.tile_rects.iter().find(|(_, r)| r.contains(x, y))?;
        let fonts = self.tab_fonts(tab, scale);
        let (col, row) = render::cell_at(&fonts, (x - r.x) * scale, (y - r.y) * scale);
        let v = self.views.get(&tab)?;
        let place = v.drawn.get(row).copied().or_else(|| v.drawn.last().copied())?;
        Some((tab, place, col, r))
    }

    pub fn mouse_down(&mut self, w: u64, x: f64, y: f64, clicks: isize, flags: u64, button: u8) {
        // Dividers between tiles.
        if let Some(win) = self.wins.get_mut(&w)
            && let Some(t) = &win.tiles
        {
            let (tw, th) = cocoa::term_size(&win.views);
            if let Some(path) = t.divider_at(Rect::new(0.0, 0.0, tw, th), x, y, 3.0) {
                win.dragging_divider = Some(path);
                return;
            }
        }
        let Some((tab, place, col, _)) = self.hit(w, x, y) else { return };
        // Focus that tile.
        if self.wins.get(&w).is_some_and(|win| win.focused != Some(tab)) {
            let win = self.wins.get_mut(&w).unwrap();
            win.focused = Some(tab);
            win.dirty = Some(Dirty::All);
            self.refresh_tab_bars();
            self.update_window_titles();
        }
        let v = self.views.get(&tab);
        let tracking = v.is_some_and(|v| v.screen.mouse_tracking);
        if flags & keys::COMMAND != 0 && button == 1 {
            if let Some(url) = self.link_at(tab, place, col) {
                cocoa::open_url(&url);
            }
            return;
        }
        if tracking && flags & keys::OPTION == 0 {
            if let Some(Place::Screen(row)) = Some(place) {
                self.mouse_report(tab, MouseAction::Press, button, col, row, flags);
            }
            return;
        }
        if button != 1 {
            return;
        }
        let unit = match clicks {
            2 => Unit::Word,
            n if n >= 3 => Unit::Line,
            _ => Unit::Character,
        };
        let pt = Point { place, column: col };
        if let Some(v) = self.views.get_mut(&tab) {
            if flags & keys::SHIFT != 0
                && let Some(sel) = v.selection.as_mut()
            {
                sel.head = pt;
            } else if unit == Unit::Character {
                // A plain click clears; dragging makes a selection.
                v.selection = None;
                self.wins.get_mut(&w).unwrap().selecting = Some(tab);
                self.pending_anchor = Some((tab, pt));
            } else {
                v.selection = Some(Selection::new(tab, pt, unit));
                self.wins.get_mut(&w).unwrap().selecting = Some(tab);
            }
        }
        self.dirty_tab(tab);
        self.redraw_dirty();
    }

    pub fn mouse_dragged(&mut self, w: u64, x: f64, y: f64, flags: u64, button: u8) {
        if let Some(win) = self.wins.get_mut(&w)
            && let Some(path) = win.dragging_divider.clone()
        {
            let (tw, th) = cocoa::term_size(&win.views);
            if let Some(t) = win.tiles.as_mut() {
                t.drag_divider(&path, Rect::new(0.0, 0.0, tw, th), x, y);
            }
            self.layout(w);
            self.redraw_dirty();
            return;
        }
        let Some(tab) = self.wins.get(&w).and_then(|win| win.selecting.or(win.focused)) else { return };
        let Some(r) = self.wins.get(&w).and_then(|win| win.tile_rects.iter().find(|(t, _)| *t == tab).map(|x| x.1))
        else {
            return;
        };
        let tracking = self.views.get(&tab).is_some_and(|v| v.screen.mouse_tracking);
        if tracking && flags & keys::OPTION == 0 && self.wins[&w].selecting.is_none() {
            if let Some((t, Place::Screen(row), col, _)) = self.hit(w, x, y)
                && t == tab
            {
                self.mouse_report(tab, MouseAction::Motion, button, col, row, flags);
            }
            return;
        }
        if self.wins[&w].selecting != Some(tab) {
            return;
        }
        // Dragging above or below the tile scrolls.
        let cols = self.views.get(&tab).map(|v| v.columns).unwrap_or(80);
        if y < r.y {
            self.scroll_rows(tab, 1, true, cols);
        } else if y > r.y + r.h {
            self.scroll_rows(tab, 1, false, cols);
        }
        self.redraw_dirty();
        let cx = x.clamp(r.x, r.x + r.w - 1.0);
        let cy = y.clamp(r.y, r.y + r.h - 1.0);
        let Some((t, place, col, _)) = self.hit(w, cx, cy) else { return };
        if t != tab {
            return;
        }
        let pt = Point { place, column: col };
        if let Some(v) = self.views.get_mut(&tab) {
            match v.selection.as_mut() {
                Some(sel) => sel.head = pt,
                None => {
                    if let Some((pt_tab, anchor)) = self.pending_anchor
                        && pt_tab == tab
                        && anchor != pt
                    {
                        let mut s = Selection::new(tab, anchor, Unit::Character);
                        s.head = pt;
                        v.selection = Some(s);
                    }
                }
            }
        }
        self.dirty_tab(tab);
        self.redraw_dirty();
    }

    pub fn mouse_up(&mut self, w: u64, x: f64, y: f64, flags: u64, button: u8) {
        self.pending_anchor = None;
        if let Some(win) = self.wins.get_mut(&w) {
            if win.dragging_divider.take().is_some() {
                self.save_state();
                return;
            }
            win.selecting = None;
        }
        if let Some((tab, Place::Screen(row), col, _)) = self.hit(w, x, y)
            && self.views.get(&tab).is_some_and(|v| v.screen.mouse_tracking)
            && flags & keys::OPTION == 0
        {
            self.mouse_report(tab, MouseAction::Release, button, col, row, flags);
        }
    }

    fn mouse_report(&mut self, tab: u64, action: MouseAction, button: u8, col: usize, row: u16, flags: u64) {
        self.input(
            tab,
            AppInput::Mouse(MouseInput {
                action,
                button,
                column: col.min(u16::MAX as usize) as u16,
                row,
                shift: flags & keys::SHIFT != 0,
                alt: flags & keys::OPTION != 0,
                ctrl: flags & keys::CONTROL != 0,
            }),
        );
    }

    pub fn mouse_moved(&mut self, w: u64, x: f64, y: f64, flags: u64) {
        let mut hover = None;
        if flags & keys::COMMAND != 0
            && let Some((tab, place, col, _)) = self.hit(w, x, y)
            && let Some(row) = self.row_at(tab, place)
        {
            let measured = matches!(place, Place::History { .. });
            if let Some(l) = crate::links::at(&row, measured, col) {
                let drawn_row = self.views[&tab].drawn.iter().position(|p| *p == place).unwrap_or(0);
                hover = Some((tab, drawn_row, l.start, l.end));
            }
        }
        if let Some(win) = self.wins.get_mut(&w)
            && win.hover_link != hover
        {
            win.hover_link = hover;
            win.dirty = Some(Dirty::All);
            cocoa::set_pointing_hand(hover.is_some());
            self.redraw_dirty();
        }
    }

    fn row_at(&self, tab: u64, place: Place) -> Option<Row> {
        let v = self.views.get(&tab)?;
        selection::Source::row(&TabSource { view: v, history: &v.history }, place)
    }

    fn link_at(&self, tab: u64, place: Place, col: usize) -> Option<String> {
        let row = self.row_at(tab, place)?;
        crate::links::at(&row, matches!(place, Place::History { .. }), col).map(|l| l.url)
    }

    /// Scroll a tab's view by whole rows (`up`: towards older lines).
    pub fn scroll_rows(&mut self, tab: u64, k: usize, up: bool, columns: usize) {
        for _ in 0..4 {
            let Some(v) = self.views.get(&tab) else { return };
            let r = if up {
                model::scroll_up(&v.screen, &v.history, v.pos, k, columns)
            } else {
                model::scroll_down(&v.screen, &v.history, v.pos, k, columns)
            };
            match r {
                Ok(p) => {
                    let v = self.views.get_mut(&tab).unwrap();
                    if v.pos != p {
                        v.pos = p;
                        self.dirty_tab(tab);
                    }
                    return;
                }
                Err(line) => self.fetch_history(tab, line),
            }
        }
    }

    /// A scroll event from AppKit: trackpads give points, wheels give lines.
    pub fn scroll_event(&mut self, w: u64, x: f64, y: f64, dy: f64, precise: bool, flags: u64) {
        let rows = if precise {
            let Some((tab, ..)) = self.hit(w, x, y) else { return };
            let scale = self.wins.get(&w).map(|win| win.scale).unwrap_or(2.0);
            let fonts = self.tab_fonts(tab, scale);
            dy / (fonts.cell_h / scale)
        } else {
            dy
        };
        self.scroll_wheel(w, x, y, rows, flags);
    }

    /// A scroll wheel or trackpad: `dy` in rows (fractional for trackpads), positive = up.
    pub fn scroll_wheel(&mut self, w: u64, x: f64, y: f64, dy: f64, flags: u64) {
        let Some((tab, place, col, _)) = self.hit(w, x, y) else { return };
        let Some(v) = self.views.get_mut(&tab) else { return };
        v.scroll_rest += dy;
        let whole = v.scroll_rest.trunc();
        v.scroll_rest -= whole;
        let n = whole.abs() as usize;
        if n == 0 {
            return;
        }
        let up = whole > 0.0;
        let tracking = v.screen.mouse_tracking;
        let full = v.screen.full_screen;
        let columns = v.columns;
        if tracking && flags & keys::OPTION == 0 {
            if let Place::Screen(row) = place {
                for _ in 0..n.min(10) {
                    self.mouse_report(tab, MouseAction::Press, if up { 4 } else { 5 }, col, row, flags);
                }
            }
            return;
        }
        if full {
            // Full-screen programs without mouse reports get arrow keys (like other terminals).
            for _ in 0..n.min(10) {
                self.input(tab, AppInput::Key { key: if up { "up".into() } else { "down".into() }, text: None });
            }
            return;
        }
        self.scroll_rows(tab, n, up, columns);
        self.redraw_dirty();
    }

    pub fn paste(&mut self, w: u64) {
        let Some(tab) = self.focused_tab(w) else { return };
        let Some(text) = cocoa::get_clipboard() else { return };
        let v = self.views.get(&tab);
        let at_prompt = v.is_some_and(|v| v.screen.at_prompt && !v.screen.full_screen);
        let lines = text.trim_end_matches('\n').lines().count();
        if self.loaded.app.confirm_multiline_paste && at_prompt && lines > 1 {
            let first = text.lines().next().unwrap_or("").chars().take(80).collect::<String>();
            let msg = format!("{first}\n… and {} more lines. Each line may run as a command.", lines - 1);
            outside(move || {
                if cocoa::ask(&format!("Paste {lines} lines?"), &msg, "Paste", "Cancel") {
                    later(move |app| {
                        app.go_live(tab);
                        app.input(tab, AppInput::Paste(text));
                    });
                }
            });
            return;
        }
        self.go_live(tab);
        self.input(tab, AppInput::Paste(text));
    }

    pub fn drop_paths(&mut self, w: u64, paths: Vec<String>) {
        let Some(tab) = self.focused_tab(w) else { return };
        let quoted: Vec<String> = paths.iter().map(|p| shell_quote(p)).collect();
        self.go_live(tab);
        self.input(tab, AppInput::Text(quoted.join(" ") + " "));
    }

    /// The selected text, fetching whatever scrollback it spans.
    pub fn selected_text(&mut self, tab: u64) -> Option<String> {
        let sel = self.views.get(&tab)?.selection?;
        let first_line = match sel.anchor.place.min(sel.head.place) {
            Place::History { line, .. } => Some(line),
            Place::Screen(_) => None,
        };
        // A complete copy of the scrollback the selection covers.
        let mut full = History::default();
        if let Some(a) = first_line {
            let end = self.views[&tab].screen.history_end;
            let mut from = a;
            while from < end {
                let count = (end - from).min(10_000) as u32;
                match self.ask("", Request::History { tab, from: Some(from), count }) {
                    Some(Response::Ok(Output::History(h))) if !h.lines.is_empty() => {
                        from = h.from + h.lines.len() as u64;
                        full.insert(h);
                    }
                    _ => break,
                }
            }
        }
        let v = self.views.get(&tab)?;
        let src = TabSource { view: v, history: if first_line.is_some() { &full } else { &v.history } };
        Some(sel.text(&src))
    }

    pub fn copy(&mut self, w: u64) {
        let Some(tab) = self.focused_tab(w) else { return };
        if let Some(t) = self.selected_text(tab).filter(|t| !t.is_empty()) {
            cocoa::set_clipboard(&t);
        }
    }

    pub fn select_all(&mut self, w: u64) {
        let Some(tab) = self.focused_tab(w) else { return };
        let Some(v) = self.views.get_mut(&tab) else { return };
        let start = if v.screen.history_end > v.screen.history_first {
            Place::History { line: v.screen.history_first, sub: 0 }
        } else {
            Place::Screen(0)
        };
        let end = Place::Screen(v.screen.rows.saturating_sub(1));
        let mut s = Selection::new(tab, Point { place: start, column: 0 }, Unit::Character);
        s.head = Point { place: end, column: usize::MAX / 4 };
        v.selection = Some(s);
        self.dirty_tab(tab);
        self.redraw_dirty();
    }

    // -----------------------------------------------------------------------------------------
    // Find
    // -----------------------------------------------------------------------------------------

    pub fn find_changed(&mut self, w: u64, text: String) {
        let Some(tab) = self.focused_tab(w) else { return };
        if text.is_empty() {
            if let Some(v) = self.views.get_mut(&tab) {
                v.find = None;
            }
            self.dirty_tab(tab);
            self.redraw_dirty();
            cocoa::set_find_status(&self.wins[&w].views, "");
            return;
        }
        let r = self.ask("", Request::SearchTab { tab, text: text.clone(), limit: 2000 });
        let Some(Response::Ok(Output::TabMatches(m))) = r else { return };
        let Some(v) = self.views.get_mut(&tab) else { return };
        // Start at the last match (nearest the bottom), as Terminal.app does.
        let current = if m.matches.is_empty() { None } else { Some(m.matches.len() - 1) };
        v.find = Some(Find { text, matches: m.matches, current, more: m.more });
        self.reveal_match(tab);
        self.update_find_status(w, tab);
    }

    pub fn find_step(&mut self, w: u64, forward: bool) {
        let Some(tab) = self.focused_tab(w) else { return };
        let Some(f) = self.views.get_mut(&tab).and_then(|v| v.find.as_mut()) else { return };
        let n = f.matches.len();
        if n == 0 {
            return;
        }
        f.current = Some(match f.current {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None => 0,
        });
        self.reveal_match(tab);
        self.update_find_status(w, tab);
    }

    fn update_find_status(&mut self, w: u64, tab: u64) {
        let s = match self.views.get(&tab).and_then(|v| v.find.as_ref()) {
            Some(f) if f.matches.is_empty() => "Not found".to_string(),
            Some(f) => format!(
                "{} of {}{}",
                f.current.map(|c| c + 1).unwrap_or(0),
                f.matches.len(),
                if f.more { "+" } else { "" }
            ),
            None => String::new(),
        };
        if let Some(win) = self.wins.get(&w) {
            cocoa::set_find_status(&win.views, &s);
        }
    }

    /// Scroll so the current match is on screen.
    fn reveal_match(&mut self, tab: u64) {
        let Some(v) = self.views.get(&tab) else { return };
        let Some(f) = &v.find else { return };
        let Some(m) = f.current.and_then(|c| f.matches.get(c)) else { return };
        match m.place {
            MatchPlace::Screen(_) => {
                if let Some(v) = self.views.get_mut(&tab) {
                    v.pos = ViewPos::Live;
                }
            }
            MatchPlace::History(line) => {
                if !v.drawn.iter().any(|p| matches!(p, Place::History { line: l, .. } if *l == line)) {
                    if v.history.get(line).is_none() {
                        self.fetch_history(tab, line);
                    }
                    if let Some(v) = self.views.get_mut(&tab) {
                        v.pos = ViewPos::At { line, sub: 0 };
                        // A few lines of context above.
                        let cols = v.columns;
                        self.scroll_rows(tab, 3, true, cols);
                    }
                }
            }
        }
        self.dirty_tab(tab);
        self.redraw_dirty();
    }

    // -----------------------------------------------------------------------------------------
    // Tiles and dragging tabs
    // -----------------------------------------------------------------------------------------

    /// Show `tab` next to the focused tile of window `w`.
    pub fn tile(&mut self, w: u64, tab: u64, target: Option<u64>, edge: Edge) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        let target = target.or(win.focused).or(win.selected);
        let Some(target) = target else { return };
        if target == tab {
            return;
        }
        let mut node = win.tiles.take().unwrap_or(Node::Tab(target));
        if node.contains(tab) {
            node = node.remove(tab).unwrap_or(Node::Tab(target));
        }
        if !node.contains(target) {
            node = Node::Tab(target);
        }
        node.insert(target, tab, edge);
        win.tiles = Some(node);
        win.focused = Some(tab);
        win.dirty = Some(Dirty::All);
        self.refresh_tab_bars();
        self.layout(w);
        self.update_subscriptions();
        self.save_state();
        self.redraw_dirty();
    }

    pub fn untile(&mut self, w: u64, tab: u64) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        if let Some(n) = win.tiles.take() {
            win.tiles = n.remove(tab).filter(|n| matches!(n, Node::Split { .. }));
        }
        win.selected = Some(tab);
        win.focused = Some(tab);
        self.layout(w);
        self.update_subscriptions();
        self.refresh_tab_bars();
        self.save_state();
        self.redraw_dirty();
    }

    pub fn focus_direction(&mut self, w: u64, edge: Edge) {
        let Some(win) = self.wins.get(&w) else { return };
        let (Some(t), Some(f)) = (&win.tiles, win.focused) else { return };
        let (tw, th) = cocoa::term_size(&win.views);
        if let Some(n) = t.neighbour(f, edge, Rect::new(0.0, 0.0, tw, th)) {
            let win = self.wins.get_mut(&w).unwrap();
            win.focused = Some(n);
            win.dirty = Some(Dirty::All);
            self.refresh_tab_bars();
            self.update_window_titles();
            self.redraw_dirty();
        }
    }

    /// A tab dragged out of a tab bar and released at a screen point.
    pub fn drop_tab(&mut self, tab: u64, from: u64, screen_x: f64, screen_y: f64, bar_index: Option<(u64, usize)>) {
        for win in self.wins.values_mut() {
            if win.drop_hint.take().is_some() {
                win.dirty = Some(Dirty::All);
            }
        }
        // On a tab bar: reorder or move between windows.
        if let Some((w, index)) = bar_index {
            self.move_tab(tab, WindowRef::Number(w), Some(index as u32 + 1));
            if w != from {
                self.want_select_after_move = Some((w, tab));
            }
            self.redraw_dirty();
            return;
        }
        // On a window's content: tile it there (moving it to that window first).
        if let Some((w, lx, ly)) = cocoa::window_under(self, screen_x, screen_y) {
            let target =
                self.wins.get(&w).and_then(|win| win.tile_rects.iter().find(|(_, r)| r.contains(lx, ly)).copied());
            if let Some((target_tab, r)) = target
                && let Some(edge) = layout::drop_edge(r, lx, ly)
            {
                if w != from {
                    self.move_tab(tab, WindowRef::Number(w), None);
                    self.pending_tile = Some((w, tab, target_tab, edge));
                } else {
                    self.tile(w, tab, Some(target_tab), edge);
                }
                self.redraw_dirty();
                return;
            }
            if w != from {
                self.move_tab(tab, WindowRef::Number(w), None);
                self.want_select_after_move = Some((w, tab));
            }
            self.redraw_dirty();
            return;
        }
        // Nowhere: a new window for it, where it was dropped.
        let only = self.window_info(from).is_some_and(|i| i.tabs.len() == 1);
        if !only {
            self.drop_at = Some((tab, screen_x, screen_y));
            self.move_tab(tab, WindowRef::New, None);
        }
    }

    /// While dragging a tab: show where it would land.
    pub fn drag_hint(&mut self, screen_x: f64, screen_y: f64) {
        let target = cocoa::window_under(self, screen_x, screen_y);
        for win in self.wins.values_mut() {
            let mut hint = None;
            if let Some((w, lx, ly)) = target
                && w == win.number
                && let Some((_, r)) = win.tile_rects.iter().find(|(_, r)| r.contains(lx, ly))
                && let Some(edge) = layout::drop_edge(*r, lx, ly)
            {
                hint = Some(match edge {
                    Edge::Left => Rect::new(r.x, r.y, r.w / 2.0, r.h),
                    Edge::Right => Rect::new(r.x + r.w / 2.0, r.y, r.w / 2.0, r.h),
                    Edge::Top => Rect::new(r.x, r.y, r.w, r.h / 2.0),
                    Edge::Bottom => Rect::new(r.x, r.y + r.h / 2.0, r.w, r.h / 2.0),
                });
            }
            if win.drop_hint != hint {
                win.drop_hint = hint;
                win.dirty = Some(Dirty::All);
            }
        }
        self.redraw_dirty();
    }

    // -----------------------------------------------------------------------------------------
    // Menus and actions
    // -----------------------------------------------------------------------------------------

    pub fn rebuild_menus(&mut self) {
        self.menu_actions = cocoa::build_menus(self);
        cocoa::rebuild_window_menu(self);
    }

    pub fn perform(&mut self, action: Action) {
        let w = self.key_window();
        let tab = w.and_then(|w| self.focused_tab(w));
        match action {
            Action::About => outside(cocoa::about),
            Action::OpenConfig => self.open_config(),
            Action::Hide => outside(cocoa::hide),
            Action::HideOthers => outside(cocoa::hide_others),
            Action::ShowAll => outside(cocoa::show_all),
            Action::Quit => outside(cocoa::terminate),
            Action::NewWindow => self.new_window(None),
            Action::NewTab => match w {
                Some(w) => self.new_tab(w),
                None => self.new_window(None),
            },
            Action::CloseTab => {
                if let Some(t) = tab {
                    self.close_tab(t, false)
                }
            }
            Action::CloseWindow => {
                if let Some(w) = w {
                    self.close_window(w, false)
                }
            }
            Action::ReopenTab => self.reopen_last(),
            Action::History => panels::history_open(self),
            Action::RenameTab => {
                if let Some(t) = tab {
                    let current = self.tab_info(t).and_then(|i| i.name.clone()).unwrap_or_default();
                    outside(move || {
                        if let Some(name) = cocoa::prompt("Rename tab", "Letters, digits, spaces, - and _.", &current) {
                            later(move |app| app.rename_tab(t, name));
                        }
                    });
                }
            }
            Action::Copy => {
                if let Some(w) = w {
                    self.copy(w)
                }
            }
            Action::Paste => {
                if let Some(w) = w {
                    self.paste(w)
                }
            }
            Action::SelectAll => {
                if let Some(w) = w {
                    self.select_all(w)
                }
            }
            Action::ClearScrollback => {
                if let Some(t) = tab {
                    self.tell(Request::Clear { tab: TabRef::Number(t) });
                    if let Some(v) = self.views.get_mut(&t) {
                        v.pos = ViewPos::Live;
                        v.selection = None;
                        v.history = History::default();
                    }
                    self.dirty_tab(t);
                }
            }
            Action::Find => {
                if let Some(w) = w {
                    self.open_find(w)
                }
            }
            Action::FindNext => {
                if let Some(w) = w {
                    self.find_step(w, true)
                }
            }
            Action::FindPrevious => {
                if let Some(w) = w {
                    self.find_step(w, false)
                }
            }
            Action::FindAll => panels::find_all_open(self),
            Action::Bigger | Action::BiggerAlt => self.font_step(tab, 1.0),
            Action::Smaller => self.font_step(tab, -1.0),
            Action::ActualSize => {
                if let Some(t) = tab {
                    self.state.font_sizes.remove(&t);
                    self.font_changed(t);
                }
            }
            Action::Activity => panels::activity_open(self),
            Action::FullScreen => {
                if let Some(win) = w.and_then(|w| self.wins.get(&w)) {
                    cocoa::toggle_full_screen(&win.views);
                }
            }
            Action::Minimise => {
                if let Some(win) = w.and_then(|w| self.wins.get(&w)) {
                    cocoa::minimise(&win.views);
                }
            }
            Action::Zoom => {
                if let Some(win) = w.and_then(|w| self.wins.get(&w)) {
                    cocoa::zoom(&win.views);
                }
            }
            Action::CycleWindows => self.cycle_windows(),
            Action::BringAllToFront => outside(cocoa::bring_all_to_front),
            Action::PreviousTab | Action::NextTab => {
                if let (Some(w), Some(t)) = (w, tab)
                    && let Some(info) = self.window_info(w)
                {
                    let tabs: Vec<u64> = info.tabs.iter().map(|x| x.number).collect();
                    if let Some(i) = tabs.iter().position(|x| *x == t) {
                        let n = tabs.len();
                        let j = if action == Action::NextTab { (i + 1) % n } else { (i + n - 1) % n };
                        self.select_tab(w, tabs[j]);
                    }
                }
            }
            Action::Tab(k) => {
                if let Some(w) = w
                    && let Some(info) = self.window_info(w)
                {
                    let tabs: Vec<u64> = info.tabs.iter().map(|x| x.number).collect();
                    // ⌘9 is the last tab, as in Terminal.app and browsers.
                    let pick = if k == 9 { tabs.last().copied() } else { tabs.get(k as usize - 1).copied() };
                    if let Some(t) = pick {
                        self.select_tab(w, t);
                    }
                }
            }
            Action::FocusLeft => self.focus_in(w, Edge::Left),
            Action::FocusRight => self.focus_in(w, Edge::Right),
            Action::FocusUp => self.focus_in(w, Edge::Top),
            Action::FocusDown => self.focus_in(w, Edge::Bottom),
        }
        self.redraw_dirty();
    }

    fn focus_in(&mut self, w: Option<u64>, e: Edge) {
        if let Some(w) = w {
            self.focus_direction(w, e);
        }
    }

    fn cycle_windows(&mut self) {
        let ws: Vec<u64> = self.wins.keys().copied().collect();
        let cur = self.key_window();
        if ws.is_empty() {
            return;
        }
        let i = cur.and_then(|c| ws.iter().position(|x| *x == c)).map(|i| (i + 1) % ws.len()).unwrap_or(0);
        if let Some(win) = self.wins.get(&ws[i]) {
            cocoa::show_window(&win.views, true);
        }
    }

    fn font_step(&mut self, tab: Option<u64>, d: f64) {
        let Some(t) = tab else { return };
        let cur = self.state.font_sizes.get(&t).copied().unwrap_or(self.loaded.app.font_size);
        let next = (cur + d).clamp(6.0, 72.0);
        self.state.font_sizes.insert(t, next);
        self.font_changed(t);
    }

    fn font_changed(&mut self, t: u64) {
        self.fonts.prune();
        if let Some(w) = self.window_of(t) {
            self.layout(w);
            if let Some(win) = self.wins.get_mut(&w) {
                win.dirty = Some(Dirty::All);
            }
        }
        self.save_state();
    }

    pub fn open_find(&mut self, w: u64) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        win.find_open = true;
        cocoa::focus_find(&win.views);
        self.layout(w);
        self.redraw_dirty();
    }

    pub fn close_find(&mut self, w: u64) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        win.find_open = false;
        cocoa::focus_term(&win.views);
        let tab = self.focused_tab(w);
        if let Some(v) = tab.and_then(|t| self.views.get_mut(&t)) {
            v.find = None;
        }
        self.layout(w);
        if let Some(t) = tab {
            self.dirty_tab(t);
        }
        self.redraw_dirty();
    }

    pub fn set_window_theme(&mut self, w: u64, name: Option<String>) {
        let theme = self.load_theme(name.as_deref());
        if let Some(win) = self.wins.get_mut(&w) {
            win.theme_name = name;
            win.theme = theme;
            win.dirty = Some(Dirty::All);
        }
        self.update_subscriptions();
        self.refresh_tab_bars();
        self.save_state();
        self.redraw_dirty();
    }

    /// macOS switched light/dark.
    pub fn appearance_changed(&mut self, dark: bool) {
        if dark == self.system_dark {
            return;
        }
        self.system_dark = dark;
        let ws: Vec<(u64, Option<String>)> = self.wins.iter().map(|(n, w)| (*n, w.theme_name.clone())).collect();
        for (n, name) in ws {
            let t = self.load_theme(name.as_deref());
            if let Some(win) = self.wins.get_mut(&n) {
                win.theme = t;
                win.dirty = Some(Dirty::All);
            }
        }
        self.update_subscriptions();
        self.refresh_tab_bars();
        self.redraw_dirty();
    }

    /// The config file changed on disk.
    pub fn reload_config(&mut self) {
        let loaded = config::load(&self.config_path);
        if loaded == self.loaded {
            return;
        }
        let (bindings, mut problems) = crate::shortcuts::bindings(&loaded.app.keys);
        if let Some(p) = &loaded.problem {
            problems.insert(0, p.clone());
        }
        self.loaded = loaded;
        self.bindings = bindings;
        self.rebuild_menus();
        self.fonts.prune();
        let ws: Vec<(u64, Option<String>)> = self.wins.iter().map(|(n, w)| (*n, w.theme_name.clone())).collect();
        for (n, name) in ws {
            let t = self.load_theme(name.as_deref());
            if let Some(win) = self.wins.get_mut(&n) {
                win.theme = t;
                win.dirty = Some(Dirty::All);
            }
        }
        self.layout_all();
        self.update_subscriptions();
        self.refresh_tab_bars();
        self.redraw_dirty();
        if !problems.is_empty() {
            let text = problems.join("\n");
            outside(move || cocoa::alert("There's a problem in the config file", &text));
        }
    }

    fn open_config(&mut self) {
        let p = self.config_path.clone();
        let existing = std::fs::read_to_string(&p).unwrap_or_default();
        if !existing.lines().any(|l| l.trim() == "[app]") {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let sep = if existing.is_empty() || existing.ends_with('\n') { "" } else { "\n" };
            let gap = if existing.is_empty() { "" } else { "\n" };
            let _ = std::fs::write(&p, format!("{existing}{sep}{gap}{}", config::EXAMPLE));
        }
        let _ = std::process::Command::new("/usr/bin/open").arg("-t").arg(&p).spawn();
    }

    // -----------------------------------------------------------------------------------------
    // Window events
    // -----------------------------------------------------------------------------------------

    pub fn window_resized(&mut self, w: u64) {
        self.layout(w);
        if let Some(win) = self.wins.get_mut(&w) {
            win.dirty = Some(Dirty::All);
        }
        self.redraw_dirty();
        self.save_state();
    }

    pub fn window_visibility(&mut self, w: u64, visible: bool) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        if win.visible == visible {
            return;
        }
        win.visible = visible;
        if !visible {
            // Covered, minimised or on another Space: give the pixels back.
            win.surface = None;
            cocoa::detach_surface(&win.views);
        } else {
            win.dirty = Some(Dirty::All);
        }
        self.update_subscriptions();
        self.redraw_dirty();
    }

    pub fn window_key(&mut self, w: u64, key: bool) {
        if let Some(win) = self.wins.get_mut(&w) {
            win.key = key;
            win.dirty = Some(Dirty::All);
        }
        if let Some(t) = self.focused_tab(w) {
            self.input(t, AppInput::Focus(key));
        }
        if key {
            cocoa::rebuild_window_menu(self);
        }
        self.redraw_dirty();
    }

    pub fn app_active(&mut self, active: bool) {
        self.active = active;
        for w in self.wins.values_mut() {
            w.dirty = Some(Dirty::All);
        }
        self.redraw_dirty();
    }

    pub fn scale_changed(&mut self, w: u64) {
        if let Some(win) = self.wins.get_mut(&w) {
            win.surface = None;
        }
        self.fonts.prune();
        self.layout(w);
        self.redraw_dirty();
    }

    /// The blink and badge timer.
    pub fn tick(&mut self) {
        self.blink_on = !self.blink_on;
        let mut need = false;
        for w in self.wins.values_mut() {
            if !w.visible || !w.key {
                continue;
            }
            let mut rows = Vec::new();
            for (t, _) in &w.tile_rects {
                if let Some(v) = self.views.get(t)
                    && v.screen.cursor_blink
                    && v.pos == ViewPos::Live
                    && let Some((_, y)) = v.screen.cursor
                    && let Some(row) = v.drawn.iter().position(|p| *p == Place::Screen(y))
                {
                    rows.push((*t, row));
                }
            }
            for (t, row) in rows {
                w.add_rows(t, &[row]);
                need = true;
            }
        }
        let badges = self.touched.values().any(|(at, _)| at.elapsed().as_secs() <= BADGE_SECONDS + 1);
        if badges {
            self.refresh_tab_bars();
            self.touched.retain(|_, (at, _)| at.elapsed().as_secs() <= BADGE_SECONDS + 1);
        }
        self.redraw_dirty();
        if !need && !badges {
            self.blink_on = true;
            if let Some(t) = self.ticker.take() {
                t.invalidate();
            }
        }
    }

    pub fn ensure_ticker(&mut self) {
        if self.ticker.is_none() {
            self.ticker = Some(cocoa::start_ticker());
        }
    }

    /// Ask whether to quit, and close the tabs (unless configured to keep them).
    pub fn quit_summary(&self) -> Option<String> {
        let busy: Vec<String> = self
            .list
            .windows
            .iter()
            .flat_map(|w| w.tabs.iter())
            .filter(|t| t.state == "busy")
            .map(|t| format!("{}: {}", tab_label(t), t.running.clone().unwrap_or_else(|| "a command".into())))
            .collect();
        if busy.is_empty() { None } else { Some(busy.join("\n")) }
    }

    pub fn quit_now(&mut self) {
        self.quitting = true;
        self.save_state();
        if !self.loaded.keep_tabs_when_app_quits {
            let ws: Vec<u64> = self.list.windows.iter().map(|w| w.number).collect();
            for w in ws {
                let _ = self.ask(
                    &format!("ctx close window {w}"),
                    Request::CloseWindow { window: WindowRef::Number(w), without_asking: true },
                );
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Reopen after a crash or reboot
    // -----------------------------------------------------------------------------------------

    fn maybe_offer_reopen(&mut self) {
        if self.loaded.reopen_on_launch != "ask" {
            return;
        }
        self.request("ctx list closed tabs", Request::ListClosedTabs { limit: 100 }, |app, r| {
            let Response::Ok(Output::ClosedTabs(c)) = r else { return };
            let open: Vec<u64> = app.list.windows.iter().flat_map(|w| w.tabs.iter().map(|t| t.number)).collect();
            let tabs: Vec<u64> = c
                .tabs
                .iter()
                .filter(|t| {
                    t.ended == "interrupted" && !open.contains(&t.number) && !app.state.offered.contains(&t.number)
                })
                .map(|t| t.number)
                .collect();
            if tabs.is_empty() {
                return;
            }
            app.state.offered.extend(tabs.iter().copied());
            app.save_state();
            if let Some(w) = app.key_window().or_else(|| app.wins.keys().next().copied()) {
                let n = tabs.len();
                if let Some(win) = app.wins.get_mut(&w) {
                    win.reopen_offer = Some(tabs);
                    cocoa::show_reopen_bar(
                        &win.views,
                        &format!("Reopen {n} {} from last time", if n == 1 { "tab" } else { "tabs" }),
                    );
                }
                app.layout(w);
                app.redraw_dirty();
            }
        });
    }

    pub fn reopen_offer(&mut self, w: u64, accept: bool) {
        let tabs = self.wins.get_mut(&w).and_then(|win| win.reopen_offer.take()).unwrap_or_default();
        if let Some(win) = self.wins.get(&w) {
            cocoa::hide_reopen_bar(&win.views);
        }
        if accept {
            for t in tabs {
                self.reopen(t);
            }
        }
        self.layout(w);
        self.redraw_dirty();
    }
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

fn prepared_tabs(rects: &[(u64, Rect)]) -> Vec<u64> {
    rects.iter().map(|(t, _)| *t).collect()
}

fn frames_tab_contains(tabs: &[u64], t: u64) -> bool {
    tabs.contains(&t)
}

fn view_is_live_and_resized(ch: &model::Changed) -> bool {
    ch.resized
}

fn mix3(a: [u8; 3], b: [u8; 3], t: f64) -> [u8; 3] {
    [
        (a[0] as f64 * (1.0 - t) + b[0] as f64 * t) as u8,
        (a[1] as f64 * (1.0 - t) + b[1] as f64 * t) as u8,
        (a[2] as f64 * (1.0 - t) + b[2] as f64 * t) as u8,
    ]
}

/// `Dev server — ~/code/site`, or just the folder.
pub fn tab_label(t: &TabInfo) -> String {
    let home = crate::client::home();
    let folder = match t.folder.strip_prefix(&home) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => t.folder.clone(),
    };
    let folder = match &t.remote_host {
        Some(h) => h.to_string(),
        None => folder,
    };
    match &t.name {
        Some(n) => format!("{n} — {folder}"),
        None => folder,
    }
}

/// `claude (pid 5512)` → `claude`.
pub fn agent_name(opened_by: &str) -> String {
    opened_by.split(" (").next().unwrap_or(opened_by).to_string()
}

/// A path as a shell word.
pub fn shell_quote(p: &str) -> String {
    if !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-~".contains(c)) {
        p.to_string()
    } else {
        format!("'{}'", p.replace('\'', "'\\''"))
    }
}

/// Where a find match is drawn: (place, from column, to column) on each row it covers.
fn locate_match(v: &TabView, m: &TabMatch) -> Vec<(Place, usize, usize)> {
    let rows: Vec<(Place, Row)> = match m.place {
        MatchPlace::Screen(y) => {
            let Some(r) = v.screen.lines.get(y as usize) else { return Vec::new() };
            vec![(Place::Screen(y), r.clone())]
        }
        MatchPlace::History(line) => {
            let Some(r) = v.history.get(line) else { return Vec::new() };
            r.wrap(v.columns).into_iter().enumerate().map(|(i, w)| (Place::History { line, sub: i }, w)).collect()
        }
    };
    let measured = matches!(m.place, MatchPlace::History(_));
    // Walk the rows' cells, counting characters.
    let mut out = Vec::new();
    let mut chars = 0usize;
    for (place, r) in rows {
        let mut from: Option<usize> = None;
        let mut to = 0usize;
        for c in r.cells(measured) {
            let n = c.text.chars().count();
            let (a, b) = (chars, chars + n);
            if b > m.start as usize && a < m.end as usize {
                from.get_or_insert(c.column);
                to = c.column + c.width;
            }
            chars = b;
        }
        if let Some(f) = from {
            out.push((place, f, to));
        }
    }
    out
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

    #[test]
    fn labels() {
        let t = TabInfo {
            number: 3,
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
        };
        assert_eq!(tab_label(&t), "Dev server — ~/code/site");
        assert_eq!(agent_name(&t.opened_by), "claude");
    }
}
