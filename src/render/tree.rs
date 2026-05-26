//! Binary space-partition layout tree — the live, resizable model behind the
//! panel tiling (`docs/design.md` §0 #10).
//!
//! [`Layout::reflow`](super::Layout::reflow) recomputes a *preset* arrangement
//! from scratch on every call and keeps no per-split state, so it cannot
//! express a manually-resized pane. The tree fills that gap: it is built from a
//! preset (so every [`LayoutMode`] still has a starting arrangement) but then
//! holds a `ratio` at each split that `Ctrl-A Ctrl-arrow` perturbs, tmux-style.
//!
//! The tree is ephemeral. `caucus resume` rebuilds it from the persisted
//! `layout_mode` + panel order via [`LayoutTree::from_preset`], so the record
//! schema is unchanged and a manual resize lasts only until the next structural
//! change (spawn / kill / move / mode switch), which rebuilds the preset — the
//! same as tmux `select-layout` resetting custom splits.

use super::{Direction, LayoutMode, Rect};
use crate::session::id::PanelId;

/// Smallest / largest a split `ratio` may be nudged to. A pane can shrink to a
/// tenth of its parent split but never collapse to nothing.
const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;

/// One `Ctrl-A Ctrl-arrow` step, as a fraction of the split extent.
const RESIZE_STEP: f32 = 0.05;

/// The axis a [`LayoutTree::Split`] divides its area along.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Axis {
    /// Children sit side by side, splitting the *width* (a vertical divider).
    Horizontal,
    /// Children stack, splitting the *height* (a horizontal divider).
    Vertical,
}

/// A binary space-partition of the screen area into panels.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutTree {
    /// A single panel filling its area.
    Leaf(PanelId),
    /// `first` takes `ratio` of the area along `axis`; `second` takes the rest.
    Split {
        /// The axis this node divides its area along.
        axis: Axis,
        /// Fraction `[MIN_RATIO, MAX_RATIO]` of the split extent given to `first`.
        ratio: f32,
        /// The leading child (top for `Vertical`, left for `Horizontal`).
        first: Box<LayoutTree>,
        /// The trailing child, taking `1 - ratio` of the extent.
        second: Box<LayoutTree>,
    },
}

impl LayoutTree {
    /// Build a tree mirroring `mode`'s preset arrangement of `ids`, with even
    /// split ratios. `None` when `ids` is empty.
    pub fn from_preset(ids: &[PanelId], mode: LayoutMode) -> Option<Self> {
        match ids {
            [] => None,
            [only] => Some(Self::Leaf(*only)),
            _ => Some(match mode {
                LayoutMode::EvenHorizontal => even_chain(leaves(ids), Axis::Horizontal),
                LayoutMode::EvenVertical => even_chain(leaves(ids), Axis::Vertical),
                LayoutMode::MainVertical => Self::Split {
                    axis: Axis::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Self::Leaf(ids[0])),
                    second: Box::new(even_chain(leaves(&ids[1..]), Axis::Vertical)),
                },
                LayoutMode::Tiled => tiled(ids),
            }),
        }
    }

    /// One `(panel, rect)` per leaf, partitioning `area` exactly — no gaps, no
    /// overlap.
    pub fn rects(&self, area: Rect) -> Vec<(PanelId, Rect)> {
        let mut out = Vec::new();
        self.collect(area, &mut out);
        out
    }

    fn collect(&self, area: Rect, out: &mut Vec<(PanelId, Rect)>) {
        match self {
            Self::Leaf(id) => out.push((*id, area)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) = split_rect(area, *axis, *ratio);
                first.collect(a, out);
                second.collect(b, out);
            }
        }
    }

    /// Resize the pane `leaf` one [`RESIZE_STEP`] in screen direction `dir`:
    /// grow it toward `dir` (`Right`/`Down`) or shrink it (`Left`/`Up`) by
    /// nudging the nearest ancestor split whose axis matches the motion.
    /// Returns whether `leaf` was found (false ⇒ no-op).
    pub fn resize(&mut self, leaf: PanelId, dir: Direction) -> bool {
        let (axis, grow) = match dir {
            Direction::Right => (Axis::Horizontal, true),
            Direction::Left => (Axis::Horizontal, false),
            Direction::Down => (Axis::Vertical, true),
            Direction::Up => (Axis::Vertical, false),
        };
        self.adjust(leaf, axis, grow).0
    }

    /// Recurse toward `leaf`. Returns `(found, adjusted)`: `found` once the leaf
    /// is located, `adjusted` once a matching-axis split on the path has had its
    /// ratio nudged. The *nearest* matching-axis ancestor is adjusted — the
    /// first one met while unwinding — so the resize moves the divider closest
    /// to the focused pane.
    fn adjust(&mut self, leaf: PanelId, axis: Axis, grow: bool) -> (bool, bool) {
        let Self::Split {
            axis: a,
            ratio,
            first,
            second,
        } = self
        else {
            return (matches!(self, Self::Leaf(id) if *id == leaf), false);
        };
        let (in_first, adjusted) = first.adjust(leaf, axis, grow);
        if in_first {
            if !adjusted && *a == axis {
                // leaf lives in `first`: growing it means `first` gets more.
                *ratio = nudge(*ratio, grow);
                return (true, true);
            }
            return (true, adjusted);
        }
        let (in_second, adjusted) = second.adjust(leaf, axis, grow);
        if in_second {
            if !adjusted && *a == axis {
                // leaf lives in `second`: growing it means `first` gets less.
                *ratio = nudge(*ratio, !grow);
                return (true, true);
            }
            return (true, adjusted);
        }
        (false, false)
    }
}

/// Wrap each id in a [`LayoutTree::Leaf`].
fn leaves(ids: &[PanelId]) -> Vec<LayoutTree> {
    ids.iter().map(|id| LayoutTree::Leaf(*id)).collect()
}

/// Combine `nodes` into a right-leaning even chain along `axis`: the head takes
/// `1/n` of the extent, the rest share the remainder (recursively even). Panics
/// on an empty slice — callers pass at least one node.
fn even_chain(mut nodes: Vec<LayoutTree>, axis: Axis) -> LayoutTree {
    let n = nodes.len();
    assert!(n > 0, "even_chain needs at least one node");
    if n == 1 {
        return nodes.pop().unwrap();
    }
    let head = nodes.remove(0);
    LayoutTree::Split {
        axis,
        ratio: 1.0 / n as f32,
        first: Box::new(head),
        second: Box::new(even_chain(nodes, axis)),
    }
}

/// Build the `Tiled` preset tree: `cols = ceil(sqrt(n))` columns over
/// `rows = ceil(n/cols)` rows, row-major. A vertical chain of rows, each a
/// horizontal chain of its cells — the last row holds the remainder and so is
/// widened to fill, matching [`Layout::reflow`](super::Layout::reflow)'s tiled
/// arrangement.
fn tiled(ids: &[PanelId]) -> LayoutTree {
    let n = ids.len();
    let cols = (n as f64).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    let mut row_nodes = Vec::with_capacity(rows);
    let mut i = 0;
    for r in 0..rows {
        let take = if r < rows - 1 { cols } else { n - i };
        row_nodes.push(even_chain(leaves(&ids[i..i + take]), Axis::Horizontal));
        i += take;
    }
    even_chain(row_nodes, Axis::Vertical)
}

/// Split `area` along `axis`, giving the first child `ratio` of the extent (at
/// least one cell, at most all-but-one when the extent is ≥ 2) and the second
/// child the rest — so the two always tile `area` exactly.
fn split_rect(area: Rect, axis: Axis, ratio: f32) -> (Rect, Rect) {
    match axis {
        Axis::Horizontal => {
            let w = area.width;
            let mut w1 = (w as f32 * ratio).round() as u16;
            if w >= 2 {
                w1 = w1.clamp(1, w - 1);
            }
            (
                Rect { width: w1, ..area },
                Rect {
                    x: area.x + w1,
                    width: w - w1,
                    ..area
                },
            )
        }
        Axis::Vertical => {
            let h = area.height;
            let mut h1 = (h as f32 * ratio).round() as u16;
            if h >= 2 {
                h1 = h1.clamp(1, h - 1);
            }
            (
                Rect { height: h1, ..area },
                Rect {
                    y: area.y + h1,
                    height: h - h1,
                    ..area
                },
            )
        }
    }
}

/// Nudge `ratio` one [`RESIZE_STEP`] up (`grow`) or down, clamped to the
/// visible band `[MIN_RATIO, MAX_RATIO]`.
fn nudge(ratio: f32, grow: bool) -> f32 {
    let next = if grow {
        ratio + RESIZE_STEP
    } else {
        ratio - RESIZE_STEP
    };
    next.clamp(MIN_RATIO, MAX_RATIO)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        }
    }

    /// Assert the tree's rects cover every cell of the 80x24 `area()` exactly
    /// once — no gaps, no overlap.
    fn assert_partitions_area(tree: &LayoutTree) {
        let mut covered = vec![0u8; 80 * 24];
        for (_, r) in tree.rects(area()) {
            for y in r.y..r.y + r.height {
                for x in r.x..r.x + r.width {
                    covered[y as usize * 80 + x as usize] += 1;
                }
            }
        }
        assert!(
            covered.iter().all(|&c| c == 1),
            "every cell covered exactly once"
        );
    }

    fn ids(n: usize) -> Vec<PanelId> {
        (0..n).map(|_| PanelId::new()).collect()
    }

    #[test]
    fn from_preset_empty_is_none() {
        assert_eq!(LayoutTree::from_preset(&[], LayoutMode::Tiled), None);
    }

    #[test]
    fn from_preset_single_is_a_leaf() {
        let id = PanelId::new();
        assert_eq!(
            LayoutTree::from_preset(&[id], LayoutMode::Tiled),
            Some(LayoutTree::Leaf(id))
        );
    }

    #[test]
    fn even_horizontal_is_full_height_columns_that_partition() {
        let ids = ids(3);
        let tree = LayoutTree::from_preset(&ids, LayoutMode::EvenHorizontal).unwrap();
        let rects = tree.rects(area());
        assert_eq!(rects.len(), 3);
        for (_, r) in &rects {
            assert_eq!(r.height, 24, "each column spans the full height");
            assert_eq!(r.y, 0);
        }
        assert_partitions_area(&tree);
    }

    #[test]
    fn even_vertical_is_full_width_rows_that_partition() {
        let ids = ids(4);
        let tree = LayoutTree::from_preset(&ids, LayoutMode::EvenVertical).unwrap();
        let rects = tree.rects(area());
        assert_eq!(rects.len(), 4);
        for (_, r) in &rects {
            assert_eq!(r.width, 80, "each row spans the full width");
            assert_eq!(r.x, 0);
        }
        assert_partitions_area(&tree);
    }

    #[test]
    fn main_vertical_gives_panel_zero_the_left_half() {
        let ids = ids(3);
        let tree = LayoutTree::from_preset(&ids, LayoutMode::MainVertical).unwrap();
        let rects = tree.rects(area());
        let main = rects.iter().find(|(id, _)| *id == ids[0]).unwrap().1;
        assert_eq!(main.x, 0);
        assert_eq!(main.y, 0);
        assert_eq!(main.height, 24, "main pane is full height");
        assert_eq!(main.width, 40, "main pane is the left half");
        assert_partitions_area(&tree);
    }

    #[test]
    fn tiled_partitions_for_a_range_of_counts() {
        for n in 2..=7 {
            let tree = LayoutTree::from_preset(&ids(n), LayoutMode::Tiled).unwrap();
            assert_eq!(tree.rects(area()).len(), n);
            assert_partitions_area(&tree);
        }
    }

    #[test]
    fn resize_grows_and_shrinks_the_focused_pane() {
        let ids = ids(2);
        let mut tree = LayoutTree::from_preset(&ids, LayoutMode::EvenHorizontal).unwrap();
        let width_of = |t: &LayoutTree, id: PanelId| {
            t.rects(area())
                .iter()
                .find(|(i, _)| *i == id)
                .unwrap()
                .1
                .width
        };
        let even = width_of(&tree, ids[0]);
        assert_eq!(even, 40, "even split starts at half width");

        // Pressing Right grows the left pane toward the divider.
        assert!(tree.resize(ids[0], Direction::Right));
        assert!(
            width_of(&tree, ids[0]) > even,
            "Right must grow the left pane"
        );
        // The partition still holds after the nudge.
        assert_partitions_area(&tree);

        // Pressing Left shrinks it back below even.
        tree = LayoutTree::from_preset(&ids, LayoutMode::EvenHorizontal).unwrap();
        assert!(tree.resize(ids[0], Direction::Left));
        assert!(
            width_of(&tree, ids[0]) < even,
            "Left must shrink the left pane"
        );
    }

    #[test]
    fn resize_of_the_trailing_pane_grows_it_toward_its_edge() {
        let ids = ids(2);
        let mut tree = LayoutTree::from_preset(&ids, LayoutMode::EvenHorizontal).unwrap();
        let width_of = |t: &LayoutTree, id: PanelId| {
            t.rects(area())
                .iter()
                .find(|(i, _)| *i == id)
                .unwrap()
                .1
                .width
        };
        let even = width_of(&tree, ids[1]);

        // The right pane grows when pressing Right (toward its own edge).
        assert!(tree.resize(ids[1], Direction::Right));
        assert!(
            width_of(&tree, ids[1]) > even,
            "Right must grow the right pane"
        );
    }

    #[test]
    fn resize_clamps_the_ratio_to_the_visible_band() {
        let ids = ids(2);
        let mut tree = LayoutTree::from_preset(&ids, LayoutMode::EvenHorizontal).unwrap();
        // Grow far past the clamp — many more steps than the band allows.
        for _ in 0..50 {
            tree.resize(ids[0], Direction::Right);
        }
        let w = tree
            .rects(area())
            .iter()
            .find(|(i, _)| *i == ids[0])
            .unwrap()
            .1
            .width;
        // MAX_RATIO 0.9 of 80 = 72; the pane must clamp there, never fill.
        assert_eq!(w, 72, "ratio clamps at MAX_RATIO");
        assert_partitions_area(&tree);
    }

    #[test]
    fn resize_against_the_grain_is_a_noop() {
        // EvenHorizontal has only horizontal splits; a vertical (Up/Down) resize
        // finds no matching-axis ancestor and changes nothing.
        let ids = ids(2);
        let mut tree = LayoutTree::from_preset(&ids, LayoutMode::EvenHorizontal).unwrap();
        let before = tree.rects(area());
        assert!(tree.resize(ids[0], Direction::Up), "leaf is found");
        assert_eq!(before, tree.rects(area()), "wrong-axis resize is a no-op");
    }

    #[test]
    fn resize_of_an_unknown_leaf_returns_false() {
        let ids = ids(2);
        let mut tree = LayoutTree::from_preset(&ids, LayoutMode::EvenHorizontal).unwrap();
        let before = tree.rects(area());
        assert!(!tree.resize(PanelId::new(), Direction::Right));
        assert_eq!(before, tree.rects(area()));
    }
}
