//! Thin, typed wrapper over the `tmux` CLI. `send_shell` (auto-quote) and
//! `send_keys` (raw) are split, mirroring dmux (see `docs/dmux-analysis.md`
//! §4.3) — they MUST NOT be merged into one helper.
