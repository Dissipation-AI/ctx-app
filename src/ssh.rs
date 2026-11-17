//! What a tab running `ssh` is connected to, for its label: the short alias from `~/.ssh/config`
//! when there is one, else the host's own name.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// ssh's options that take a value (`ssh -p 22 host`, or `-p22`).
const WITH_VALUE: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// The host an ssh command line connects to (`ssh -J jump -p 22 me@host ls` → `host`): its first
/// argument that isn't an option or an option's value, without the user, port or `ssh://`.
pub fn destination(args: &[String]) -> Option<String> {
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "--" {
            return it.next().and_then(|d| host_of(d));
        }
        if let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) {
            // A value follows its letter, here or in the next argument.
            if let Some(i) = flags.find(|c| WITH_VALUE.contains(c))
                && i + 1 == flags.len()
            {
                it.next();
            }
            continue;
        }
        return host_of(a);
    }
    None
}

fn host_of(d: &str) -> Option<String> {
    let d = d.strip_prefix("ssh://").unwrap_or(d);
    let d = d.rsplit_once('@').map(|(_, h)| h).unwrap_or(d);
    let d = match d.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None if d.matches(':').count() == 1 => d.split(':').next().unwrap_or(d),
        None => d,
    };
    (!d.is_empty()).then(|| d.to_string())
}

/// `Host` aliases in an ssh config (no patterns), each with its `HostName` if it has one.
pub fn aliases(config: &str, dir: &Path) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    // The aliases the current `Host` line named (their index in `out`).
    let mut current: Vec<usize> = Vec::new();
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => {
                (k.to_ascii_lowercase(), v.trim_start_matches(|c: char| c.is_whitespace() || c == '=').trim())
            }
            None => continue,
        };
        match key.as_str() {
            "host" => {
                current.clear();
                for a in value.split_whitespace().filter(|a| !a.contains(['*', '?', '!'])) {
                    current.push(out.len());
                    out.push((a.to_string(), None));
                }
            }
            "match" => current.clear(),
            "hostname" => {
                for &i in &current {
                    out[i].1.get_or_insert_with(|| value.to_string());
                }
            }
            "include" => {
                for f in value.split_whitespace() {
                    for p in included(f, dir) {
                        if let Ok(text) = std::fs::read_to_string(&p) {
                            out.extend(aliases(&text, dir));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The files an `Include` names: relative to `~/.ssh`, with `*` in the last part.
fn included(pattern: &str, dir: &Path) -> Vec<PathBuf> {
    let p = match pattern.strip_prefix("~/") {
        Some(rest) => PathBuf::from(crate::client::home()).join(rest),
        None if pattern.starts_with('/') => PathBuf::from(pattern),
        None => dir.join(pattern),
    };
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains('*') {
        return vec![p];
    }
    let (pre, post) = name.split_once('*').unwrap_or((&name, ""));
    let mut files: Vec<PathBuf> = p
        .parent()
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|f| {
                    let n = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    n.starts_with(pre) && n.ends_with(post) && n.len() >= pre.len() + post.len()
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

thread_local! {
    /// `~/.ssh/config`'s aliases, read at most every few seconds (labels are drawn often).
    static CONFIG: RefCell<Option<(Instant, Vec<(String, Option<String>)>)>> = const { RefCell::new(None) };
}

fn config_aliases() -> Vec<(String, Option<String>)> {
    CONFIG.with(|c| {
        let mut c = c.borrow_mut();
        if c.as_ref().is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(5)) {
            let dir = PathBuf::from(crate::client::home()).join(".ssh");
            let text = std::fs::read_to_string(dir.join("config")).unwrap_or_default();
            *c = Some((Instant::now(), aliases(&text, &dir)));
        }
        c.as_ref().map(|(_, a)| a.clone()).unwrap_or_default()
    })
}

/// What to call the host: the alias you typed, or the alias whose `HostName` is what you typed;
/// else `remote` (the machine's own name, as its shell reports it), else what you typed.
pub fn label(dest: &str, remote: Option<&str>, aliases: &[(String, Option<String>)]) -> String {
    if let Some((a, _)) = aliases.iter().find(|(a, _)| a == dest) {
        return a.clone();
    }
    if let Some((a, _)) = aliases.iter().find(|(_, h)| h.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(dest))) {
        return a.clone();
    }
    remote.filter(|r| !r.is_empty()).unwrap_or(dest).to_string()
}

/// A busy tab's ssh host, for its label (None when it isn't running ssh).
pub fn tab_host(args: &[String], remote: Option<&str>) -> Option<String> {
    let dest = destination(args)?;
    Some(label(&dest, remote, &config_aliases()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn finds_the_host_among_ssh_options() {
        let d = |s: &str| destination(&args(s));
        assert_eq!(d("ssh swirles").as_deref(), Some("swirles"));
        assert_eq!(d("ssh -J maths swirles -L 8866:localhost:8866").as_deref(), Some("swirles"));
        assert_eq!(d("ssh -p 2222 -i ~/.ssh/k me@box.example.org uptime").as_deref(), Some("box.example.org"));
        assert_eq!(d("ssh -vp2222 -A me@host").as_deref(), Some("host"));
        assert_eq!(d("ssh -o ServerAliveInterval=30 ssh://me@h.org:2200").as_deref(), Some("h.org"));
        assert_eq!(d("ssh -- [fe80::1]").as_deref(), Some("fe80::1"));
        assert_eq!(d("ssh -N"), None);
    }

    #[test]
    fn reads_aliases_and_their_hostnames() {
        let cfg = "Host *\n  ServerAliveInterval 30\n\nHost swirles sw\n  HostName sw-mn02.maths.cam.ac.uk\n  User rs2441\nHost maths\nHostName=gate.maths.cam.ac.uk\nMatch host x\n  HostName nope\n";
        let a = aliases(cfg, Path::new("/nonexistent"));
        assert_eq!(
            a,
            vec![
                ("swirles".into(), Some("sw-mn02.maths.cam.ac.uk".into())),
                ("sw".into(), Some("sw-mn02.maths.cam.ac.uk".into())),
                ("maths".into(), Some("gate.maths.cam.ac.uk".into())),
            ]
        );
        // The alias you typed; the alias for the host you typed; else the remote's own name.
        assert_eq!(label("sw", Some("sw-mn02"), &a), "sw");
        assert_eq!(label("gate.maths.cam.ac.uk", None, &a), "maths");
        assert_eq!(label("10.0.0.7", Some("buildbox"), &a), "buildbox");
        assert_eq!(label("10.0.0.7", None, &a), "10.0.0.7");
    }
}
