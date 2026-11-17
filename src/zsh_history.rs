//! Whether your zsh shares history between tabs as commands run (`setopt share_history`, which
//! oh-my-zsh turns on), and turning that off in your `.zshrc`. Shared live, every tab's ↑ brings
//! up what the other tabs ran, agents' commands included.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MARK: &str = "CTX-SHARE-HISTORY";

/// What a tab's zsh says, read the way a tab starts it (interactive, your startup files).
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub shares: bool,
    /// The `.zshrc` it read.
    pub zshrc: PathBuf,
}

/// Ask an interactive `zsh` (your login shell, if it's zsh) whether `share_history` is on. None:
/// not zsh, or zsh didn't answer within a few seconds.
pub fn check(shell: &str) -> Option<Found> {
    if Path::new(shell).file_name()? != "zsh" {
        return None;
    }
    let script = script();
    let mut child = Command::new(shell)
        .args(["-i", "-c", &script])
        .env("DISABLE_AUTO_UPDATE", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    while child.try_wait().ok()?.is_none() {
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().ok()?;
    parse(&String::from_utf8_lossy(&out.stdout))
}

/// Prints the mark, `on`/`off`, and the folder zsh read `.zshrc` from.
fn script() -> String {
    format!("print -r -- \"{MARK} ${{options[sharehistory]}} ${{ZDOTDIR:-$HOME}}\"")
}

fn parse(out: &str) -> Option<Found> {
    let line = out.lines().find_map(|l| l.split_once(MARK).map(|(_, r)| r.trim()))?;
    let (on, dir) = line.split_once(' ')?;
    Some(Found { shares: on == "on", zshrc: Path::new(dir).join(".zshrc") })
}

/// What goes at the end of your `.zshrc`.
pub const PER_TAB: &str = "
# Added by CTX: each tab keeps its own ↑ history while it's open, instead of taking in other
# tabs' commands (agents' too) as they run. New tabs still start with everything saved so far.
unsetopt share_history
setopt inc_append_history
";

/// Add [`PER_TAB`] to the end of `zshrc` (after oh-my-zsh, which turns sharing on).
pub fn keep_per_tab(zshrc: &Path) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(zshrc)?;
    f.write_all(PER_TAB.as_bytes())
}

/// Your login shell, from the user database.
pub fn login_shell() -> String {
    // SAFETY: getpwuid returns a pointer into static storage, read at once on this thread.
    let from_db = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        (!pw.is_null() && !(*pw).pw_shell.is_null())
            .then(|| std::ffi::CStr::from_ptr((*pw).pw_shell).to_string_lossy().into_owned())
    };
    from_db.filter(|s| !s.is_empty()).or_else(|| std::env::var("SHELL").ok()).unwrap_or_else(|| "/bin/zsh".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_answer_past_startup_noise() {
        let f = parse("\x1b]0;title\x07welcome!\nCTX-SHARE-HISTORY on /Users/me\n").unwrap();
        assert_eq!(f, Found { shares: true, zshrc: "/Users/me/.zshrc".into() });
        assert!(!parse("CTX-SHARE-HISTORY off /h").unwrap().shares);
        assert_eq!(parse("nothing"), None);
    }

    #[test]
    fn asks_a_real_zsh() {
        let dir = std::env::temp_dir().join(format!("ctx-app-zsh-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".zshrc"), "setopt share_history\n").unwrap();
        let ask = || {
            let out = Command::new("/bin/zsh")
                .args(["-i", "-c", &script()])
                .env("ZDOTDIR", &dir)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            parse(&String::from_utf8_lossy(&out.stdout)).unwrap()
        };
        assert!(ask().shares);
        keep_per_tab(&dir.join(".zshrc")).unwrap();
        let after = ask();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!after.shares);
        assert_eq!(after.zshrc, dir.join(".zshrc"));
        assert!(check("/bin/bash").is_none());
    }
}
