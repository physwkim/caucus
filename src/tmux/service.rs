//! Typed wrapper around the `tmux` CLI. Real implementation issues
//! `Command::new("tmux")` calls; failures bubble up as `TmuxError`.
//!
//! Invariant I (single-owner): pane creation goes through [`TmuxService::spawn_pane`],
//! pane termination through [`TmuxService::kill_pane`]. No other module may
//! shell out to `tmux split-window` / `tmux kill-pane` directly — see
//! `docs/design.md` §9.1.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;

use thiserror::Error;
use tokio::process::Command;

use super::escape::single_quote_shell;

/// Configuration used by [`TmuxService`] to invoke `tmux`. Override
/// `binary` in tests to point at a stub.
#[derive(Debug, Clone)]
pub struct TmuxConfig {
    pub binary: OsString,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            binary: OsString::from("tmux"),
        }
    }
}

/// Errors from any tmux call.
#[derive(Debug, Error)]
pub enum TmuxError {
    #[error("tmux command spawn ({command}): {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("tmux command failed ({command}, exit {code:?}): {stderr}")]
    NonZero {
        command: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("pane id missing in tmux output (command: {command})")]
    MissingPaneId { command: String },
}

/// Options for [`TmuxService::spawn_pane`].
#[derive(Debug, Clone, Default)]
pub struct SpawnPaneOptions {
    /// Target pane to split. `None` splits the current pane.
    pub target_pane: Option<String>,
    /// Working directory for the new pane.
    pub cwd: Option<PathBuf>,
    /// Shell command to run inside the new pane. If `None`, tmux spawns
    /// the user's default shell.
    pub command: Option<String>,
    /// Vertical split if true, horizontal otherwise (default: horizontal).
    pub vertical: bool,
    /// Environment variables to inject into the new pane's process.
    pub env: HashMap<String, String>,
    /// Pane title (`select-pane -T`) applied after spawn.
    pub title: Option<String>,
}

/// Typed wrapper over `tmux`.
#[derive(Debug, Clone)]
pub struct TmuxService {
    config: TmuxConfig,
}

impl TmuxService {
    pub fn new() -> Self {
        Self::with_config(TmuxConfig::default())
    }

    pub fn with_config(config: TmuxConfig) -> Self {
        Self { config }
    }

    /// `tmux split-window -P -F '#{pane_id}' [opts] [command]` — captures
    /// the new pane id from stdout. See `docs/dmux-analysis.md` §4.5: the
    /// `-P -F '#{pane_id}'` combo is mandatory.
    pub async fn spawn_pane(&self, opts: SpawnPaneOptions) -> Result<String, TmuxError> {
        let mut args: Vec<String> = vec![
            "split-window".into(),
            if opts.vertical {
                "-v".into()
            } else {
                "-h".into()
            },
            "-P".into(),
            "-F".into(),
            "#{pane_id}".into(),
        ];
        if let Some(target) = &opts.target_pane {
            args.push("-t".into());
            args.push(target.clone());
        }
        if let Some(cwd) = &opts.cwd {
            args.push("-c".into());
            args.push(cwd.display().to_string());
        }
        for (k, v) in &opts.env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        if let Some(cmd) = &opts.command {
            args.push(cmd.clone());
        }

        let out = self.run(args).await?;
        let pane_id = out
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(str::trim)
            .ok_or_else(|| TmuxError::MissingPaneId {
                command: "split-window".into(),
            })?
            .to_string();

        if let Some(title) = opts.title {
            self.set_pane_title(&pane_id, &title).await?;
        }
        Ok(pane_id)
    }

    /// `tmux new-session -d -s NAME` — start a fresh detached session and
    /// return its name. Used by tests; the user-facing CLI uses the caller's
    /// existing tmux session.
    pub async fn new_session(&self, name: &str) -> Result<String, TmuxError> {
        self.run(vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            name.into(),
        ])
        .await?;
        Ok(name.to_string())
    }

    /// `tmux kill-session -t NAME`.
    pub async fn kill_session(&self, name: &str) -> Result<(), TmuxError> {
        let _ = self
            .run(vec!["kill-session".into(), "-t".into(), name.into()])
            .await?;
        Ok(())
    }

    /// `tmux set-option -t SESSION -g remain-on-exit on` (or equivalent).
    pub async fn set_option_global(&self, option: &str, value: &str) -> Result<(), TmuxError> {
        let _ = self
            .run(vec![
                "set-option".into(),
                "-g".into(),
                option.into(),
                value.into(),
            ])
            .await?;
        Ok(())
    }

    /// `tmux select-layout LAYOUT` — re-balance every pane in the current
    /// window (or in the target window if `target` is set). The standard
    /// preset names accepted are `even-horizontal`, `even-vertical`,
    /// `main-horizontal`, `main-vertical`, and `tiled` (a 2D grid). After
    /// `caucus session new --roles a,b,c,d` the four role panes end up at
    /// 50% / 25% / 12.5% / 12.5% widths because each `split-window`
    /// halves the current pane; calling this with `tiled` afterwards
    /// produces an evenly-distributed grid that scales with the window.
    pub async fn select_layout(&self, layout: &str, target: Option<&str>) -> Result<(), TmuxError> {
        let mut args = vec!["select-layout".to_string()];
        if let Some(t) = target {
            args.push("-t".into());
            args.push(t.into());
        }
        args.push(layout.into());
        let _ = self.run(args).await?;
        Ok(())
    }

    /// Pick a sensible balanced layout for `pane_count` panes in one window.
    /// Used by `caucus session new` / `execute start` / `session relayout`
    /// so role panes don't end up in geometric halves. Contract:
    /// - 1 pane → no-op (nothing to balance).
    /// - 2 panes → `even-horizontal`.
    /// - 3+ panes → `tiled` (2D grid; uses both row and column splits).
    pub async fn rebalance_window_for_panes(
        &self,
        pane_count: usize,
        target: Option<&str>,
    ) -> Result<(), TmuxError> {
        let layout = match pane_count {
            0 | 1 => return Ok(()),
            2 => "even-horizontal",
            _ => "tiled",
        };
        self.select_layout(layout, target).await
    }

    /// Apply an explicit layout name (caller-chosen). Honours the `auto`
    /// path via `rebalance_window_for_panes` when `explicit` is None.
    pub async fn apply_layout(
        &self,
        explicit: Option<&str>,
        pane_count: usize,
        target: Option<&str>,
    ) -> Result<(), TmuxError> {
        match explicit {
            Some(name) => self.select_layout(name, target).await,
            None => self.rebalance_window_for_panes(pane_count, target).await,
        }
    }

    /// `tmux select-pane -t PANE -T TITLE`.
    pub async fn set_pane_title(&self, pane: &str, title: &str) -> Result<(), TmuxError> {
        let _ = self
            .run(vec![
                "select-pane".into(),
                "-t".into(),
                pane.into(),
                "-T".into(),
                title.into(),
            ])
            .await?;
        Ok(())
    }

    /// Send a *shell command line* — auto-quoted via [`single_quote_shell`].
    /// `enter` controls whether `Enter` is appended (the common case).
    ///
    /// This is the right call for: `git status`, `claude --print "hi"`,
    /// `echo done > foo`. It is the **wrong** call for tmux key names like
    /// `Enter` or `C-l` (those go through [`send_keys`]).
    pub async fn send_shell(
        &self,
        pane: &str,
        command: &str,
        enter: bool,
    ) -> Result<(), TmuxError> {
        let quoted = single_quote_shell(command);
        let mut args = vec!["send-keys".into(), "-t".into(), pane.into(), quoted];
        if enter {
            args.push("Enter".into());
        }
        let _ = self.run(args).await?;
        Ok(())
    }

    /// Send raw tmux key sequences (e.g. `Enter`, `C-l`, `Escape`, `Tab`).
    /// `keys` is passed to `tmux send-keys` unquoted; tmux parses each
    /// argument as a key name.
    pub async fn send_keys(&self, pane: &str, keys: &[&str]) -> Result<(), TmuxError> {
        let mut args: Vec<String> = vec!["send-keys".into(), "-t".into(), pane.into()];
        args.extend(keys.iter().map(|k| (*k).to_string()));
        let _ = self.run(args).await?;
        Ok(())
    }

    /// `tmux capture-pane -p -t PANE [-S start -E end]`.
    /// Returns the captured text (lines joined by `\n`).
    pub async fn capture_pane(
        &self,
        pane: &str,
        start: Option<i32>,
        end: Option<i32>,
    ) -> Result<String, TmuxError> {
        let mut args = vec!["capture-pane".into(), "-p".into(), "-t".into(), pane.into()];
        if let Some(s) = start {
            args.push("-S".into());
            args.push(s.to_string());
        }
        if let Some(e) = end {
            args.push("-E".into());
            args.push(e.to_string());
        }
        self.run(args).await
    }

    /// `tmux list-panes -F '#{pane_id}'` (window scope).
    pub async fn list_pane_ids(&self) -> Result<Vec<String>, TmuxError> {
        let out = self
            .run(vec!["list-panes".into(), "-F".into(), "#{pane_id}".into()])
            .await?;
        Ok(out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect())
    }

    /// `tmux list-panes -F '#{pane_id}'` scoped to a session.
    pub async fn list_pane_ids_in_session(&self, session: &str) -> Result<Vec<String>, TmuxError> {
        let out = self
            .run(vec![
                "list-panes".into(),
                "-t".into(),
                session.into(),
                "-F".into(),
                "#{pane_id}".into(),
            ])
            .await?;
        Ok(out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect())
    }

    /// `tmux kill-pane -t PANE`. Missing pane is not an error — tmux already
    /// thinks of it as gone.
    pub async fn kill_pane(&self, pane: &str) -> Result<(), TmuxError> {
        match self
            .run(vec!["kill-pane".into(), "-t".into(), pane.into()])
            .await
        {
            Ok(_) => Ok(()),
            Err(TmuxError::NonZero { stderr, .. })
                if stderr.contains("can't find pane") || stderr.contains("no such pane") =>
            {
                Ok(())
            }
            Err(other) => Err(other),
        }
    }

    /// True if the pane id is present in `list-panes`.
    pub async fn pane_exists(&self, pane: &str) -> Result<bool, TmuxError> {
        Ok(self.list_pane_ids().await?.iter().any(|p| p == pane))
    }

    /// Deliver `text` to a TUI pane as a single paste, optionally followed by
    /// `Enter`. Implemented as `load-buffer -b <unique> -` (stdin) +
    /// `paste-buffer -d -b <unique> -t PANE` + `send-keys Enter` so the
    /// running TUI receives the whole payload in one shot (which most input
    /// handlers, including Claude Code and Codex, treat as a paste and
    /// then a submit) rather than as a stream of individual keystrokes that
    /// races with the input handler and risks losing the Enter.
    ///
    /// This is the **only** correct path for nudging an interactive `claude`
    /// or `codex` pane. [`send_shell`] is reserved for plain shell panes
    /// (tests, ad-hoc bash use).
    pub async fn send_text(&self, pane: &str, text: &str, enter: bool) -> Result<(), TmuxError> {
        // Unique buffer name so concurrent agents don't collide. Buffer is
        // auto-deleted by `paste-buffer -d`.
        let buf = format!(
            "caucus-{:016x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64 ^ std::process::id() as u64)
                .unwrap_or(0)
        );
        self.run_with_stdin(
            vec!["load-buffer".into(), "-b".into(), buf.clone(), "-".into()],
            text.as_bytes(),
        )
        .await?;
        self.run(vec![
            "paste-buffer".into(),
            "-d".into(),
            "-b".into(),
            buf,
            "-t".into(),
            pane.into(),
        ])
        .await?;
        if enter {
            self.run(vec![
                "send-keys".into(),
                "-t".into(),
                pane.into(),
                "Enter".into(),
            ])
            .await?;
        }
        Ok(())
    }

    /// Same as [`run`] but pipes `stdin` into the spawned tmux command.
    async fn run_with_stdin(&self, args: Vec<String>, stdin: &[u8]) -> Result<String, TmuxError> {
        use tokio::io::AsyncWriteExt as _;
        let label = format!("tmux {}", args.join(" "));
        let mut child = Command::new(&self.config.binary)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| TmuxError::Spawn {
                command: label.clone(),
                source,
            })?;
        if let Some(mut child_stdin) = child.stdin.take() {
            child_stdin
                .write_all(stdin)
                .await
                .map_err(|source| TmuxError::Spawn {
                    command: label.clone(),
                    source,
                })?;
            // Drop closes stdin → tmux sees EOF.
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|source| TmuxError::Spawn {
                command: label.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(TmuxError::NonZero {
                command: label,
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if stdout.ends_with('\n') {
            stdout.pop();
            if stdout.ends_with('\r') {
                stdout.pop();
            }
        }
        Ok(stdout)
    }

    /// Lowest-level invocation. Returns stdout (trimmed of a single trailing newline).
    async fn run(&self, args: Vec<String>) -> Result<String, TmuxError> {
        let mut cmd = Command::new(&self.config.binary);
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let label = format!("tmux {}", args.join(" "));
        let output = cmd.output().await.map_err(|source| TmuxError::Spawn {
            command: label.clone(),
            source,
        })?;
        if !output.status.success() {
            return Err(TmuxError::NonZero {
                command: label,
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if stdout.ends_with('\n') {
            stdout.pop();
            if stdout.ends_with('\r') {
                stdout.pop();
            }
        }
        Ok(stdout)
    }
}

impl Default for TmuxService {
    fn default() -> Self {
        Self::new()
    }
}

/// A `tmux` binary on PATH (or wherever `TmuxConfig::binary` points). Detect
/// it before issuing real calls; cheap.
pub async fn detect_tmux(svc: &TmuxService) -> Result<String, TmuxError> {
    svc.run(vec!["-V".into()]).await
}

/// Helper: pane id valid as a tmux `-t` argument? Returns false for blatant
/// shell-injection attempts. (Not a security boundary — tmux itself rejects
/// nonsense — just a sanity check before we shell out.)
pub fn is_plausible_pane_id(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return false;
    }
    // tmux pane ids look like %38, %123. Window pane references can be
    // session:window.pane too; we accept :/. and digits/letters/_.
    bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'%' | b':' | b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plausible_pane_ids() {
        assert!(is_plausible_pane_id("%0"));
        assert!(is_plausible_pane_id("%42"));
        assert!(is_plausible_pane_id("caucus:0.0"));
        assert!(is_plausible_pane_id("session_1:1.2"));
    }

    #[test]
    fn implausible_pane_ids() {
        assert!(!is_plausible_pane_id(""));
        assert!(!is_plausible_pane_id("$(rm -rf /)"));
        assert!(!is_plausible_pane_id("a b"));
        assert!(!is_plausible_pane_id("`whoami`"));
    }

    /// Integration smoke: spawn a detached tmux session, split a pane,
    /// list it, kill the session. Skipped if tmux is missing.
    #[tokio::test]
    #[ignore = "requires tmux on PATH; run with `cargo test --ignored` after `tmux -V` works"]
    async fn end_to_end_session_lifecycle() -> Result<(), TmuxError> {
        let svc = TmuxService::new();
        let session = format!("caucus-test-{}", std::process::id());
        svc.new_session(&session).await?;
        let panes_before = svc.list_pane_ids_in_session(&session).await?;
        assert_eq!(panes_before.len(), 1);

        // Drive the new pane to print something predictable.
        svc.send_shell(&panes_before[0], "echo caucus-marker", true)
            .await?;

        // Give tmux a beat to render.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let captured = svc.capture_pane(&panes_before[0], None, None).await?;
        assert!(captured.contains("caucus-marker"));

        svc.kill_session(&session).await?;
        Ok(())
    }

    /// Round-trip a multi-word, special-char-laden payload through
    /// `send_text` and confirm the pane shell received it as one paste +
    /// executed it (the body contains `&&` and a single quote, both of
    /// which are murderous for naive send-keys).
    #[tokio::test]
    #[ignore = "requires tmux on PATH"]
    async fn send_text_pastes_multi_line_payload_intact() -> Result<(), TmuxError> {
        let svc = TmuxService::new();
        let session = format!("caucus-paste-{}", std::process::id());
        svc.new_session(&session).await?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let panes = svc.list_pane_ids_in_session(&session).await?;
        let pane = panes[0].clone();

        let payload = "echo 'caucus-paste' && echo it-works";
        svc.send_text(&pane, payload, true).await?;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let captured = svc.capture_pane(&pane, None, None).await?;
        assert!(captured.contains("caucus-paste"), "captured:\n{captured}");
        assert!(captured.contains("it-works"), "captured:\n{captured}");
        svc.kill_session(&session).await?;
        Ok(())
    }
}
