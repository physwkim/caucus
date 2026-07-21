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

use crate::render::LayoutMode;

/// Default round safety-net deadline (seconds). A round is force-delivered when
/// every panel settles or this deadline passes (`docs/design.md` §4).
pub(crate) const ROUND_FALLBACK_DEFAULT_SECS: u64 = 600;
/// Hard cap on the round fallback deadline — bounds a misconfigured
/// `round_fallback_secs` setting and any per-`register_round` override alike.
pub(crate) const ROUND_FALLBACK_MAX_SECS: u64 = 3600;

/// Hard ceiling on `scrollback_lines` — bounds per-panel scrollback memory
/// against an absurd or typo'd setting. The ring fills lazily, so this caps
/// worst-case retained rows, not an upfront allocation. There is deliberately
/// no floor: `0` is a valid value meaning *scrollback disabled* (`Grid` treats
/// a 0 limit as "retain nothing"), unlike the capture caps where a 0 is
/// degenerate and lifted to 1.
pub(crate) const SCROLLBACK_MAX_LINES: usize = 1_000_000;

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
    /// Whether caucus captures the mouse (`docs/design.md` §1). On, a scroll
    /// wheel notch reaches caucus as a `PageUp`/`PageDown` keypress; off, the
    /// terminal keeps its native mouse behaviour (drag-to-select / copy).
    /// Default off — native selection/copy works out of the box; set
    /// `mouse = true` to capture the wheel for scrollback (the pager still
    /// scrolls with `PageUp`/`PageDown` regardless).
    pub mouse: bool,
    /// The reserved prefix letter (`prefix = "b"` → `Ctrl-B`), or `None` when
    /// unset. Sits between the CLI and the compiled default in the prefix
    /// resolution chain (`--prefix`/`CAUCUS_PREFIX` > this > default `Ctrl-A`
    /// with tmux auto-dodge — see [`crate::input::effective_prefix`]).
    pub prefix: Option<char>,
    /// The fixed panel arrangement a fresh session tiles into
    /// ([`crate::render::LayoutMode`]). Runtime rearrangement was removed, so
    /// this is the sole way to select a non-`Tiled` arrangement; a resumed
    /// session restores its persisted mode instead of this default. Default
    /// `Tiled`.
    pub layout: LayoutMode,
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
            mouse: false,
            prefix: None,
            layout: LayoutMode::Tiled,
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
    mouse: Option<bool>,
    prefix: Option<PrefixOverride>,
    layout: Option<LayoutMode>,
}

/// The `prefix` settings value, validated at parse time through the same
/// grammar as `--prefix` ([`crate::cli::PrefixKey`]: a bare letter or a
/// `ctrl-`/`c-`/`^` form) so the two spellings of the one knob cannot drift.
/// A bad value is a load error, matching the `deny_unknown_fields` posture of
/// surfacing mistakes instead of silently doing nothing.
#[derive(Debug, Clone, Copy)]
struct PrefixOverride(char);

impl<'de> serde::Deserialize<'de> for PrefixOverride {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let key: crate::cli::PrefixKey = raw.parse().map_err(serde::de::Error::custom)?;
        Ok(Self(key.0))
    }
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
            mouse: other.mouse.or(self.mouse),
            prefix: other.prefix.or(self.prefix),
            layout: other.layout.or(self.layout),
        }
    }

    /// Resolve against the compiled defaults, clamping to safe ranges: the round
    /// fallback into `[1, ROUND_FALLBACK_MAX_SECS]`, the capture caps to a floor
    /// of 1 (a 0 limit would evict/spill every turn), and the scrollback depth to
    /// a ceiling of [`SCROLLBACK_MAX_LINES`] (0 stays valid — disabled scrollback
    /// — so it takes no floor).
    fn resolve(self) -> Settings {
        let d = Settings::default();
        Settings {
            scrollback_lines: self
                .scrollback_lines
                .unwrap_or(d.scrollback_lines)
                .min(SCROLLBACK_MAX_LINES),
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
            mouse: self.mouse.unwrap_or(d.mouse),
            // Already validated at parse time; `None` stays `None` — the
            // default-with-tmux-dodge is applied by `input::effective_prefix`,
            // not here, so an unset key is distinguishable from a chosen one.
            prefix: self.prefix.map(|p| p.0),
            layout: self.layout.unwrap_or(d.layout),
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
    fn mouse_defaults_off_and_can_be_enabled() {
        // Default is off — the terminal keeps native drag-to-select/copy.
        assert!(!Settings::default().mouse);
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("settings.toml"),
            "[settings]\nmouse = true\n",
        )
        .unwrap();
        assert!(
            load(None, tmp.path()).unwrap().mouse,
            "mouse = true captures the mouse for wheel scrollback"
        );
    }

    #[test]
    fn layout_defaults_tiled_and_selects_a_fixed_mode() {
        // Default is `Tiled` — the historical auto-tile.
        assert_eq!(Settings::default().layout, LayoutMode::Tiled);
        let tmp = TempDir::new().unwrap();
        // A kebab-case mode name selects a fixed arrangement (runtime cycling
        // was removed, so this is the only way to reach a non-`Tiled` layout).
        std::fs::write(
            tmp.path().join("settings.toml"),
            "[settings]\nlayout = \"main-vertical\"\n",
        )
        .unwrap();
        assert_eq!(
            load(None, tmp.path()).unwrap().layout,
            LayoutMode::MainVertical
        );

        // An unknown mode name is a load error, not a silent fall-through.
        std::fs::write(
            tmp.path().join("settings.toml"),
            "[settings]\nlayout = \"diagonal\"\n",
        )
        .unwrap();
        assert!(matches!(
            load(None, tmp.path()),
            Err(SettingsError::Toml { .. })
        ));
    }

    #[test]
    fn scrollback_lines_is_clamped_to_the_ceiling() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("settings.toml"),
            format!(
                "[settings]\nscrollback_lines = {}\n",
                SCROLLBACK_MAX_LINES as u64 + 50_000_000
            ),
        )
        .unwrap();
        let settings = load(None, tmp.path()).unwrap();
        assert_eq!(
            settings.scrollback_lines, SCROLLBACK_MAX_LINES,
            "an absurd scrollback depth is bounded to the ceiling"
        );
    }

    #[test]
    fn scrollback_lines_zero_disables_scrollback_without_a_floor() {
        let tmp = TempDir::new().unwrap();
        // Unlike the capture caps, 0 is a valid value (scrollback disabled) and
        // must survive resolution rather than being lifted to a floor.
        std::fs::write(
            tmp.path().join("settings.toml"),
            "[settings]\nscrollback_lines = 0\n",
        )
        .unwrap();
        assert_eq!(load(None, tmp.path()).unwrap().scrollback_lines, 0);
    }

    #[test]
    fn prefix_parses_the_cli_grammar_and_rejects_bad_values() {
        let tmp = TempDir::new().unwrap();
        // Unset → None: the default (with tmux auto-dodge) is applied later by
        // `input::effective_prefix`, so unset must stay distinguishable.
        assert_eq!(load(None, tmp.path()).unwrap().prefix, None);

        // A bare letter and a `ctrl-` form parse alike (the `--prefix` grammar).
        std::fs::write(
            tmp.path().join("settings.toml"),
            "[settings]\nprefix = \"B\"\n",
        )
        .unwrap();
        assert_eq!(load(None, tmp.path()).unwrap().prefix, Some('b'));
        std::fs::write(
            tmp.path().join("settings.toml"),
            "[settings]\nprefix = \"ctrl-g\"\n",
        )
        .unwrap();
        assert_eq!(load(None, tmp.path()).unwrap().prefix, Some('g'));

        // A non-letter is a load error, not a silent no-op.
        std::fs::write(
            tmp.path().join("settings.toml"),
            "[settings]\nprefix = \"1\"\n",
        )
        .unwrap();
        assert!(matches!(
            load(None, tmp.path()),
            Err(SettingsError::Toml { .. })
        ));
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
