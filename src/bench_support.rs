//! Shared plumbing for the `#[ignore]`d `*_profile` tests. Run one in release
//! mode, alone:
//!
//! ```text
//! cargo test --release --lib <name>_profile -- --ignored --nocapture --test-threads=1
//! ```
//!
//! A profile prints numbers and asserts nothing about them: wall-clock limits
//! flake on shared runners, so the numbers are evidence for a change, read
//! against the same profile run on the base commit.

use std::time::Duration;

use crate::panel::{Panel, PanelState};
use crate::pty::{Pty, PtyCommand};
use crate::session::id::{AgentId, PanelId};
use crate::term::{Grid, OutputCapture};

/// Median, 95th percentile and maximum of one profile's samples.
pub(crate) struct Summary {
    pub median: Duration,
    pub p95: Duration,
    pub max: Duration,
}

impl Summary {
    pub(crate) fn of(samples: &mut [Duration]) -> Self {
        assert!(!samples.is_empty(), "a profile took no samples");
        samples.sort_unstable();
        let last = samples.len() - 1;
        let at = |q: f64| samples[(last as f64 * q).round() as usize];
        Self {
            median: at(0.5),
            p95: at(0.95),
            max: samples[last],
        }
    }
}

/// A live panel around `/bin/cat`: a real PTY and grid, no agent CLI needed.
pub(crate) fn cat_panel(role: &str) -> Panel {
    let pty = Pty::spawn(&PtyCommand::new("/bin/cat"), 78, 22).unwrap();
    Panel {
        id: PanelId::new(),
        role: role.into(),
        agent_id: AgentId::new(),
        state: PanelState::Idle,
        worktree_path: None,
        pty,
        grid: Grid::new(78, 22),
        capture: OutputCapture::new(),
    }
}
