//! Terminal layer: vte-backed per-panel screen grid plus turn-segmented
//! output capture. See `docs/design.md` §0 #3, §8.5.

pub mod capture;
pub mod grid;

pub use capture::{OutputCapture, TurnSegment};
pub use grid::{Cell, Grid};
