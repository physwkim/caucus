//! Assemble a `transcript.md` from the per-round agenda and response files.
//!
//! Pure I/O over the session-root directory. The scribe role can also be
//! delegated this work via its system prompt (see `roles/scribe.md`); this
//! module provides the deterministic baseline that the CEO can always fall
//! back to.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::lifecycle::RoundLayout;

#[derive(Debug, Error)]
pub enum TranscriptError {
    #[error("transcript io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Write a transcript at `<session_root>/transcript.md` covering rounds
/// `1..=last_round_number` for every role in `roles`.
pub fn assemble(
    session_root: &Path,
    last_round_number: u32,
    roles: &[String],
    topic: &str,
) -> Result<PathBuf, TranscriptError> {
    let mut out = String::new();
    let _ = writeln!(out, "# {topic}");
    let _ = writeln!(out);
    let _ = writeln!(out, "**Participants:** {}", roles.join(", "));
    let _ = writeln!(out);

    for round in 1..=last_round_number {
        let layout = RoundLayout::new(session_root.to_path_buf(), round);
        let _ = writeln!(out, "## Round {round}");
        let _ = writeln!(out);

        let agenda = layout.agenda_path();
        if agenda.exists() {
            let agenda_body =
                std::fs::read_to_string(&agenda).map_err(|source| TranscriptError::Io {
                    path: agenda.clone(),
                    source,
                })?;
            let _ = writeln!(out, "### Agenda");
            let _ = writeln!(out);
            out.push_str(agenda_body.trim_end());
            let _ = writeln!(out);
            let _ = writeln!(out);
        }

        for role in roles {
            let response = layout.response_path(role);
            if response.exists() {
                let body =
                    std::fs::read_to_string(&response).map_err(|source| TranscriptError::Io {
                        path: response.clone(),
                        source,
                    })?;
                let _ = writeln!(out, "### {role}");
                let _ = writeln!(out);
                out.push_str(body.trim_end());
                let _ = writeln!(out);
                let _ = writeln!(out);
            }
        }
    }

    // Decision file, if present.
    let decision = session_root.join("decision.md");
    if decision.exists() {
        let body = std::fs::read_to_string(&decision).map_err(|source| TranscriptError::Io {
            path: decision.clone(),
            source,
        })?;
        let _ = writeln!(out, "## Decision");
        let _ = writeln!(out);
        out.push_str(body.trim_end());
        let _ = writeln!(out);
    }

    let path = session_root.join("transcript.md");
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, &out).map_err(|source| TranscriptError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &path).map_err(|source| TranscriptError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn assembles_one_round_with_two_roles() {
        let tmp = TempDir::new().unwrap();
        let layout = RoundLayout::new(tmp.path().to_path_buf(), 1);
        std::fs::create_dir_all(layout.round_dir()).unwrap();
        std::fs::write(layout.agenda_path(), "Topic: refactor write_loop").unwrap();
        std::fs::write(layout.response_path("architect"), "I propose option B.").unwrap();
        std::fs::write(layout.response_path("reviewer"), "Approve with caveat X.").unwrap();

        let path = assemble(
            tmp.path(),
            1,
            &["architect".into(), "reviewer".into()],
            "write_loop refactor",
        )
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();

        assert!(body.starts_with("# write_loop refactor"));
        assert!(body.contains("**Participants:** architect, reviewer"));
        assert!(body.contains("## Round 1"));
        assert!(body.contains("### Agenda"));
        assert!(body.contains("Topic: refactor write_loop"));
        assert!(body.contains("### architect"));
        assert!(body.contains("I propose option B."));
        assert!(body.contains("### reviewer"));
        assert!(body.contains("Approve with caveat X."));
    }

    #[test]
    fn includes_decision_when_present() {
        let tmp = TempDir::new().unwrap();
        let layout = RoundLayout::new(tmp.path().to_path_buf(), 1);
        std::fs::create_dir_all(layout.round_dir()).unwrap();
        std::fs::write(layout.agenda_path(), "topic").unwrap();
        std::fs::write(tmp.path().join("decision.md"), "Option B locked.").unwrap();

        let path = assemble(tmp.path(), 1, &[], "demo").unwrap();
        let body = std::fs::read_to_string(path).unwrap();
        assert!(body.contains("## Decision"));
        assert!(body.contains("Option B locked."));
    }

    #[test]
    fn missing_response_is_skipped_without_error() {
        let tmp = TempDir::new().unwrap();
        let layout = RoundLayout::new(tmp.path().to_path_buf(), 1);
        std::fs::create_dir_all(layout.round_dir()).unwrap();
        std::fs::write(layout.agenda_path(), "topic").unwrap();
        // Only one of two roles wrote a response.
        std::fs::write(layout.response_path("architect"), "yes").unwrap();

        let path = assemble(
            tmp.path(),
            1,
            &["architect".into(), "reviewer".into()],
            "demo",
        )
        .unwrap();
        let body = std::fs::read_to_string(path).unwrap();
        assert!(body.contains("### architect"));
        assert!(!body.contains("### reviewer"));
    }
}
