//! Pre-grant codex directory trust so a caucus-launched codex panel does not
//! stall on codex's interactive *"Do you trust the contents of this
//! directory?"* gate. That gate fires before the agent's first turn — caucus
//! drives codex non-interactively, so nothing answers it and the panel hangs.
//!
//! codex reads directory trust from its on-disk config (`$CODEX_HOME/config.toml`,
//! default `~/.codex/config.toml`) as `[projects."<path>"] trust_level =
//! "trusted"` — the same entry codex itself persists when the user answers
//! "Yes". A runtime `-c projects."<path>".trust_level="trusted"` override is
//! *not* honored for the trust decision (verified against codex 0.133: the gate
//! still appears), so caucus must write the on-disk entry. codex canonicalizes
//! the cwd before matching, so the entry must key on the realpath.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, Table, value};

/// codex's config file: `$CODEX_HOME/config.toml` when `CODEX_HOME` is set
/// (mirroring codex itself), otherwise `~/.codex/config.toml`. `None` when
/// neither `CODEX_HOME` nor `HOME` is set — there is then no config to edit.
fn config_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(home).join("config.toml"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex").join("config.toml"))
}

/// Mark `dir` trusted in codex's config so a codex panel launched there skips
/// the directory-trust gate. Canonicalizes `dir` first because codex matches
/// trust on the realpath.
///
/// Best-effort by the caller's contract: a returned `Err` should be logged, not
/// fatal — the panel can still launch and the user can answer the gate by hand.
pub fn ensure_trusted(dir: &Path) -> Result<()> {
    let config =
        config_path().context("locate codex config: neither CODEX_HOME nor HOME is set")?;
    let canonical = std::fs::canonicalize(dir)
        .with_context(|| format!("canonicalize {} for codex trust", dir.display()))?;
    ensure_trusted_in(&config, &canonical)
}

/// Set `projects."<canonical_dir>".trust_level = "trusted"` in the codex config
/// at `config`, preserving everything else in the file (comments, key order,
/// other `[projects.*]` entries, `[notice]` etc.) via a format-preserving edit.
/// Idempotent: a no-op leaving the file byte-for-byte untouched when the entry
/// is already `trusted`. Creates the file (and parent dir) when absent.
fn ensure_trusted_in(config: &Path, canonical_dir: &Path) -> Result<()> {
    let key = canonical_dir.to_string_lossy().into_owned();

    let existing = match std::fs::read_to_string(config) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("read codex config {}", config.display()));
        }
    };

    let mut doc: DocumentMut = existing
        .parse()
        .with_context(|| format!("parse codex config {}", config.display()))?;

    // Already trusted → leave the file untouched (idempotent, no needless write).
    if doc
        .get("projects")
        .and_then(Item::as_table_like)
        .and_then(|p| p.get(&key))
        .and_then(Item::as_table_like)
        .and_then(|e| e.get("trust_level"))
        .and_then(Item::as_str)
        == Some("trusted")
    {
        return Ok(());
    }

    // Insert or update in place. `projects` is a super-table of per-path
    // sub-tables; keep it implicit so a fresh config gets `[projects."<dir>"]`
    // rather than a stray empty `[projects]` header.
    let projects = doc
        .entry("projects")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("codex config 'projects' is not a table")?;
    projects.set_implicit(true);
    let entry = projects
        .entry(&key)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("codex config 'projects.<dir>' is not a table")?;
    entry["trust_level"] = value("trusted");

    if let Some(parent) = config.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create codex config dir {}", parent.display()))?;
    }
    std::fs::write(config, doc.to_string())
        .with_context(|| format!("write codex config {}", config.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    /// A missing config (and missing parent dir) is created with the entry.
    #[test]
    fn writes_trust_entry_into_a_missing_config() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("nested").join("config.toml");
        ensure_trusted_in(&cfg, Path::new("/proj/alpha")).unwrap();
        let doc: DocumentMut = read(&cfg).parse().unwrap();
        assert_eq!(
            doc["projects"]["/proj/alpha"]["trust_level"].as_str(),
            Some("trusted")
        );
        // No stray empty `[projects]` header.
        assert!(
            !read(&cfg).contains("[projects]"),
            "projects super-table must stay implicit: {}",
            read(&cfg)
        );
    }

    /// Re-trusting an already-trusted dir leaves the file byte-for-byte.
    #[test]
    fn is_idempotent_and_leaves_the_file_byte_identical() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.toml");
        ensure_trusted_in(&cfg, Path::new("/proj/beta")).unwrap();
        let after_first = read(&cfg);
        ensure_trusted_in(&cfg, Path::new("/proj/beta")).unwrap();
        assert_eq!(
            read(&cfg),
            after_first,
            "re-trusting must not rewrite the file"
        );
    }

    /// Comments, top-level keys, other sections and other project entries all
    /// survive the edit; the new entry is added alongside them.
    #[test]
    fn preserves_comments_other_sections_and_existing_projects() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(
            &cfg,
            "# user comment\n\
             model = \"gpt-5.5\"\n\n\
             [projects.\"/existing/repo\"]\n\
             trust_level = \"trusted\"\n\n\
             [notice]\n\
             fast_default_opt_out = true\n",
        )
        .unwrap();
        ensure_trusted_in(&cfg, Path::new("/proj/gamma")).unwrap();
        let out = read(&cfg);
        assert!(out.contains("# user comment"), "comment preserved: {out}");
        assert!(
            out.contains("model = \"gpt-5.5\""),
            "top key preserved: {out}"
        );
        assert!(out.contains("[notice]"), "other section preserved: {out}");
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(
            doc["projects"]["/existing/repo"]["trust_level"].as_str(),
            Some("trusted"),
            "existing project still trusted: {out}"
        );
        assert_eq!(
            doc["projects"]["/proj/gamma"]["trust_level"].as_str(),
            Some("trusted"),
            "new project trusted: {out}"
        );
    }

    /// An existing entry with a non-"trusted" value is updated in place — no
    /// duplicate `[projects."<dir>"]` table (which would be a TOML parse error).
    #[test]
    fn updates_a_non_trusted_entry_without_duplicating_the_table() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("config.toml");
        std::fs::write(
            &cfg,
            "[projects.\"/proj/delta\"]\ntrust_level = \"untrusted\"\n",
        )
        .unwrap();
        ensure_trusted_in(&cfg, Path::new("/proj/delta")).unwrap();
        let out = read(&cfg);
        // Must still parse (a duplicate table would fail here).
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(
            doc["projects"]["/proj/delta"]["trust_level"].as_str(),
            Some("trusted"),
            "entry updated to trusted: {out}"
        );
        assert_eq!(
            out.matches("/proj/delta").count(),
            1,
            "exactly one entry, no duplicate table: {out}"
        );
    }
}
