//! What the menus, shortcuts and buttons do; tiles; themes; panels; window events; the timer.

use super::chrome::{self, Status};
use super::panels::{self, Kind, Panel};
use super::ui::{self, Menu, MenuAction, MenuItem};
use super::{App, Dirty, Focus, Starter, plat};
use crate::layout::{Edge, Node};
use crate::model::{History, ViewPos};
use crate::shortcuts::Action;
use ctx_protocol::*;
use std::time::{Duration, Instant};

impl App {
    pub fn rebuild_menus(&mut self) {
        self.menu_actions = plat::build_menus(self);
        plat::rebuild_window_menu(self);
    }

    pub fn perform(&mut self, action: Action) {
        let w = self.key_window();
        let tab = w.and_then(|w| self.focused_tab(w));
        match action {
            Action::About => outside_about(),
            Action::OpenConfig => self.open_config(),
            Action::Hide => super::outside(plat::hide),
            Action::HideOthers => super::outside(plat::hide_others),
            Action::ShowAll => super::outside(plat::show_all),
            Action::Quit => super::outside(plat::request_quit),
            Action::NewWindow => self.new_window(None),
            Action::NewTab => match w {
                Some(w) => self.new_tab(w),
                None => self.new_window(None),
            },
            Action::CloseTab => {
                if let Some(w) = w
                    && self.wins.get(&w).is_some_and(|x| x.panel.is_some())
                {
                    self.close_panel(w);
                } else if let Some(t) = tab {
                    self.close_tab(t, false)
                }
            }
            Action::CloseWindow => {
                if let Some(w) = w {
                    self.close_window(w, false)
                }
            }
            Action::ReopenTab => self.reopen_last(),
            Action::History => {
                if let Some(w) = w {
                    self.toggle_panel(w, Kind::History)
                }
            }
            Action::RenameTab => {
                if let Some(t) = tab {
                    self.rename_dialog(t);
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
            Action::FindAll => {
                if let Some(w) = w {
                    self.open_panel(w, Kind::FindAll)
                }
            }
            Action::Bigger | Action::BiggerAlt => self.font_step(tab, 1.0),
            Action::Smaller => self.font_step(tab, -1.0),
            Action::ActualSize => {
                if let Some(t) = tab {
                    self.state.font_sizes.remove(&t);
                    self.font_changed(t);
                }
            }
            Action::Activity => {
                if let Some(w) = w {
                    self.toggle_panel(w, Kind::Activity)
                }
            }
            Action::Overview => {
                if let Some(w) = w {
                    self.toggle_panel(w, Kind::Overview)
                }
            }
            Action::FullScreen => {
                if let Some(win) = w.and_then(|w| self.wins.get(&w)) {
                    plat::toggle_full_screen(&win.views);
                }
            }
            Action::Minimise => {
                if let Some(win) = w.and_then(|w| self.wins.get(&w)) {
                    plat::minimise(&win.views);
                }
            }
            Action::Zoom => {
                if let Some(win) = w.and_then(|w| self.wins.get(&w)) {
                    plat::zoom(&win.views);
                }
            }
            Action::CycleWindows => self.cycle_windows(),
            Action::BringAllToFront => super::outside(plat::bring_all_to_front),
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
            Action::SplitRight => self.split(w, Edge::Right),
            Action::SplitDown => self.split(w, Edge::Bottom),
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
            plat::show_window(&win.views, true);
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
            self.dirty_win(w);
        }
        self.save_state();
    }

    // -----------------------------------------------------------------------------------------
    // Tiles
    // -----------------------------------------------------------------------------------------

    /// A new tab in the focused tile's folder, shown beside it (`edge`: where).
    pub fn split(&mut self, w: Option<u64>, edge: Edge) {
        let Some(w) = w else { return };
        let Some(target) = self.focused_tab(w) else { return };
        let folder = self.tab_info(target).map(|t| t.folder.clone());
        let r = Request::OpenTab { window: Some(WindowRef::Number(w)), folder, name: None, agent: None };
        self.request("ctx open tab", r, move |app, r| match r {
            Response::Ok(Output::Opened(o)) => {
                if let Some(t) = o.tab {
                    if app.window_of(t) == Some(w) {
                        app.tile(w, t, Some(target), edge);
                    } else {
                        // Tiled once the tab list with it arrives.
                        app.pending_tile = Some((w, t, target, edge));
                    }
                }
            }
            Response::Error(e) => app.error(&e.message),
            _ => {}
        });
    }

    /// Whether a focus move in a direction has somewhere to go (for the menu).
    pub fn can_focus(&self, edge: Edge) -> bool {
        let Some(win) = self.key_window().and_then(|w| self.wins.get(&w)) else { return false };
        match (&win.tiles, win.focused) {
            (Some(t), Some(f)) => t.neighbour(f, edge, win.term_rect).is_some(),
            _ => false,
        }
    }

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
        win.chrome_dirty = true;
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
        win.chrome_dirty = true;
        self.layout(w);
        self.update_subscriptions();
        self.save_state();
        self.redraw_dirty();
    }

    pub fn focus_direction(&mut self, w: u64, edge: Edge) {
        let Some(win) = self.wins.get(&w) else { return };
        let (Some(t), Some(f)) = (&win.tiles, win.focused) else { return };
        if let Some(n) = t.neighbour(f, edge, win.term_rect) {
            let win = self.wins.get_mut(&w).unwrap();
            win.focused = Some(n);
            win.dirty = Some(Dirty::All);
            win.chrome_dirty = true;
            self.update_window_titles();
            self.redraw_dirty();
        }
    }

    // -----------------------------------------------------------------------------------------
    // Themes
    // -----------------------------------------------------------------------------------------

    /// Give a tab its own theme (`None`: back to the default).
    pub fn set_tab_theme(&mut self, tab: u64, name: Option<String>) {
        match name.filter(|n| *n != self.loaded.app.theme) {
            Some(n) => self.state.tab_themes.insert(tab, n),
            None => self.state.tab_themes.remove(&tab),
        };
        if let Some(w) = self.window_of(tab) {
            self.dirty_win(w);
        }
        self.update_subscriptions();
        self.save_state();
        plat::rebuild_window_menu(self);
        self.redraw_dirty();
    }

    /// Give a window its own theme, for the tabs in it without one (`None`: back to the default).
    pub fn set_window_theme(&mut self, w: u64, name: Option<String>) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        win.theme_name = name.filter(|n| *n != self.loaded.app.theme);
        self.dirty_win(w);
        self.update_subscriptions();
        self.save_state();
        plat::rebuild_window_menu(self);
        self.redraw_dirty();
    }

    /// macOS switched light/dark.
    pub fn appearance_changed(&mut self, dark: bool) {
        if dark == self.system_dark {
            return;
        }
        self.system_dark = dark;
        let ws: Vec<u64> = self.wins.keys().copied().collect();
        for w in ws {
            self.dirty_win(w);
        }
        self.update_subscriptions();
        self.redraw_dirty();
    }

    /// The config file changed on disk.
    pub fn reload_config(&mut self) {
        let loaded = crate::config::load(&self.config_path);
        if loaded == self.loaded {
            return;
        }
        let (bindings, mut problems) = crate::shortcuts::bindings(&loaded.app.keys);
        if let Some(p) = &loaded.problem {
            problems.insert(0, p.clone());
        }
        self.loaded = loaded;
        self.bindings = bindings;
        self.themes.clear();
        self.rebuild_menus();
        self.fonts.prune();
        let ws: Vec<u64> = self.wins.keys().copied().collect();
        for w in ws {
            self.dirty_win(w);
        }
        self.layout_all();
        self.update_subscriptions();
        self.redraw_dirty();
        if !problems.is_empty() {
            self.alert("There's a problem in the config file", &problems.join("\n"));
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
            let _ = std::fs::write(&p, format!("{existing}{sep}{gap}{}", crate::config::EXAMPLE));
        }
        plat::open_path(&p);
    }

    // -----------------------------------------------------------------------------------------
    // Menus of the chrome
    // -----------------------------------------------------------------------------------------

    pub fn rename_dialog(&mut self, tab: u64) {
        let current = self.tab_info(tab).and_then(|i| i.name.clone()).unwrap_or_default();
        self.prompt("Rename tab", "Letters, digits, spaces, - and _.", &current, move |app, name| {
            if let Some(name) = name {
                app.rename_tab(tab, name);
            }
        });
    }

    fn open_menu(&mut self, w: u64, m: Menu) {
        if let Some(win) = self.wins.get_mut(&w) {
            win.menu = Some(m);
            win.dirty = Some(Dirty::All);
        }
        self.redraw_dirty();
    }

    fn shortcut_of(&self, a: Action) -> Option<String> {
        self.bindings.get(&a).filter(|k| !k.is_empty()).map(|k| plat::shortcut_label(k))
    }

    /// The + button: a new tab, or one of the things you usually start tabs with.
    pub fn plus_menu(&mut self, w: u64, x: f64, y: f64) {
        self.refresh_starters();
        let mut items = vec![MenuItem {
            shortcut: self.shortcut_of(Action::NewTab),
            ..MenuItem::new("New Tab", MenuAction::NewTabIn(w))
        }];
        if !self.starters.is_empty() {
            items.push(MenuItem::separator());
            items.push(MenuItem::heading("Start a tab with…"));
            for s in self.starters.iter().take(8) {
                let label = shorten(&s.command, 48);
                items.push(MenuItem {
                    detail: Some(format!("in {}", super::short_folder(&s.folder))),
                    ..MenuItem::new(
                        &format!("+ {label}"),
                        MenuAction::Launch { window: w, folder: s.folder.clone(), command: s.command.clone() },
                    )
                });
            }
        }
        items.push(MenuItem::separator());
        items.push(MenuItem {
            shortcut: self.shortcut_of(Action::NewWindow),
            ..MenuItem::new("New Window", MenuAction::Perform(Action::NewWindow))
        });
        items.push(MenuItem {
            shortcut: self.shortcut_of(Action::ReopenTab),
            ..MenuItem::new("Reopen Closed Tab", MenuAction::Perform(Action::ReopenTab))
        });
        let _ = y;
        self.open_menu(w, Menu::new(x - 10.0, chrome::BAR_H - 2.0, items));
    }

    /// Ask the server for starters (the menu shows what it has until the answer comes).
    pub fn refresh_starters(&mut self) {
        if self.starters_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
            return;
        }
        self.starters_at = Some(Instant::now());
        self.request("", Request::Starters { limit: 12 }, |app, r| {
            if let Response::Ok(Output::Starters(list)) = r {
                app.starters = list
                    .starters
                    .into_iter()
                    .map(|s| Starter { folder: s.folder, command: s.command, count: s.count as usize })
                    .collect();
                // Refresh an open + menu.
                let ws: Vec<u64> = app
                    .wins
                    .iter()
                    .filter(|(_, w)| {
                        w.menu.as_ref().is_some_and(|m| m.items.first().is_some_and(|i| i.label == "New Tab"))
                    })
                    .map(|(n, _)| *n)
                    .collect();
                for w in ws {
                    let x = app.wins[&w].menu.as_ref().map(|m| m.x + 10.0).unwrap_or(0.0);
                    app.starters_at = Some(Instant::now());
                    app.plus_menu(w, x, 0.0);
                }
            }
        });
    }

    /// Right-click on a tab.
    pub fn tab_menu(&mut self, w: u64, tab: u64, x: f64, y: f64) {
        let tiled = self.wins.get(&w).and_then(|win| win.tiles.as_ref()).is_some_and(|n| n.contains(tab));
        let selected = self.focused_tab(w) == Some(tab);
        let mut items = vec![MenuItem::new("Rename Tab…", MenuAction::Rename(tab))];
        if tiled {
            items.push(MenuItem::new("Stop Showing Side by Side", MenuAction::Untile(tab)));
        } else if !selected {
            items.push(MenuItem::new("Show Side by Side", MenuAction::Tile(tab)));
        }
        items.push(MenuItem::new("Move to New Window", MenuAction::MoveToNewWindow(tab)));
        items.push(MenuItem::separator());
        items.push(MenuItem::heading("Theme"));
        let current = self.state.tab_themes.get(&tab).cloned().unwrap_or_else(|| self.loaded.app.theme.clone());
        for name in crate::theme::available(&self.themes_dir) {
            let bg = self.theme(Some(&name)).background;
            let label = if name == self.loaded.app.theme { format!("{name} (default)") } else { name.clone() };
            items.push(MenuItem {
                checked: name == current,
                swatch: Some(bg),
                ..MenuItem::new(&label, MenuAction::Theme { tab, name: Some(name.clone()) })
            });
        }
        items.push(MenuItem::separator());
        items.push(MenuItem {
            shortcut: self.shortcut_of(Action::CloseTab),
            ..MenuItem::new("Close Tab", MenuAction::Close(tab))
        });
        self.open_menu(w, Menu::new(x, y, items));
    }

    /// Right-click in the terminal.
    pub fn term_menu(&mut self, w: u64, x: f64, y: f64) {
        let items = vec![
            MenuItem {
                shortcut: self.shortcut_of(Action::Copy),
                ..MenuItem::new("Copy", MenuAction::Perform(Action::Copy))
            },
            MenuItem {
                shortcut: self.shortcut_of(Action::Paste),
                ..MenuItem::new("Paste", MenuAction::Perform(Action::Paste))
            },
            MenuItem {
                shortcut: self.shortcut_of(Action::SelectAll),
                ..MenuItem::new("Select All", MenuAction::Perform(Action::SelectAll))
            },
            MenuItem::separator(),
            MenuItem {
                shortcut: self.shortcut_of(Action::Find),
                ..MenuItem::new("Find…", MenuAction::Perform(Action::Find))
            },
            MenuItem {
                shortcut: self.shortcut_of(Action::ClearScrollback),
                ..MenuItem::new("Clear Scrollback", MenuAction::Perform(Action::ClearScrollback))
            },
        ];
        self.open_menu(w, Menu::new(x, y, items));
    }

    // -----------------------------------------------------------------------------------------
    // Panels
    // -----------------------------------------------------------------------------------------

    pub fn toggle_panel(&mut self, w: u64, kind: Kind) {
        if self.wins.get(&w).and_then(|x| x.panel.as_ref()).is_some_and(|p| p.kind == kind) {
            self.close_panel(w);
        } else {
            self.open_panel(w, kind);
        }
    }

    pub fn open_panel(&mut self, w: u64, kind: Kind) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        win.panel = Some(Panel::new(kind));
        win.focus = if matches!(kind, Kind::FindAll | Kind::History) { Focus::Panel } else { Focus::Term };
        win.menu = None;
        self.dirty_win(w);
        self.ensure_ticker();
        match kind {
            Kind::History => {
                self.request("ctx list closed tabs", Request::ListClosedTabs { limit: 500 }, move |a, r| {
                    if let Response::Ok(Output::ClosedTabs(c)) = r
                        && let Some(p) = a.wins.get_mut(&w).and_then(|x| x.panel.as_mut())
                        && p.kind == Kind::History
                    {
                        p.closed = c.tabs;
                        let first = p.visible_closed().first().map(|t| t.number);
                        a.dirty_win(w);
                        if let Some(t) = first {
                            a.history_select(w, t);
                        }
                    }
                });
            }
            Kind::Activity => {
                let r = Request::ListActivity {
                    filter: Filter { tab: None, folder: None, by: None, since: None },
                    limit: 200,
                };
                self.request("ctx list activity", r, move |a, r| {
                    if let Response::Ok(Output::Activity(list)) = r {
                        let mut seen: Vec<ActivityEntry> = list.entries;
                        for e in a.activity.drain(..) {
                            if !seen.contains(&e) {
                                seen.push(e);
                            }
                        }
                        seen.sort_by(|x, y| x.at.cmp(&y.at));
                        a.activity = seen.into_iter().collect();
                        a.dirty_win(w);
                        a.redraw_dirty();
                    }
                });
            }
            Kind::Overview => self.refresh_processes(),
            Kind::FindAll => {}
        }
        self.update_subscriptions();
        self.redraw_dirty();
    }

    pub fn close_panel(&mut self, w: u64) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        win.panel = None;
        win.focus = if win.find.is_some() { Focus::Find } else { Focus::Term };
        for v in self.views.values_mut() {
            v.drawn.clear();
        }
        self.dirty_win(w);
        self.update_subscriptions();
        self.redraw_dirty();
    }

    /// Something a panel shows changed: redraw windows showing that kind.
    pub fn panels_changed(&mut self, kind: Kind) {
        for win in self.wins.values_mut() {
            if win.panel.as_ref().is_some_and(|p| p.kind == kind) {
                win.dirty = Some(Dirty::All);
            }
        }
    }

    /// Ask what runs in every tab (the Overview shows it, and refreshes it while open).
    pub fn refresh_processes(&mut self) {
        self.processes_at = Some(Instant::now());
        let tabs: Vec<u64> = self.list.windows.iter().flat_map(|w| w.tabs.iter().map(|t| t.number)).collect();
        self.processes.retain(|t, _| tabs.contains(t));
        for t in tabs {
            self.request("", Request::Processes { tab: TabRef::Number(t) }, move |a, r| {
                if let Response::Ok(Output::Processes(list)) = r
                    && a.processes.get(&t) != Some(&list.processes)
                {
                    a.processes.insert(t, list.processes);
                    a.panels_changed(Kind::Overview);
                    a.redraw_dirty();
                }
            });
        }
    }

    pub fn history_select(&mut self, w: u64, tab: u64) {
        if let Some(p) = self.wins.get_mut(&w).and_then(|x| x.panel.as_mut()) {
            p.selected = Some(tab);
            p.scroll[1] = 0.0;
        }
        self.dirty_win(w);
        let ansi = self.theme(None).ansi;
        let palette = ctx_protocol::sgr::full_palette(&ansi);
        self.request(&format!("ctx read tab {tab}"), Request::SavedTab { tab }, move |a, r| {
            let rows = match r {
                Response::Ok(Output::Records(rec)) => panels::record_rows(&rec, &palette),
                Response::Error(e) => vec![crate::text::Row::plain(&e.message)],
                _ => Vec::new(),
            };
            if let Some(p) = a.wins.get_mut(&w).and_then(|x| x.panel.as_mut())
                && p.selected == Some(tab)
            {
                p.records = Some((tab, rows));
                a.dirty_win(w);
                a.redraw_dirty();
            }
        });
        self.redraw_dirty();
    }

    /// Return in a panel's field.
    pub fn panel_submit(&mut self, w: u64) {
        let Some(p) = self.wins.get_mut(&w).and_then(|x| x.panel.as_mut()) else { return };
        match p.kind {
            Kind::FindAll => {
                let text = p.field.text.clone();
                if text.trim().is_empty() {
                    return;
                }
                p.searching = true;
                p.found = None;
                let r = Request::Find {
                    text: text.clone(),
                    filter: Filter { tab: None, folder: None, by: None, since: None },
                    limit: 100,
                };
                self.request(&format!("ctx find \"{text}\""), r, move |a, r| {
                    if let Some(p) = a.wins.get_mut(&w).and_then(|x| x.panel.as_mut()) {
                        p.searching = false;
                        p.found = match r {
                            Response::Ok(Output::Found(f)) => Some(f),
                            _ => Some(FindResults { text: String::new(), hits: Vec::new(), total: 0, note: None }),
                        };
                        p.scroll = [0.0; 2];
                    }
                    a.dirty_win(w);
                    a.redraw_dirty();
                });
            }
            Kind::History => {
                if let Some(t) = p.selected.or(p.visible_closed().first().map(|t| t.number)) {
                    self.reopen(t);
                }
            }
            Kind::Overview => {
                let words = p.field.text.to_lowercase();
                let first = self
                    .list
                    .windows
                    .iter()
                    .flat_map(|w| w.tabs.iter())
                    .find(|t| super::tab_label(t).to_lowercase().contains(words.trim()))
                    .map(|t| t.number);
                if let Some(t) = first {
                    self.close_panel(w);
                    self.show_tab(t, true);
                }
            }
            Kind::Activity => {}
        }
        self.redraw_dirty();
    }

    /// Up/Down in a panel's field: History moves its selection.
    pub fn panel_step(&mut self, w: u64, down: bool) {
        let Some(p) = self.wins.get(&w).and_then(|x| x.panel.as_ref()) else { return };
        if p.kind != Kind::History {
            return;
        }
        let shown: Vec<u64> = p.visible_closed().iter().map(|t| t.number).collect();
        let i = p.selected.and_then(|s| shown.iter().position(|x| *x == s));
        let j = match (i, down) {
            (None, _) => 0,
            (Some(i), true) => (i + 1).min(shown.len().saturating_sub(1)),
            (Some(i), false) => i.saturating_sub(1),
        };
        if let Some(t) = shown.get(j) {
            self.history_select(w, *t);
        }
    }

    /// A Find result chosen: show its tab (or its saved record) and find the text there.
    pub fn found_chosen(&mut self, w: u64, i: usize) {
        let Some(p) = self.wins.get(&w).and_then(|x| x.panel.as_ref()) else { return };
        let Some(h) = p.found.as_ref().and_then(|f| f.hits.get(i)).cloned() else { return };
        let text = p.field.text.clone();
        if self.tab_info(h.tab).is_some() {
            self.close_panel(w);
            self.show_tab(h.tab, true);
            if let Some(tw) = self.window_of(h.tab) {
                self.open_find(tw);
                if let Some(f) = self.wins.get_mut(&tw).and_then(|x| x.find.as_mut()) {
                    f.field.set(&text);
                }
                self.find_changed(tw, text);
            }
        } else {
            // A closed tab: show it in History.
            self.open_panel(w, Kind::History);
            if let Some(p) = self.wins.get_mut(&w).and_then(|x| x.panel.as_mut()) {
                p.field.set(&format!("tab {}", h.tab));
            }
            self.history_select(w, h.tab);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Window events
    // -----------------------------------------------------------------------------------------

    pub fn window_resized(&mut self, w: u64) {
        self.layout(w);
        self.dirty_win(w);
        self.redraw_dirty();
        self.save_state();
    }

    pub fn window_full_screen(&mut self, w: u64, on: bool) {
        if let Some(win) = self.wins.get_mut(&w) {
            win.full_screen = on;
        }
        self.window_resized(w);
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
            win.spares.clear();
            plat::release(&win.views);
        } else {
            win.dirty = Some(Dirty::All);
            win.chrome_dirty = true;
        }
        self.update_subscriptions();
        self.redraw_dirty();
    }

    pub fn window_key(&mut self, w: u64, key: bool) {
        if let Some(win) = self.wins.get_mut(&w) {
            win.key = key;
            win.dirty = Some(Dirty::All);
            win.chrome_dirty = true;
        }
        if let Some(t) = self.focused_tab(w) {
            self.input(t, AppInput::Focus(key));
        }
        if key {
            plat::rebuild_window_menu(self);
        }
        self.redraw_dirty();
    }

    pub fn app_active(&mut self, active: bool) {
        self.active = active;
        for w in self.wins.values_mut() {
            w.dirty = Some(Dirty::All);
            w.chrome_dirty = true;
        }
        self.redraw_dirty();
    }

    pub fn scale_changed(&mut self, w: u64) {
        if let Some(win) = self.wins.get_mut(&w) {
            win.surface = None;
            win.spares.clear();
        }
        self.fonts.prune();
        self.layout(w);
        self.dirty_win(w);
        self.redraw_dirty();
    }

    /// The blink, badge and status timer. It only runs while something needs it.
    pub fn tick(&mut self) {
        self.blink_on = !self.blink_on;
        let mut need = false;
        for w in self.wins.values_mut() {
            if !w.visible || !w.key {
                continue;
            }
            // A caret in a field, or a dialog's.
            if w.dialog.is_some() || w.focus != Focus::Term && (w.find.is_some() || w.panel.is_some()) {
                w.chrome_dirty = true;
                if w.panel.is_some() || w.dialog.is_some() {
                    w.dirty = Some(Dirty::All);
                }
                need = true;
                continue;
            }
            let mut rows = Vec::new();
            for (t, _) in &w.tile_rects {
                if let Some(v) = self.views.get(t)
                    && v.screen.cursor_blink
                    && v.pos == ViewPos::Live
                    && let Some((_, y)) = v.screen.cursor
                    && let Some(row) = v.drawn.iter().position(|p| *p == crate::model::Place::Screen(y))
                {
                    rows.push((*t, row));
                }
            }
            for (t, row) in rows {
                w.add_rows(t, &[row]);
                need = true;
            }
        }
        let badges = self.touched.values().any(|(at, _)| at.elapsed().as_secs() <= super::BADGE_SECONDS + 1);
        if badges {
            self.chrome_changed();
            self.touched.retain(|_, (at, _)| at.elapsed().as_secs() <= super::BADGE_SECONDS + 1);
        }
        // The Overview keeps what runs in each tab current.
        let overview =
            self.wins.values().any(|w| w.visible && w.panel.as_ref().is_some_and(|p| p.kind == Kind::Overview));
        if overview && self.processes_at.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            self.refresh_processes();
        }
        let need = need || overview;
        // Agents' "working"/"waiting" follows their output.
        let agents = self.statuses.values().any(|s| matches!(s, Status::AgentWorking));
        if agents {
            self.update_statuses();
        }
        self.redraw_dirty();
        if !need && !badges && !agents {
            self.blink_on = true;
            if let Some(t) = self.ticker.take() {
                t.stop();
            }
        }
    }

    pub fn ensure_ticker(&mut self) {
        if self.ticker.is_none() {
            self.ticker = Some(plat::start_ticker(530));
        }
    }

    /// What's still running, for the quit confirmation (none when quitting keeps the tabs).
    pub fn quit_summary(&self) -> Option<String> {
        if self.loaded.keep_tabs_when_app_quits {
            return None;
        }
        let busy: Vec<String> = self
            .list
            .windows
            .iter()
            .flat_map(|w| w.tabs.iter())
            .filter(|t| t.state == "busy")
            .map(|t| format!("{}: {}", super::tab_label(t), t.running.clone().unwrap_or_else(|| "a command".into())))
            .collect();
        if busy.is_empty() { None } else { Some(busy.join("\n")) }
    }

    /// Close the tabs (unless configured to keep them) before quitting.
    pub fn quit_now(&mut self) {
        self.quitting = true;
        self.save_state();
        if !self.loaded.keep_tabs_when_app_quits {
            let ws: Vec<u64> = self.list.windows.iter().map(|w| w.number).collect();
            for w in ws {
                let _ = self.ask_server(
                    &format!("ctx close window {w}"),
                    Request::CloseWindow { window: WindowRef::Number(w), without_asking: true },
                );
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Reopen after a crash or reboot
    // -----------------------------------------------------------------------------------------

    pub fn maybe_offer_reopen(&mut self) {
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
                if let Some(win) = app.wins.get_mut(&w) {
                    win.reopen_offer = Some(tabs);
                }
                app.layout(w);
                app.dirty_win(w);
                app.redraw_dirty();
            }
        });
    }

    pub fn reopen_offer(&mut self, w: u64, accept: bool) {
        let tabs = self.wins.get_mut(&w).and_then(|win| win.reopen_offer.take()).unwrap_or_default();
        if accept {
            for t in tabs {
                self.reopen(t);
            }
        }
        self.layout(w);
        self.dirty_win(w);
        self.redraw_dirty();
    }
}

fn outside_about() {
    super::outside(plat::about);
}

/// At most `n` characters, with an ellipsis.
fn shorten(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n - 1).collect::<String>() + "…" }
}

#[allow(dead_code)]
fn _ui(_: ui::Field) {}
