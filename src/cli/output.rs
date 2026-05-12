//! Output formatting for the CLI. `--format json` writes a machine-readable
//! JSON object to stdout; `--format text` writes a short human-readable
//! summary. Errors go to stderr regardless.

use std::io::Write;

use clap::ValueEnum;
use serde::Serialize;

/// Selector exposed by every command via `--format`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, ValueEnum)]
pub enum OutputFormat {
    Json,
    #[default]
    Text,
}

/// Emit a JSON-serialisable value as the primary stdout payload. The text
/// fallback is supplied by the caller because the rendering varies per
/// command.
pub fn emit<T: Serialize>(format: OutputFormat, json_value: &T, text: impl FnOnce() -> String) {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    match format {
        OutputFormat::Json => {
            let _ = serde_json::to_writer_pretty(&mut handle, json_value);
            let _ = handle.write_all(b"\n");
        }
        OutputFormat::Text => {
            let _ = writeln!(handle, "{}", text());
        }
    }
}

/// Emit a status update to stderr — used for progress narration.
pub fn note(message: &str) {
    let _ = writeln!(std::io::stderr(), "{message}");
}
