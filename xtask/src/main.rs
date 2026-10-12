//! `cargo xtask bundle` builds `CTX.app`: the app, the `ctx` server/CLI and `ctx-launch` (from
//! `../ctx`), `bro` (from `../bro`, when present), the LaunchAgent plist and the icon. Signs it
//! with `CTX_SIGN_IDENTITY` (Developer ID, hardened runtime) or ad hoc, and notarizes when
//! `CTX_NOTARY_PROFILE` names a `notarytool` keychain profile. No Xcode project.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("bundle") => bundle(args.iter().any(|a| a == "--debug")),
        _ => Err("usage: cargo xtask bundle [--debug]".to_string()),
    };
    if let Err(e) = r {
        eprintln!("xtask: {e}");
        std::process::exit(1);
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn run(cmd: &mut Command) -> Result<(), String> {
    eprintln!("  $ {cmd:?}");
    let s = cmd.status().map_err(|e| format!("{cmd:?}: {e}"))?;
    if s.success() { Ok(()) } else { Err(format!("{cmd:?} failed ({s})")) }
}

fn cargo_build(dir: &Path, release: bool, bins: &[&str]) -> Result<PathBuf, String> {
    let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    c.current_dir(dir).arg("build");
    if release {
        c.arg("--release");
    }
    for b in bins {
        c.args(["--bin", b]);
    }
    run(&mut c)?;
    Ok(dir.join("target").join(if release { "release" } else { "debug" }))
}

fn copy(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::copy(src, dst).map(|_| ()).map_err(|e| format!("copy {} → {}: {e}", src.display(), dst.display()))
}

fn bundle(debug: bool) -> Result<(), String> {
    let root = root();
    let release = !debug;
    let version = env!("CARGO_PKG_VERSION");
    let app_out = cargo_build(&root, release, &["CTX"])?;
    let ctx_dir = root.join("../ctx");
    let ctx_out = cargo_build(&ctx_dir, release, &["ctx", "ctx-launch"])?;
    let bro_dir = root.join("../bro");
    let bro_out =
        if bro_dir.join("Cargo.toml").exists() { Some(cargo_build(&bro_dir, release, &["bro"])?) } else { None };

    let app = root.join("target/CTX.app");
    let _ = std::fs::remove_dir_all(&app);
    let contents = app.join("Contents");
    for d in ["MacOS", "Resources/bin", "Library/LaunchAgents"] {
        std::fs::create_dir_all(contents.join(d)).map_err(|e| e.to_string())?;
    }
    copy(&app_out.join("CTX"), &contents.join("MacOS/CTX"))?;
    copy(&ctx_out.join("ctx"), &contents.join("Resources/bin/ctx"))?;
    copy(&ctx_out.join("ctx-launch"), &contents.join("Resources/bin/ctx-launch"))?;
    if let Some(b) = &bro_out {
        copy(&b.join("bro"), &contents.join("Resources/bin/bro"))?;
    }
    let plist = std::fs::read_to_string(root.join("bundle/Info.plist")).map_err(|e| e.to_string())?;
    std::fs::write(contents.join("Info.plist"), plist.replace("@VERSION@", version)).map_err(|e| e.to_string())?;
    copy(&root.join("bundle/com.ctx.server.plist"), &contents.join("Library/LaunchAgents/com.ctx.server.plist"))?;

    // Icon: the app draws it; iconutil packs it.
    let iconset = root.join("target/CTX.iconset");
    let _ = std::fs::remove_dir_all(&iconset);
    run(Command::new(contents.join("MacOS/CTX")).arg("--icon").arg(&iconset))?;
    run(Command::new("/usr/bin/iconutil")
        .args(["-c", "icns", "-o"])
        .arg(contents.join("Resources/CTX.icns"))
        .arg(&iconset))?;

    // Sign the inner programs first, then the app.
    let identity = std::env::var("CTX_SIGN_IDENTITY").unwrap_or_else(|_| "-".into());
    let entitlements = root.join("bundle/entitlements.plist");
    let mut inner: Vec<PathBuf> =
        ["ctx", "ctx-launch", "bro"].iter().map(|b| contents.join("Resources/bin").join(b)).collect();
    inner.retain(|p| p.exists());
    for p in inner.iter().chain(std::iter::once(&app)) {
        let mut c = Command::new("/usr/bin/codesign");
        c.args(["--force", "--options", "runtime", "--sign", &identity]);
        if p == &app {
            c.arg("--entitlements").arg(&entitlements);
        }
        // A real identity gets a secure timestamp (needed for notarization).
        c.arg(if identity == "-" { "--timestamp=none" } else { "--timestamp" });
        c.arg(p);
        run(&mut c)?;
    }
    run(Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(&app))?;

    if let Ok(profile) = std::env::var("CTX_NOTARY_PROFILE") {
        let zip = root.join("target/CTX.zip");
        run(Command::new("/usr/bin/ditto").args(["-c", "-k", "--keepParent"]).arg(&app).arg(&zip))?;
        run(Command::new("/usr/bin/xcrun")
            .args(["notarytool", "submit", "--wait", "--keychain-profile", &profile])
            .arg(&zip))?;
        run(Command::new("/usr/bin/xcrun").args(["stapler", "staple"]).arg(&app))?;
    } else if identity == "-" {
        eprintln!("Signed ad hoc (set CTX_SIGN_IDENTITY to a Developer ID, and CTX_NOTARY_PROFILE to notarize).");
    }
    println!("{}", app.display());
    Ok(())
}
