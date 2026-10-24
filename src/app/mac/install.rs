//! First run: register the `ctx` server as a LaunchAgent (`SMAppService.agent`) and offer to
//! install the `ctx` and `bro` command-line tools (symlinks into the bundle). CTX.app asks for no
//! privacy permissions; the server and its tabs have their own identity (see the ctx spec).

use objc2_foundation::NSString;
use std::path::{Path, PathBuf};

pub const AGENT_PLIST: &str = "com.ctx.server.plist";
pub const LABEL: &str = "com.ctx.server";

/// The app bundle this binary runs from, if any (`…/CTX.app`).
pub fn bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS" && app.extension()? == "app").then(|| app.to_path_buf())
}

/// A bundled tool (`ctx`, `bro`).
fn bundled(tool: &str) -> Option<PathBuf> {
    let p = bundle()?.join("Contents/Resources/bin").join(tool);
    p.exists().then_some(p)
}

/// Where a tool is installed already, searching the usual folders (apps don't get your shell's
/// PATH).
pub fn installed(tool: &str) -> Option<PathBuf> {
    let home = crate::client::home();
    let mut dirs: Vec<PathBuf> = std::env::var("PATH").unwrap_or_default().split(':').map(PathBuf::from).collect();
    for d in ["/usr/local/bin", "/opt/homebrew/bin"] {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(PathBuf::from(&home).join(".local/bin"));
    dirs.push(PathBuf::from(&home).join(".cargo/bin"));
    dirs.into_iter().map(|d| d.join(tool)).find(|p| p.exists())
}

/// The `ctx` program to use for starting the server: an override, the bundled one, an installed
/// one, or (when run from a build folder) the sibling ctx checkout.
pub fn ctx_program() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CTX_BIN").map(PathBuf::from).filter(|p| p.exists()) {
        return Some(p);
    }
    if let Some(p) = bundled("ctx") {
        return Some(p);
    }
    if let Some(p) = installed("ctx") {
        return Some(p);
    }
    let exe = std::env::current_exe().ok()?;
    for up in exe.ancestors() {
        for profile in ["release", "debug"] {
            let p = up.join("ctx/target").join(profile).join("ctx");
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

/// This copy of the app: its path, then the bundled `ctx`'s size and modification time (a
/// reinstalled app is another copy even at the same path).
fn build_id() -> Option<String> {
    let m = std::fs::metadata(bundled("ctx")?).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(format!("{}\n{} {}", bundle()?.display(), m.len(), t.as_nanos()))
}

/// Where the app notes which copy of itself registered the server. The `ctx` command reads the
/// first line to start the server through this app rather than registering one of its own.
fn registered_file() -> PathBuf {
    crate::client::paths().data.join("server-registration")
}

/// launchd has the server's job loaded (after a login, or a registration in this session).
fn loaded() -> bool {
    let uid = unsafe { libc::getuid() };
    std::process::Command::new("/bin/launchctl")
        .args(["print", &format!("gui/{uid}/{LABEL}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn agent_service() -> objc2::rc::Retained<objc2_service_management::SMAppService> {
    unsafe { objc2_service_management::SMAppService::agentServiceWithPlistName(&NSString::from_str(AGENT_PLIST)) }
}

fn cli_plist() -> PathBuf {
    PathBuf::from(crate::client::home()).join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

fn server_answers() -> bool {
    std::os::unix::net::UnixStream::connect(crate::client::paths().socket()).is_ok()
}

fn register_agent() -> Result<(), String> {
    use objc2_service_management::SMAppServiceStatus;
    let status = unsafe { agent_service().status() };
    let registered = std::fs::read_to_string(registered_file()).ok();
    if status == SMAppServiceStatus::Enabled
        && !cli_plist().exists()
        && build_id().is_some()
        && registered == build_id()
        && loaded()
    {
        return Ok(());
    }
    // A registration from another copy of the app still says "enabled" after a reinstall, but
    // launchd can't start its program, so the server doesn't start at login; one the command line
    // made has the same label, and macOS refuses one of the two at login. Replacing either stops
    // the server it started, ending every tab's shell, so a running server is left alone until
    // the app next starts without one.
    if server_answers() {
        return Ok(());
    }
    reregister()
}

/// Replace whatever registration the server has with this app's.
fn reregister() -> Result<(), String> {
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    if cli_plist().exists() {
        let uid = unsafe { libc::getuid() };
        let _ = std::process::Command::new("/bin/launchctl").args(["bootout", &format!("gui/{uid}/{LABEL}")]).output();
        let _ = std::fs::remove_file(cli_plist());
    }
    let service = agent_service();
    if unsafe { service.status() } == SMAppServiceStatus::Enabled {
        let _ = unsafe { service.unregisterAndReturnError() };
    }
    match unsafe { service.registerAndReturnError() } {
        Ok(()) => {
            if let Some(b) = build_id() {
                let _ = std::fs::create_dir_all(crate::client::paths().data);
                let _ = std::fs::write(registered_file(), b);
            }
            Ok(())
        }
        Err(e) => {
            if unsafe { service.status() } == SMAppServiceStatus::RequiresApproval {
                unsafe { SMAppService::openSystemSettingsLoginItems() };
                Err("Allow CTX in System Settings → General → Login Items so the ctx server can run.".into())
            } else {
                Err(e.localizedDescription().to_string())
            }
        }
    }
}

/// Ask launchd to start the server and wait until it answers.
fn kickstart() -> Result<(), String> {
    let uid = unsafe { libc::getuid() };
    let k = std::process::Command::new("/bin/launchctl").args(["kickstart", &format!("gui/{uid}/{LABEL}")]).output();
    match k {
        Ok(o) if !o.status.success() => {
            return Err(format!(
                "launchd couldn't start the ctx server: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => return Err(format!("Couldn't run launchctl: {e}")),
        Ok(_) => {}
    }
    let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < until {
        if server_answers() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err(format!(
        "The ctx server didn't start. Its log is {}.",
        crate::client::paths().data.join("server.log").display()
    ))
}

/// Holds the registration to this process (the app and `CTX --start-server` can both be starting
/// the server, and one replacing the registration stops the server the other just started).
struct Lock {
    _file: std::fs::File,
}

impl Lock {
    fn take(wait: bool) -> Option<Lock> {
        let data = crate::client::paths().data;
        let _ = std::fs::create_dir_all(&data);
        let f = std::fs::File::create(data.join("server-registration.lock")).ok()?;
        let op = if wait { libc::LOCK_EX } else { libc::LOCK_EX | libc::LOCK_NB };
        // SAFETY: flock on a file descriptor we own; the lock goes with the file.
        (unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&f), op) } == 0).then_some(Lock { _file: f })
    }
}

/// Register the server from this app (if it isn't) and start it. A registration can be stale even
/// when it's new: macOS keeps its record of an earlier copy of an app without a Team ID (signed ad
/// hoc), launchd refuses to start that (exit 78), and macOS replaces the record only when a second
/// start fails. So a server that doesn't come up is started once more, then registered again.
fn launch() -> Result<(), String> {
    let _lock = Lock::take(true);
    register_agent()?;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(40);
    loop {
        if server_answers() || kickstart().is_ok() || kickstart().is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() > until {
            return kickstart();
        }
        reregister()?;
    }
}

/// `CTX --start-server`: register the server from this app and start it, without opening a
/// window. The `ctx` command runs this when it finds no server, so the server keeps one
/// registration (the app's) whichever of the two starts it.
pub fn start_server_main() -> i32 {
    if bundle().is_none() {
        eprintln!("CTX isn't running from its app bundle, so it can't register the ctx server.");
        return 1;
    }
    match launch() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// Start the server: launchd runs it (never this app's child: it would inherit our identity).
pub fn start_server() {
    if ctx_protocol::config::Paths::is_custom() {
        return;
    }
    if bundle().is_some() {
        let _ = launch();
        return;
    }
    // Not bundled (a development build): the ctx command registers and starts it.
    if let Some(ctx) = ctx_program() {
        let _ = std::process::Command::new(ctx)
            .args(["list", "tabs"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// The folder to link tools into: `/usr/local/bin` when it's writable, else `~/.local/bin`.
fn link_dir() -> PathBuf {
    let usr = Path::new("/usr/local/bin");
    let writable = usr.is_dir()
        && std::fs::metadata(usr).is_ok_and(|m| !m.permissions().readonly())
        && std::fs::File::create(usr.join(".ctx-write-test"))
            .map(|_| std::fs::remove_file(usr.join(".ctx-write-test")))
            .is_ok();
    if writable { usr.to_path_buf() } else { PathBuf::from(crate::client::home()).join(".local/bin") }
}

/// Link the bundled tools that aren't installed. Returns what was done, for the person.
fn install_tools(tools: &[(&str, PathBuf)]) -> Result<String, String> {
    let dir = link_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    let mut done = Vec::new();
    for (name, src) in tools {
        let dst = dir.join(name);
        let _ = std::fs::remove_file(&dst);
        std::os::unix::fs::symlink(src, &dst).map_err(|e| format!("Couldn't link {}: {e}", dst.display()))?;
        done.push(dst.display().to_string());
    }
    let mut msg = format!("Installed {}.", done.join(" and "));
    let on_path = std::env::var("PATH").unwrap_or_default().split(':').any(|p| Path::new(p) == dir);
    if !on_path && dir.ends_with(".local/bin") {
        msg.push_str(" Add ~/.local/bin to your PATH to use them in other terminals.");
    }
    Ok(msg)
}

/// First-run setup (bundled app only), then `then`.
pub fn first_run_then(then: impl FnOnce() + 'static) {
    // A test or trial setup (CTX_HOME) never touches launchd or your PATH.
    if bundle().is_some() && !ctx_protocol::config::Paths::is_custom() {
        // Unless another process is registering it right now.
        if let Some(_lock) = Lock::take(false)
            && let Err(e) = register_agent()
        {
            super::alert("The ctx server isn't allowed to run yet", &e);
        }
        let set_up = super::super::with_app(|a| a.state.set_up).unwrap_or(true);
        if !set_up {
            let missing: Vec<(&str, PathBuf)> = ["ctx", "bro"]
                .into_iter()
                .filter(|t| installed(t).is_none())
                .filter_map(|t| bundled(t).map(|p| (t, p)))
                .collect();
            if !missing.is_empty() {
                let names: Vec<&str> = missing.iter().map(|(n, _)| *n).collect();
                let q = format!(
                    "CTX includes the {} command{}. Linking {} into {} lets you and your agents use {} in any terminal.",
                    names.join(" and "),
                    if names.len() == 1 { "" } else { "s" },
                    if names.len() == 1 { "it" } else { "them" },
                    link_dir().display(),
                    if names.len() == 1 { "it" } else { "them" }
                );
                if super::ask_native("Install the command-line tools?", &q, "Install", "Not Now") {
                    match install_tools(&missing) {
                        Ok(m) => super::alert("Command-line tools installed", &m),
                        Err(e) => super::alert("Couldn't install the command-line tools", &e),
                    }
                }
            }
            super::super::with_app(|a| {
                a.state.set_up = true;
                a.save_state();
            });
        }
    }
    then();
}
