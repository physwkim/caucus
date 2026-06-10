//! PTY layer: a thin wrapper over `portable-pty` — one PTY per panel.
//! See `docs/design.md` §0 #3, §9.
//!
//! **Invariant I-5** (`docs/design.md` §12): PTYs are created and destroyed
//! only by `Pty::spawn` / `Pty::kill`. No module calls `openpty`/`fork`
//! directly.
//!
//! Layout: `Pty::spawn` opens a PTY pair, spawns the child on the slave,
//! drops the slave, and starts two threads: a *reader* thread draining the
//! master reader into an `mpsc` channel, and a *writer* thread draining a
//! second `mpsc` channel into the master writer. `Pty::read` non-blockingly
//! drains the reader channel; `Pty::write` non-blockingly *enqueues* onto the
//! writer channel (`docs/design.md` §0 #11, the fully bidirectional input
//! path); `Pty::kill` kills the child and joins both threads so no fd or
//! thread leaks.
//!
//! The writer thread exists so the blocking `write_all` to the PTY master
//! happens off the caucus event-loop thread. An agent that has stopped reading
//! its stdin (busy, hung, suspended) fills the kernel PTY input buffer; a
//! synchronous `write_all` from the event loop would then block the entire
//! multiplexer — no input, no pump, no redraw. With the writer thread, that
//! block lands harmlessly in the per-panel writer thread while the event loop
//! keeps ticking.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use thiserror::Error;

/// How to launch the child process inside a PTY.
#[derive(Debug, Clone)]
pub struct PtyCommand {
    /// Program to exec (e.g. `claude`, `codex`).
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
    #[error("pty writer closed")]
    WriterClosed,
}

/// One pseudo-terminal owning a child agent process (`docs/design.md` §9.1,
/// Invariant I-5).
pub struct Pty {
    size: PtySize,
    /// Master side of the PTY pair. Owns the master fd; used for `resize`.
    /// `None` after `kill` drops it. Held as an `Option` so `kill` can close it
    /// while the `Pty` lives on: dropping the *last* master-side reference (this
    /// handle plus the reader's and writer's cloned fds) closes the master
    /// device, which makes a child wedged in a PTY write fail with `EIO` and
    /// exit — macOS does not interrupt a flow-controlled PTY write even with
    /// `SIGKILL`, so this is what lets `kill` reap such a child instead of
    /// leaving it wedged until the `Pty` is finally dropped.
    master: Option<Box<dyn MasterPty + Send>>,
    /// The child agent process. `kill` tears it down.
    child: Box<dyn Child + Send + Sync>,
    /// Sender to the writer thread (`docs/design.md` §0 #11). `write` enqueues
    /// bytes here non-blockingly; the writer thread performs the blocking
    /// `write_all` to the master off the event loop. `None` after `kill` (the
    /// drop ends the writer thread).
    writer_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Receiving end of the reader thread's channel; `read` drains it.
    /// `None` after `kill` drops it: dropping the receiver makes a reader thread
    /// parked in `send` on a full channel observe `Err` and exit, so the join in
    /// `kill` cannot hang the event loop.
    rx: Option<Receiver<Vec<u8>>>,
    /// Join handle for the reader thread; `kill` joins it.
    reader: Option<JoinHandle<()>>,
    /// Join handle for the writer thread; `kill` joins it.
    writer: Option<JoinHandle<()>>,
}

impl Pty {
    /// Max chunks buffered between the reader thread and the event loop before
    /// back-pressure kicks in. The event loop drains the whole queue every tick
    /// (~4 ms), so this only fills when the loop stalls; at 8 KiB/chunk it caps
    /// the per-panel read buffer at ~8 MiB instead of letting it grow without
    /// bound when a panel firehoses output.
    const READER_CHANNEL_BOUND: usize = 1024;

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

        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Open(e.to_string()))?;

        let mut master_reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PtyError::Open(e.to_string()))?;

        // Bounded so a panel firehosing output while the event loop is busy
        // cannot grow this queue without limit. A full channel blocks the
        // reader thread's `send`, which stops draining the PTY master — kernel
        // back-pressure then stalls the child's writes until the event loop
        // drains a tick later. Memory is capped at
        // `READER_CHANNEL_BOUND × 8 KiB` per panel instead of unbounded.
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(Self::READER_CHANNEL_BOUND);
        let reader = std::thread::Builder::new()
            .name("caucus-pty-reader".to_string())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match master_reader.read(&mut buf) {
                        // EOF: child closed the PTY. Stop the thread.
                        Ok(0) => break,
                        Ok(n) => {
                            // Blocks when the queue is full (consumer behind);
                            // an Err means the receiver dropped (Pty gone).
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

        // Writer thread: the blocking `write_all` to the master lives here, off
        // the event loop. Drains the writer channel in order (single consumer →
        // FIFO, so a paste body always precedes its later submitting Enter).
        // Ends when the Sender is dropped (`kill`) or a write fails (child gone
        // / PTY closed) — at which point a write blocked on a non-reading child
        // unwinds via the broken-pipe error and the thread exits.
        let (writer_tx, writer_rx) = mpsc::channel::<Vec<u8>>();
        let writer_thread = std::thread::Builder::new()
            .name("caucus-pty-writer".to_string())
            .spawn(move || {
                while let Ok(chunk) = writer_rx.recv() {
                    if writer.write_all(&chunk).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
            })
            .map_err(PtyError::Io)?;

        Ok(Self {
            size,
            master: Some(pair.master),
            child,
            writer_tx: Some(writer_tx),
            rx: Some(rx),
            reader: Some(reader),
            writer: Some(writer_thread),
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
        // Drain every queued chunk; any error ends the drain — `Empty` means
        // nothing pending, `Disconnected` means the reader thread finished
        // after a clean child exit. Both surface as an empty (or partial) read.
        if let Some(rx) = &self.rx {
            while let Ok(chunk) = rx.try_recv() {
                out.extend_from_slice(&chunk);
            }
        }
        Ok(out)
    }

    /// Enqueue input bytes for the PTY master — the fully bidirectional input
    /// path (`docs/design.md` §0 #11).
    ///
    /// Non-blocking: the bytes are handed to the per-panel writer thread, which
    /// performs the blocking `write_all` off the event loop. `Ok` therefore
    /// means "queued", not "delivered to the kernel" — the decoupling that
    /// stops a non-reading agent from stalling the whole multiplexer. The only
    /// error is [`PtyError::WriterClosed`]: the writer thread has gone (the PTY
    /// was killed), i.e. the panel is dead.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<(), PtyError> {
        match &self.writer_tx {
            Some(tx) => tx.send(bytes.to_vec()).map_err(|_| PtyError::WriterClosed),
            None => Err(PtyError::WriterClosed),
        }
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
        if let Some(master) = &self.master {
            master
                .resize(size)
                .map_err(|e| PtyError::Open(e.to_string()))?;
        }
        self.size = size;
        Ok(())
    }

    /// Kill the child process and tear down the PTY in bounded time.
    ///
    /// Single owner of PTY destruction (Invariant I-5). A naive
    /// `child.kill()` then `wait` then join leaves three failure modes open;
    /// teardown closes each so the event loop never hangs and nothing leaks:
    ///
    /// 1. **The reader is parked in `send` on a full channel.** When the event
    ///    loop stopped draining while a panel firehosed output, the reader
    ///    thread blocks in `send`; killing the child does not wake it. Dropping
    ///    the receiver makes that `send` return `Err`, so the thread exits and
    ///    the join completes.
    /// 2. **The child is wedged in a PTY write.** A firehosing child whose
    ///    output filled the master buffer blocks in `write`; macOS does not
    ///    interrupt that write even for `SIGKILL`, so neither the signal nor
    ///    `wait` makes progress. Closing every master-side fd makes the write
    ///    fail with `EIO`, which is the only thing that unwedges the child.
    /// 3. **Descendants outlive the direct child.** The agent (`claude`) spawns
    ///    its own children — MCP servers, language servers, helpers — that share
    ///    the child's process group. `portable-pty`'s `kill` (which escalates
    ///    `SIGHUP` to `SIGKILL` after a grace period) signals only the direct
    ///    child, so those descendants survive as orphans after every teardown.
    ///    The child is a session leader (`portable-pty` calls `setsid`), so its
    ///    process-group id equals its pid; one `SIGKILL` to that whole group
    ///    reaps them. On Linux a surviving descendant also holds the slave open,
    ///    so reaping the group is additionally what lets the reader see EOF;
    ///    macOS revokes the controlling terminal when the session leader exits,
    ///    so there the group kill is purely leak prevention.
    pub(crate) fn kill(&mut self) -> Result<(), PtyError> {
        // 1. Graceful: `SIGHUP` via portable-pty lets a well-behaved agent exit
        //    on its own. An already-exited child yields an error we ignore so a
        //    second `kill` (or `kill` after natural exit) still tears down.
        let _ = self.child.kill();

        // 2. Forceful, whole-group: `child.kill()` above already escalates
        //    `SIGHUP` to `SIGKILL` on the *single* child pid, but never signals
        //    the child's descendants (failure mode 3). `killpg` to the group the
        //    child leads reaps every descendant so none leaks; on Linux it is
        //    also what closes their slave handles so the reader can reach EOF.
        //    Best-effort: `ESRCH`/`EPERM` on an already-dead or unsignalable
        //    group is fine. caucus is unaffected — the group is the child's own
        //    (it called `setsid`), not caucus's.
        #[cfg(unix)]
        if let Some(pid) = self.child.process_id() {
            let pid = pid as libc::pid_t;
            // SAFETY: best-effort `SIGKILL` to the child's process group; the
            // return value is deliberately ignored.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }

        // 3. Unwedge and tear down the I/O threads, closing every master-side fd
        //    so a child blocked in a PTY write (the firehose case `SIGKILL` does
        //    not interrupt) fails with `EIO` and exits:
        //    a. Drop the reader's receiver so a reader parked in `send` on a full
        //       channel returns `Err`, breaks, and drops its master-reader clone.
        self.rx = None;
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
        //    b. Drop the writer Sender so the writer thread exits and drops its
        //       master writer.
        self.writer_tx = None;
        if let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
        //    c. Drop our own master handle — the last master-side reference. The
        //       child's slave write now fails with `EIO` and the child exits, so
        //       the reap below collects it here rather than leaving it wedged
        //       until the `Pty` itself is dropped.
        self.master = None;

        // 4. Reap so the child is not left a zombie. Bounded: every path above
        //    drives the child toward exit, but the event loop must never block
        //    indefinitely on `wait`, so poll `try_wait` for a short window and
        //    give up (leaving the OS to reap) rather than hang.
        for _ in 0..50 {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
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
        assert!(
            text.contains("hi"),
            "expected child output 'hi', got {text:?}"
        );

        pty.kill().unwrap();
    }

    #[test]
    fn reader_channel_applies_backpressure_without_loss() {
        // A child emits more than the reader channel can buffer
        // (READER_CHANNEL_BOUND × 8 KiB ≈ 8 MiB). The reader thread blocks on a
        // full queue and resumes as the consumer drains, so every byte still
        // arrives — bounded memory, no loss, no deadlock. /dev/zero has no
        // newlines, so PTY output processing cannot change the byte count.
        const TOTAL: usize = 12 * 1024 * 1024; // > the ~8 MiB channel bound
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args = vec!["-c".into(), format!("head -c {TOTAL} /dev/zero").into()];
        let mut pty = Pty::spawn(&cmd, 80, 24).unwrap();

        let mut got = 0usize;
        let deadline = Instant::now() + Duration::from_secs(30);
        while got < TOTAL && Instant::now() < deadline {
            let chunk = pty.read().unwrap();
            if chunk.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            } else {
                got += chunk.len();
            }
        }
        assert_eq!(got, TOTAL, "every byte flows through the bounded channel");

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
    fn write_does_not_block_on_a_non_reading_child() {
        // A child that never reads its stdin: a synchronous `write_all` to the
        // PTY master blocks once the kernel input buffer fills. The writer
        // thread makes `write` a non-blocking enqueue, so even a multi-megabyte
        // write returns immediately — the event loop is never stalled by a
        // wedged agent. This is the regression guard for that stall class.
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args = vec!["-c".into(), "sleep 5".into()];
        let mut pty = Pty::spawn(&cmd, 80, 24).unwrap();

        let big = vec![b'x'; 1 << 20]; // 1 MiB — far over any PTY input buffer.
        let start = Instant::now();
        pty.write(&big).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "write blocked for {elapsed:?}; it must enqueue and return at once"
        );

        pty.kill().unwrap();
    }

    #[test]
    fn kill_completes_when_the_reader_channel_is_full() {
        // A child firehoses output while the event loop never drains, so the
        // reader thread fills the bounded channel and parks in `send`. `kill`
        // must still return promptly: it drops the receiver, which makes that
        // `send` return `Err` so the reader thread exits and the join completes.
        // Without the drop the join hangs forever and freezes the multiplexer.
        //
        // `exec head …` so the child *is* `head` (no forked shell that could
        // outlive SIGHUP and keep the PTY open — that orphan case is covered by
        // the process-group teardown test). 64 MiB ≫ the ~8 MiB channel bound,
        // so with no consumer draining the reader is parked in `send` — not
        // waiting on `read` — when we kill.
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args = vec!["-c".into(), "exec head -c 67108864 /dev/zero".into()];
        let mut pty = Pty::spawn(&cmd, 80, 24).unwrap();
        // Let the reader fill the channel and block on `send`; we never read.
        std::thread::sleep(Duration::from_millis(500));

        let start = Instant::now();
        pty.kill().unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "kill blocked for {elapsed:?}; dropping the receiver must unblock the reader"
        );
    }

    #[test]
    fn kill_reaps_the_childs_whole_process_group() {
        // The agent the child runs spawns its own children — MCP servers,
        // language servers, helpers — that share the child's process group (the
        // child is the session leader, since `portable-pty` calls `setsid`).
        // `portable-pty`'s kill signals only the direct child, so without a
        // group-wide SIGKILL those descendants survive as orphans after every
        // panel teardown. `kill` must reap the whole group.
        //
        // The descendant here both ignores SIGHUP and `exec`s `sleep`, so
        // neither the SIGHUP `portable-pty` sends nor the SIGHUP the kernel
        // delivers when the session leader exits (and the controlling terminal
        // is revoked) can stop it — only the uncatchable group SIGKILL does. A
        // unique sleep interval lets us detect a survivor by command line.
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.args = vec![
            "-c".into(),
            "(trap '' HUP; exec sleep 31337) & echo started".into(),
        ];
        let mut pty = Pty::spawn(&cmd, 80, 24).unwrap();
        std::thread::sleep(Duration::from_millis(200)); // let the descendant install its trap

        let start = Instant::now();
        pty.kill().unwrap();
        let elapsed = start.elapsed();

        // Let the group SIGKILL land before we look for survivors.
        std::thread::sleep(Duration::from_millis(300));
        let found = std::process::Command::new("pgrep")
            .args(["-f", "sleep 31337"])
            .output()
            .unwrap();
        let survivors = String::from_utf8_lossy(&found.stdout);
        let survivors = survivors.trim();
        // Best-effort cleanup so a failed assertion never leaks the process on.
        for pid in survivors.split_whitespace() {
            let _ = std::process::Command::new("kill")
                .args(["-9", pid])
                .status();
        }

        assert!(
            elapsed < Duration::from_secs(5),
            "kill blocked for {elapsed:?}; teardown must stay bounded"
        );
        assert!(
            survivors.is_empty(),
            "descendant {survivors:?} survived kill; the child's process group was not reaped"
        );
    }

    #[test]
    fn write_after_kill_reports_writer_closed() {
        let cmd = PtyCommand::new("/bin/cat");
        let mut pty = Pty::spawn(&cmd, 80, 24).unwrap();
        pty.kill().unwrap();
        assert!(matches!(pty.write(b"x"), Err(PtyError::WriterClosed)));
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
