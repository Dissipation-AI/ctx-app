//! The app's own state between launches: which `ctx` windows it shows, their frames, tile
//! layouts, the selected tab, per-window themes and per-tab font sizes. Written on every change
//! (it's small), atomically, so a `kill -9` loses nothing.

use crate::layout::Node;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    /// x, y, width, height in screen points.
    pub frame: Option<[f64; 4]>,
    pub tiles: Option<Node>,
    pub selected: Option<u64>,
    pub theme: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppState {
    /// `ctx` window number → how the app shows it. Windows not here aren't shown.
    pub windows: BTreeMap<u64, WindowState>,
    /// Font size chosen for a tab with ⌘+ / ⌘-.
    pub font_sizes: BTreeMap<u64, f64>,
    /// Theme chosen for a tab (its colours mark what it's for).
    pub tab_themes: BTreeMap<u64, String>,
    /// Closed tabs already offered in the "Reopen N tabs from last time" bar.
    pub offered: Vec<u64>,
    /// The app has run before (first-run setup done).
    pub set_up: bool,
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("app-state.json")
}

impl AppState {
    pub fn load(file: &Path) -> AppState {
        std::fs::read_to_string(file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        if let Some(d) = file.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, file)
    }

    /// Forget windows and tabs that no longer exist.
    pub fn prune(&mut self, windows: &[u64], tabs: &[u64]) {
        self.windows.retain(|w, _| windows.contains(w));
        self.font_sizes.retain(|t, _| tabs.contains(t));
        self.tab_themes.retain(|t, _| tabs.contains(t));
        for w in self.windows.values_mut() {
            if let Some(n) = w.tiles.take() {
                let mut node = Some(n);
                for t in node.as_ref().map(|n| n.tabs()).unwrap_or_default() {
                    if !tabs.contains(&t) {
                        node = node.and_then(|n| n.remove(t));
                    }
                }
                // A single tile is just the tab.
                w.tiles = node.filter(|n| matches!(n, Node::Split { .. }));
            }
            if w.selected.is_some_and(|t| !tabs.contains(&t)) {
                w.selected = None;
            }
        }
        if self.offered.len() > 1000 {
            let cut = self.offered.len() - 1000;
            self.offered.drain(..cut);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Edge;

    #[test]
    fn round_trip_and_prune() {
        let dir = std::env::temp_dir().join(format!("ctx-app-state-{}", std::process::id()));
        let file = path(&dir);
        let mut s = AppState::default();
        let mut tiles = Node::Tab(1);
        tiles.insert(1, 2, Edge::Right);
        s.windows.insert(
            1,
            WindowState { frame: Some([10.0, 20.0, 800.0, 600.0]), tiles: Some(tiles), selected: Some(2), theme: None },
        );
        s.windows.insert(7, WindowState::default());
        s.font_sizes.insert(2, 16.0);
        s.tab_themes.insert(2, "forest".into());
        s.save(&file).unwrap();
        let mut back = AppState::load(&file);
        assert_eq!(back, s);
        back.prune(&[1], &[1]);
        assert_eq!(back.windows.len(), 1);
        assert_eq!(back.windows[&1].tiles, None, "one tab left: no tiles");
        assert_eq!(back.windows[&1].selected, None);
        assert!(back.font_sizes.is_empty());
        assert!(back.tab_themes.is_empty());
        std::fs::remove_dir_all(dir).ok();
        assert_eq!(AppState::load(Path::new("/nonexistent/x.json")), AppState::default());
    }
}
