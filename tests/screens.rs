//! Reference screens: fixed programs (colours, emoji and CJK, box drawing, an Ink-style redraw,
//! vim) run in a throwaway ctx server, drawn by the app's renderer without a window, and compared
//! with approved PNGs in `tests/reference/` (a tolerant diff, since font rasterisation changes a
//! little between macOS versions). Each platform has its own references (`reference/macos`,
//! `reference/linux`): its fonts differ. A missing reference is written and the test fails once,
//! so a person looks at it before it's committed. `CTX_UPDATE_REFERENCES=1` rewrites them all.
//!
//! Needs the ctx binary (`../ctx/target/debug/ctx`, or `CTX_BIN`).

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    home: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn ctx_bin() -> PathBuf {
    std::env::var_os("CTX_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../ctx/target/debug/ctx"))
}

fn ctx(args: &[&str]) -> String {
    let out = Command::new(ctx_bin()).args(args).output().expect("run ctx");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn start() -> Server {
    let home = std::env::temp_dir().join(format!("ctx-app-screens-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: set before any other thread reads the environment (this test is one function).
    unsafe { std::env::set_var("CTX_HOME", &home) };
    let child = Command::new(ctx_bin()).arg("server").env("LANG", "C.UTF-8").spawn().expect("start ctx server");
    let sock = home.join("run/server.sock");
    let start = Instant::now();
    while !sock.exists() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    Server { child, home }
}

fn open_tab(name: &str) -> u64 {
    let out = ctx(&["open", "tab", "named", name]);
    out.split_whitespace().skip_while(|w| *w != "tab").nth(1).and_then(|n| n.parse().ok()).expect(&out)
}

#[test]
fn reference_screens() {
    if !ctx_bin().exists() {
        eprintln!("skipped: no ctx binary at {} (build ../ctx or set CTX_BIN)", ctx_bin().display());
        return;
    }
    let _server = start();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let update = std::env::var_os("CTX_UPDATE_REFERENCES").is_some();
    let out_dir = std::env::temp_dir().join(format!("ctx-app-screens-out-{}", std::process::id()));
    std::fs::create_dir_all(&out_dir).unwrap();
    let sample = dir.join("screens/sample.txt");
    let screens: Vec<(&str, String)> = vec![
        ("colours", format!("sh {}", dir.join("screens/colours.sh").display())),
        ("emoji", format!("sh {}", dir.join("screens/emoji.sh").display())),
        ("boxes", format!("sh {}", dir.join("screens/boxes.sh").display())),
        ("ink", format!("sh {}", dir.join("screens/ink.sh").display())),
        ("vim", format!("TERM=xterm-256color vim -u NONE -N -i NONE -c 'set laststatus=2' {}", sample.display())),
    ];
    let mut problems = Vec::new();
    for (name, command) in &screens {
        let tab = open_tab(name);
        ctx_app::headless::resize_tab(tab, 80, 24).expect("resize");
        std::thread::sleep(Duration::from_millis(300));
        ctx(&["run", "in", "tab", &tab.to_string(), &format!("clear; {command}"), "within", "2", "seconds"]);
        std::thread::sleep(Duration::from_millis(800));
        let got = out_dir.join(format!("{name}.png"));
        ctx_app::headless::render(tab, &got, 0, 0, "basic").expect("render");
        let platform = if cfg!(target_os = "macos") { "macos" } else { "linux" };
        let want = dir.join(format!("reference/{platform}/{name}.png"));
        std::fs::create_dir_all(want.parent().unwrap()).unwrap();
        if update || !want.exists() {
            std::fs::copy(&got, &want).unwrap();
            if !update {
                problems
                    .push(format!("{name}: no reference yet; wrote {} — look at it, then commit it", want.display()));
            }
            continue;
        }
        let diff = ctx_app::render::png_difference(&got, &want, 48).unwrap();
        if diff > 0.01 {
            problems.push(format!("{name}: {:.1}% of pixels differ (see {})", diff * 100.0, got.display()));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
