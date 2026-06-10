//! Terminal layer: vte-backed per-panel screen grid plus turn-segmented
//! output capture. See `docs/design.md` §0 #3, §8.5.

pub mod capture;
pub mod grid;
pub mod prompt_scan;

pub use capture::{OutputCapture, TurnSegment};
pub use grid::{Cell, Grid};
pub use prompt_scan::{Menu, MenuOption, scan_menu, scan_yes_no_prompt};
