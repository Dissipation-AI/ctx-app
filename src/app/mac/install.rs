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

/// This copy of the app, as the bundled `ctx`'s size and modification time: a reinstalled app is
/// another copy even at the same path.
fn build_id() -> Option<String> {
    let m = std::fs::metadata(bundled("ctx")?).ok()?;
    let t = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(format!("{} {}", m.len(), t.as_nanos()))
}

/// Where the app notes which copy of itself registered the server.
fn registered_file() -> PathBuf {
    crate::client::paths().data.join("server-registration")
}

fn register_agent() -> Result<(), String> {
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    let service = unsafe { SMAppService::agentServiceWithPlistName(&NSString::from_str(AGENT_PLIST)) };
    let status = unsafe { service.status() };
    let cli_plist = PathBuf::from(crate::client::home()).join("Library/LaunchAgents").join(format!("{LABEL}.plist"));
    let build = build_id();
    let registered = std::fs::read_to_string(registered_file()).ok();
    if status == SMAppServiceStatus::Enabled && !cli_plist.exists() && build.is_some() && registered == build {
        return Ok(());
    }
    // A registration from another copy of the app still says "enabled" after a reinstall, but
    // launchd can't find its program, so the server doesn't start at login; one the command line
    // made has the same label, and macOS refuses one of the two at login. Replacing either stops
    // the server it started, ending every tab's shell, so a running server is left alone until
    // the app next starts without one.
    if std::os::unix::net::UnixStream::connect(crate::client::paths().socket()).is_ok() {
        return Ok(());
    }
    if cli_plist.exists() {
        let uid = unsafe { libc::getuid() };
        let _ = std::process::Command::new("/bin/launchctl").args(["bootout", &format!("gui/{uid}/{LABEL}")]).output();
        let _ = std::fs::remove_file(&cli_plist);
    }
    if status == SMAppServiceStatus::Enabled {
        let _ = unsafe { service.unregisterAndReturnError() };
    }
    match unsafe { service.registerAndReturnError() } {
        Ok(()) => {
            if let Some(b) = build {
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

/// Start the server: launchd runs it (never this app's child: it would inherit our identity).
pub fn start_server() {
    if ctx_protocol::config::Paths::is_custom() {
        return;
    }
    if bundle().is_some() && register_agent().is_ok() {
        let uid = unsafe { libc::getuid() };
        let _ =
            std::process::Command::new("/bin/launchctl").args(["kickstart", &format!("gui/{uid}/{LABEL}")]).output();
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
        if let Err(e) = register_agent() {
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
