//! Secondary windows: the Activity panel (what other clients did), History (closed tabs, their
//! saved commands and output, read-only, with a Reopen button) and Find in All Tabs. They are
//! plain AppKit text views; links in them select a tab or a closed tab.

use super::{App, later};
use ctx_protocol::*;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::*;

pub struct TextPanel {
    pub window: Retained<NSWindow>,
    pub text: Retained<NSTextView>,
}

pub struct HistoryPanel {
    pub window: Retained<NSWindow>,
    pub search: Retained<NSSearchField>,
    pub list: Retained<NSTextView>,
    pub viewer: Retained<NSTextView>,
    pub reopen: Retained<NSButton>,
    pub closed: Vec<ClosedTab>,
    pub selected: Option<u64>,
}

pub struct FindPanel {
    pub window: Retained<NSWindow>,
    pub search: Retained<NSSearchField>,
    pub results: Retained<NSTextView>,
}

#[derive(Default)]
pub struct Panels {
    pub activity: Option<TextPanel>,
    pub history: Option<HistoryPanel>,
    pub find: Option<FindPanel>,
    pub delegate: Option<Retained<PanelDelegate>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CTXPanelDelegate"]
    pub struct PanelDelegate;

    unsafe impl NSObjectProtocol for PanelDelegate {}

    unsafe impl NSTextDelegate for PanelDelegate {}

    unsafe impl NSTextViewDelegate for PanelDelegate {
        #[unsafe(method(textView:clickedOnLink:atIndex:))]
        fn clicked_link(&self, _tv: &NSTextView, link: &AnyObject, _i: NSUInteger) -> bool {
            clicked(link)
        }
    }

    unsafe impl NSControlTextEditingDelegate for PanelDelegate {}
    unsafe impl NSTextFieldDelegate for PanelDelegate {}
    unsafe impl NSSearchFieldDelegate for PanelDelegate {}

    impl PanelDelegate {
        #[unsafe(method(historySearch:))]
        fn history_search(&self, _s: &NSSearchField) {
            later(history_render_list);
        }

        #[unsafe(method(historyReopen:))]
        fn history_reopen(&self, _b: &NSButton) {
            later(|a| {
                if let Some(t) = a.panels.history.as_ref().and_then(|h| h.selected) {
                    a.reopen(t);
                }
            });
        }

        #[unsafe(method(findAllSearch:))]
        fn find_all_search(&self, s: &NSSearchField) {
            let text = s.stringValue().to_string();
            later(move |a| find_all_run(a, text));
        }
    }
);

fn clicked(link: &AnyObject) -> bool {
    let s = if let Some(u) = link.downcast_ref::<NSURL>() {
        u.absoluteString().map(|s| s.to_string()).unwrap_or_default()
    } else if let Some(s) = link.downcast_ref::<NSString>() {
        s.to_string()
    } else {
        return false;
    };
    later(move |a| follow(a, &s));
    true
}

fn delegate(app: &mut App) -> Retained<PanelDelegate> {
    if app.panels.delegate.is_none() {
        let this = PanelDelegate::alloc(app.mtm).set_ivars(());
        let d: Retained<PanelDelegate> = unsafe { msg_send![super(this), init] };
        app.panels.delegate = Some(d);
    }
    app.panels.delegate.clone().unwrap()
}

fn follow(app: &mut App, link: &str) {
    if let Some(n) = link.strip_prefix("ctx://tab/").and_then(|n| n.parse::<u64>().ok()) {
        if app.tab_info(n).is_some() {
            app.show_tab(n, true);
        } else {
            history_open(app);
            history_show(app, n);
        }
    } else if let Some(n) = link.strip_prefix("ctx://closed/").and_then(|n| n.parse::<u64>().ok()) {
        history_show(app, n);
    }
}

fn panel_window(mtm: MainThreadMarker, title: &str, w: f64, h: f64) -> Retained<NSWindow> {
    let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable;
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
    let win = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { win.setReleasedWhenClosed(false) };
    win.setTitle(&NSString::from_str(title));
    win.setTabbingMode(NSWindowTabbingMode::Disallowed);
    win.center();
    win
}

fn text_view(
    mtm: MainThreadMarker,
    frame: NSRect,
    d: &PanelDelegate,
) -> (Retained<NSScrollView>, Retained<NSTextView>) {
    let scroll = NSTextView::scrollableTextView(mtm);
    scroll.setFrame(frame);
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    let tv: Retained<NSTextView> = scroll.documentView().unwrap().downcast().unwrap();
    tv.setEditable(false);
    tv.setSelectable(true);
    tv.setRichText(true);
    tv.setTextContainerInset(NSSize::new(6.0, 6.0));
    tv.setDelegate(Some(ProtocolObject::from_ref(d)));
    (scroll, tv)
}

fn mono(size: f64, bold: bool) -> Retained<NSFont> {
    let weight = if bold { unsafe { NSFontWeightBold } } else { unsafe { NSFontWeightRegular } };
    NSFont::monospacedSystemFontOfSize_weight(size, weight)
}

/// A styled piece of text for the panels.
struct Piece {
    text: String,
    color: Option<[u8; 3]>,
    bold: bool,
    link: Option<String>,
    dim: bool,
}

fn piece(text: impl Into<String>) -> Piece {
    Piece { text: text.into(), color: None, bold: false, link: None, dim: false }
}

fn attributed(pieces: &[Piece], size: f64) -> Retained<NSMutableAttributedString> {
    let out = NSMutableAttributedString::new();
    for p in pieces {
        let font = mono(size, p.bold);
        let color = match p.color {
            Some(c) => NSColor::colorWithSRGBRed_green_blue_alpha(
                c[0] as f64 / 255.0,
                c[1] as f64 / 255.0,
                c[2] as f64 / 255.0,
                1.0,
            ),
            None if p.dim => NSColor::secondaryLabelColor(),
            None => NSColor::labelColor(),
        };
        let mut keys: Vec<&NSAttributedStringKey> =
            unsafe { vec![NSFontAttributeName, NSForegroundColorAttributeName] };
        let mut vals: Vec<Retained<AnyObject>> = vec![font.into_super().into_super(), color.into_super().into()];
        if let Some(l) = &p.link {
            keys.push(unsafe { NSLinkAttributeName });
            vals.push(NSString::from_str(l).into_super().into());
        }
        let vrefs: Vec<&AnyObject> = vals.iter().map(|v| &**v).collect();
        let attrs = NSDictionary::from_slices(&keys, &vrefs);
        let s = unsafe {
            NSAttributedString::initWithString_attributes(
                NSAttributedString::alloc(),
                &NSString::from_str(&p.text),
                Some(&attrs),
            )
        };
        out.appendAttributedString(&s);
    }
    out
}

fn set_text(tv: &NSTextView, pieces: &[Piece]) {
    let s = attributed(pieces, 12.0);
    if let Some(storage) = unsafe { tv.textStorage() } {
        storage.setAttributedString(&s);
    }
}

/// "12s ago", "3m ago", "2h ago", or a date.
fn ago(iso: &str) -> String {
    let Some(ms) = ctx_protocol::time::parse_iso(iso) else { return iso.to_string() };
    let s = ((ctx_protocol::time::now_ms() - ms) / 1000).max(0);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => iso.get(..10).unwrap_or(iso).to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// Activity
// ---------------------------------------------------------------------------------------------

pub fn activity_open(app: &mut App) {
    if app.panels.activity.is_none() {
        let d = delegate(app);
        let win = panel_window(app.mtm, "Activity", 640.0, 420.0);
        let content = win.contentView().unwrap();
        let (scroll, text) = text_view(app.mtm, content.bounds(), &d);
        content.addSubview(&scroll);
        app.panels.activity = Some(TextPanel { window: win, text });
        // Earlier activity from the server's log.
        let r = Request::ListActivity { filter: Filter { tab: None, folder: None, by: None, since: None }, limit: 200 };
        app.request("ctx list activity", r, |a, r| {
            if let Response::Ok(Output::Activity(list)) = r {
                let mut seen: Vec<ActivityEntry> = list.entries;
                for e in a.activity.drain(..) {
                    if !seen.contains(&e) {
                        seen.push(e);
                    }
                }
                seen.sort_by(|x, y| x.at.cmp(&y.at));
                a.activity = seen.into_iter().collect();
                activity_refresh(a);
            }
        });
    }
    activity_refresh(app);
    if let Some(p) = &app.panels.activity {
        p.window.makeKeyAndOrderFront(None);
    }
}

/// Plain-language lines, newest first.
pub fn activity_refresh(app: &mut App) {
    let Some(p) = &app.panels.activity else { return };
    if !p.window.isVisible() {
        return;
    }
    let mut pieces = Vec::new();
    if app.activity.is_empty() {
        pieces.push(Piece { dim: true, ..piece("Nothing other than you has touched a tab yet.\n") });
    }
    for e in app.activity.iter().rev() {
        let who = match &e.declared_name {
            Some(d) => format!("{} (says it's {d})", e.client),
            None => e.client.clone(),
        };
        pieces.push(Piece { bold: true, ..piece(who) });
        match (&e.typed, e.tab) {
            (Some(t), Some(tab)) => {
                pieces.push(piece(" typed "));
                pieces.push(Piece { color: Some([180, 90, 0]), ..piece(format!("`{t}`")) });
                pieces.push(piece(" in "));
                pieces.push(Piece { link: Some(format!("ctx://tab/{tab}")), ..piece(format!("tab {tab}")) });
            }
            (None, Some(tab)) => {
                pieces.push(piece(" ran "));
                pieces.push(Piece { color: Some([60, 110, 200]), ..piece(format!("`{}`", e.sentence)) });
                pieces.push(piece(" on "));
                pieces.push(Piece { link: Some(format!("ctx://tab/{tab}")), ..piece(format!("tab {tab}")) });
            }
            (_, None) => {
                pieces.push(piece(" ran "));
                pieces.push(Piece { color: Some([60, 110, 200]), ..piece(format!("`{}`", e.sentence)) });
            }
        }
        pieces.push(Piece { dim: true, ..piece(format!(" — {}\n", ago(&e.at))) });
    }
    set_text(&p.text, &pieces);
}

// ---------------------------------------------------------------------------------------------
// History of saved tabs
// ---------------------------------------------------------------------------------------------

pub fn history_open(app: &mut App) {
    if app.panels.history.is_none() {
        let mtm = app.mtm;
        let d = delegate(app);
        let win = panel_window(mtm, "History", 980.0, 600.0);
        let content = win.contentView().unwrap();
        let b = content.bounds();
        let search = NSSearchField::new(mtm);
        search.setFrame(NSRect::new(NSPoint::new(10.0, b.size.height - 34.0), NSSize::new(320.0, 24.0)));
        search.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        search.setPlaceholderString(Some(&NSString::from_str("Filter by folder, name, who opened it, or date")));
        unsafe {
            search.setTarget(Some(&d));
            search.setAction(Some(sel!(historySearch:)));
        }
        search.setSendsSearchStringImmediately(true);
        let reopen = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Reopen"),
                Some(&d),
                Some(sel!(historyReopen:)),
                mtm,
            )
        };
        reopen.setFrame(NSRect::new(NSPoint::new(b.size.width - 100.0, b.size.height - 36.0), NSSize::new(90.0, 28.0)));
        reopen
            .setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin);
        reopen.setEnabled(false);
        let list_frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(340.0, b.size.height - 44.0));
        let (ls, list) = text_view(mtm, list_frame, &d);
        ls.setAutoresizingMask(NSAutoresizingMaskOptions::ViewHeightSizable);
        let viewer_frame =
            NSRect::new(NSPoint::new(341.0, 0.0), NSSize::new(b.size.width - 341.0, b.size.height - 44.0));
        let (vs, viewer) = text_view(mtm, viewer_frame, &d);
        content.addSubview(&search);
        content.addSubview(&reopen);
        content.addSubview(&ls);
        content.addSubview(&vs);
        app.panels.history =
            Some(HistoryPanel { window: win, search, list, viewer, reopen, closed: Vec::new(), selected: None });
    }
    history_refresh(app);
    if let Some(h) = &app.panels.history {
        h.window.makeKeyAndOrderFront(None);
        h.window.makeFirstResponder(Some(&h.search));
    }
}

pub fn history_refresh_if_open(app: &mut App) {
    if app.panels.history.as_ref().is_some_and(|h| h.window.isVisible()) {
        history_refresh(app);
    }
}

fn history_refresh(app: &mut App) {
    app.request("ctx list closed tabs", Request::ListClosedTabs { limit: 500 }, |a, r| {
        if let Response::Ok(Output::ClosedTabs(c)) = r
            && let Some(h) = a.panels.history.as_mut()
        {
            h.closed = c.tabs;
            history_render_list(a);
        }
    });
}

fn history_render_list(app: &mut App) {
    let Some(h) = &app.panels.history else { return };
    let filter = h.search.stringValue().to_string().to_lowercase();
    let mut pieces = Vec::new();
    let mut shown = 0;
    for t in &h.closed {
        let hay = format!(
            "{} {} {} {} {} tab {}",
            t.name.clone().unwrap_or_default(),
            t.folder,
            t.opened_by,
            t.closed_at,
            t.opened_at,
            t.number
        )
        .to_lowercase();
        if !filter.split_whitespace().all(|w| hay.contains(w)) {
            continue;
        }
        shown += 1;
        let title = match &t.name {
            Some(n) => format!("{n} — {}", t.folder),
            None => t.folder.clone(),
        };
        let sel = h.selected == Some(t.number);
        pieces.push(Piece {
            link: Some(format!("ctx://closed/{}", t.number)),
            bold: sel,
            ..piece(format!("tab {}  {title}\n", t.number))
        });
        let by = if t.opened_by == "you" { String::new() } else { format!(" · opened by {}", t.opened_by) };
        let ended = if t.ended == "interrupted" { " · interrupted" } else { "" };
        pieces.push(Piece {
            dim: true,
            ..piece(format!("    closed {} · {} commands{by}{ended}\n", ago(&t.closed_at), t.commands))
        });
    }
    if shown == 0 {
        pieces.push(Piece {
            dim: true,
            ..piece(if h.closed.is_empty() { "No closed tabs are saved.\n" } else { "Nothing matches.\n" })
        });
    }
    set_text(&h.list, &pieces);
}

fn history_show(app: &mut App, tab: u64) {
    if let Some(h) = app.panels.history.as_mut() {
        h.selected = Some(tab);
        h.reopen.setEnabled(true);
    }
    history_render_list(app);
    let theme = app
        .key_window()
        .and_then(|w| app.wins.get(&w))
        .map(|w| w.theme.clone())
        .unwrap_or_else(|| app.load_theme(None));
    let palette = ctx_protocol::sgr::full_palette(&theme.ansi);
    app.request(&format!("ctx read tab {tab}"), Request::SavedTab { tab }, move |a, r| {
        let Some(h) = &a.panels.history else { return };
        let mut pieces = Vec::new();
        match r {
            Response::Ok(Output::Records(rec)) => {
                if rec.commands.is_empty() {
                    pieces.push(Piece { dim: true, ..piece("No commands were saved for this tab.\n") });
                }
                for c in &rec.commands {
                    let by = if c.ran_by == "you" { String::new() } else { format!("  (ran by {})", c.ran_by) };
                    pieces.push(Piece { bold: true, ..piece(format!("{} {}{by}\n", c.location, c.command)) });
                    let events: Vec<u64> = c.events.iter().map(|e| e.line).collect();
                    for (i, line) in c.output.iter().enumerate() {
                        if events.contains(&(i as u64)) {
                            pieces.push(Piece {
                                dim: true,
                                ..piece(format!(
                                    "{}\n",
                                    ctx_protocol::sgr::runs(line, &palette)
                                        .iter()
                                        .map(|r| r.text.as_str())
                                        .collect::<String>()
                                ))
                            });
                            continue;
                        }
                        for run in ctx_protocol::sgr::runs(line, &palette) {
                            pieces.push(Piece {
                                text: run.text,
                                color: run.style.foreground,
                                bold: run.style.flags & 1 != 0,
                                link: None,
                                dim: run.style.flags & 4 != 0,
                            });
                        }
                        pieces.push(piece("\n"));
                    }
                    if let Some(n) = &c.note {
                        pieces.push(Piece { dim: true, ..piece(format!("[{n}]\n")) });
                    }
                    pieces.push(piece("\n"));
                }
            }
            Response::Error(e) => pieces.push(piece(format!("{}\n", e.message))),
            _ => {}
        }
        set_text(&h.viewer, &pieces);
    });
}

// ---------------------------------------------------------------------------------------------
// Find in all tabs
// ---------------------------------------------------------------------------------------------

pub fn find_all_open(app: &mut App) {
    if app.panels.find.is_none() {
        let mtm = app.mtm;
        let d = delegate(app);
        let win = panel_window(mtm, "Find in All Tabs", 720.0, 480.0);
        let content = win.contentView().unwrap();
        let b = content.bounds();
        let search = NSSearchField::new(mtm);
        search.setFrame(NSRect::new(NSPoint::new(10.0, b.size.height - 34.0), NSSize::new(b.size.width - 20.0, 24.0)));
        search.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinYMargin | NSAutoresizingMaskOptions::ViewWidthSizable,
        );
        search.setPlaceholderString(Some(&NSString::from_str("Text to find in open and saved tabs (press Return)")));
        unsafe {
            search.setTarget(Some(&d));
            search.setAction(Some(sel!(findAllSearch:)));
        }
        search.setSendsWholeSearchString(true);
        let (scroll, results) =
            text_view(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(b.size.width, b.size.height - 44.0)), &d);
        content.addSubview(&search);
        content.addSubview(&scroll);
        app.panels.find = Some(FindPanel { window: win, search, results });
    }
    if let Some(f) = &app.panels.find {
        f.window.makeKeyAndOrderFront(None);
        f.window.makeFirstResponder(Some(&f.search));
    }
}

fn find_all_run(app: &mut App, text: String) {
    if text.trim().is_empty() {
        return;
    }
    let r = Request::Find {
        text: text.clone(),
        filter: Filter { tab: None, folder: None, by: None, since: None },
        limit: 100,
    };
    app.request(&format!("ctx find \"{text}\""), r, |a, r| {
        let Some(f) = &a.panels.find else { return };
        let mut pieces = Vec::new();
        match r {
            Response::Ok(Output::Found(found)) => {
                if found.hits.is_empty() {
                    pieces.push(Piece { dim: true, ..piece("Nothing found.\n") });
                }
                for h in &found.hits {
                    let name = h.tab_name.as_ref().map(|n| format!(" \u{201c}{n}\u{201d}")).unwrap_or_default();
                    pieces.push(Piece {
                        link: Some(format!("ctx://tab/{}", h.tab)),
                        bold: true,
                        ..piece(format!("tab {}{name}", h.tab))
                    });
                    let what = match (&h.command_text, h.found_in.as_str()) {
                        (Some(c), "command") => format!(" — command `{c}`"),
                        (Some(c), _) => format!(" — output of `{c}`"),
                        (None, w) => format!(" — {w}"),
                    };
                    pieces.push(Piece { dim: true, ..piece(format!("{what} · {}\n", ago(&h.at))) });
                    for b in &h.before {
                        pieces.push(Piece { dim: true, ..piece(format!("    {b}\n")) });
                    }
                    pieces.push(piece(format!("    {}\n", h.text)));
                    for x in &h.after {
                        pieces.push(Piece { dim: true, ..piece(format!("    {x}\n")) });
                    }
                    pieces.push(piece("\n"));
                }
            }
            Response::Error(e) => pieces.push(piece(format!("{}\n", e.message))),
            _ => {}
        }
        set_text(&f.results, &pieces);
    });
}

/// The self-test's way of pressing Return in the find field.
pub fn find_all_search_for_test(app: &mut App, text: &str) {
    if let Some(f) = &app.panels.find {
        f.search.setStringValue(&NSString::from_str(text));
    }
    find_all_run(app, text.to_string());
}
