//! `[settings]` — process-wide tunables (`docs/design.md` §6).
//!
//! Loaded from `settings.toml` with the same layering as roles:
//! built-in defaults < `~/.caucus/settings.toml` < `<repo>/.caucus/settings.toml`.
//! Each key is optional, so an unset key falls through to the layer below and
//! finally to the compiled defaults ([`Settings::default`]) — the same
//! constants the code used when these values were hardcoded. The resolved
//! [`Settings`] is copied onto [`crate::config::Config`] and read by the owners
//! of the values that used to be constants: `Grid` scrollback depth,
//! `OutputCapture` caps, and the round fallback deadline.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Default round safety-net deadline (seconds). A round is force-delivered when
/// every panel settles or this deadline passes (`docs/design.md` §4).
pub(crate) const ROUND_FALLBACK_DEFAULT_SECS: u64 = 600;
/// Hard cap on the round fallback deadline — bounds a misconfigured
/// `round_fallback_secs` setting and any per-`register_round` override alike.
pub(crate) const ROUND_FALLBACK_MAX_SECS: u64 = 3600;

/// Resolved, process-wide tunables. Built once at [`crate::config::Config`]
/// load (built-in defaults < global < project) and read where the values were
/// previously hardcoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Per-panel terminal scrollback depth, in rows.
    pub scrollback_lines: usize,
    /// Default round fallback deadline, in seconds (resolved into
    /// `[1, ROUND_FALLBACK_MAX_SECS]`).
    pub round_fallback_secs: u64,
    /// Closed turns kept in memory per panel before older turns spill to the
    /// panel log.
    pub capture_turn_limit: usize,
    /// In-memory byte cap for a single open (in-progress) turn.
    pub capture_open_turn_bytes: usize,
}

impl Default for Settings {
    /// The compiled defaults — the canonical constants the values had when they
    /// were hardcoded, kept with their owners (`Grid` / `OutputCapture`) so this
    /// module does not fork the magic numbers.
    fn default() -> Self {
        Self {
            scrollback_lines: crate::term::Grid::DEFAULT_SCROLLBACK,
            round_fallback_secs: ROUND_FALLBACK_DEFAULT_SECS,
            capture_turn_limit: crate::term::OutputCapture::DEFAULT_TURN_LIMIT,
            capture_open_turn_bytes: crate::term::OutputCapture::DEFAULT_OPEN_TURN_BYTES,
        }
    }
}

/// Errors from loading `settings.toml`.
#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("settings io ({path}): {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("settings toml ({path}): {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

/// On-disk `settings.toml`: a single `[settings]` table of optional overrides.
/// An unknown top-level table is ignored (forward-compatible), but an unknown
/// key *inside* `[settings]` is rejected so a typo surfaces instead of silently
/// doing nothing.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SettingsFile {
    settings: SettingsOverrides,
}

/// The `[settings]` table — every key optional so it can layer.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SettingsOverrides {
    scrollback_lines: Option<usize>,
    round_fallback_secs: Option<u64>,
    capture_turn_limit: Option<usize>,
    capture_open_turn_bytes: Option<usize>,
}

impl SettingsOverrides {
    /// Read one `settings.toml`. A missing file yields empty overrides so the
    /// global+project merge short-circuits to the layer below.
    fn load(path: &Path) -> Result<Self, SettingsError> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let file: SettingsFile =
                    toml::from_str(&text).map_err(|source| SettingsError::Toml {
                        path: path.to_owned(),
                        source,
                    })?;
                Ok(file.settings)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(SettingsError::Io {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Override `self`'s set keys with `other`'s set keys (project beats global);
    /// a key unset in `other` keeps `self`'s value.
    fn override_with(self, other: Self) -> Self {
        Self {
            scrollback_lines: other.scrollback_lines.or(self.scrollback_lines),
            round_fallback_secs: other.round_fallback_secs.or(self.round_fallback_secs),
            capture_turn_limit: other.capture_turn_limit.or(self.capture_turn_limit),
            capture_open_turn_bytes: other
                .capture_open_turn_bytes
                .or(self.capture_open_turn_bytes),
        }
    }

    /// Resolve against the compiled defaults, clamping to safe ranges: the round
    /// fallback into `[1, ROUND_FALLBACK_MAX_SECS]`, the capture caps to a floor
    /// of 1 (a 0 limit would evict/spill every turn).
    fn resolve(self) -> Settings {
        let d = Settings::default();
        Settings {
            scrollback_lines: self.scrollback_lines.unwrap_or(d.scrollback_lines),
            round_fallback_secs: self
                .round_fallback_secs
                .unwrap_or(d.round_fallback_secs)
                .clamp(1, ROUND_FALLBACK_MAX_SECS),
            capture_turn_limit: self
                .capture_turn_limit
                .unwrap_or(d.capture_turn_limit)
                .max(1),
            capture_open_turn_bytes: self
                .capture_open_turn_bytes
                .unwrap_or(d.capture_open_turn_bytes)
                .max(1),
        }
    }
}

/// Load and merge `settings.toml`: built-in defaults < `~/.caucus/settings.toml`
/// (when `global_dir` is set) < `<project_dir>/settings.toml`.
pub fn load(global_dir: Option<&Path>, project_dir: &Path) -> Result<Settings, SettingsError> {
    let mut overrides = SettingsOverrides::default();
    if let Some(dir) = global_dir {
        overrides = overrides.override_with(SettingsOverrides::load(&dir.join("settings.toml"))?);
    }
    overrides =
        overrides.override_with(SettingsOverrides::load(&project_dir.join("settings.toml"))?);
    Ok(overrides.resolve())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_files_resolve_to_compiled_defaults() {
        let tmp = TempDir::new().unwrap();
        let settings = load(None, tmp.path()).unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn project_settings_override_global_per_key() {
        let global = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        // Global sets two keys; project overrides one and adds another. The
        // unset keys fall through to the compiled defaults.
        std::fs::write(
            global.path().join("settings.toml"),
            "[settings]\nscrollback_lines = 500\nround_fallback_secs = 120\n",
        )
        .unwrap();
        std::fs::write(
            project.path().join("settings.toml"),
            "[settings]\nscrollback_lines = 999\ncapture_turn_limit = 8\n",
        )
        .unwrap();

        let settings = load(Some(global.path()), project.path()).unwrap();
        assert_eq!(settings.scrollback_lines, 999, "project overrides global");
        assert_eq!(settings.round_fallback_secs, 120, "global key survives");
        assert_eq!(settings.capture_turn_limit, 8, "project-only key applied");
        assert_eq!(
            settings.capture_open_turn_bytes,
            Settings::default().capture_open_turn_bytes,
            "unset key keeps the compiled default"
        );
    }

    #[test]
    fn round_fallback_is_clamped_to_the_cap() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("settings.toml"),
            format!(
                "[settings]\nround_fallback_secs = {}\n",
                ROUND_FALLBACK_MAX_SECS + 10_000
            ),
        )
        .unwrap();
        let settings = load(None, tmp.path()).unwrap();
        assert_eq!(settings.round_fallback_secs, ROUND_FALLBACK_MAX_SECS);

        // ...and a zero is lifted to the floor of 1.
        std::fs::write(
            tmp.path().join("settings.toml"),
            "[settings]\nround_fallback_secs = 0\n",
        )
        .unwrap();
        assert_eq!(load(None, tmp.path()).unwrap().round_fallback_secs, 1);
    }

    #[test]
    fn unknown_key_in_the_settings_table_is_rejected() {
        let tmp = TempDir::new().unwrap();
        // A typo'd key must error rather than silently do nothing.
        std::fs::write(
            tmp.path().join("settings.toml"),
            "[settings]\nscrollback_line = 500\n",
        )
        .unwrap();
        assert!(matches!(
            load(None, tmp.path()),
            Err(SettingsError::Toml { .. })
        ));
    }
}
