//! Starting the `ctx` server on Linux: there is no launchd, so the app starts `ctx server` itself
//! (in its own session, so it outlives the app) when nothing answers on the socket. A trial or
//! test setup (`CTX_HOME`) works the same way.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The `ctx` program: `CTX_BIN`, next to this program, a checkout next to this one, or `PATH`.
pub fn ctx_program() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CTX_BIN").map(PathBuf::from).filter(|p| p.exists()) {
        return Some(p);
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let mut candidates = vec![dir.join("ctx")];
    for up in [dir.join("../../../ctx/target/release/ctx"), dir.join("../../../ctx/target/debug/ctx")] {
        candidates.push(up);
    }
    if let Some(p) = candidates.into_iter().find(|p| p.exists()) {
        return Some(p);
    }
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("ctx")).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .find(|p| p.exists())
}

pub fn start_server() {
    let Some(ctx) = ctx_program() else {
        eprintln!("CTX: couldn't find the ctx program (set CTX_BIN)");
        return;
    };
    let log = crate::client::paths().data.join("server.log");
    if let Some(d) = log.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log).ok();
    let mut cmd = Command::new(ctx);
    cmd.arg("server").stdin(Stdio::null());
    match out {
        Some(f) => {
            let e = f.try_clone().ok();
            cmd.stdout(f);
            if let Some(e) = e {
                cmd.stderr(e);
            }
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    // Its own session: it keeps running when the app quits.
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe and has no preconditions.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

/// Nothing to set up on Linux before connecting.
pub fn first_run_then(then: impl FnOnce() + 'static) {
    then();
}

/// `CTX --start-server`: start the server without opening a window.
pub fn start_server_main() -> i32 {
    start_server();
    0
}
