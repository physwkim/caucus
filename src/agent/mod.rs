//! Agent: a `claude` / `codex` / `gemini` process running in one panel — one
//! instance of one role. Manifest, lane events, derived state, provenance.
//! See `docs/design.md` §8.

pub mod derive_state;
pub mod lane_event;
pub mod manifest;
pub mod provenance;
pub mod spawn;

pub use derive_state::{DerivedState, GridHint, derive_agent_state};
pub use lane_event::{LaneEvent, LaneEventBlocker, LaneEventKind, LaneFailureClass};
pub use manifest::{AgentManifest, AgentStatus};
pub use provenance::{LaneCommitProvenance, extract_commit_sha};

pub use crate::session::id::AgentId;
