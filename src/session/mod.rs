//! Session: one caucus multiplexer instance — the set of panels convened
//! around one topic. See `docs/design.md` §3.

pub mod id;
pub mod state;

pub use id::{AgentId, PanelId, SessionId};
pub use state::{Session, SessionState};
