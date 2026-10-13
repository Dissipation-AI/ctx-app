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
    pub left_option_is_meta: bool,
    pub right_option_is_meta: bool,
    pub confirm_multiline_paste: bool,
    /// `block`, `bar` or `underline`.
    pub cursor: String,
    pub cursor_blink: bool,
    /// macOS: tabs share the title bar with the window's buttons (off: a normal title bar, the
    /// tabs under it).
    pub tabs_in_titlebar: bool,
    /// Shortcut overrides: action name → keys, e.g. `new_tab = "cmd-t"`.
    pub keys: BTreeMap<String, String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            font: "SF Mono".into(),
            font_size: 13.0,
            theme: "ghostty".into(),
            left_option_is_meta: true,
            right_option_is_meta: false,
            confirm_multiline_paste: true,
            cursor: "block".into(),
            cursor_blink: false,
            tabs_in_titlebar: true,
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
theme = "ghostty"           # ghostty, midnight, forest, plum, ember, ocean, graphite, basic, basic-dark,
                            # system (Basic, following macOS light/dark), or a theme file name
left_option_is_meta = true
right_option_is_meta = false
confirm_multiline_paste = true
cursor = "block"            # block | bar | underline
cursor_blink = false
tabs_in_titlebar = true     # macOS: tabs share the title bar with the window's buttons

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
        assert!(parse("[app]\nfontsize = 3").problem.is_some());
        // A broken file still gives usable defaults.
        assert_eq!(parse("[app]\nfont_size = 400").app, AppConfig::default());
    }
}
