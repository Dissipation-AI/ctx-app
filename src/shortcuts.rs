//! Every action the app has, its default shortcut (Terminal.app conventions), and the menu it
//! lives in. The menus are built from this table, so every shortcut is discoverable; `[app.keys]`
//! in the config rebinds any of them by name.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    About,
    OpenConfig,
    Hide,
    HideOthers,
    ShowAll,
    Quit,
    NewWindow,
    NewTab,
    CloseTab,
    CloseWindow,
    ReopenTab,
    History,
    RenameTab,
    Copy,
    Paste,
    SelectAll,
    ClearScrollback,
    Find,
    FindNext,
    FindPrevious,
    FindAll,
    Bigger,
    BiggerAlt,
    Smaller,
    ActualSize,
    Activity,
    FullScreen,
    Minimise,
    Zoom,
    PreviousTab,
    NextTab,
    Tab(u8),
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    CycleWindows,
    BringAllToFront,
}

pub struct Entry {
    pub action: Action,
    pub name: &'static str,
    pub title: &'static str,
    pub keys: &'static str,
    /// Works by its shortcut but isn't listed (a second key for the same thing).
    pub hidden: bool,
}

const fn e(action: Action, name: &'static str, title: &'static str, keys: &'static str) -> Entry {
    Entry { action, name, title, keys, hidden: false }
}

/// Menus in order, with their items. `None` is a separator.
pub fn menus() -> Vec<(&'static str, Vec<Option<Entry>>)> {
    use Action::*;
    vec![
        (
            "CTX",
            vec![
                Some(e(About, "about", "About CTX", "")),
                None,
                Some(e(OpenConfig, "open_config", "Settings…", "cmd-,")),
                None,
                Some(e(Hide, "hide", "Hide CTX", "cmd-h")),
                Some(e(HideOthers, "hide_others", "Hide Others", "cmd-alt-h")),
                Some(e(ShowAll, "show_all", "Show All", "")),
                None,
                Some(e(Quit, "quit", "Quit CTX", "cmd-q")),
            ],
        ),
        (
            "Shell",
            vec![
                Some(e(NewWindow, "new_window", "New Window", "cmd-n")),
                Some(e(NewTab, "new_tab", "New Tab", "cmd-t")),
                None,
                Some(e(RenameTab, "rename_tab", "Rename Tab…", "cmd-shift-i")),
                None,
                Some(e(ReopenTab, "reopen_tab", "Reopen Closed Tab", "cmd-shift-t")),
                Some(e(History, "history", "History…", "cmd-y")),
                None,
                Some(e(CloseTab, "close_tab", "Close Tab", "cmd-w")),
                Some(e(CloseWindow, "close_window", "Close Window", "cmd-shift-w")),
            ],
        ),
        (
            "Edit",
            vec![
                Some(e(Copy, "copy", "Copy", "cmd-c")),
                Some(e(Paste, "paste", "Paste", "cmd-v")),
                Some(e(SelectAll, "select_all", "Select All", "cmd-a")),
                None,
                Some(e(ClearScrollback, "clear_scrollback", "Clear Scrollback", "cmd-k")),
                None,
                Some(e(Find, "find", "Find…", "cmd-f")),
                Some(e(FindNext, "find_next", "Find Next", "cmd-g")),
                Some(e(FindPrevious, "find_previous", "Find Previous", "cmd-shift-g")),
                Some(e(FindAll, "find_all", "Find in All Tabs…", "cmd-shift-f")),
            ],
        ),
        (
            "View",
            vec![
                Some(e(Bigger, "bigger", "Bigger", "cmd-+")),
                Some(Entry { action: BiggerAlt, name: "bigger_alt", title: "Bigger", keys: "cmd-=", hidden: true }),
                Some(e(Smaller, "smaller", "Smaller", "cmd--")),
                Some(e(ActualSize, "actual_size", "Actual Size", "cmd-0")),
                None,
                Some(e(Activity, "activity", "Activity", "cmd-shift-a")),
                None,
                Some(e(FullScreen, "full_screen", "Enter Full Screen", "ctrl-cmd-f")),
            ],
        ),
        (
            "Window",
            vec![
                Some(e(Minimise, "minimise", "Minimise", "cmd-m")),
                Some(e(Zoom, "zoom", "Zoom", "")),
                Some(e(CycleWindows, "cycle_windows", "Cycle Through Windows", "cmd-`")),
                None,
                Some(e(PreviousTab, "previous_tab", "Show Previous Tab", "cmd-shift-[")),
                Some(e(NextTab, "next_tab", "Show Next Tab", "cmd-shift-]")),
                Some(e(Tab(1), "tab_1", "Tab 1", "cmd-1")),
                Some(e(Tab(2), "tab_2", "Tab 2", "cmd-2")),
                Some(e(Tab(3), "tab_3", "Tab 3", "cmd-3")),
                Some(e(Tab(4), "tab_4", "Tab 4", "cmd-4")),
                Some(e(Tab(5), "tab_5", "Tab 5", "cmd-5")),
                Some(e(Tab(6), "tab_6", "Tab 6", "cmd-6")),
                Some(e(Tab(7), "tab_7", "Tab 7", "cmd-7")),
                Some(e(Tab(8), "tab_8", "Tab 8", "cmd-8")),
                Some(e(Tab(9), "tab_9", "Tab 9", "cmd-9")),
                None,
                Some(e(FocusLeft, "focus_left", "Focus Tile on the Left", "cmd-alt-left")),
                Some(e(FocusRight, "focus_right", "Focus Tile on the Right", "cmd-alt-right")),
                Some(e(FocusUp, "focus_up", "Focus Tile Above", "cmd-alt-up")),
                Some(e(FocusDown, "focus_down", "Focus Tile Below", "cmd-alt-down")),
                None,
                Some(e(BringAllToFront, "bring_all_to_front", "Bring All to Front", "")),
            ],
        ),
    ]
}

pub fn entries() -> Vec<Entry> {
    menus().into_iter().flat_map(|(_, items)| items.into_iter().flatten()).collect()
}

/// The keys for every action after `[app.keys]` overrides. Unknown names are reported.
pub fn bindings(overrides: &BTreeMap<String, String>) -> (BTreeMap<Action, String>, Vec<String>) {
    let all = entries();
    let mut out: BTreeMap<Action, String> = all.iter().map(|e| (e.action, e.keys.to_string())).collect();
    let mut problems = Vec::new();
    for (name, keys) in overrides {
        match all.iter().find(|e| e.name == name) {
            Some(e) => {
                if !keys.is_empty() && keys != "none" && crate::keys::parse_shortcut(keys).is_none() {
                    problems.push(format!("[app.keys] {name} = \"{keys}\" isn't a shortcut like \"cmd-shift-t\""));
                    continue;
                }
                out.insert(e.action, keys.clone());
            }
            None => problems.push(format!("[app.keys] has no action named {name}")),
        }
    }
    // A rebinding that collides takes the key away from the default holder.
    let mut seen: BTreeMap<(String, u64), Action> = BTreeMap::new();
    let overridden: Vec<Action> =
        overrides.keys().filter_map(|n| all.iter().find(|e| e.name == n).map(|e| e.action)).collect();
    let mut order: Vec<Action> = overridden.clone();
    order.extend(out.keys().filter(|a| !overridden.contains(a)));
    for a in order {
        if let Some(k) = out.get(&a).and_then(|k| crate::keys::parse_shortcut(k)) {
            if let std::collections::btree_map::Entry::Vacant(e) = seen.entry(k) {
                e.insert(a);
            } else {
                out.insert(a, String::new());
            }
        }
    }
    (out, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_two_actions_share_a_key() {
        let mut seen = std::collections::HashMap::new();
        for e in entries() {
            if let Some(k) = crate::keys::parse_shortcut(e.keys) {
                assert!(
                    seen.insert(k, e.name).is_none(),
                    "{} shares its key with {}",
                    e.name,
                    seen[&crate::keys::parse_shortcut(e.keys).unwrap()]
                );
            }
        }
    }

    #[test]
    fn every_default_parses_and_names_are_unique() {
        let mut names = std::collections::HashSet::new();
        for e in entries() {
            assert!(names.insert(e.name), "duplicate name {}", e.name);
            assert!(e.keys.is_empty() || crate::keys::parse_shortcut(e.keys).is_some(), "{}", e.keys);
        }
    }

    #[test]
    fn spec_shortcuts_are_present() {
        let keys: Vec<String> = entries().iter().map(|e| crate::keys::display(e.keys)).collect();
        for k in [
            "⌘N",
            "⌘Q",
            "⌘C",
            "⌘V",
            "⌘A",
            "⌘T",
            "⌘W",
            "⇧⌘W",
            "⌘1",
            "⌘9",
            "⇧⌘[",
            "⇧⌘]",
            "⌘K",
            "⇧⌘T",
            "⌘Y",
            "⌘F",
            "⇧⌘F",
            "⌘+",
            "⌘-",
            "⌘0",
            "⌥⌘←",
            "⌥⌘→",
            "⌥⌘↑",
            "⌥⌘↓",
            "⇧⌘I",
            "⇧⌘A",
            "⌘,",
            "⌃⌘F",
            "⌘M",
            "⌘H",
            "⌘`",
        ] {
            assert!(keys.iter().any(|x| x == k), "missing {k}");
        }
    }

    #[test]
    fn every_action_is_in_a_menu() {
        // The menus are the table, so this checks the enum against it.
        let listed: Vec<Action> = entries().iter().map(|e| e.action).collect();
        use Action::*;
        let all = [
            About,
            OpenConfig,
            Hide,
            HideOthers,
            ShowAll,
            Quit,
            NewWindow,
            NewTab,
            CloseTab,
            CloseWindow,
            ReopenTab,
            History,
            RenameTab,
            Copy,
            Paste,
            SelectAll,
            ClearScrollback,
            Find,
            FindNext,
            FindPrevious,
            FindAll,
            Bigger,
            BiggerAlt,
            Smaller,
            ActualSize,
            Activity,
            FullScreen,
            Minimise,
            Zoom,
            PreviousTab,
            NextTab,
            FocusLeft,
            FocusRight,
            FocusUp,
            FocusDown,
            CycleWindows,
            BringAllToFront,
        ];
        for a in all {
            assert!(listed.contains(&a), "{a:?} isn't in a menu");
        }
        for n in 1..=9 {
            assert!(listed.contains(&Tab(n)));
        }
    }

    #[test]
    fn rebinding() {
        let mut o = BTreeMap::new();
        o.insert("history".to_string(), "cmd-t".to_string());
        o.insert("nonsense".to_string(), "cmd-x".to_string());
        let (b, problems) = bindings(&o);
        assert_eq!(b[&Action::History], "cmd-t");
        assert_eq!(b[&Action::NewTab], "", "the default holder loses the key");
        assert_eq!(problems.len(), 1);
    }
}
