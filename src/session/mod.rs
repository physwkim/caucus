//! Session: one caucus multiplexer instance — the set of panels convened
//! around one topic. See `docs/design.md` §3.

pub mod id;
pub mod lock;
pub mod record;
pub mod round_record;
pub mod runtime;
pub mod state;

pub use id::{AgentId, PanelId, RoundId, SessionId};
pub use record::{PanelRecord, SessionRecord, SessionRecordError};
pub use runtime::Multiplexer;
pub use state::{Session, SessionState};
