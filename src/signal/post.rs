//! `caucus signal post` / `caucus signal codex-notify` clients
//! (`docs/design.md` §7.3).
//!
//! The Claude `Stop` hook script (`~/.claude/hooks/caucus-turn-signal`) `exec`s
//! `caucus signal post`. This module is that subcommand's body: connect to
//! the session's unix socket, read the hook payload from stdin, lift
//! `last_message` out of it, and write one JSON line — a [`TurnSignal`] —
//! to the socket. No files, no polling: the caucus process reads the line
//! live (Invariant I-6's socket).
//!
//! codex has no `Stop` hook; instead it invokes a `notify` program on
//! `agent-turn-complete`, passing the event JSON as an argument. `run_codex_notify`
//! is that program's body — it posts the *same* `Stop` [`TurnSignal`], so both
//! backends settle a panel through one turn-completion owner (`handle_signal`).

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::session::id::{PanelId, SessionId};

use super::{AgentNote, NoteKind, SignalReply, TurnKind, TurnSignal};

/// How long the client waits for the server's one reply line — the client
/// half of the reply-timeout ladder (`docs/design.md` §7.6): above the
/// server's own wait (`server::HOOK_REPLY_WAIT`, 1500ms), so on a slow tick
/// the *server* resolves first with an explicit allow line and this timeout
/// only fires when caucus is truly gone; far below Claude's hook timeout
/// (600s default).
const HOOK_REPLY_READ_TIMEOUT: Duration = Duration::from_millis(2500);

/// Longest reply line the client will buffer before giving up (fail-open to
/// allow). Reply lines are one small JSON object — a round summary teaser is
/// ~1 KiB per panel — so the cap only guards a corrupted or hostile server.
const MAX_REPLY_LINE_BYTES: usize = 256 * 1024;

/// Run `caucus signal post`.
///
/// Mirrors the CLI `caucus signal post --sock <s> --session <id> --panel <id>
/// --kind <k>` (`docs/design.md` §7.3 / §10). Reads the Claude hook payload as
/// JSON from `stdin`, builds a [`TurnSignal`], and writes it as one
/// newline-terminated JSON line to the socket at `sock_path`.
///
/// With `wants_reply` (the `CAUCUS_HOOK_REPLY=1` env caucus injects into the
/// claude main panel, §7.6) the signal asks the server for a directive and
/// one reply line is read back: a `deliver` reply prints Claude's Stop-hook
/// block JSON on stdout — Claude then continues the same turn with the reply's
/// `reason` (a due round's summary) as feedback. Every other outcome — allow,
/// timeout, EOF, an unparseable or unknown reply — prints nothing, and the
/// turn ends exactly as it does today (fail-open).
///
/// This is a short-lived client process invoked once per turn by the hook;
/// it uses blocking std sockets — no async runtime is needed.
pub(crate) fn run(
    sock_path: &Path,
    session_id: SessionId,
    panel_id: PanelId,
    kind: TurnKind,
    wants_reply: bool,
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

    let mut signal = TurnSignal::now(session_id, panel_id, kind, last_message, raw_hook_payload);
    signal.wants_reply = wants_reply;
    if !wants_reply {
        return send_line(sock_path, &signal);
    }
    let stream = send_line_on(sock_path, &signal)?;
    if let Some(reason) = read_deliver_reason(stream) {
        // Claude's Stop-hook block JSON: `reason` continues the same turn.
        println!(
            "{}",
            serde_json::json!({ "decision": "block", "reason": reason })
        );
    }
    Ok(())
}

/// Run `caucus signal note` — post one mid-turn [`AgentNote`].
///
/// Invoked by an agent from its own shell (the `--sock`/`--session`/`--panel`
/// flags default to the `CAUCUS_*` env caucus injects into every panel), so
/// unlike the hook clients above a human-readable error is worth surfacing:
/// the agent sees it and can tell the difference between a typo and a dead
/// session.
pub(crate) fn run_note(
    sock_path: &Path,
    session_id: SessionId,
    panel_id: PanelId,
    kind: NoteKind,
    body: String,
) -> Result<()> {
    let note = AgentNote::now(session_id, panel_id, kind, body);
    send_line(sock_path, &note)
}

/// Run `caucus signal codex-notify` — the codex counterpart of [`run`].
///
/// codex has no `Stop` hook; it invokes its `notify` program on
/// `agent-turn-complete`, appending the event JSON as a single argument (not
/// stdin). This parses that `payload`, and **only** for an
/// `agent-turn-complete` event posts the same `Stop` [`TurnSignal`] the claude
/// hook posts — so both backends land on one turn-completion owner. Any other
/// event type, or an absent/unparseable payload, is a silent no-op (it does
/// not even open the socket): codex fires `notify` for more than turn
/// completion, and only completion means the agent is idle again.
pub(crate) fn run_codex_notify(
    sock_path: &Path,
    session_id: SessionId,
    panel_id: PanelId,
    payload: Option<&str>,
) -> Result<()> {
    let raw: serde_json::Value = payload
        .and_then(|p| serde_json::from_str(p.trim()).ok())
        .unwrap_or(serde_json::Value::Null);

    if raw.get("type").and_then(serde_json::Value::as_str) != Some("agent-turn-complete") {
        return Ok(());
    }

    let last_message = extract_last_message(&raw);
    let signal = TurnSignal::now(session_id, panel_id, TurnKind::Stop, last_message, raw);
    send_line(sock_path, &signal)
}

/// Write one newline-terminated JSON line to the socket at `sock_path`. The
/// single socket-write shared by every post path — the claude Stop hook
/// ([`run`]), codex notify ([`run_codex_notify`]), and mid-turn notes
/// ([`run_note`]).
fn send_line<T: serde::Serialize>(sock_path: &Path, value: &T) -> Result<()> {
    send_line_on(sock_path, value).map(drop)
}

/// [`send_line`], returning the connected stream so a reply-reading caller
/// ([`run`] under `wants_reply`) can keep the connection open for the server's
/// reply line.
fn send_line_on<T: serde::Serialize>(sock_path: &Path, value: &T) -> Result<UnixStream> {
    let mut line = serde_json::to_string(value).context("serialise signal line")?;
    line.push('\n');

    let mut stream = UnixStream::connect(sock_path)
        .with_context(|| format!("connect to caucus socket {}", sock_path.display()))?;
    std::io::Write::write_all(&mut stream, line.as_bytes())
        .context("write signal line to caucus socket")?;
    Ok(stream)
}

/// Read the server's one reply line off `stream` and extract a deliver reason.
///
/// Fail-open by construction: returns `None` — meaning "allow, print nothing"
/// — on a read timeout, EOF before a full line, an oversized line, a parse
/// failure, an unknown `action`, or an explicit allow. The hook must never
/// wedge or fail the main worker's turn because the caucus that answers is
/// old, gone, or slow; the only path that changes anything is a well-formed
/// deliver reply.
fn read_deliver_reason(stream: UnixStream) -> Option<String> {
    stream
        .set_read_timeout(Some(HOOK_REPLY_READ_TIMEOUT))
        .ok()?;
    let mut stream = stream;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.contains(&b'\n') {
        if buf.len() > MAX_REPLY_LINE_BYTES {
            return None;
        }
        match stream.read(&mut chunk) {
            // EOF: judge whatever arrived (a reply without a trailing newline).
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            // Timeout or transport error.
            Err(_) => return None,
        }
    }
    let line = buf.split(|&b| b == b'\n').next()?;
    match serde_json::from_slice(line) {
        Ok(SignalReply::Deliver { reason }) => Some(reason),
        // Allow, an unknown action, or garbage: nothing to inject.
        _ => None,
    }
}

/// Lift the agent's final assistant message out of a Claude hook payload.
///
/// The design (§7.3) names the field `last_message`; Claude hook payloads
/// have used `last_assistant_message` historically, and codex's notify JSON
/// uses the kebab-case `last-assistant-message`, so we accept any of them.
/// Returns `None` when none is present (the main worker falls back to reading
/// the panel's turn output — §8.5).
fn extract_last_message(payload: &serde_json::Value) -> Option<String> {
    for key in [
        "last_message",
        "last_assistant_message",
        "last-assistant-message",
    ] {
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
    fn extract_last_message_accepts_codex_kebab_field() {
        // codex's notify JSON uses the kebab-case key.
        let payload = serde_json::json!({ "last-assistant-message": "hi" });
        assert_eq!(extract_last_message(&payload).as_deref(), Some("hi"));
    }

    #[test]
    fn extract_last_message_absent_is_none() {
        let payload = serde_json::json!({ "stop_hook_active": true });
        assert_eq!(extract_last_message(&payload), None);
    }

    /// A non-`agent-turn-complete` codex event is a silent no-op: it must not
    /// even open the socket (so a missing socket is no error), since codex fires
    /// `notify` for events other than turn completion.
    #[test]
    fn codex_notify_ignores_non_turn_complete_events() {
        let res = run_codex_notify(
            Path::new("/nonexistent/caucus.sock"),
            SessionId::new(),
            PanelId::new(),
            Some(r#"{"type":"agent-turn-failed"}"#),
        );
        assert!(res.is_ok(), "non-completion notify must be a silent no-op");
    }

    /// An absent or unparseable payload is likewise a no-op rather than an error
    /// — the notify process never fails a codex turn.
    #[test]
    fn codex_notify_absent_or_unparseable_payload_is_a_no_op() {
        assert!(
            run_codex_notify(
                Path::new("/nonexistent/caucus.sock"),
                SessionId::new(),
                PanelId::new(),
                None,
            )
            .is_ok()
        );
        assert!(
            run_codex_notify(
                Path::new("/nonexistent/caucus.sock"),
                SessionId::new(),
                PanelId::new(),
                Some("not json"),
            )
            .is_ok()
        );
    }

    /// `run_codex_notify` on an `agent-turn-complete` event posts a `Stop`
    /// signal carrying the codex `last-assistant-message` — the same shape the
    /// claude Stop hook delivers, so `handle_signal` settles the panel.
    #[tokio::test]
    async fn codex_notify_posts_stop_with_last_message_on_turn_complete() {
        use crate::signal::server::SignalServer;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("caucus.sock");
        let mut server = SignalServer::bind(&sock).unwrap();

        let session_id = SessionId::new();
        let panel_id = PanelId::new();
        let payload =
            r#"{"type":"agent-turn-complete","last-assistant-message":"3 findings, all fixed"}"#;

        let sock_for_client = sock.clone();
        tokio::task::spawn_blocking(move || {
            run_codex_notify(&sock_for_client, session_id, panel_id, Some(payload)).unwrap();
        })
        .await
        .unwrap();

        let (event, reply) = server.signals().recv().await.expect("signal received");
        let crate::signal::SignalEvent::Turn(sig) = event else {
            panic!("expected a turn signal");
        };
        assert!(reply.is_none(), "codex notify never asks for a reply");
        assert_eq!(sig.session_id, session_id);
        assert_eq!(sig.panel_id, panel_id);
        assert_eq!(sig.kind, TurnKind::Stop);
        assert_eq!(sig.last_message.as_deref(), Some("3 findings, all fixed"));
    }

    /// `run_note` connects, posts, and the server's channel yields the note —
    /// the sub-agent backchannel end to end.
    #[tokio::test]
    async fn run_note_delivers_note_to_server() {
        use crate::signal::server::SignalServer;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("caucus.sock");
        let mut server = SignalServer::bind(&sock).unwrap();

        let session_id = SessionId::new();
        let panel_id = PanelId::new();

        let sock_for_client = sock.clone();
        tokio::task::spawn_blocking(move || {
            run_note(
                &sock_for_client,
                session_id,
                panel_id,
                NoteKind::Artifact,
                "wrote design draft to doc/draft.md".to_string(),
            )
            .unwrap();
        })
        .await
        .unwrap();

        let (event, reply) = server.signals().recv().await.expect("note received");
        let crate::signal::SignalEvent::Note(note) = event else {
            panic!("expected a note");
        };
        assert!(reply.is_none(), "a note never asks for a reply");
        assert_eq!(note.session_id, session_id);
        assert_eq!(note.panel_id, panel_id);
        assert_eq!(note.kind, NoteKind::Artifact);
        assert_eq!(note.body, "wrote design draft to doc/draft.md");
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

        let (event, _) = server.signals().recv().await.expect("signal received");
        let crate::signal::SignalEvent::Turn(sig) = event else {
            panic!("expected a turn signal");
        };
        assert_eq!(sig.session_id, session_id);
        assert_eq!(sig.panel_id, panel_id);
        assert_eq!(sig.kind, TurnKind::Stop);
        assert_eq!(sig.last_message.as_deref(), Some("reviewer pass complete"));
    }

    /// Write `reply` to one end of a socket pair, close it, and run
    /// [`read_deliver_reason`] on the other end.
    fn deliver_reason_for(reply: &[u8]) -> Option<String> {
        use std::io::Write as _;
        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.write_all(reply).unwrap();
        drop(writer);
        read_deliver_reason(reader)
    }

    /// The client fail-open matrix (`docs/design.md` §7.6): only a well-formed
    /// deliver reply injects anything; allow, garbage, and an unknown action
    /// all print nothing. The deliver case has no trailing newline — a reply
    /// finished by EOF is still judged.
    #[test]
    fn read_deliver_reason_fail_open_matrix() {
        assert_eq!(
            deliver_reason_for(br#"{"action":"deliver","reason":"round done"}"#).as_deref(),
            Some("round done"),
            "a deliver reply carries its reason"
        );
        assert_eq!(
            deliver_reason_for(b"{\"action\":\"allow\"}\n"),
            None,
            "an explicit allow injects nothing"
        );
        assert_eq!(
            deliver_reason_for(b"not json\n"),
            None,
            "garbage injects nothing"
        );
        assert_eq!(
            deliver_reason_for(b"{\"action\":\"reticulate\"}\n"),
            None,
            "an unknown action injects nothing"
        );
        assert_eq!(
            deliver_reason_for(b""),
            None,
            "EOF with no reply injects nothing"
        );
    }

    /// A server that never answers is cut off by the client's read timeout —
    /// the hook exits open rather than wedging the main worker's turn. Takes
    /// [`HOOK_REPLY_READ_TIMEOUT`] (2.5s) of real time by design.
    #[test]
    fn read_deliver_reason_times_out_to_allow() {
        let (writer, reader) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        assert_eq!(read_deliver_reason(reader), None);
        assert!(
            started.elapsed() >= HOOK_REPLY_READ_TIMEOUT,
            "the read must wait out the full timeout before failing open"
        );
        drop(writer);
    }
}
