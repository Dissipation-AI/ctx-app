//! Agents in the app: the list (from `ctx list agents as json`), their colours, and running
//! `ctx` sentences about them. The app never changes accounts or permissions itself: every
//! change is a `ctx` sentence run as you, so the CLI is the one place the rules live.

use super::{App, plat, with_app};
use crate::render::Rgb;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Agent {
    pub name: String,
    pub full_name: String,
    pub home: String,
    /// `ctx-agent.toml`'s colour, if set.
    pub colour: Option<String>,
    /// Who may use its home: (who, "read" / "read and write" / …).
    pub home_access: Vec<(String, String)>,
    pub grants: Vec<(String, bool, bool)>,
    pub warnings: Vec<String>,
    pub tabs: Vec<u64>,
    pub limits: Option<(u64, u64, bool)>,
}

#[derive(Default)]
pub struct Agents {
    pub list: Vec<Agent>,
    pub at: Option<Instant>,
    pub loading: bool,
}

/// Parse `ctx list agents as json`.
pub fn parse(json: &str) -> Vec<Agent> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return Vec::new() };
    let s = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    v["accounts"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| Agent {
                    name: s(x, "name"),
                    full_name: s(x, "full_name"),
                    home: s(x, "home"),
                    colour: x["colour"].as_str().map(str::to_string),
                    home_access: x["home_access"]
                        .as_array()
                        .map(|h| h.iter().map(|e| (s(e, "who"), s(e, "access"))).collect())
                        .unwrap_or_default(),
                    grants: x["grants"]
                        .as_array()
                        .map(|g| {
                            g.iter()
                                .map(|e| {
                                    (
                                        s(e, "path"),
                                        e["write"].as_bool().unwrap_or(false),
                                        e["holds"].as_bool().unwrap_or(true),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    warnings: x["warnings"]
                        .as_array()
                        .map(|w| w.iter().filter_map(|e| e.as_str().map(str::to_string)).collect())
                        .unwrap_or_default(),
                    tabs: x["tabs"]
                        .as_array()
                        .map(|t| t.iter().filter_map(|e| e.as_u64()).collect())
                        .unwrap_or_default(),
                    limits: x["limits"].as_object().map(|l| {
                        (
                            l.get("memory_mb").and_then(|v| v.as_u64()).unwrap_or(0),
                            l.get("processes").and_then(|v| v.as_u64()).unwrap_or(0),
                            l.get("low_priority").and_then(|v| v.as_bool()).unwrap_or(false),
                        )
                    }),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// An agent's colour: its own (a name or `#rrggbb`), else one picked from its name, so the same
/// agent always has the same colour.
pub fn colour(name: &str, own: Option<&str>) -> Rgb {
    if let Some(c) = own.and_then(named_or_hex) {
        return c;
    }
    const PALETTE: [&str; 8] = ["teal", "orange", "violet", "green", "pink", "blue", "amber", "red"];
    let h = name.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    named_or_hex(PALETTE[(h % PALETTE.len() as u64) as usize]).unwrap_or([0x2a, 0xa1, 0x98])
}

fn named_or_hex(s: &str) -> Option<Rgb> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#')
        && hex.len() == 6
    {
        let n = u32::from_str_radix(hex, 16).ok()?;
        return Some([(n >> 16) as u8, (n >> 8) as u8, n as u8]);
    }
    Some(match s.to_ascii_lowercase().as_str() {
        "teal" => [0x2a, 0xa1, 0x98],
        "orange" => [0xe0, 0x7b, 0x39],
        "violet" | "purple" => [0x8e, 0x6c, 0xd8],
        "green" => [0x4c, 0xa8, 0x5a],
        "pink" => [0xd6, 0x5d, 0x9e],
        "blue" => [0x3d, 0x8b, 0xd9],
        "amber" | "yellow" => [0xc9, 0x9a, 0x2e],
        "red" => [0xd0, 0x4a, 0x4a],
        _ => return None,
    })
}

/// Run a `ctx` sentence as you, off the main thread; `then` gets (succeeded, stdout, stderr).
pub fn run_ctx(args: Vec<String>, then: impl FnOnce(&mut App, bool, String, String) + 'static) {
    let then = SendBox(Box::new(then));
    std::thread::spawn(move || {
        let r = match plat::install::ctx_program() {
            Some(ctx) => std::process::Command::new(ctx)
                .args(&args)
                .stdin(std::process::Stdio::null())
                .output()
                .map(|o| {
                    (
                        o.status.success(),
                        String::from_utf8_lossy(&o.stdout).into_owned(),
                        String::from_utf8_lossy(&o.stderr).into_owned(),
                    )
                })
                .unwrap_or_else(|e| (false, String::new(), format!("couldn't run ctx: {e}"))),
            None => (false, String::new(), "ctx isn't installed.".into()),
        };
        plat::post_from_any_thread(Box::new(move || {
            let then = then;
            with_app(move |app| (then.0)(app, r.0, r.1, r.2));
        }));
    });
}

type Then = Box<dyn FnOnce(&mut App, bool, String, String)>;
struct SendBox(Then);
// SAFETY: the closure is only called on the main thread, where it was made.
unsafe impl Send for SendBox {}

impl App {
    /// Refresh the agents list unless it's fresh; `force` refreshes now.
    pub fn refresh_agents(&mut self, force: bool) {
        if self.agents.loading || (!force && self.agents.at.is_some_and(|t| t.elapsed() < Duration::from_secs(5))) {
            return;
        }
        self.agents.loading = true;
        run_ctx(vec!["list".into(), "agents".into(), "as".into(), "json".into()], |app, ok, out, _| {
            app.agents.loading = false;
            app.agents.at = Some(Instant::now());
            if ok {
                let list = parse(&out);
                if list != app.agents.list {
                    app.agents.list = list;
                    for w in app.wins.values_mut() {
                        w.dirty = Some(super::Dirty::All);
                    }
                    app.redraw_dirty();
                }
            }
        });
    }

    /// The colour of agent `name`'s tabs.
    pub fn agent_colour(&self, name: &str) -> Rgb {
        colour(name, self.agents.list.iter().find(|a| a.name == name).and_then(|a| a.colour.as_deref()))
    }

    /// Run a sentence about agents and show what it says; then refresh the list.
    pub fn agent_sentence(&mut self, args: Vec<String>) {
        run_ctx(args, |app, ok, out, err| {
            if !ok {
                app.error(err.trim());
            } else if !out.trim().is_empty() {
                app.alert("ctx", out.trim());
            }
            app.refresh_agents(true);
        });
    }
}

/// `["ctx", "allow", …]` words for a sentence, without the program name.
fn words(s: &[&str]) -> Vec<String> {
    s.iter().map(|w| w.to_string()).collect()
}

impl App {
    /// The agent the Agents panel in window `w` shows.
    fn panel_agent(&self, w: u64) -> Option<Agent> {
        let name = self.wins.get(&w)?.panel.as_ref()?.agent.clone()?;
        self.agents.list.iter().find(|a| a.name == name).cloned()
    }

    /// Lines the editor shows in window `w`.
    pub fn editor_rows(&mut self, w: u64) -> usize {
        let Some(area) = self.panel_area(w) else { return 1 };
        let inner = super::panels::editor_area(super::panels::body_of(area));
        self.with_panel_ctx(w, |_, ctx| ((inner.h - 12.0) / (ctx.mono.cell_h / ctx.scale)).floor().max(1.0) as usize)
            .unwrap_or(1)
    }

    /// A click in the editor: put the caret there.
    pub fn editor_click(&mut self, w: u64, x: f64, y: f64) {
        let Some(area) = self.panel_area(w) else { return };
        let inner = super::panels::editor_area(super::panels::body_of(area));
        let Some((cw, ch)) =
            self.with_panel_ctx(w, |_, ctx| (ctx.mono.cell_w / ctx.scale, ctx.mono.cell_h / ctx.scale))
        else {
            return;
        };
        let col = ((x - inner.x - 8.0) / cw).round().max(0.0) as usize;
        let row = ((y - inner.y - 6.0) / ch).floor().max(0.0) as usize;
        if let Some(win) = self.wins.get_mut(&w)
            && let Some(e) = win.panel.as_mut().and_then(|p| p.editor.as_mut())
        {
            e.click(row, col);
            win.focus = super::Focus::Panel;
            win.dirty = Some(super::Dirty::All);
        }
        self.redraw_dirty();
    }

    fn set_editor(&mut self, w: u64, e: Option<super::editor::Editor>) {
        if let Some(win) = self.wins.get_mut(&w)
            && let Some(p) = win.panel.as_mut()
        {
            p.editor = e;
            win.focus = super::Focus::Panel;
            win.dirty = Some(super::Dirty::All);
        }
        self.redraw_dirty();
    }

    fn save_editor(&mut self, w: u64) -> bool {
        let Some(e) = self.wins.get_mut(&w).and_then(|x| x.panel.as_mut()).and_then(|p| p.editor.as_mut()) else {
            return true;
        };
        match std::fs::write(&e.path, e.text()) {
            Ok(()) => {
                e.changed = false;
                if let Some(win) = self.wins.get_mut(&w) {
                    win.dirty = Some(super::Dirty::All);
                }
                self.redraw_dirty();
                true
            }
            Err(err) => {
                let path = e.path.clone();
                self.error(&format!("Couldn't save {path}: {err}"));
                false
            }
        }
    }

    /// A button in the Agents panel.
    pub fn agent_act(&mut self, w: u64, act: super::panels::AgentAct) {
        use super::panels::AgentAct as A;
        let Some(a) = self.panel_agent(w) else { return };
        let name = a.name.clone();
        match act {
            A::OpenTab => self.new_tab_as(w, name),
            A::Edit(n) => {
                let file = if n == 0 { "CLAUDE.md" } else { "AGENTS.md" };
                let path = format!("{}/{file}", a.home);
                let text = match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => return self.error(&format!("Couldn't read {path}: {e}")),
                };
                self.set_editor(w, Some(super::editor::Editor::new(&name, &path, &text)));
            }
            A::Save => {
                self.save_editor(w);
            }
            A::CloseEditor => {
                let changed = self
                    .wins
                    .get(&w)
                    .and_then(|x| x.panel.as_ref())
                    .and_then(|p| p.editor.as_ref())
                    .is_some_and(|e| e.changed);
                if !changed {
                    return self.set_editor(w, None);
                }
                self.ask(
                    "Save the changes?",
                    "The instructions were edited.",
                    "Save",
                    "Don't Save",
                    move |app, save| {
                        if !save || app.save_editor(w) {
                            app.set_editor(w, None);
                        }
                    },
                );
            }
            A::Text => {}
            A::Share => {
                let buttons = vec!["Read".to_string(), "Read and write".to_string(), "Cancel".to_string()];
                let text = format!(
                    "A file or folder of yours for {name} (e.g. ~/Context/career). It may read it, or also write it."
                );
                self.ask_with(
                    &format!("Share with {name}"),
                    &text,
                    buttons,
                    Some(String::new()),
                    move |app, b, path| {
                        let path = path.trim().to_string();
                        if path.is_empty() || !matches!(b, Some(0 | 1)) {
                            return;
                        }
                        let how = if b == Some(1) { "write" } else { "read" };
                        app.agent_sentence(words(&["allow", "agent", &name, "to", how, &path]));
                    },
                );
            }
            A::Administer => {
                let others: Vec<String> =
                    self.agents.list.iter().filter(|x| x.name != name).map(|x| x.name.clone()).collect();
                let text = format!(
                    "Which agent? ({}). Reading {name}'s home lets it see {name}'s tabs; writing it lets it drive them and change {name}'s instructions and settings.",
                    if others.is_empty() { "there are no others yet".to_string() } else { others.join(", ") }
                );
                let buttons = vec!["Administer".to_string(), "Only see its tabs".to_string(), "Cancel".to_string()];
                self.ask_with(
                    &format!("Let an agent look after {name}"),
                    &text,
                    buttons,
                    Some(String::new()),
                    move |app, b, who| {
                        let who = who.trim().to_string();
                        if who.is_empty() || !matches!(b, Some(0 | 1)) {
                            return;
                        }
                        let how = if b == Some(0) { "write" } else { "read" };
                        app.agent_sentence(words(&["allow", "agent", &who, "to", how, "agent", &name]));
                    },
                );
            }
            A::Memory => self.prompt(
                &format!("Memory for {name}"),
                "Across its tabs, e.g. 8 GB or 512 MB.",
                "",
                move |app, v| {
                    if let Some(v) = v.filter(|v| !v.trim().is_empty()) {
                        let mut w = words(&["limit", "agent", &name, "to"]);
                        w.extend(v.split_whitespace().map(str::to_string));
                        w.extend(words(&["of", "memory"]));
                        app.agent_sentence(w);
                    }
                },
            ),
            A::Processes => self.prompt(
                &format!("Processes for {name}"),
                "How many it may run at once, e.g. 200.",
                "",
                move |app, v| {
                    if let Some(v) = v.filter(|v| !v.trim().is_empty()) {
                        app.agent_sentence(words(&["limit", "agent", &name, "to", v.trim(), "processes"]));
                    }
                },
            ),
            A::LowPriority => {
                let low = a.limits.is_some_and(|l| l.2);
                let to = if low { "normal" } else { "low" };
                self.agent_sentence(words(&["limit", "agent", &name, "to", to, "priority"]));
            }
            A::NoLimits => self.agent_sentence(words(&["limit", "agent", &name, "to", "nothing"])),
            A::Reset => self.ask(
                &format!("Reset {name}'s settings?"),
                "Its ctx-agent.toml is rewritten as yours, with the defaults: no limits, its own colour.",
                "Reset",
                "Cancel",
                move |app, yes| {
                    if yes {
                        app.agent_sentence(words(&["reset", "agent", &name]));
                    }
                },
            ),
            A::Remove => {
                let buttons = vec![
                    "Remove, keep its home".to_string(),
                    "Remove and trash its home".to_string(),
                    "Cancel".to_string(),
                ];
                let text = format!("This deletes the account {name}. Its home ({}) stays unless you trash it.", a.home);
                self.ask_with(&format!("Remove {name}?"), &text, buttons, None, move |app, b, _| match b {
                    Some(0) => app.agent_sentence(words(&["remove", "agent", &name])),
                    Some(1) => app.agent_sentence(words(&["remove", "agent", &name, "with", "its", "files"])),
                    _ => {}
                });
            }
            A::Forbid(i) => {
                let Some((who, _)) = a.home_access.get(i).cloned() else { return };
                let mut w = if who == "every agent" {
                    words(&["forbid", "every", "agent"])
                } else {
                    words(&["forbid", "agent", &who])
                };
                w.extend(words(&["to", "read", "agent", &name]));
                self.agent_sentence(w);
            }
            A::TakeBack(i) => {
                let Some((path, _, _)) = a.grants.get(i).cloned() else { return };
                self.agent_sentence(words(&["forbid", "agent", &name, "to", "read", &path]));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_cli_list() {
        let j = r#"{"schema":1,"accounts":[{"name":"mail","full_name":"Mail","uid":601,"home":"/Users/c/Agents/mail","home_access":[{"who":"you","access":"read and write"}],"grants":[{"path":"/Users/c/Context","write":false,"holds":true}],"limits":{"memory_mb":0,"processes":50,"low_priority":true},"colour":"teal","warnings":[],"tabs":[3]}],"agents":[]}"#;
        let a = parse(j);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "mail");
        assert_eq!(a[0].colour.as_deref(), Some("teal"));
        assert_eq!(a[0].tabs, vec![3]);
        assert_eq!(a[0].limits, Some((0, 50, true)));
        assert_eq!(a[0].home_access[0], ("you".into(), "read and write".into()));
    }

    #[test]
    fn colours_are_stable_and_own_colours_win() {
        assert_eq!(colour("mail", None), colour("mail", None));
        assert_eq!(colour("mail", Some("#102030")), [0x10, 0x20, 0x30]);
        assert_eq!(colour("mail", Some("teal")), [0x2a, 0xa1, 0x98]);
    }
}
