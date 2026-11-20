//! Wire protocol between a main caucus (the client) and a sub-host's
//! `caucus host serve` daemon: one JSON object per line, tagged by `t`.
//!
//! The daemon numbers every byte a panel's PTY ever printed. An [`Output`]
//! frame carries its starting offset, and a client that lost its link asks
//! for the stream again from the first byte it has not seen
//! ([`ClientFrame::Attach`]), so a reconnect replays exactly the missing
//! bytes into the panel's grid.
//!
//! [`Output`]: DaemonFrame::Output

use std::collections::BTreeMap;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::load::HostLoad;
use crate::pty::PtyCommand;
use crate::session::id::{PanelId, SessionId};

/// Bumped on any incompatible frame change; a daemon refuses a client
/// speaking another version instead of misreading its frames.
pub const PROTO_VERSION: u32 = 1;

/// Longest line either side reads. Input frames carry whole pastes and spawn
/// frames carry the role prompt, so this sits well above the signal socket's
/// 1 MiB line cap; it still bounds a peer that never sends a newline.
pub(crate) const MAX_FRAME_BYTES: u64 = 16 << 20;

/// Most raw PTY bytes the daemon packs into one output frame.
pub(crate) const OUTPUT_CHUNK: usize = 32 * 1024;

/// Raw bytes, base64 on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bytes(pub Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD
            .decode(text.as_bytes())
            .map(Bytes)
            .map_err(serde::de::Error::custom)
    }
}

/// What a panel runs on the sub-host. Paths and arguments are UTF-8 on the
/// wire; a command that is not valid UTF-8 cannot be sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
    /// Pre-grant codex's directory trust for `cwd` in the host's codex config
    /// (`agent::codex_trust`); main cannot write that file.
    #[serde(default)]
    pub trust_codex_cwd: bool,
}

impl SpawnSpec {
    pub fn from_command(command: &PtyCommand, cols: u16, rows: u16) -> Result<Self, String> {
        let utf8 = |s: &std::ffi::OsStr| {
            s.to_str()
                .map(str::to_string)
                .ok_or_else(|| format!("not valid UTF-8: {}", s.to_string_lossy()))
        };
        Ok(Self {
            program: utf8(&command.program)?,
            args: command
                .args
                .iter()
                .map(|a| utf8(a))
                .collect::<Result<_, _>>()?,
            cwd: command
                .cwd
                .as_ref()
                .map(|c| utf8(c.as_os_str()))
                .transpose()?,
            env: command
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            cols,
            rows,
            trust_codex_cwd: false,
        })
    }

    pub fn to_command(&self) -> PtyCommand {
        PtyCommand {
            program: self.program.clone().into(),
            args: self.args.iter().map(Into::into).collect(),
            cwd: self.cwd.as_ref().map(PathBuf::from),
            env: self
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }
}

/// Main caucus → daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientFrame {
    /// First frame on every connection. Binds the connection to `session`; a
    /// newer connection for the same session replaces an older one, and every
    /// panel of the session stops streaming until the client attaches it.
    Hello {
        proto: u32,
        session: SessionId,
    },
    /// Start `spec` as `panel`. Idempotent per panel id: a client that resends
    /// an unconfirmed spawn after a reconnect gets the existing panel.
    Spawn {
        panel: PanelId,
        spec: SpawnSpec,
    },
    Input {
        panel: PanelId,
        data: Bytes,
    },
    Resize {
        panel: PanelId,
        cols: u16,
        rows: u16,
    },
    /// Stream `panel`'s output starting at byte `from`.
    Attach {
        panel: PanelId,
        from: u64,
    },
    /// Kill the panel's process and forget it.
    Kill {
        panel: PanelId,
    },
    /// Check out the existing `branch` at `path` as a new worktree of the
    /// repo at `repo` (a leading `~` is the host user's home). Answered by
    /// [`DaemonFrame::WorktreeDone`] with the worktree's absolute path. A
    /// request re-sent after a reconnect finds the worktree it made.
    AddWorktree {
        req: u64,
        repo: String,
        path: String,
        branch: String,
    },
    /// Remove the worktree at `path`, committing its uncommitted work onto its
    /// branch first, then delete `delete_branch` if given. Runs after every
    /// kill sent before it, so no panel still writes there. A path already
    /// gone counts as removed.
    RemoveWorktree {
        req: u64,
        repo: String,
        path: String,
        delete_branch: Option<String>,
    },
}

/// What the daemon tells a client about itself, in reply to `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub proto: u32,
    pub version: String,
    pub hostname: String,
    /// The daemon's own executable, which a panel on this host runs for its
    /// turn-signal hooks and codex `notify`.
    pub caucus_bin: PathBuf,
    /// The daemon's turn-signal socket — `CAUCUS_SOCK` for panels here.
    pub signal_sock: PathBuf,
    /// This session's shared directory on the sub-host — `CAUCUS_SESSION_DIR`
    /// for panels here. Not shared with the main host or other sub-hosts.
    pub session_dir: PathBuf,
    /// This session's panels still running here, which a resumed main
    /// re-adopts instead of starting again.
    pub panels: Vec<PanelId>,
}

/// Daemon → main caucus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum DaemonFrame {
    Welcome(HostInfo),
    /// The host's load, sent after the welcome and then periodically.
    Load(HostLoad),
    /// The daemon now owns `panel` (a failed launch included: its error is in
    /// the output and an `exited` follows).
    Spawned {
        panel: PanelId,
    },
    /// `data` is the panel's output starting at byte `offset`. An offset past
    /// the client's next expected byte means the daemon no longer holds the
    /// bytes in between.
    Output {
        panel: PanelId,
        offset: u64,
        data: Bytes,
    },
    /// The panel's process is gone. Sent once all of its output was sent.
    Exited {
        panel: PanelId,
        reason: Option<String>,
    },
    /// A line a panel on this host posted to the daemon's turn-signal socket,
    /// re-serialised as the signal server parsed it.
    Signal {
        line: String,
    },
    /// The answer to worktree request `req`: the worktree's absolute path on
    /// the host, or why the request failed.
    WorktreeDone {
        req: u64,
        result: Result<String, String>,
    },
    Error {
        msg: String,
    },
}

/// One frame as a newline-terminated line.
pub(crate) fn encode<T: Serialize>(frame: &T) -> String {
    let mut line = serde_json::to_string(frame).expect("host frames serialise");
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_bytes_round_trip_as_base64() {
        let panel = PanelId::new();
        let frame = DaemonFrame::Output {
            panel,
            offset: 7,
            data: Bytes(vec![0, 0x1b, b'[', 0xff, b'\n']),
        };
        let line = encode(&frame);
        assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
        assert!(line.contains("\"t\":\"output\""));
        let back: DaemonFrame = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn spawn_spec_round_trips_a_command() {
        let mut command = PtyCommand::new("claude");
        command.args = vec!["--model".into(), "opus".into()];
        command.cwd = Some(PathBuf::from("/work/repo"));
        command
            .env
            .insert("CAUCUS_SOCK".into(), "/tmp/s.sock".into());
        let spec = SpawnSpec::from_command(&command, 100, 30).unwrap();
        let back = spec.to_command();
        assert_eq!(back.program, command.program);
        assert_eq!(back.args, command.args);
        assert_eq!(back.cwd, command.cwd);
        assert_eq!(back.env, command.env);
    }

    #[test]
    fn spawn_spec_rejects_a_non_utf8_argument() {
        use std::os::unix::ffi::OsStringExt;
        let mut command = PtyCommand::new("sh");
        command.args = vec![std::ffi::OsString::from_vec(vec![0xff, 0xfe])];
        assert!(SpawnSpec::from_command(&command, 80, 24).is_err());
    }
}
