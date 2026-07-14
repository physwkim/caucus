//! Agent: a `claude` / `codex` process running in one panel — one
//! instance of one role. Manifest, lane events, derived state, provenance.
//! See `docs/design.md` §8.

pub mod codex_trust;
pub mod derive_state;
pub mod lane_event;
pub mod manifest;
pub mod provenance;
pub mod spawn;

pub use derive_state::{DerivedState, derive_agent_state};
pub use lane_event::{LaneEvent, LaneEventBlocker, LaneEventKind, LaneFailureClass};
pub use manifest::{AgentManifest, AgentStatus};
pub use provenance::{LaneCommitProvenance, SupersededBy, extract_commit_sha};

pub use crate::session::id::AgentId;
