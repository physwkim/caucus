//! Session: a single topic of work. Owns the state machine; state transitions
//! must go through `state::transition` (see `docs/design.md` Invariant I-1).

pub mod id;
pub mod state;
