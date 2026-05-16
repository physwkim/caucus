//! PTY layer: a thin wrapper over `portable-pty` — one PTY per panel.
//! See `docs/design.md` §0 #3, §9.
//!
//! **Invariant I-5** (`docs/design.md` §12): PTYs are created and destroyed
//! only by [`Pty::spawn`] / [`Pty::kill`]. No module calls `openpty`/`fork`
//! directly.
//!
//! Layout: [`Pty::spawn`] opens a PTY pair, spawns the child on the slave,
//! drops the slave, and starts a single reader thread that drains the master
//! reader into an `mpsc` channel. [`Pty::read`] non-blockingly drains that
//! channel; [`Pty::write`] feeds the master writer (`docs/design.md` §0 #11,
//! the fully bidirectional input path); [`Pty::kill`] kills the child and
//! joins the reader thread so no fd or thread leaks.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::JoinHandle;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
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

    /// Translate into a `portable-pty` [`CommandBuilder`].
    fn to_builder(&self) -> CommandBuilder {
        let mut builder = CommandBuilder::new(&self.program);
        builder.args(&self.args);
        if let Some(cwd) = &self.cwd {
            builder.cwd(cwd);
        }
        // The child inherits the caucus process environment by default;
        // these entries (the `CAUCUS_*` vars, §7.1) are layered on top.
        for (key, value) in &self.env {
            builder.env(key, value);
        }
        builder
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
    /// Master side of the PTY pair. Owns the fd; used for `resize` and to
    /// hand out the writer / reader. Dropping it closes the master fd.
    master: Box<dyn MasterPty + Send>,
    /// The child agent process. `kill` tears it down.
    child: Box<dyn Child + Send + Sync>,
    /// Writer half of the master — the input path (`docs/design.md` §0 #11).
    writer: Box<dyn Write + Send>,
    /// Receiving end of the reader thread's channel; `read` drains it.
    rx: Receiver<Vec<u8>>,
    /// Join handle for the reader thread; `kill` joins it.
    reader: Option<JoinHandle<()>>,
}

impl Pty {
    /// Spawn `command` inside a fresh PTY sized `cols x rows`.
    ///
    /// Single owner of PTY creation (Invariant I-5).
    pub(crate) fn spawn(command: &PtyCommand, cols: u16, rows: u16) -> Result<Self, PtyError> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| PtyError::Open(e.to_string()))?;

        let child = pair
            .slave
            .spawn_command(command.to_builder())
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        // The slave fd is no longer needed once the child holds it; dropping
        // it lets the reader see EOF when the child exits.
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Open(e.to_string()))?;

        let mut master_reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Open(e.to_string()))?;

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let reader = std::thread::Builder::new()
            .name("caucus-pty-reader".to_string())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match master_reader.read(&mut buf) {
                        // EOF: child closed the PTY. Stop the thread.
                        Ok(0) => break,
                        Ok(n) => {
                            // Receiver dropped (Pty gone): nothing left to do.
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .map_err(PtyError::Io)?;

        Ok(Self {
            size,
            master: pair.master,
            child,
            writer,
            rx,
            reader: Some(reader),
        })
    }

    /// Current PTY size, `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        (self.size.cols, self.size.rows)
    }

    /// Read available output bytes from the PTY master (non-blocking).
    ///
    /// Drains every chunk the reader thread has queued so far. Returns an
    /// empty `Vec` when nothing is pending; the disconnected channel (reader
    /// thread finished after the child exited) is also reported as empty so
    /// callers keep draining without erroring on a clean exit.
    pub(crate) fn read(&mut self) -> Result<Vec<u8>, PtyError> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => out.extend_from_slice(&chunk),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        Ok(out)
    }

    /// Write input bytes to the PTY master — the fully bidirectional input
    /// path (`docs/design.md` §0 #11).
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<(), PtyError> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Whether the child agent process is still running.
    ///
    /// Non-blocking: a [`Child::try_wait`] probe. A child that has exited (or
    /// whose status can no longer be read) reports `false`, so the panel
    /// event loop can transition the panel to `Exited` and reflow.
    pub(crate) fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Resize the PTY (on layout reflow).
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) -> Result<(), PtyError> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.master
            .resize(size)
            .map_err(|e| PtyError::Open(e.to_string()))?;
        self.size = size;
        Ok(())
    }

    /// Kill the child process and tear down the PTY.
    ///
    /// Single owner of PTY destruction (Invariant I-5). Killing the child
    /// closes its end of the PTY; the reader thread then sees EOF and exits,
    /// so the join completes without a leaked thread or fd.
    pub(crate) fn kill(&mut self) -> Result<(), PtyError> {
        // Killing the child is idempotent enough for a kill path: an
        // already-exited child yields an error we deliberately ignore so a
        // second `kill` (or `kill` after natural exit) still tears down.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.reader.take() {
            // Reader exits on EOF once the child's PTY fd is closed.
            let _ = handle.join();
        }
        Ok(())
    }
}

impl Drop for Pty {
    /// Backstop teardown: if a `Pty` is dropped without an explicit `kill`,
    /// still kill the child and join the reader thread so nothing leaks.
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn pty_command_constructor() {
        let cmd = PtyCommand::new("claude");
        assert_eq!(cmd.program, OsString::from("claude"));
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn spawn_records_size() {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args = vec!["-c".into(), "true".into()];
        let pty = Pty::spawn(&cmd, 80, 24).unwrap();
        assert_eq!(pty.size(), (80, 24));
    }

    /// Block (with a deadline) until the reader thread has surfaced output.
    fn drain_until_nonempty(pty: &mut Pty, deadline: Duration) -> Vec<u8> {
        let start = Instant::now();
        let mut out = Vec::new();
        while start.elapsed() < deadline {
            out.extend(pty.read().unwrap());
            if !out.is_empty() {
                // Give the child a beat to flush any trailing bytes.
                std::thread::sleep(Duration::from_millis(20));
                out.extend(pty.read().unwrap());
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        out
    }

    #[test]
    fn spawn_then_read_captures_child_output() {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args = vec!["-c".into(), "printf hi".into()];
        let mut pty = Pty::spawn(&cmd, 80, 24).unwrap();

        let out = drain_until_nonempty(&mut pty, Duration::from_secs(5));
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("hi"), "expected child output 'hi', got {text:?}");

        pty.kill().unwrap();
    }

    #[test]
    fn env_vars_reach_the_child() {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args = vec!["-c".into(), "printf %s \"$CAUCUS_PANEL_ID\"".into()];
        cmd.env
            .insert("CAUCUS_PANEL_ID".to_string(), "panel-7".to_string());
        let mut pty = Pty::spawn(&cmd, 80, 24).unwrap();

        let out = drain_until_nonempty(&mut pty, Duration::from_secs(5));
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("panel-7"),
            "expected injected env in child output, got {text:?}"
        );

        pty.kill().unwrap();
    }

    #[test]
    fn write_then_read_round_trips_through_cat() {
        // `cat` echoes stdin back to stdout; on a PTY the line discipline
        // also echoes the input, so the written bytes appear at least once.
        let cmd = PtyCommand::new("/bin/cat");
        let mut pty = Pty::spawn(&cmd, 80, 24).unwrap();

        pty.write(b"caucus-roundtrip\n").unwrap();
        let out = drain_until_nonempty(&mut pty, Duration::from_secs(5));
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("caucus-roundtrip"),
            "expected written bytes echoed back, got {text:?}"
        );

        pty.kill().unwrap();
    }

    #[test]
    fn resize_updates_stored_size() {
        let cmd = PtyCommand::new("/bin/cat");
        let mut pty = Pty::spawn(&cmd, 80, 24).unwrap();
        pty.resize(120, 40).unwrap();
        assert_eq!(pty.size(), (120, 40));
        pty.kill().unwrap();
    }

    #[test]
    fn kill_is_idempotent_and_read_after_exit_is_empty_ok() {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args = vec!["-c".into(), "true".into()];
        let mut pty = Pty::spawn(&cmd, 80, 24).unwrap();

        pty.kill().unwrap();
        // Second kill must not panic or error.
        pty.kill().unwrap();
        // Reading after the child exited returns Ok(empty), never an error.
        assert!(pty.read().unwrap().is_empty());
    }
}
