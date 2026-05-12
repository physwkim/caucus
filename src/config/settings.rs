//! Repo-level (and global) `settings.toml`. Holds knobs that are not
//! per-role — currently the `teammate_mode` selector. Mirrors claw-code's
//! `teammateMode` config (see `docs/claw-code-analysis.md` §2).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::roles::RolesError;

/// Teammate execution mode. v0 implements `tmux` only; `in-process` and
/// `auto` are accepted by the parser but currently fall back to `tmux` with
/// a one-shot warning, documented in `docs/design.md` §13.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Serialize, Deserialize)]
pub enum TeammateMode {
    #[default]
    #[serde(rename = "tmux")]
    Tmux,
    #[serde(rename = "in-process")]
    InProcess,
    #[serde(rename = "auto")]
    Auto,
}

impl TeammateMode {
    /// Resolve to the mode actually used at spawn time. v0: always Tmux.
    pub fn resolve(self) -> Self {
        match self {
            Self::Tmux => Self::Tmux,
            Self::InProcess | Self::Auto => Self::Tmux,
        }
    }
}

/// Top-level settings record. Persisted at `<root>/.caucus/settings.toml`
/// (or `~/.caucus/settings.toml` for global) — both are optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub teammate_mode: TeammateMode,
}

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
    #[error(transparent)]
    Roles(#[from] RolesError),
}

impl Settings {
    /// Load `<root>/.caucus/settings.toml`. Missing file → defaults.
    pub fn load(path: &Path) -> Result<Self, SettingsError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|source| SettingsError::Toml {
                path: path.to_owned(),
                source,
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(SettingsError::Io {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Layered load: embedded default ← global ← project. Later overrides.
    pub fn layered(repo_root: &Path) -> Result<Self, SettingsError> {
        let mut current = Self::default();
        if let Some(home) = std::env::var_os("HOME") {
            let global = PathBuf::from(home).join(".caucus").join("settings.toml");
            current = current.override_with(Self::load(&global)?);
        }
        let project = repo_root.join(".caucus").join("settings.toml");
        Ok(current.override_with(Self::load(&project)?))
    }

    fn override_with(self, other: Self) -> Self {
        // Each field replaces, since Settings is small.
        Self {
            teammate_mode: other.teammate_mode,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_settings_yields_defaults() {
        let tmp = TempDir::new().unwrap();
        let s = Settings::load(&tmp.path().join("nope.toml")).unwrap();
        assert_eq!(s.teammate_mode, TeammateMode::Tmux);
    }

    #[test]
    fn parses_in_process_mode() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.toml");
        std::fs::write(&path, r#"teammate_mode = "in-process""#).unwrap();
        let s = Settings::load(&path).unwrap();
        assert_eq!(s.teammate_mode, TeammateMode::InProcess);
        // But resolve() still returns Tmux in v0.
        assert_eq!(s.teammate_mode.resolve(), TeammateMode::Tmux);
    }

    #[test]
    fn parses_auto_mode() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.toml");
        std::fs::write(&path, r#"teammate_mode = "auto""#).unwrap();
        let s = Settings::load(&path).unwrap();
        assert_eq!(s.teammate_mode, TeammateMode::Auto);
    }

    #[test]
    fn rejects_unknown_mode() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("s.toml");
        std::fs::write(&path, r#"teammate_mode = "supercluster""#).unwrap();
        let err = Settings::load(&path).unwrap_err();
        assert!(matches!(err, SettingsError::Toml { .. }));
    }
}
