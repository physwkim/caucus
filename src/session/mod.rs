//! Session: a single topic of work. Owns the state machine; state transitions
//! must go through `state::transition` (see `docs/design.md` Invariant I-1).

pub mod id;
pub mod record;
pub mod state;

pub use record::{Session, SessionRecordError, list_sessions, read_session, write_session};
