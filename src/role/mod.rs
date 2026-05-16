//! Role: architect / backend / reviewer etc. — a system prompt + tool
//! allowlist + default `model`/`agent_cli`. See `docs/design.md` §6.

pub mod registry;
pub mod spec;

pub use registry::{RoleRegistry, UnknownRole};
pub use spec::{AgentCli, RoleSpec};
