//! Reloading the config file when it changes: a small thread checks its modification time once a
//! second (no inotify dependency; the file is tiny).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

pub struct Watcher {
    stop: Arc<AtomicBool>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn stamp(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

impl Watcher {
    pub fn start(config: &Path) -> Option<Watcher> {
        let path: PathBuf = config.to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        std::thread::Builder::new()
            .name("ctx-config-watch".into())
            .spawn(move || {
                let mut last = stamp(&path);
                while !s.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(1));
                    let now = stamp(&path);
                    if now != last {
                        last = now;
                        super::post_from_any_thread(Box::new(|| super::super::later(|a| a.reload_config())));
                    }
                }
            })
            .ok()?;
        Some(Watcher { stop })
    }
}
