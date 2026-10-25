//! Side-by-side tabs ("tiles"): a window's content area split into regions, each showing exactly
//! one tab. Purely a view: the server never hears about it. Saved per window in the app's state.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// Which way a split divides its area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Split {
    /// Side by side (left | right).
    Columns,
    /// One above the other (top / bottom).
    Rows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Node {
    Tab(u64),
    Split { split: Split, ratio: f64, first: Box<Node>, second: Box<Node> },
}

/// Gap between tiles, in points (the divider is drawn in it).
pub const GAP: f64 = 1.0;

impl Node {
    pub fn tabs(&self) -> Vec<u64> {
        match self {
            Node::Tab(t) => vec![*t],
            Node::Split { first, second, .. } => {
                let mut v = first.tabs();
                v.extend(second.tabs());
                v
            }
        }
    }

    pub fn contains(&self, tab: u64) -> bool {
        self.tabs().contains(&tab)
    }

    /// Each tab's region within `area`.
    pub fn rects(&self, area: Rect) -> Vec<(u64, Rect)> {
        match self {
            Node::Tab(t) => vec![(*t, area)],
            Node::Split { split, ratio, first, second } => {
                let (a, b) = divide(area, *split, *ratio);
                let mut v = first.rects(a);
                v.extend(second.rects(b));
                v
            }
        }
    }

    /// Show `new` next to `target`, on `edge` of it. `new` mustn't already be shown.
    pub fn insert(&mut self, target: u64, new: u64, edge: Edge) -> bool {
        if self.contains(new) {
            return false;
        }
        match self {
            Node::Tab(t) if *t == target => {
                let (split, new_first) = match edge {
                    Edge::Left => (Split::Columns, true),
                    Edge::Right => (Split::Columns, false),
                    Edge::Top => (Split::Rows, true),
                    Edge::Bottom => (Split::Rows, false),
                };
                let (first, second) =
                    if new_first { (Node::Tab(new), Node::Tab(target)) } else { (Node::Tab(target), Node::Tab(new)) };
                *self = Node::Split { split, ratio: 0.5, first: Box::new(first), second: Box::new(second) };
                true
            }
            Node::Tab(_) => false,
            Node::Split { first, second, .. } => first.insert(target, new, edge) || second.insert(target, new, edge),
        }
    }

    /// Remove a tab; its neighbour takes the space. `None` when nothing is left.
    pub fn remove(self, tab: u64) -> Option<Node> {
        match self {
            Node::Tab(t) if t == tab => None,
            Node::Tab(t) => Some(Node::Tab(t)),
            Node::Split { split, ratio, first, second } => match (first.remove(tab), second.remove(tab)) {
                (Some(a), Some(b)) => Some(Node::Split { split, ratio, first: Box::new(a), second: Box::new(b) }),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            },
        }
    }

    /// Put `new` where `old` is (switching which tab a tile shows).
    pub fn replace(&mut self, old: u64, new: u64) {
        match self {
            Node::Tab(t) if *t == old => *t = new,
            Node::Tab(_) => {}
            Node::Split { first, second, .. } => {
                first.replace(old, new);
                second.replace(old, new);
            }
        }
    }

    /// The tile next to `from` in a direction (⌘⌥ arrows).
    pub fn neighbour(&self, from: u64, edge: Edge, area: Rect) -> Option<u64> {
        let rects = self.rects(area);
        let (_, r) = rects.iter().find(|(t, _)| *t == from)?;
        let (cx, cy) = r.center();
        rects
            .iter()
            .filter(|(t, _)| *t != from)
            .filter(|(_, o)| match edge {
                Edge::Left => o.x + o.w <= r.x + GAP * 2.0 && overlaps(o.y, o.h, r.y, r.h),
                Edge::Right => o.x >= r.x + r.w - GAP * 2.0 && overlaps(o.y, o.h, r.y, r.h),
                Edge::Top => o.y + o.h <= r.y + GAP * 2.0 && overlaps(o.x, o.w, r.x, r.w),
                Edge::Bottom => o.y >= r.y + r.h - GAP * 2.0 && overlaps(o.x, o.w, r.x, r.w),
            })
            .min_by(|(_, a), (_, b)| {
                let d = |o: &Rect| {
                    let (x, y) = o.center();
                    (x - cx).powi(2) + (y - cy).powi(2)
                };
                d(a).total_cmp(&d(b))
            })
            .map(|(t, _)| *t)
    }

    /// The divider under a point, for dragging: a path of first/second choices to the split.
    pub fn divider_at(&self, area: Rect, px: f64, py: f64, slop: f64) -> Option<Vec<bool>> {
        match self {
            Node::Tab(_) => None,
            Node::Split { split, ratio, first, second } => {
                let (a, b) = divide(area, *split, *ratio);
                let on = match split {
                    Split::Columns => {
                        (px - (a.x + a.w + GAP / 2.0)).abs() <= slop && py >= area.y && py < area.y + area.h
                    }
                    Split::Rows => (py - (a.y + a.h + GAP / 2.0)).abs() <= slop && px >= area.x && px < area.x + area.w,
                };
                if on {
                    return Some(Vec::new());
                }
                if let Some(mut p) = first.divider_at(a, px, py, slop) {
                    p.insert(0, true);
                    return Some(p);
                }
                if let Some(mut p) = second.divider_at(b, px, py, slop) {
                    p.insert(0, false);
                    return Some(p);
                }
                None
            }
        }
    }

    /// Which way the divider at `path` (from `divider_at`) divides.
    pub fn split_at(&self, path: &[bool]) -> Option<Split> {
        match (self, path.split_first()) {
            (Node::Split { split, .. }, None) => Some(*split),
            (Node::Split { first, .. }, Some((true, rest))) => first.split_at(rest),
            (Node::Split { second, .. }, Some((false, rest))) => second.split_at(rest),
            _ => None,
        }
    }

    /// Move the divider found by `divider_at` to a point.
    pub fn drag_divider(&mut self, path: &[bool], area: Rect, px: f64, py: f64) {
        if let Node::Split { split, ratio, first, second } = self {
            let (a, b) = divide(area, *split, *ratio);
            match path.split_first() {
                None => {
                    let r = match split {
                        Split::Columns => (px - area.x) / area.w.max(1.0),
                        Split::Rows => (py - area.y) / area.h.max(1.0),
                    };
                    *ratio = r.clamp(0.1, 0.9);
                }
                Some((true, rest)) => first.drag_divider(rest, a, px, py),
                Some((false, rest)) => second.drag_divider(rest, b, px, py),
            }
        }
    }
}

fn overlaps(a: f64, al: f64, b: f64, bl: f64) -> bool {
    a < b + bl && b < a + al
}

fn divide(area: Rect, split: Split, ratio: f64) -> (Rect, Rect) {
    match split {
        Split::Columns => {
            let w1 = ((area.w - GAP) * ratio).round();
            (Rect::new(area.x, area.y, w1, area.h), Rect::new(area.x + w1 + GAP, area.y, area.w - w1 - GAP, area.h))
        }
        Split::Rows => {
            let h1 = ((area.h - GAP) * ratio).round();
            (Rect::new(area.x, area.y, area.w, h1), Rect::new(area.x, area.y + h1 + GAP, area.w, area.h - h1 - GAP))
        }
    }
}

/// Which edge of a content area a drop lands on (the outer quarter on each side), if any.
pub fn drop_edge(area: Rect, px: f64, py: f64) -> Option<Edge> {
    if !area.contains(px, py) {
        return None;
    }
    let fx = (px - area.x) / area.w;
    let fy = (py - area.y) / area.h;
    let candidates = [(fx, Edge::Left), (1.0 - fx, Edge::Right), (fy, Edge::Top), (1.0 - fy, Edge::Bottom)];
    let (d, e) = candidates.into_iter().min_by(|a, b| a.0.total_cmp(&b.0))?;
    if d < 0.25 { Some(e) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0.0, 0.0, 801.0, 601.0)
    }

    #[test]
    fn split_and_close() {
        let mut n = Node::Tab(1);
        assert!(n.insert(1, 2, Edge::Right));
        assert!(n.insert(2, 3, Edge::Bottom));
        assert!(!n.insert(1, 3, Edge::Left), "a tab is shown once");
        let rects = n.rects(area());
        assert_eq!(rects.len(), 3);
        let r1 = rects.iter().find(|r| r.0 == 1).unwrap().1;
        let r2 = rects.iter().find(|r| r.0 == 2).unwrap().1;
        let r3 = rects.iter().find(|r| r.0 == 3).unwrap().1;
        assert_eq!(r1, Rect::new(0.0, 0.0, 400.0, 601.0));
        assert_eq!(r2.x, 401.0);
        assert_eq!(r3.y, r2.y + r2.h + GAP);
        // Tiles never overlap.
        for (a, ra) in &rects {
            for (b, rb) in &rects {
                if a != b {
                    assert!(!(overlaps(ra.x, ra.w, rb.x, rb.w) && overlaps(ra.y, ra.h, rb.y, rb.h)));
                }
            }
        }
        // Closing tab 2: tab 3 takes the right half.
        let n = n.remove(2).unwrap();
        let rects = n.rects(area());
        assert_eq!(rects.iter().find(|r| r.0 == 3).unwrap().1, Rect::new(401.0, 0.0, 400.0, 601.0));
        assert_eq!(n.remove(3).unwrap(), Node::Tab(1));
        assert_eq!(Node::Tab(1).remove(1), None);
    }

    #[test]
    fn neighbours_by_direction() {
        let mut n = Node::Tab(1);
        n.insert(1, 2, Edge::Right);
        n.insert(2, 3, Edge::Bottom);
        assert_eq!(n.neighbour(1, Edge::Right, area()), Some(2));
        assert_eq!(n.neighbour(3, Edge::Left, area()), Some(1));
        assert_eq!(n.neighbour(2, Edge::Bottom, area()), Some(3));
        assert_eq!(n.neighbour(3, Edge::Top, area()), Some(2));
        assert_eq!(n.neighbour(1, Edge::Left, area()), None);
    }

    #[test]
    fn dividers_drag_and_resize() {
        let mut n = Node::Tab(1);
        n.insert(1, 2, Edge::Left);
        let path = n.divider_at(area(), 400.5, 10.0, 3.0).unwrap();
        assert!(path.is_empty());
        assert_eq!(n.split_at(&path), Some(Split::Columns));
        n.drag_divider(&path, area(), 200.0, 10.0);
        let rects = n.rects(area());
        assert_eq!(rects[0].0, 2);
        assert!((rects[0].1.w - 200.0).abs() <= 1.0);
        n.drag_divider(&path, area(), 1.0, 10.0);
        assert!(n.rects(area())[0].1.w >= 79.0, "ratio is clamped");
    }

    #[test]
    fn drop_edges() {
        let a = area();
        assert_eq!(drop_edge(a, 10.0, 300.0), Some(Edge::Left));
        assert_eq!(drop_edge(a, 790.0, 300.0), Some(Edge::Right));
        assert_eq!(drop_edge(a, 400.0, 590.0), Some(Edge::Bottom));
        assert_eq!(drop_edge(a, 400.0, 300.0), None);
        assert_eq!(drop_edge(a, -5.0, 300.0), None);
    }

    #[test]
    fn replace_and_serialize() {
        let mut n = Node::Tab(1);
        n.insert(1, 2, Edge::Top);
        n.replace(2, 9);
        assert_eq!(n.tabs(), vec![9, 1]);
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(serde_json::from_str::<Node>(&json).unwrap(), n);
    }
}
