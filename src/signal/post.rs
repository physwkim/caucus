//! `caucus signal post` client (`docs/design.md` §7.3).
//!
//! The Claude `Stop` hook script (`.caucus/bin/turn-signal`) `exec`s
//! `caucus signal post`. This module is that subcommand's body: connect to
//! the session's unix socket, read the hook payload from stdin, lift
//! `last_message` out of it, and write one JSON line — a [`TurnSignal`] —
//! to the socket. No files, no polling: the caucus process reads the line
//! live (Invariant I-6's socket).

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result};

use crate::session::id::{PanelId, SessionId};

use super::{TurnKind, TurnSignal};

/// Run `caucus signal post`.
///
/// Mirrors the CLI `caucus signal post --sock <s> --session <id> --panel <id>
/// --kind <k>` (`docs/design.md` §7.3 / §10). Reads the Claude hook payload as
/// JSON from `stdin`, builds a [`TurnSignal`], and writes it as one
/// newline-terminated JSON line to the socket at `sock_path`.
///
/// This is a short-lived client process invoked once per turn by the hook;
/// it uses blocking std sockets — no async runtime is needed.
pub(crate) fn run(
    sock_path: &Path,
    session_id: SessionId,
    panel_id: PanelId,
    kind: TurnKind,
) -> Result<()> {
    let mut payload_text = String::new();
    std::io::stdin()
        .read_to_string(&mut payload_text)
        .context("read Claude hook payload from stdin")?;

    // The hook payload is JSON; if stdin was empty or unparseable, fall back
    // to a null payload rather than failing the agent's turn.
    let raw_hook_payload: serde_json::Value =
        serde_json::from_str(payload_text.trim()).unwrap_or(serde_json::Value::Null);

    let last_message = extract_last_message(&raw_hook_payload);

    let signal = TurnSignal::now(session_id, panel_id, kind, last_message, raw_hook_payload);

    let mut line = serde_json::to_string(&signal).context("serialise turn signal")?;
    line.push('\n');

    let mut stream = UnixStream::connect(sock_path)
        .with_context(|| format!("connect to caucus socket {}", sock_path.display()))?;
    std::io::Write::write_all(&mut stream, line.as_bytes())
        .context("write turn signal to caucus socket")?;
    Ok(())
}

/// Lift the agent's final assistant message out of a Claude hook payload.
///
/// The design (§7.3) names the field `last_message`; Claude hook payloads
/// have used `last_assistant_message` historically, so we accept either.
/// Returns `None` when neither is present (the main worker falls back to reading
/// the panel's turn output — §8.5).
fn extract_last_message(payload: &serde_json::Value) -> Option<String> {
    for key in ["last_message", "last_assistant_message"] {
        if let Some(text) = payload.get(key).and_then(|v| v.as_str()) {
            return Some(text.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_last_message_reads_design_field() {
        let payload = serde_json::json!({ "last_message": "3 findings." });
        assert_eq!(
            extract_last_message(&payload).as_deref(),
            Some("3 findings.")
        );
    }

    #[test]
    fn extract_last_message_accepts_claude_field() {
        let payload = serde_json::json!({ "last_assistant_message": "done" });
        assert_eq!(extract_last_message(&payload).as_deref(), Some("done"));
    }

    #[test]
    fn extract_last_message_absent_is_none() {
        let payload = serde_json::json!({ "stop_hook_active": true });
        assert_eq!(extract_last_message(&payload), None);
    }

    /// `run` connects, posts, and the server's channel yields the signal.
    #[tokio::test]
    async fn post_run_delivers_signal_to_server() {
        use crate::signal::server::SignalServer;
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("caucus.sock");
        let mut server = SignalServer::bind(&sock).unwrap();

        let session_id = SessionId::new();
        let panel_id = PanelId::new();

        // `run` reads the hook payload from process stdin, which a test
        // cannot redirect cleanly; exercise the same path it does — connect,
        // build a signal, write one JSON line — with an explicit payload.
        let payload = serde_json::json!({ "last_message": "reviewer pass complete" });
        let last_message = extract_last_message(&payload);
        let signal = TurnSignal::now(session_id, panel_id, TurnKind::Stop, last_message, payload);
        let mut line = serde_json::to_string(&signal).unwrap();
        line.push('\n');

        let sock_for_client = sock.clone();
        tokio::task::spawn_blocking(move || {
            let mut stream = std::os::unix::net::UnixStream::connect(&sock_for_client).unwrap();
            stream.write_all(line.as_bytes()).unwrap();
        })
        .await
        .unwrap();

        let sig = server.signals().recv().await.expect("signal received");
        assert_eq!(sig.session_id, session_id);
        assert_eq!(sig.panel_id, panel_id);
        assert_eq!(sig.kind, TurnKind::Stop);
        assert_eq!(sig.last_message.as_deref(), Some("reviewer pass complete"));
    }
}
