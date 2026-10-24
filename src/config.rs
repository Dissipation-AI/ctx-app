//! The `[app]` section of `~/.config/ctx/config.toml` (the rest of the file is the server's).

use ctx_protocol::CursorShape;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub font: String,
    pub font_size: f64,
    /// The default theme for tabs: a built-in (`ghostty`, the default), `system` (Terminal.app's
    /// Basic, following macOS light/dark) or a theme file name. Each tab can choose its own.
    pub theme: String,
    /// The theme each new window starts with: a theme name, `random` (a different one for each
    /// window, from `random_themes`), or empty for `theme`.
    pub new_window_theme: String,
    /// The theme each new tab (or pane) starts with, as `new_window_theme`. It's the tab's own, so
    /// it wins over its window's.
    pub new_tab_theme: String,
    /// What `random` chooses from (empty: the dark built-in themes).
    pub random_themes: Vec<String>,
    pub left_option_is_meta: bool,
    pub right_option_is_meta: bool,
    pub confirm_multiline_paste: bool,
    /// `block`, `bar` or `underline`.
    pub cursor: String,
    pub cursor_blink: bool,
    /// macOS: tabs share the title bar with the window's buttons (off: a normal title bar, the
    /// tabs under it).
    pub tabs_in_titlebar: bool,
    /// Each tab shows "name — folder" on one line, in a tab bar as short as macOS's own title bar
    /// (off: the name over the folder, in a taller bar).
    pub one_line_tabs: bool,
    /// Shortcut overrides: action name → keys, e.g. `new_tab = "cmd-t"`.
    pub keys: BTreeMap<String, String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            font: "SF Mono".into(),
            font_size: 13.0,
            theme: "ghostty".into(),
            new_window_theme: String::new(),
            new_tab_theme: String::new(),
            random_themes: Vec::new(),
            left_option_is_meta: true,
            right_option_is_meta: false,
            confirm_multiline_paste: true,
            cursor: "block".into(),
            cursor_blink: false,
            tabs_in_titlebar: true,
            one_line_tabs: false,
            keys: BTreeMap::new(),
        }
    }
}

/// Everything the app reads from the config file.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub app: AppConfig,
    pub keep_tabs_when_app_quits: bool,
    pub reopen_on_launch: String,
    pub agent_window_name: String,
    /// A problem with the file, shown once; defaults are used meanwhile.
    pub problem: Option<String>,
}

impl AppConfig {
    pub fn from_table(t: &toml::Table) -> Result<AppConfig, String> {
        let c: AppConfig = toml::Value::Table(t.clone()).try_into().map_err(|e: toml::de::Error| e.to_string())?;
        if !["block", "bar", "underline"].contains(&c.cursor.as_str()) {
            return Err("[app] cursor must be \"block\", \"bar\" or \"underline\"".into());
        }
        if !(6.0..=72.0).contains(&c.font_size) {
            return Err("[app] font_size must be between 6 and 72".into());
        }
        Ok(c)
    }

    /// What `random` picks from.
    pub fn random_pool(&self) -> Vec<String> {
        if self.random_themes.is_empty() {
            crate::theme::RANDOM_POOL.iter().map(|s| s.to_string()).collect()
        } else {
            self.random_themes.clone()
        }
    }

    pub fn cursor_shape(&self) -> CursorShape {
        match self.cursor.as_str() {
            "bar" => CursorShape::Bar,
            "underline" => CursorShape::Underline,
            _ => CursorShape::Block,
        }
    }
}

pub fn parse(text: &str) -> Loaded {
    match ctx_protocol::config::Config::parse(text) {
        Ok(c) => match AppConfig::from_table(&c.app) {
            Ok(app) => Loaded {
                app,
                keep_tabs_when_app_quits: c.keep_tabs_when_app_quits,
                reopen_on_launch: c.reopen_on_launch,
                agent_window_name: c.agent_window_name,
                problem: None,
            },
            Err(e) => Loaded { problem: Some(e), ..defaults_from(&c) },
        },
        Err(e) => Loaded { problem: Some(e), ..defaults_from(&ctx_protocol::config::Config::default()) },
    }
}

fn defaults_from(c: &ctx_protocol::config::Config) -> Loaded {
    Loaded {
        app: AppConfig::default(),
        keep_tabs_when_app_quits: c.keep_tabs_when_app_quits,
        reopen_on_launch: c.reopen_on_launch.clone(),
        agent_window_name: c.agent_window_name.clone(),
        problem: None,
    }
}

pub fn load(path: &std::path::Path) -> Loaded {
    match std::fs::read_to_string(path) {
        Ok(t) => {
            let mut l = parse(&t);
            if let Some(p) = &l.problem {
                l.problem = Some(format!("{}: {p}", path.display()));
            }
            l
        }
        Err(_) => parse(""),
    }
}

/// The spec's example `[app]` section, written when ⌘, opens a config file that doesn't exist.
pub const EXAMPLE: &str = r#"[app]
font = "SF Mono"
font_size = 13
theme = "ghostty"           # ghostty, jet, midnight, forest, plum, ember, ocean, graphite, basic, basic-dark,
                            # system (Basic, following macOS light/dark), or a theme file name
new_window_theme = ""       # each new window's theme: a name, "random" (a different one each time), or "" for theme
new_tab_theme = ""          # the same for each new tab or pane (a tab's theme wins over its window's)
random_themes = []          # what "random" picks from; [] for the dark built-ins
left_option_is_meta = true
right_option_is_meta = false
confirm_multiline_paste = true
cursor = "block"            # block | bar | underline
cursor_blink = false
tabs_in_titlebar = true     # macOS: tabs share the title bar with the window's buttons
one_line_tabs = false       # true: "name — folder" on one line, in a bar as short as a title bar

# [app.keys]
# new_tab = "cmd-t"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_example_parses() {
        let l = parse(EXAMPLE);
        assert_eq!(l.problem, None);
        assert_eq!(l.app, AppConfig::default());
        assert_eq!(l.reopen_on_launch, "ask");
    }

    #[test]
    fn values_and_errors() {
        let l = parse(
            "keep_tabs_when_app_quits = true\n[app]\nfont_size = 15\ncursor = \"bar\"\n[app.keys]\nnew_tab = \"cmd-shift-t\"\n",
        );
        assert_eq!(l.problem, None);
        assert!(l.keep_tabs_when_app_quits);
        assert_eq!(l.app.font_size, 15.0);
        assert_eq!(l.app.cursor_shape(), CursorShape::Bar);
        assert_eq!(l.app.keys.get("new_tab").map(String::as_str), Some("cmd-shift-t"));
        assert!(parse("[app]\ncursor = \"triangle\"").problem.is_some());
        assert!(parse("[app]\none_line_tabs = true").app.one_line_tabs);
        let l = parse("[app]\nnew_window_theme = \"random\"\nrandom_themes = [\"jet\", \"plum\"]\n");
        assert_eq!(l.problem, None);
        assert_eq!(l.app.new_window_theme, "random");
        assert_eq!(l.app.random_themes, vec!["jet".to_string(), "plum".to_string()]);
        assert!(parse("[app]\nfontsize = 3").problem.is_some());
        // A broken file still gives usable defaults.
        assert_eq!(parse("[app]\nfont_size = 400").app, AppConfig::default());
    }
}
