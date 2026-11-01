//! Frame cost against panel count. Any panel's PTY output bumps
//! `render_signature`, and a changed signature repaints every visible panel,
//! so one spinning agent costs a frame proportional to the whole screen.
//! `one_active` measures that case; `all_active` is every panel spinning.
//!
//! The backend is crossterm writing into a byte counter, so a sample covers
//! widget layout, ratatui's buffer diff and escape encoding — everything
//! `tui::draw` does but the tty write — and `bytes/frame` is what the host
//! terminal would receive.

use std::cell::Cell;
use std::fmt::Write as _;
use std::io;
use std::rc::Rc;
use std::time::Instant;

use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect as TuiRect;
use ratatui::{Terminal, TerminalOptions, Viewport};

use super::{Layout, LayoutMode, Rect, draw};
use crate::bench_support::{Summary, cat_panel};
use crate::panel::Panel;

const COLS: u16 = 120;
const ROWS: u16 = 40;
const PANEL_COUNTS: [usize; 3] = [1, 15, 50];
const WARMUP_FRAMES: usize = 50;
const FRAMES: usize = 500;
const SPINNER: [char; 4] = ['·', '✢', '✳', '✶'];

/// Swallows the backend's output, keeping only its length. Shared because
/// ratatui's accessor for the backend writer is unstable.
#[derive(Clone, Default)]
struct ByteCounter(Rc<Cell<usize>>);

impl io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.set(self.0.get() + buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Activity {
    OneActive,
    AllActive,
}

impl Activity {
    fn label(self) -> &'static str {
        match self {
            Activity::OneActive => "one_active",
            Activity::AllActive => "all_active",
        }
    }
}

/// Fill the whole viewport with coloured text, one colour per row.
fn paint_screen(panel: &mut Panel, seed: usize) {
    let (cols, rows) = panel.grid.size();
    let mut bytes = String::new();
    for r in 0..rows {
        let _ = write!(bytes, "\x1b[{};1H\x1b[3{}m", r + 1, (r + seed) % 7 + 1);
        bytes.extend("lorem ipsum dolor sit amet ".chars().cycle().take(cols));
        bytes.push_str("\x1b[0m");
    }
    panel.grid.advance(bytes.as_bytes());
}

/// One spinner step on the bottom row, the shape of an agent's working line.
fn spin(panel: &mut Panel, frame: usize) {
    let (_, rows) = panel.grid.size();
    let glyph = SPINNER[frame % SPINNER.len()];
    let line =
        format!("\x1b[{rows};1H\x1b[33m{glyph}\x1b[0m Working… ({frame}s · esc to interrupt)");
    panel.grid.advance(line.as_bytes());
}

/// Time `FRAMES` draws of `n` tiled panels; returns the frame-time summary
/// and the mean bytes written per frame.
fn profile(n: usize, activity: Activity) -> (Summary, usize) {
    let body = Rect {
        x: 0,
        y: 0,
        width: COLS,
        height: ROWS - 1,
    };
    let mut panels: Vec<Panel> = (0..n).map(|i| cat_panel(&format!("worker-{i}"))).collect();
    let ids: Vec<_> = panels.iter().map(|p| p.id).collect();
    let layout = Layout::reflow(&ids, body, LayoutMode::Tiled);
    for (panel, (_, rect)) in panels.iter_mut().zip(&layout.slots) {
        panel.resize(*rect).unwrap();
    }
    for (i, panel) in panels.iter_mut().enumerate() {
        paint_screen(panel, i);
    }
    let focused = Some(ids[0]);

    let counter = ByteCounter::default();
    let viewport = Viewport::Fixed(TuiRect::new(0, 0, COLS, ROWS));
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(counter.clone()),
        TerminalOptions { viewport },
    )
    .unwrap();

    let mut samples = Vec::with_capacity(FRAMES);
    let mut bytes = 0;
    for frame in 0..WARMUP_FRAMES + FRAMES {
        match activity {
            Activity::OneActive => spin(&mut panels[0], frame),
            Activity::AllActive => panels.iter_mut().for_each(|p| spin(p, frame)),
        }
        let before = counter.0.get();
        let start = Instant::now();
        terminal
            .draw(|f| draw(f, &layout, &panels, focused))
            .unwrap();
        let elapsed = start.elapsed();
        if frame >= WARMUP_FRAMES {
            samples.push(elapsed);
            bytes += counter.0.get() - before;
        }
    }
    (Summary::of(&mut samples), bytes / FRAMES)
}

#[test]
#[ignore = "profile: cargo test --release --lib render_scale_profile -- --ignored --nocapture --test-threads=1"]
fn render_scale_profile() {
    println!(
        "render_scale_profile: {COLS}x{ROWS} tiled, {FRAMES} frames after {WARMUP_FRAMES} warmup"
    );
    println!(
        "{:<10} {:>6} {:>10} {:>8} {:>8} {:>7} {:>11}",
        "activity", "panels", "median_us", "p95_us", "max_us", "x1", "bytes/frame"
    );
    for activity in [Activity::OneActive, Activity::AllActive] {
        let mut base = None;
        for n in PANEL_COUNTS {
            let (s, bytes) = profile(n, activity);
            let base = *base.get_or_insert(s.median);
            println!(
                "{:<10} {:>6} {:>10} {:>8} {:>8} {:>7.2} {:>11}",
                activity.label(),
                n,
                s.median.as_micros(),
                s.p95.as_micros(),
                s.max.as_micros(),
                s.median.as_secs_f64() / base.as_secs_f64(),
                bytes
            );
        }
    }
}
