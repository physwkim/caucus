//! Exit code conventions, per `docs/design.md` §10.1.

use std::process::ExitCode;

/// Success.
pub const OK: u8 = 0;
/// Unexpected failure / panic class. The default for `From<anyhow::Error>`.
pub const GENERIC_FAILURE: u8 = 1;
/// User error — bad flags, missing session, etc.
pub const USER_ERROR: u8 = 2;
/// Environment error — tmux missing, git missing, claude CLI missing.
pub const ENVIRONMENT_ERROR: u8 = 3;
/// caucus state corruption — manifest unparseable, state file missing, etc.
pub const STATE_ERROR: u8 = 4;

pub fn code(c: u8) -> ExitCode {
    ExitCode::from(c)
}
