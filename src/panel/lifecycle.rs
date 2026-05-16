//! Panel lifecycle (`docs/design.md` §3, §9.1).
//!
//! A panel is one cell of the caucus screen: a PTY + a vte grid + a render
//! area. The real lifecycle lives here, at the panel level.
//!
//! **Invariant I-5** (`docs/design.md` §12): panels are created/destroyed only
//! by [`spawn`] / [`kill`], and panel state transitions only by [`transition`].

use std::path::PathBuf;

use thiserror::Error;

use crate::agent::spawn::SpawnRequest;
use crate::session::id::{AgentId, PanelId};
use crate::term::{Grid, OutputCapture};

/// Coarse panel state machine (`docs/design.md` §3).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PanelState {
    /// PTY allocated, agent process starting.
    Spawning,
    /// Agent is processing a turn.
    Working,
    /// Turn signal received — waiting for the next instruction.
    Idle,
    /// Agent is blocked (permission prompt / merge conflict / background job).
    Blocked,
    /// Agent process has exited.
    Exited,
}

/// One panel: a PTY-backed cell of the caucus screen.
///
/// `state` is `pub(crate)` so only [`transition`] can change it (Invariant
/// I-5). The grid is mutated only by PTY bytes (Invariant on `term::Grid`).
pub struct Panel {
    pub id: PanelId,
    /// Role name driving this panel (`architect`, `backend`, ...).
    pub role: String,
    /// The agent instance running here.
    pub agent_id: AgentId,
    /// Authoritative panel state.
    pub(crate) state: PanelState,
    /// Worktree cwd, if this is an execute-phase panel.
    pub worktree_path: Option<PathBuf>,
    /// vte-parsed screen for this panel.
    pub(crate) grid: Grid,
    /// Turn-segmented output capture (`docs/design.md` §8.5).
    pub(crate) capture: OutputCapture,
}

impl Panel {
    /// Current panel state. Mutation goes through [`transition`].
    pub fn state(&self) -> PanelState {
        self.state
    }

    /// Read-only view of the panel's grid.
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Read-only view of the panel's output capture.
    pub fn capture(&self) -> &OutputCapture {
        &self.capture
    }
}

/// Rejected panel transition.
#[derive(Debug, Error)]
#[error("illegal panel transition: {from:?} -> {to:?}")]
pub struct IllegalTransition {
    pub from: PanelState,
    pub to: PanelState,
}

/// Errors from panel lifecycle operations.
#[derive(Debug, Error)]
pub enum PanelError {
    #[error(transparent)]
    Transition(#[from] IllegalTransition),
    #[error("panel spawn: {0}")]
    Spawn(String),
}

/// Single owner of panel state transitions (Invariant I-5).
///
/// Legal moves: `Spawning -> Working`, `Working <-> Idle`, `Working|Idle ->
/// Blocked`, `Blocked -> Working`, and any state `-> Exited`.
pub(crate) fn transition(panel: &mut Panel, to: PanelState) -> Result<(), IllegalTransition> {
    use PanelState::*;
    let from = panel.state;
    let legal = matches!(
        (from, to),
        (Spawning, Working)
            | (Working, Idle)
            | (Idle, Working)
            | (Working, Blocked)
            | (Idle, Blocked)
            | (Blocked, Working)
            | (_, Exited)
    );
    if legal {
        panel.state = to;
        Ok(())
    } else {
        Err(IllegalTransition { from, to })
    }
}

/// Single owner of panel creation (Invariant I-5).
///
/// Allocates the PTY, the grid, and the capture, registers a new panel for
/// `request`, and reflows the layout.
pub(crate) fn spawn(request: &SpawnRequest) -> Result<Panel, PanelError> {
    // TODO(phase 2): `pty::Pty::spawn`, size the grid to the panel area,
    // register in the panel vector, reflow the layout via `render/`.
    let _ = request;
    todo!("phase 2: panel spawn")
}

/// Single owner of panel destruction (Invariant I-5).
///
/// Kills the PTY, removes the panel, enqueues any worktree for cleanup, and
/// reflows the layout.
pub(crate) fn kill(panel: &mut Panel) -> Result<(), PanelError> {
    // TODO(phase 2): `pty::Pty::kill`, transition to `Exited`, enqueue the
    // worktree via `worktree::cleanup::enqueue`, reflow.
    transition(panel, PanelState::Exited)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(state: PanelState) -> Panel {
        Panel {
            id: PanelId::new(),
            role: "reviewer".into(),
            agent_id: AgentId::new(),
            state,
            worktree_path: None,
            grid: Grid::new(80, 24),
            capture: OutputCapture::new(),
        }
    }

    #[test]
    fn spawning_to_working_is_legal() {
        let mut p = panel(PanelState::Spawning);
        transition(&mut p, PanelState::Working).unwrap();
        assert_eq!(p.state(), PanelState::Working);
    }

    #[test]
    fn working_idle_toggle_is_legal() {
        let mut p = panel(PanelState::Working);
        transition(&mut p, PanelState::Idle).unwrap();
        transition(&mut p, PanelState::Working).unwrap();
        assert_eq!(p.state(), PanelState::Working);
    }

    #[test]
    fn spawning_to_idle_is_rejected() {
        let mut p = panel(PanelState::Spawning);
        assert!(transition(&mut p, PanelState::Idle).is_err());
    }

    #[test]
    fn anything_to_exited_is_legal() {
        let mut p = panel(PanelState::Blocked);
        transition(&mut p, PanelState::Exited).unwrap();
        assert_eq!(p.state(), PanelState::Exited);
    }
}
