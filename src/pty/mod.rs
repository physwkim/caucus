//! PTY layer: a thin wrapper over `portable-pty` — one PTY per panel.
//! See `docs/design.md` §0 #3, §9.
//!
//! **Invariant I-5** (`docs/design.md` §12): PTYs are created and destroyed
//! only by [`Pty::spawn`] / [`Pty::kill`]. No module calls `openpty`/`fork`
//! directly.
//!
//! The real PTY plumbing (reader thread, byte pump into `term::Grid`) is
//! Phase 2; this skeleton fixes the type + method surface.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use portable_pty::PtySize;
use thiserror::Error;

/// How to launch the child process inside a PTY.
#[derive(Debug, Clone)]
pub struct PtyCommand {
    /// Program to exec (e.g. `claude`, `codex`, `gemini`).
    pub program: OsString,
    /// Arguments.
    pub args: Vec<OsString>,
    /// Working directory — the panel's worktree, when execute-phase.
    pub cwd: Option<PathBuf>,
    /// Extra environment to inject — the `CAUCUS_*` vars (`docs/design.md`
    /// §7.1) go here.
    pub env: HashMap<String, String>,
}

impl PtyCommand {
    /// A command launching `program` with no args.
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
        }
    }
}

/// Errors from PTY operations.
#[derive(Debug, Error)]
pub enum PtyError {
    #[error("pty open: {0}")]
    Open(String),
    #[error("pty spawn: {0}")]
    Spawn(String),
    #[error("pty io: {0}")]
    Io(#[from] std::io::Error),
}

/// One pseudo-terminal owning a child agent process (`docs/design.md` §9.1,
/// Invariant I-5).
pub struct Pty {
    size: PtySize,
    // TODO(phase 2): hold the `portable_pty` master handle, the `Box<dyn
    // Child>`, the writer, and the reader thread join handle.
}

impl Pty {
    /// Spawn `command` inside a fresh PTY sized `cols x rows`.
    ///
    /// Single owner of PTY creation (Invariant I-5).
    pub(crate) fn spawn(command: &PtyCommand, cols: u16, rows: u16) -> Result<Self, PtyError> {
        // TODO(phase 2): `native_pty_system().openpty(..)`, build a
        // `CommandBuilder` from `command`, `slave.spawn_command(..)`, start the
        // reader thread.
        let _ = command;
        Ok(Self {
            size: PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
        })
    }

    /// Current PTY size, `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        (self.size.cols, self.size.rows)
    }

    /// Read available output bytes from the PTY master (non-blocking).
    pub(crate) fn read(&mut self) -> Result<Vec<u8>, PtyError> {
        // TODO(phase 2): drain the reader thread's channel.
        todo!("phase 2: PTY read")
    }

    /// Write input bytes to the PTY master — the fully bidirectional input
    /// path (`docs/design.md` §0 #11).
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<(), PtyError> {
        // TODO(phase 2): write to the master writer.
        let _ = bytes;
        todo!("phase 2: PTY write")
    }

    /// Resize the PTY (on layout reflow).
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) -> Result<(), PtyError> {
        // TODO(phase 2): `master.resize(PtySize { .. })`.
        self.size.cols = cols;
        self.size.rows = rows;
        Ok(())
    }

    /// Kill the child process and tear down the PTY.
    ///
    /// Single owner of PTY destruction (Invariant I-5).
    pub(crate) fn kill(&mut self) -> Result<(), PtyError> {
        // TODO(phase 2): `child.kill()`, join the reader thread.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_command_constructor() {
        let cmd = PtyCommand::new("claude");
        assert_eq!(cmd.program, OsString::from("claude"));
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn spawn_records_size() {
        let cmd = PtyCommand::new("claude");
        let pty = Pty::spawn(&cmd, 80, 24).unwrap();
        assert_eq!(pty.size(), (80, 24));
    }
}
