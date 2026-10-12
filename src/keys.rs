//! Turning a key press into input for the server. The app never encodes escape sequences: it names
//! the key the way `ctx press` does (`enter`, `ctrl-c`, `alt-f`, `shift-up`), and the server's
//! emulator encodes it for whatever the program asked for (xterm or kitty keyboard protocol).
//! Plain typing goes through macOS text input, so accents, the emoji picker and dictation work.

pub const SHIFT: u64 = 1 << 17;
pub const CONTROL: u64 = 1 << 18;
pub const OPTION: u64 = 1 << 19;
pub const COMMAND: u64 = 1 << 20;
pub const FUNCTION: u64 = 1 << 23;
/// Device-dependent bits: which Option key is down.
pub const LEFT_OPTION: u64 = 0x20;
pub const RIGHT_OPTION: u64 = 0x40;

/// What we know about a key press (from `NSEvent`).
#[derive(Debug, Clone, PartialEq)]
pub struct Press {
    pub keycode: u16,
    /// `characters`.
    pub chars: String,
    /// `charactersIgnoringModifiers` (Shift still applies).
    pub chars_ignoring: String,
    pub flags: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// A named key for the server to encode.
    Key { key: String, text: Option<String> },
    /// Let macOS text input handle it (it calls back with text, or marked text while composing).
    TextInput,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetaKeys {
    pub left: bool,
    pub right: bool,
}

/// The name of a key that isn't text, from its virtual key code.
pub fn named(keycode: u16) -> Option<&'static str> {
    Some(match keycode {
        36 | 76 => "enter",
        48 => "tab",
        49 => "space",
        51 => "backspace",
        53 => "escape",
        117 => "delete",
        114 => "insert",
        115 => "home",
        119 => "end",
        116 => "pageup",
        121 => "pagedown",
        123 => "left",
        124 => "right",
        125 => "down",
        126 => "up",
        122 => "f1",
        120 => "f2",
        99 => "f3",
        118 => "f4",
        96 => "f5",
        97 => "f6",
        98 => "f7",
        100 => "f8",
        101 => "f9",
        109 => "f10",
        103 => "f11",
        111 => "f12",
        _ => return None,
    })
}

fn prefix(flags: u64, ctrl: bool, alt: bool, shift: bool) -> String {
    let mut p = String::new();
    if ctrl && flags & CONTROL != 0 {
        p.push_str("ctrl-");
    }
    if alt && flags & OPTION != 0 {
        p.push_str("alt-");
    }
    if shift && flags & SHIFT != 0 {
        p.push_str("shift-");
    }
    p
}

/// Whether Option acts as Meta for this press (per side, like Terminal.app).
pub fn option_is_meta(flags: u64, meta: MetaKeys) -> bool {
    if flags & OPTION == 0 {
        return false;
    }
    let left = flags & LEFT_OPTION != 0;
    let right = flags & RIGHT_OPTION != 0;
    match (left, right) {
        (true, true) => meta.left || meta.right,
        (true, false) => meta.left,
        (false, true) => meta.right,
        // No side bits (synthetic events): treat as the left key.
        (false, false) => meta.left,
    }
}

/// Map a key press. Command shortcuts are handled by the menus before this is called.
pub fn map(p: &Press, meta: MetaKeys, composing: bool) -> Input {
    if composing {
        return Input::TextInput;
    }
    let meta_on = option_is_meta(p.flags, meta);
    if let Some(name) = named(p.keycode) {
        // Space and Enter with no modifiers are just text/keys the plain way.
        if name == "space" && p.flags & (CONTROL | OPTION) == 0 {
            return Input::TextInput;
        }
        let pre = prefix(p.flags, true, meta_on || p.flags & OPTION != 0, true);
        let text = if name == "space" { Some(" ".to_string()) } else { None };
        return Input::Key { key: format!("{pre}{name}"), text };
    }
    let base = p.chars_ignoring.to_lowercase();
    if p.flags & CONTROL != 0 && !base.is_empty() {
        let pre = prefix(p.flags, true, meta_on, false);
        return Input::Key { key: format!("{pre}{base}"), text: None };
    }
    if meta_on && !p.chars_ignoring.is_empty() {
        let upper = p.chars_ignoring.chars().all(|c| c.is_uppercase());
        let shift = if upper && base.chars().any(char::is_alphabetic) { "shift-" } else { "" };
        let key = if shift.is_empty() { p.chars_ignoring.clone() } else { base };
        return Input::Key { key: format!("alt-{shift}{key}"), text: None };
    }
    Input::TextInput
}

/// Parse a shortcut like `cmd-shift-t` into (key equivalent, modifier flags) for a menu item.
pub fn parse_shortcut(s: &str) -> Option<(String, u64)> {
    let s = s.trim().to_lowercase();
    if s.is_empty() || s == "none" {
        return None;
    }
    let (mods, key) = match s.rsplit_once('-') {
        Some((m, "")) => (m.trim_end_matches('-'), "-".to_string()),
        Some((m, k)) => (m, k.to_string()),
        None => ("", s.clone()),
    };
    let mut flags = 0u64;
    for m in mods.split('-').filter(|m| !m.is_empty()) {
        flags |= match m {
            "cmd" => COMMAND,
            "shift" => SHIFT,
            "alt" | "opt" | "option" => OPTION,
            "ctrl" => CONTROL,
            _ => return None,
        };
    }
    let key = match key.as_str() {
        "left" => "\u{f702}".to_string(),
        "right" => "\u{f703}".to_string(),
        "up" => "\u{f700}".to_string(),
        "down" => "\u{f701}".to_string(),
        "plus" => "+".to_string(),
        "minus" => "-".to_string(),
        "tab" => "\t".to_string(),
        k if k.chars().count() == 1 => k.to_string(),
        _ => return None,
    };
    Some((key, flags))
}

/// A shortcut as macOS writes it (⌘⇧T), for tests and messages.
pub fn display(s: &str) -> String {
    let Some((key, flags)) = parse_shortcut(s) else { return String::new() };
    let mut out = String::new();
    if flags & CONTROL != 0 {
        out.push('⌃');
    }
    if flags & OPTION != 0 {
        out.push('⌥');
    }
    if flags & SHIFT != 0 {
        out.push('⇧');
    }
    if flags & COMMAND != 0 {
        out.push('⌘');
    }
    let k = match key.as_str() {
        "\u{f702}" => "←".to_string(),
        "\u{f703}" => "→".to_string(),
        "\u{f700}" => "↑".to_string(),
        "\u{f701}" => "↓".to_string(),
        k => k.to_uppercase(),
    };
    out + &k
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(keycode: u16, chars: &str, ignoring: &str, flags: u64) -> Press {
        Press { keycode, chars: chars.into(), chars_ignoring: ignoring.into(), flags }
    }

    const META: MetaKeys = MetaKeys { left: true, right: false };

    fn key(k: &str) -> Input {
        Input::Key { key: k.into(), text: None }
    }

    #[test]
    fn named_keys_and_modifiers() {
        assert_eq!(map(&press(36, "\r", "\r", 0), META, false), key("enter"));
        assert_eq!(map(&press(126, "", "", SHIFT), META, false), key("shift-up"));
        assert_eq!(map(&press(123, "", "", CONTROL | OPTION | LEFT_OPTION), META, false), key("ctrl-alt-left"));
        assert_eq!(map(&press(48, "\t", "\t", SHIFT), META, false), key("shift-tab"));
        assert_eq!(map(&press(51, "", "", 0), META, false), key("backspace"));
        assert_eq!(map(&press(122, "", "", FUNCTION), META, false), key("f1"));
    }

    #[test]
    fn control_combinations_go_to_the_program() {
        assert_eq!(map(&press(8, "\u{3}", "c", CONTROL), META, false), key("ctrl-c"));
        assert_eq!(map(&press(8, "\u{3}", "C", CONTROL | SHIFT), META, false), key("ctrl-c"));
    }

    #[test]
    fn option_as_meta_per_side() {
        // Left Option is Meta: alt-f.
        assert_eq!(map(&press(3, "ƒ", "f", OPTION | LEFT_OPTION), META, false), key("alt-f"));
        assert_eq!(map(&press(3, "Ï", "F", OPTION | LEFT_OPTION | SHIFT), META, false), key("alt-shift-f"));
        // Right Option types special characters through text input.
        assert_eq!(map(&press(3, "ƒ", "f", OPTION | RIGHT_OPTION), META, false), Input::TextInput);
        // Turned off on both sides: text input (e.g. Option-e for an accent).
        let off = MetaKeys { left: false, right: false };
        assert_eq!(map(&press(14, "", "e", OPTION | LEFT_OPTION), off, false), Input::TextInput);
    }

    #[test]
    fn typing_and_composition_use_text_input() {
        assert_eq!(map(&press(0, "a", "a", 0), META, false), Input::TextInput);
        assert_eq!(map(&press(49, " ", " ", 0), META, false), Input::TextInput);
        // While composing (IME), even Enter belongs to the input method.
        assert_eq!(map(&press(36, "\r", "\r", 0), META, true), Input::TextInput);
    }

    #[test]
    fn shortcuts_parse() {
        assert_eq!(parse_shortcut("cmd-shift-t"), Some(("t".into(), COMMAND | SHIFT)));
        assert_eq!(parse_shortcut("cmd--"), Some(("-".into(), COMMAND)));
        assert_eq!(parse_shortcut("cmd-alt-left"), Some(("\u{f702}".into(), COMMAND | OPTION)));
        assert_eq!(parse_shortcut("hyper-x"), None);
        assert_eq!(display("cmd-shift-t"), "⇧⌘T");
        assert_eq!(display("ctrl-cmd-f"), "⌃⌘F");
    }
}
