//! Agent lifecycle: spawn, manifest persistence, lane events, derived state, commit provenance.
//!
//! See `docs/design.md` §8 for the data model and §9.1 for the single-owner rules
//! (manifest writes go through `manifest::write_json`, nothing else).

pub mod derive_state;
pub mod lane_event;
pub mod manifest;
pub mod provenance;
