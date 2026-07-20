//! Binary space-partition layout tree — the live model behind the panel tiling
//! (`docs/design.md` §0 #10).
//!
//! The multiplexer projects this tree onto the screen area (`Multiplexer::reflow`)
//! to place every panel. It is built from a [`LayoutMode`] preset via
//! [`LayoutTree::from_preset`] — an even split `ratio` at each node — and
//! rebuilt on every structural change (spawn / kill) so it always matches the
//! live panel set, mirroring [`Layout::reflow`](super::Layout::reflow)'s preset
//! arrangement while keeping the projection state in one place.
//!
//! The tree is ephemeral. `caucus resume` reconstructs it from the persisted
//! `layout_mode` + panel order via [`LayoutTree::from_preset`], so the record
//! schema is unchanged.

use super::{LayoutMode, Rect};
use crate::session::id::PanelId;

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
        /// Fraction of the split extent given to `first` — the preset's even
        /// split (`1/n` for a chain head, `0.5` for `MainVertical`).
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
}
