//! Consensus policy. v0 only supports CEO-decided convergence; rule-based and
//! LLM-judge policies are deferred to v1+.

pub mod policy;

pub use policy::{ConsensusPolicy, DecisionRecord};
