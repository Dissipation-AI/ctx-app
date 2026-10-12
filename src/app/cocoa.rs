//! AppKit: the application delegate, windows, the terminal view (keyboard, text input, mouse,
//! drops), the tab bar, the find and reopen bars, menus and alerts. These classes hold almost no
//! state; they forward to the [`App`](super::App).

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

// ---------------------------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------------------------

thread_local! {
    static DELEGATE: RefCell<Option<Retained<AppDelegate>>> = const { RefCell::new(None) };
}

fn delegate() -> Option<Retained<AppDelegate>> {
    DELEGATE.with(|d| d.borrow().clone())
}

pub fn init_application(mtm: MainThreadMarker) {
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
            let mtm = self.mtm();
            super::launched(mtm);
            #[allow(deprecated)]
            NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            let summary = with_app(|a| if a.quitting { None } else { a.quit_summary() }).flatten();
            if let Some(s) = summary
                && !ask("Quit CTX?", &format!("These are still running and will be stopped:\n{s}"), "Quit", "Cancel")
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

    unsafe impl NSControlTextEditingDelegate for AppDelegate {
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn control_command(&self, control: &NSControl, _tv: &NSTextView, sel: Sel) -> bool {
            find_command(self.mtm(), control.tag() as u64, sel)
        }
    }

    unsafe impl NSSearchFieldDelegate for AppDelegate {}
    unsafe impl NSTextFieldDelegate for AppDelegate {}

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
                if let Some(w) = a.key_window() {
                    let name = name.filter(|n| *n != a.loaded.app.theme);
                    a.set_window_theme(w, name);
                    rebuild_window_menu(a);
                }
            });
        }

        #[unsafe(method(findChanged:))]
        fn find_changed(&self, sender: &NSSearchField) {
            let w = sender.tag() as u64;
            let text = sender.stringValue().to_string();
            later(move |a| a.find_changed(w, text));
        }

        #[unsafe(method(findDone:))]
        fn find_done(&self, sender: &NSButton) {
            let w = sender.tag() as u64;
            later(move |a| a.close_find(w));
        }

        #[unsafe(method(reopenAccept:))]
        fn reopen_accept(&self, sender: &NSButton) {
            let w = sender.tag() as u64;
            later(move |a| a.reopen_offer(w, true));
        }

        #[unsafe(method(reopenDecline:))]
        fn reopen_decline(&self, sender: &NSButton) {
            let w = sender.tag() as u64;
            later(move |a| a.reopen_offer(w, false));
        }

        #[unsafe(method(tabRename:))]
        fn tab_rename(&self, sender: &NSMenuItem) {
            let t = sender.tag() as u64;
            later(move |a| {
                let current = a.tab_info(t).and_then(|i| i.name.clone()).unwrap_or_default();
                super::outside(move || {
                    if let Some(name) = prompt("Rename tab", "Letters, digits, spaces, - and _.", &current) {
                        later(move |a| a.rename_tab(t, name));
                    }
                });
            });
        }

        #[unsafe(method(tabClose:))]
        fn tab_close(&self, sender: &NSMenuItem) {
            let t = sender.tag() as u64;
            later(move |a| a.close_tab(t, false));
        }

        #[unsafe(method(tabTile:))]
        fn tab_tile(&self, sender: &NSMenuItem) {
            let t = sender.tag() as u64;
            later(move |a| {
                if let Some(w) = a.tab_info(t).map(|i| i.window) {
                    a.tile(w, t, None, crate::layout::Edge::Right);
                }
            });
        }

        #[unsafe(method(tabUntile:))]
        fn tab_untile(&self, sender: &NSMenuItem) {
            let t = sender.tag() as u64;
            later(move |a| {
                if let Some(w) = a.tab_info(t).map(|i| i.window) {
                    a.untile(w, t);
                }
            });
        }

        #[unsafe(method(tabNewWindow:))]
        fn tab_new_window(&self, sender: &NSMenuItem) {
            let t = sender.tag() as u64;
            later(move |a| a.move_tab(t, ctx_protocol::WindowRef::New, None));
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn find_command(mtm: MainThreadMarker, w: u64, sel: Sel) -> bool {
    if sel == sel!(cancelOperation:) {
        later(move |a| a.close_find(w));
        return true;
    }
    if sel == sel!(insertNewline:) {
        let back = current_flags(mtm) & keys::SHIFT != 0;
        later(move |a| a.find_step(w, !back));
        return true;
    }
    false
}

pub fn system_is_dark(mtm: MainThreadMarker) -> bool {
    let app = NSApplication::sharedApplication(mtm);
    let appearance = app.effectiveAppearance();
    let names = unsafe { NSArray::from_slice(&[NSAppearanceNameAqua, NSAppearanceNameDarkAqua]) };
    appearance.bestMatchFromAppearancesWithNames(&names).is_some_and(|n| &*n == unsafe { NSAppearanceNameDarkAqua })
}

fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("main thread")
}

fn current_flags(mtm: MainThreadMarker) -> u64 {
    NSApplication::sharedApplication(mtm).currentEvent().map(|e| e.modifierFlags().0 as u64).unwrap_or(0)
}

pub fn terminate() {
    NSApplication::sharedApplication(mtm()).terminate(None);
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

pub fn alert(title: &str, text: &str) {
    let a = NSAlert::new(mtm());
    a.setMessageText(&NSString::from_str(title));
    a.setInformativeText(&NSString::from_str(text));
    a.runModal();
}

/// A question with two buttons; true for the first.
pub fn ask(title: &str, text: &str, yes: &str, no: &str) -> bool {
    let a = NSAlert::new(mtm());
    a.setMessageText(&NSString::from_str(title));
    a.setInformativeText(&NSString::from_str(text));
    a.addButtonWithTitle(&NSString::from_str(yes));
    a.addButtonWithTitle(&NSString::from_str(no));
    a.runModal() == NSAlertFirstButtonReturn
}

/// Ask for a line of text.
pub fn prompt(title: &str, info: &str, default: &str) -> Option<String> {
    let mtm = mtm();
    let a = NSAlert::new(mtm);
    a.setMessageText(&NSString::from_str(title));
    a.setInformativeText(&NSString::from_str(info));
    a.addButtonWithTitle(&NSString::from_str("OK"));
    a.addButtonWithTitle(&NSString::from_str("Cancel"));
    let field = NSTextField::textFieldWithString(&NSString::from_str(default), mtm);
    field.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(280.0, 24.0)));
    a.setAccessoryView(Some(&field));
    a.window().setInitialFirstResponder(Some(&field));
    if a.runModal() == NSAlertFirstButtonReturn {
        let s = field.stringValue().to_string();
        if !s.trim().is_empty() {
            return Some(s.trim().to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Clipboard, links, cursors
// ---------------------------------------------------------------------------------------------

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

pub fn set_pointing_hand(on: bool) {
    if on { NSCursor::pointingHandCursor().set() } else { NSCursor::IBeamCursor().set() }
}

pub fn start_ticker() -> Retained<NSTimer> {
    let block = RcBlock::new(|_t: NonNull<NSTimer>| {
        if with_app(|a| a.tick()).is_none() {
            later(|a| a.tick());
        }
    });
    unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.53, true, &block) }
}

// ---------------------------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------------------------

/// The AppKit objects of one window.
pub struct Views {
    pub window: Retained<NSWindow>,
    pub delegate: Retained<WindowDelegate>,
    pub container: Retained<Container>,
    pub tabbar: Retained<TabBar>,
    pub term: Retained<TermView>,
    pub findbar: Retained<NSView>,
    pub find_field: Retained<NSSearchField>,
    pub find_status: Retained<NSTextField>,
    pub reopen: Retained<NSView>,
    pub reopen_label: Retained<NSTextField>,
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
            let n = self.ivars().window.get();
            if with_app(|a| a.window_resized(n)).is_none() {
                later(move |a| a.window_resized(n));
            }
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
    }
);

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CTXContainer"]
    pub struct Container;

    impl Container {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

pub fn make_window(mtm: MainThreadMarker, n: u64, frame: Option<[f64; 4]>, size: (f64, f64), index: usize) -> Views {
    let content = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size.0, size.1));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            content,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTabbingMode(NSWindowTabbingMode::Disallowed);
    window.setRestorable(false);
    window.setContentMinSize(NSSize::new(240.0, 120.0));
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
    let container: Retained<Container> = unsafe { msg_send![Container::alloc(mtm), initWithFrame: content] };
    let tabbar = TabBar::new(mtm, n);
    let term = TermView::new(mtm, n);
    // Find bar.
    let findbar = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
    let find_field = NSSearchField::new(mtm);
    find_field.setTag(n as isize);
    if let Some(d) = delegate_obj() {
        unsafe {
            find_field.setTarget(Some(&d));
            find_field.setAction(Some(sel!(findChanged:)));
            find_field.setDelegate(Some(ProtocolObject::from_ref(&*delegate().unwrap())));
        }
    }
    find_field.setSendsSearchStringImmediately(true);
    find_field.setPlaceholderString(Some(&NSString::from_str("Find in this tab")));
    let find_status = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    let done = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Done"),
            delegate_obj().as_deref(),
            Some(sel!(findDone:)),
            mtm,
        )
    };
    done.setTag(n as isize);
    findbar.addSubview(&find_field);
    findbar.addSubview(&find_status);
    findbar.addSubview(&done);
    findbar.setHidden(true);
    // Reopen bar.
    let reopen = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
    let reopen_label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    let yes = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Reopen"),
            delegate_obj().as_deref(),
            Some(sel!(reopenAccept:)),
            mtm,
        )
    };
    yes.setTag(n as isize);
    let no = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Not Now"),
            delegate_obj().as_deref(),
            Some(sel!(reopenDecline:)),
            mtm,
        )
    };
    no.setTag(n as isize);
    reopen.addSubview(&reopen_label);
    reopen.addSubview(&yes);
    reopen.addSubview(&no);
    reopen.setHidden(true);
    container.addSubview(&tabbar);
    container.addSubview(&reopen);
    container.addSubview(&term);
    container.addSubview(&findbar);
    window.setContentView(Some(&container));
    window.makeFirstResponder(Some(&term));
    Views {
        window,
        delegate: wdelegate,
        container,
        tabbar,
        term,
        findbar,
        find_field,
        find_status,
        reopen,
        reopen_label,
    }
}

fn delegate_obj() -> Option<Retained<AnyObject>> {
    delegate().map(|d| Retained::into_super(Retained::into_super(d)))
}

pub fn backing_scale(v: &Views) -> f64 {
    v.window.backingScaleFactor()
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

/// Lay out the bars; returns the terminal view's size in points.
pub fn layout_bars(v: &Views, tab_bar: bool, reopen: bool, find: bool) -> (f64, f64) {
    let b = v.container.bounds();
    let (w, h) = (b.size.width, b.size.height);
    let mut top = 0.0;
    v.tabbar.setHidden(!tab_bar);
    if tab_bar {
        v.tabbar.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, super::TAB_BAR_HEIGHT)));
        top += super::TAB_BAR_HEIGHT;
    }
    v.reopen.setHidden(!reopen);
    if reopen {
        v.reopen.setFrame(NSRect::new(NSPoint::new(0.0, top), NSSize::new(w, super::BAR_HEIGHT)));
        let subs = v.reopen.subviews();
        if subs.count() == 3 {
            subs.objectAtIndex(0)
                .setFrame(NSRect::new(NSPoint::new(10.0, 6.0), NSSize::new((w - 220.0).max(60.0), 18.0)));
            subs.objectAtIndex(1).setFrame(NSRect::new(NSPoint::new(w - 200.0, 2.0), NSSize::new(90.0, 26.0)));
            subs.objectAtIndex(2).setFrame(NSRect::new(NSPoint::new(w - 105.0, 2.0), NSSize::new(95.0, 26.0)));
        }
        top += super::BAR_HEIGHT;
    }
    let mut bottom = 0.0;
    v.findbar.setHidden(!find);
    if find {
        bottom = super::BAR_HEIGHT;
        v.findbar.setFrame(NSRect::new(NSPoint::new(0.0, h - bottom), NSSize::new(w, bottom)));
        v.find_field.setFrame(NSRect::new(NSPoint::new(8.0, 4.0), NSSize::new((w - 200.0).clamp(80.0, 360.0), 22.0)));
        v.find_status
            .setFrame(NSRect::new(NSPoint::new((w - 200.0).clamp(80.0, 360.0) + 16.0, 7.0), NSSize::new(100.0, 18.0)));
        let subs = v.findbar.subviews();
        if subs.count() == 3 {
            subs.objectAtIndex(2).setFrame(NSRect::new(NSPoint::new(w - 80.0, 2.0), NSSize::new(72.0, 26.0)));
        }
    }
    let th = (h - top - bottom).max(1.0);
    v.term.setFrame(NSRect::new(NSPoint::new(0.0, top), NSSize::new(w, th)));
    (w, th)
}

pub fn term_size(v: &Views) -> (f64, f64) {
    let b = v.term.bounds();
    (b.size.width, b.size.height)
}

pub fn attach_surface(v: &Views, s: &Surface, scale: f64) {
    if let (Some(layer), Some(io)) = (v.term.layer(), s.io_surface()) {
        layer.setContentsScale(scale);
        unsafe {
            layer.setContentsGravity(objc2_quartz_core::kCAGravityTopLeft);
            layer.setContents(Some(io));
        }
    }
}

pub fn detach_surface(v: &Views) {
    if let Some(layer) = v.term.layer() {
        unsafe { layer.setContents(None) };
    }
}

pub fn surface_changed(v: &Views) {
    if let Some(layer) = v.term.layer() {
        let _: () = unsafe { msg_send![&*layer, setContentsChanged] };
    }
}

pub fn set_find_status(v: &Views, s: &str) {
    v.find_status.setStringValue(&NSString::from_str(s));
}

pub fn focus_find(v: &Views) {
    v.window.makeFirstResponder(Some(&v.find_field));
    unsafe { v.find_field.selectText(None) };
}

pub fn focus_term(v: &Views) {
    v.window.makeFirstResponder(Some(&v.term));
}

pub fn show_reopen_bar(v: &Views, text: &str) {
    v.reopen_label.setStringValue(&NSString::from_str(text));
    v.reopen.setHidden(false);
}

pub fn hide_reopen_bar(v: &Views) {
    v.reopen.setHidden(true);
}

/// The frontmost shown window under a screen point, and the point in its terminal view.
pub fn window_under(app: &App, sx: f64, sy: f64) -> Option<(u64, f64, f64)> {
    let ordered = NSApplication::sharedApplication(app.mtm).orderedWindows();
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
        let lp = win.views.term.convertPoint_fromView(p, None);
        let b = win.views.term.bounds();
        if lp.x >= 0.0 && lp.y >= 0.0 && lp.x < b.size.width && lp.y < b.size.height {
            return Some((*n, lp.x, lp.y));
        }
        return None;
    }
    None
}

/// The tab bar under a screen point: window and insertion index.
pub fn tab_bar_under(app: &App, sx: f64, sy: f64) -> Option<(u64, usize)> {
    let ordered = NSApplication::sharedApplication(app.mtm).orderedWindows();
    for nsw in ordered.iter() {
        let Some((n, win)) = app.wins.iter().find(|(_, w)| Retained::as_ptr(&w.views.window) == Retained::as_ptr(&nsw))
        else {
            continue;
        };
        let f = nsw.frame();
        if !(sx >= f.origin.x && sx < f.origin.x + f.size.width && sy >= f.origin.y && sy < f.origin.y + f.size.height)
        {
            continue;
        }
        if win.views.tabbar.isHidden() {
            return None;
        }
        let p = nsw.convertPointFromScreen(NSPoint::new(sx, sy));
        let lp = win.views.tabbar.convertPoint_fromView(p, None);
        let b = win.views.tabbar.bounds();
        if lp.y >= 0.0 && lp.y < b.size.height && lp.x >= 0.0 && lp.x < b.size.width {
            let count = win.views.tabbar.ivars().items.borrow().len();
            let rects = tab_rects(b.size.width, count);
            let idx = rects.iter().position(|(x, w)| lp.x < x + w / 2.0).unwrap_or(count);
            return Some((*n, idx));
        }
        return None;
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

fn menu_item(mtm: MainThreadMarker, title: &str, action: Sel, keys: &str, tag: isize) -> Retained<NSMenuItem> {
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

/// Build the menu bar from the shortcut table. Returns the action for each menu item tag.
pub fn build_menus(app: &App) -> Vec<Action> {
    let mtm = app.mtm;
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
                    let item = menu_item(mtm, e.title, sel!(menuAction:), &keys, actions.len() as isize);
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
            themes.setTitle(&NSString::from_str("Theme"));
            let sub = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Theme"));
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

const WINDOW_LIST_TAG: isize = 1_000_000;

/// Refresh the dynamic parts: `ctx` windows that aren't shown (Window menu), and themes (View).
pub fn rebuild_window_menu(app: &App) {
    let mtm = app.mtm;
    let Some(main) = NSApplication::sharedApplication(mtm).mainMenu() else { return };
    // Window menu: windows the app doesn't show (an Agents window, say).
    if let Some(wm) = main.itemWithTitle(&NSString::from_str("Window")).and_then(|i| i.submenu()) {
        while let Some(item) = wm.itemWithTag(WINDOW_LIST_TAG) {
            wm.removeItem(&item);
        }
        let hidden: Vec<&ctx_protocol::WindowInfo> =
            app.list.windows.iter().filter(|w| !app.wins.contains_key(&w.number)).collect();
        if !hidden.is_empty() {
            let sep = NSMenuItem::separatorItem(mtm);
            sep.setTag(WINDOW_LIST_TAG);
            wm.addItem(&sep);
            for w in hidden {
                let name = match &w.name {
                    Some(n) => format!("Show Window {} \u{201c}{n}\u{201d}", w.number),
                    None => format!("Show Window {}", w.number),
                };
                let title = format!("{name} ({} {})", w.tabs.len(), if w.tabs.len() == 1 { "tab" } else { "tabs" });
                let item = menu_item(mtm, &title, sel!(showCtxWindow:), "", w.number as isize);
                // The tag identifies the window; mark the item as ours by its represented object.
                unsafe { item.setRepresentedObject(Some(&NSString::from_str("ctx-window"))) };
                wm.addItem(&item);
                item.setTag(w.number as isize);
            }
        }
        // Remove stale window items (they carry the window number as tag).
        let count = wm.numberOfItems();
        let mut i = count - 1;
        while i >= 0 {
            if let Some(item) = wm.itemAtIndex(i)
                && item
                    .representedObject()
                    .and_then(|o| o.downcast::<NSString>().ok())
                    .is_some_and(|s| s.to_string() == "ctx-window")
                && app.wins.contains_key(&(item.tag() as u64))
            {
                wm.removeItem(&item);
            }
            i -= 1;
        }
    }
    // Theme submenu, with the key window's theme checked.
    if let Some(vm) = main.itemWithTitle(&NSString::from_str("View")).and_then(|i| i.submenu())
        && let Some(sub) = vm.itemWithTitle(&NSString::from_str("Theme")).and_then(|i| i.submenu())
    {
        sub.removeAllItems();
        let current = app
            .key_window()
            .and_then(|w| app.wins.get(&w))
            .and_then(|w| w.theme_name.clone())
            .unwrap_or_else(|| app.loaded.app.theme.clone());
        for name in crate::theme::available(&app.themes_dir) {
            let title = if name == "system" { "System (light or dark)".to_string() } else { name.clone() };
            let item = menu_item(mtm, &title, sel!(setTheme:), "", 0);
            unsafe { item.setRepresentedObject(Some(&NSString::from_str(&name))) };
            item.setState(if name == current { NSControlStateValueOn } else { NSControlStateValueOff });
            sub.addItem(&item);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The terminal view
// ---------------------------------------------------------------------------------------------

pub struct TermIvars {
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
    #[name = "CTXTermView"]
    #[ivars = TermIvars]
    pub struct TermView;

    unsafe impl NSObjectProtocol for TermView {}

    impl TermView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
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
            later(move |a| a.mouse_moved(w, loc.x, loc.y, flags));
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
            later(move |a| a.mouse_down(w, p.x, p.y, clicks, flags, 1));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            if with_app(|a| a.mouse_dragged(w, p.x, p.y, flags, 1)).is_none() {
                later(move |a| a.mouse_dragged(w, p.x, p.y, flags, 1));
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            later(move |a| a.mouse_up(w, p.x, p.y, flags, 1));
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            let tracking = with_app(|a| {
                a.hit(w, p.x, p.y).and_then(|(t, ..)| a.views.get(&t)).is_some_and(|v| v.screen.mouse_tracking)
            })
            .unwrap_or(false);
            if tracking && flags & keys::OPTION == 0 {
                later(move |a| a.mouse_down(w, p.x, p.y, 1, flags, 2));
                return;
            }
            // A small menu: Copy, Paste.
            let mtm = self.mtm();
            let menu = NSMenu::new(mtm);
            let copy = menu_item(mtm, "Copy", sel!(menuAction:), "", action_tag(Action::Copy));
            let paste = menu_item(mtm, "Paste", sel!(menuAction:), "", action_tag(Action::Paste));
            menu.addItem(&copy);
            menu.addItem(&paste);
            NSMenu::popUpContextMenu_withEvent_forView(&menu, e, self);
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            later(move |a| a.mouse_up(w, p.x, p.y, flags, 2));
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            let b = button_of(e);
            later(move |a| a.mouse_down(w, p.x, p.y, 1, flags, b));
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            let b = button_of(e);
            later(move |a| a.mouse_up(w, p.x, p.y, flags, b));
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            later(move |a| a.mouse_moved(w, p.x, p.y, flags));
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, e: &NSEvent) {
            let w = self.ivars().window.get();
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let flags = e.modifierFlags().0 as u64;
            let precise = e.hasPreciseScrollingDeltas();
            let dy = e.scrollingDeltaY();
            if with_app(|a| a.scroll_event(w, p.x, p.y, dy, precise, flags)).is_none() {
                later(move |a| a.scroll_event(w, p.x, p.y, dy, precise, flags));
            }
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            if let Some(old) = self.ivars().tracking.borrow_mut().take() {
                self.removeTrackingArea(&old);
            }
            let opts = NSTrackingAreaOptions::MouseMoved
                | NSTrackingAreaOptions::ActiveInKeyWindow
                | NSTrackingAreaOptions::InVisibleRect
                | NSTrackingAreaOptions::CursorUpdate;
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(NSTrackingArea::alloc(), NSRect::ZERO, opts, Some(self), None)
            };
            self.addTrackingArea(&area);
            *self.ivars().tracking.borrow_mut() = Some(area);
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
        }

        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _e: &NSEvent) {
            NSCursor::IBeamCursor().set();
        }

        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn appearance_changed(&self) {
            let dark = system_is_dark(self.mtm());
            later(move |a| a.appearance_changed(dark));
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            true
        }
    }

    unsafe impl NSTextInputClient for TermView {
        #[unsafe(method(insertText:replacementRange:))]
        fn insert_text(&self, string: &AnyObject, _r: NSRange) {
            let w = self.ivars().window.get();
            let s = string_of(string);
            if with_app(|a| a.insert_text(w, &s)).is_none() {
                later(move |a| a.insert_text(w, &s));
            }
        }

        #[unsafe(method(doCommandBySelector:))]
        fn do_command(&self, sel: Sel) {
            let w = self.ivars().window.get();
            let key = if sel == sel!(insertNewline:) {
                "enter"
            } else if sel == sel!(insertTab:) {
                "tab"
            } else if sel == sel!(deleteBackward:) {
                "backspace"
            } else if sel == sel!(cancelOperation:) {
                "escape"
            } else {
                return;
            };
            later(move |a| {
                if let Some(t) = a.focused_tab(w) {
                    a.input(t, ctx_protocol::AppInput::Key { key: key.into(), text: None });
                }
            });
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        fn set_marked_text(&self, string: &AnyObject, _sel: NSRange, _rep: NSRange) {
            let w = self.ivars().window.get();
            let s = string_of(string);
            if with_app(|a| a.set_marked(w, Some(s.clone()))).is_none() {
                later(move |a| a.set_marked(w, Some(s)));
            }
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            let w = self.ivars().window.get();
            later(move |a| a.set_marked(w, None));
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

    unsafe impl NSDraggingDestination for TermView {
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

fn action_tag(action: Action) -> isize {
    with_app(|a| a.menu_actions.iter().position(|x| *x == action)).flatten().map(|i| i as isize).unwrap_or(-1)
}

impl TermView {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TermIvars { window: Cell::new(window), tracking: RefCell::new(None) });
        let v: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        v.setWantsLayer(true);
        v.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::Never);
        let types = NSArray::from_slice(&[unsafe { NSPasteboardTypeFileURL }]);
        v.registerForDraggedTypes(&types);
        v
    }
}

// ---------------------------------------------------------------------------------------------
// The tab bar
// ---------------------------------------------------------------------------------------------

/// What the tab bar shows for one tab.
#[derive(Debug, Clone, PartialEq)]
pub struct TabItem {
    pub tab: u64,
    pub label: String,
    pub title: Option<String>,
    /// Opened by an agent: its name.
    pub agent: Option<String>,
    /// Runs with a scoped launcher's permissions.
    pub launcher: Option<String>,
    /// Another client touched it recently: true if it typed, false if it only read.
    pub badge: Option<bool>,
    pub selected: bool,
    pub tiled: bool,
    pub busy: bool,
}

struct BarPress {
    index: usize,
    start: NSPoint,
    dragging: bool,
}

pub struct TabBarIvars {
    window: Cell<u64>,
    items: RefCell<Vec<TabItem>>,
    dark: Cell<bool>,
    hover: Cell<Option<usize>>,
    press: RefCell<Option<BarPress>>,
    tracking: RefCell<Option<Retained<NSTrackingArea>>>,
    drag_x: Cell<Option<f64>>,
}

/// Each tab's x and width.
pub fn tab_rects(width: f64, n: usize) -> Vec<(f64, f64)> {
    if n == 0 {
        return Vec::new();
    }
    let w = ((width - 8.0) / n as f64).clamp(40.0, 240.0);
    (0..n).map(|i| (4.0 + i as f64 * w, w)).collect()
}

fn color(rgb: (f64, f64, f64), a: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(rgb.0, rgb.1, rgb.2, a)
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CTXTabBar"]
    #[ivars = TabBarIvars]
    pub struct TabBar;

    unsafe impl NSObjectProtocol for TabBar {}

    impl TabBar {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _r: NSRect) {
            self.draw();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let Some(i) = self.index_at(p.x) else { return };
            let items = self.ivars().items.borrow().clone();
            let Some(item) = items.get(i).cloned() else { return };
            let w = self.ivars().window.get();
            // The close button.
            let rects = tab_rects(self.bounds().size.width, items.len());
            let (x, _) = rects[i];
            if self.ivars().hover.get() == Some(i) && p.x < x + 20.0 {
                later(move |a| a.close_tab(item.tab, false));
                return;
            }
            if e.clickCount() == 2 {
                let current = item.label.split(" — ").next().unwrap_or("").to_string();
                let current = if item.label.contains(" — ") { current } else { String::new() };
                let t = item.tab;
                super::outside(move || {
                    if let Some(name) = prompt("Rename tab", "Letters, digits, spaces, - and _.", &current) {
                        later(move |a| a.rename_tab(t, name));
                    }
                });
                return;
            }
            *self.ivars().press.borrow_mut() = Some(BarPress { index: i, start: p, dragging: false });
            later(move |a| a.select_tab(w, item.tab));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let mut press = self.ivars().press.borrow_mut();
            let Some(pr) = press.as_mut() else { return };
            if !pr.dragging && ((p.x - pr.start.x).abs() > 4.0 || (p.y - pr.start.y).abs() > 4.0) {
                pr.dragging = true;
                NSCursor::closedHandCursor().set();
            }
            if pr.dragging {
                self.ivars().drag_x.set(Some(p.x));
                self.setNeedsDisplay(true);
                let screen = NSEvent::mouseLocation();
                later(move |a| a.drag_hint(screen.x, screen.y));
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _e: &NSEvent) {
            let press = self.ivars().press.borrow_mut().take();
            self.ivars().drag_x.set(None);
            self.setNeedsDisplay(true);
            NSCursor::arrowCursor().set();
            let Some(pr) = press else { return };
            if !pr.dragging {
                return;
            }
            let Some(item) = self.ivars().items.borrow().get(pr.index).cloned() else { return };
            let w = self.ivars().window.get();
            let screen = NSEvent::mouseLocation();
            later(move |a| {
                let bar = tab_bar_under(a, screen.x, screen.y);
                // Dropping onto its own place changes nothing.
                let bar = bar.map(|(bw, idx)| if bw == w && idx > pr.index { (bw, idx - 1) } else { (bw, idx) });
                if bar == Some((w, pr.index)) {
                    for win in a.wins.values_mut() {
                        if win.drop_hint.take().is_some() {
                            win.dirty = Some(super::Dirty::All);
                        }
                    }
                    a.redraw_dirty();
                    return;
                }
                a.drop_tab(item.tab, w, screen.x, screen.y, bar);
            });
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, e: &NSEvent) {
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let Some(i) = self.index_at(p.x) else { return };
            let Some(item) = self.ivars().items.borrow().get(i).cloned() else { return };
            let mtm = self.mtm();
            let menu = NSMenu::new(mtm);
            let t = item.tab as isize;
            menu.addItem(&menu_item(mtm, "Rename Tab…", sel!(tabRename:), "", t));
            if item.tiled {
                menu.addItem(&menu_item(mtm, "Stop Showing Side by Side", sel!(tabUntile:), "", t));
            } else if !item.selected {
                menu.addItem(&menu_item(mtm, "Show Side by Side", sel!(tabTile:), "", t));
            }
            menu.addItem(&menu_item(mtm, "Move to New Window", sel!(tabNewWindow:), "", t));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&menu_item(mtm, "Close Tab", sel!(tabClose:), "", t));
            NSMenu::popUpContextMenu_withEvent_forView(&menu, e, self);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, e: &NSEvent) {
            let p = self.convertPoint_fromView(e.locationInWindow(), None);
            let i = self.index_at(p.x);
            if self.ivars().hover.get() != i {
                self.ivars().hover.set(i);
                let tip = i.and_then(|i| self.ivars().items.borrow().get(i).cloned()).map(|it| {
                    let mut s = it.label.clone();
                    if let Some(t) = &it.title {
                        s.push_str(&format!("\n{t}"));
                    }
                    if let Some(a) = &it.agent {
                        s.push_str(&format!("\nOpened by {a}"));
                    }
                    if let Some(l) = &it.launcher {
                        s.push_str(&format!("\nRunning with the permissions of {l}"));
                    }
                    match it.badge {
                        Some(true) => s.push_str("\nAnother program typed here in the last minute (⇧⌘A shows what)"),
                        Some(false) => s.push_str("\nAnother program read this tab in the last minute (⇧⌘A shows what)"),
                        None => {}
                    }
                    s
                });
                self.setToolTip(tip.map(|t| NSString::from_str(&t)).as_deref());
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _e: &NSEvent) {
            self.ivars().hover.set(None);
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            if let Some(old) = self.ivars().tracking.borrow_mut().take() {
                self.removeTrackingArea(&old);
            }
            let opts = NSTrackingAreaOptions::MouseMoved
                | NSTrackingAreaOptions::MouseEnteredAndExited
                | NSTrackingAreaOptions::ActiveAlways
                | NSTrackingAreaOptions::InVisibleRect;
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(NSTrackingArea::alloc(), NSRect::ZERO, opts, Some(self), None)
            };
            self.addTrackingArea(&area);
            *self.ivars().tracking.borrow_mut() = Some(area);
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
        }
    }
);

impl TabBar {
    fn new(mtm: MainThreadMarker, window: u64) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TabBarIvars {
            window: Cell::new(window),
            items: RefCell::new(Vec::new()),
            dark: Cell::new(false),
            hover: Cell::new(None),
            press: RefCell::new(None),
            tracking: RefCell::new(None),
            drag_x: Cell::new(None),
        });
        let v: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        v.setWantsLayer(true);
        v
    }

    fn index_at(&self, x: f64) -> Option<usize> {
        let n = self.ivars().items.borrow().len();
        tab_rects(self.bounds().size.width, n).iter().position(|(rx, w)| x >= *rx && x < rx + w)
    }

    fn draw(&self) {
        let b = self.bounds();
        let dark = self.ivars().dark.get();
        let items = self.ivars().items.borrow();
        let bg = if dark { (0.16, 0.16, 0.17) } else { (0.88, 0.88, 0.89) };
        let sel_bg = if dark { (0.25, 0.25, 0.27) } else { (0.98, 0.98, 0.98) };
        let fg = if dark { (0.92, 0.92, 0.92) } else { (0.12, 0.12, 0.12) };
        let dim = if dark { (0.6, 0.6, 0.62) } else { (0.42, 0.42, 0.44) };
        color(bg, 1.0).setFill();
        NSRectFill(b);
        // Bottom edge.
        color(if dark { (0.08, 0.08, 0.08) } else { (0.72, 0.72, 0.74) }, 1.0).setFill();
        NSRectFill(NSRect::new(NSPoint::new(0.0, b.size.height - 1.0), NSSize::new(b.size.width, 1.0)));
        let rects = tab_rects(b.size.width, items.len());
        let hover = self.ivars().hover.get();
        let dragging = self.ivars().press.borrow().as_ref().filter(|p| p.dragging).map(|p| p.index);
        let font = NSFont::systemFontOfSize(12.0);
        let small = NSFont::systemFontOfSize(10.0);
        for (i, item) in items.iter().enumerate() {
            let (mut x, w) = rects[i];
            if dragging == Some(i)
                && let Some(dx) = self.ivars().drag_x.get()
            {
                x = dx - w / 2.0;
            }
            let r = NSRect::new(NSPoint::new(x + 1.0, 3.0), NSSize::new(w - 2.0, b.size.height - 6.0));
            if item.selected || item.tiled || hover == Some(i) {
                let c = if item.selected {
                    sel_bg
                } else if item.tiled {
                    mix(bg, sel_bg, 0.6)
                } else {
                    mix(bg, sel_bg, 0.4)
                };
                color(c, 1.0).setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r, 5.0, 5.0).fill();
            }
            let mut left = x + 10.0;
            let mut right = x + w - 8.0;
            // Close button on hover.
            if hover == Some(i) {
                draw_text(
                    "×",
                    NSRect::new(NSPoint::new(x + 6.0, 5.0), NSSize::new(12.0, 18.0)),
                    &font,
                    color(dim, 1.0),
                    false,
                );
                left = x + 20.0;
            }
            // Activity badge: a dot, filled orange for "typed", hollow blue for "read only".
            if let Some(typed) = item.badge {
                let d = NSRect::new(NSPoint::new(right - 8.0, b.size.height / 2.0 - 4.0), NSSize::new(8.0, 8.0));
                let path = NSBezierPath::bezierPathWithOvalInRect(d);
                if typed {
                    color((0.95, 0.55, 0.1), 1.0).setFill();
                    path.fill();
                } else {
                    color((0.25, 0.55, 0.95), 1.0).setStroke();
                    path.setLineWidth(1.5);
                    path.stroke();
                }
                right -= 14.0;
            }
            // Agent marker and launcher marker.
            for (tag, c) in [(&item.agent, (0.45, 0.35, 0.85)), (&item.launcher, (0.15, 0.6, 0.45))] {
                if let Some(name) = tag {
                    let text = if c.1 > 0.5 { format!("⚿ {name}") } else { name.clone() };
                    let tw = (text.chars().count() as f64 * 6.0 + 10.0).min(w * 0.4);
                    if right - tw > left + 20.0 {
                        let pr =
                            NSRect::new(NSPoint::new(right - tw, b.size.height / 2.0 - 7.0), NSSize::new(tw, 14.0));
                        color(c, 0.18).setFill();
                        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pr, 4.0, 4.0).fill();
                        draw_text(
                            &text,
                            NSRect::new(NSPoint::new(pr.origin.x + 4.0, pr.origin.y), NSSize::new(tw - 6.0, 14.0)),
                            &small,
                            color(c, 1.0),
                            false,
                        );
                        right -= tw + 4.0;
                    }
                }
            }
            // The name stays whole; the folder is shortened from the left ("…code/site").
            let c = color(if item.selected { fg } else { dim }, 1.0);
            let avail = (right - left).max(10.0);
            let busy = if item.busy { " ●" } else { "" };
            match item.label.split_once(" — ") {
                Some((name, folder)) => {
                    let name = format!("{name}{busy} — ");
                    let nw = text_width(&name, &font).min(avail * 0.7);
                    draw_text(
                        &name,
                        NSRect::new(NSPoint::new(left, 6.0), NSSize::new(nw, 17.0)),
                        &font,
                        c.clone(),
                        false,
                    );
                    let fr = NSRect::new(NSPoint::new(left + nw, 6.0), NSSize::new((avail - nw).max(1.0), 17.0));
                    draw_text(folder, fr, &font, color(dim, 1.0), true);
                }
                None => {
                    let tr = NSRect::new(NSPoint::new(left, 6.0), NSSize::new(avail, 17.0));
                    draw_text(&format!("{}{busy}", item.label), tr, &font, c, true);
                }
            }
        }
    }
}

fn mix(a: (f64, f64, f64), b: (f64, f64, f64), t: f64) -> (f64, f64, f64) {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
}

/// Draw a line of text; `truncate_head` shortens it from the left ("…code/site").
fn draw_text(s: &str, r: NSRect, font: &NSFont, color: Retained<NSColor>, truncate_head: bool) {
    let style = NSMutableParagraphStyle::new();
    style.setLineBreakMode(if truncate_head { NSLineBreakMode::ByTruncatingHead } else { NSLineBreakMode::ByClipping });
    let keys: [&NSAttributedStringKey; 3] =
        unsafe { [NSFontAttributeName, NSForegroundColorAttributeName, NSParagraphStyleAttributeName] };
    let vals: [&AnyObject; 3] = [font.as_ref(), color.as_ref(), style.as_ref()];
    let attrs = NSDictionary::from_slices(&keys, &vals);
    unsafe { NSString::from_str(s).drawInRect_withAttributes(r, Some(&attrs)) };
}

fn text_width(s: &str, font: &NSFont) -> f64 {
    let keys: [&NSAttributedStringKey; 1] = unsafe { [NSFontAttributeName] };
    let vals: [&AnyObject; 1] = [font.as_ref()];
    let attrs = NSDictionary::from_slices(&keys, &vals);
    unsafe { NSString::from_str(s).sizeWithAttributes(Some(&attrs)) }.width.ceil()
}

pub fn set_tab_items(v: &Views, items: Vec<TabItem>, dark: bool) {
    let iv = v.tabbar.ivars();
    let changed = *iv.items.borrow() != items || iv.dark.get() != dark;
    if changed {
        *iv.items.borrow_mut() = items;
        iv.dark.set(dark);
        v.tabbar.setNeedsDisplay(true);
    }
}

/// Save a view's drawing as a PNG (debugging and tests; needs no screen-recording permission).
pub fn dump_view(view: &NSView, path: &std::path::Path) {
    let b = view.bounds();
    let Some(rep) = view.bitmapImageRepForCachingDisplayInRect(b) else { return };
    view.cacheDisplayInRect_toBitmapImageRep(b, &rep);
    let props = NSDictionary::new();
    if let Some(data) = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) } {
        let _ = std::fs::write(path, data.to_vec());
    }
}
