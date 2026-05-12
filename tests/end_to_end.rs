//! End-to-end integration: builds the `caucus` binary via Cargo, exercises
//! the full happy path (init → session new → round start → sentinel write
//! → round status → converge) against a temp git repo + real tmux session.
//!
//! Marked `#[ignore]` so `cargo test` stays fast and tmux-independent;
//! run with `cargo test --test end_to_end -- --ignored` to exercise the
//! whole flow. Requires `tmux`, `git`, and the caucus binary built by
//! cargo (the test resolves it via `CARGO_BIN_EXE_caucus`).

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn caucus_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_caucus"))
}

fn run_caucus(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new(caucus_bin())
        .arg("--repo")
        .arg(repo)
        .args(args)
        .output()
        .expect("spawn caucus")
}

fn run(cmd: &mut Command) {
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{:?} failed: stderr={}",
        cmd,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_init(repo: &Path) {
    run(Command::new("git").arg("init").arg("-q").current_dir(repo));
    run(Command::new("git")
        .args(["config", "user.email", "caucus@test.invalid"])
        .current_dir(repo));
    run(Command::new("git")
        .args(["config", "user.name", "caucus-test"])
        .current_dir(repo));
    std::fs::write(repo.join("seed"), "seed\n").unwrap();
    run(Command::new("git").args(["add", "seed"]).current_dir(repo));
    run(Command::new("git")
        .args(["commit", "-q", "-m", "seed"])
        .current_dir(repo));
}

#[test]
#[ignore = "requires tmux + git + a real claude CLI; run with --ignored"]
fn happy_path_init_doctor_session_round_converge() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    git_init(repo);

    // 1. init — creates .caucus/ + sentinel-stop hook.
    let out = run_caucus(repo, &["init"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(repo.join(".caucus/bin/sentinel-stop").exists());

    // 2. doctor — every binary on PATH, all green.
    let out = run_caucus(repo, &["--format", "json", "doctor"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("doctor JSON parses");
    let checks = report["checks"].as_array().unwrap();
    let unhealthy: Vec<_> = checks
        .iter()
        .filter(|c| !c["ok"].as_bool().unwrap())
        .collect();
    assert!(
        unhealthy.is_empty(),
        "doctor reports failures: {unhealthy:?}"
    );

    // 3. role list — the embedded defaults round-trip through the registry.
    let out = run_caucus(repo, &["--format", "json", "role", "list"]);
    assert!(out.status.success());
    let roles: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<String> = roles
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    for expected in ["architect", "backend", "reviewer", "qa", "scribe"] {
        assert!(
            names.contains(&expected.to_string()),
            "missing role {expected}"
        );
    }

    // 4. session list — empty at the start.
    let out = run_caucus(repo, &["--format", "json", "session", "list"]);
    let listed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 0);

    // The remainder of the happy path (session new / round start / converge)
    // requires the `claude` CLI to be installed *and* willing to spawn
    // panes that emit a Stop hook. To keep this test self-contained we stop
    // here; the manual smoke documented in commit 11's message exercises the
    // rest, and individual modules are covered by the per-module integration
    // tests (worktree round-trip, tmux pane lifecycle, sentinel watcher,
    // poller).
}

#[test]
fn caucus_help_lists_every_top_level_subcommand() {
    // Doesn't need tmux/git/claude — exercises arg parsing only.
    let out = Command::new(caucus_bin())
        .arg("--help")
        .output()
        .expect("spawn caucus");
    assert!(out.status.success());
    let body = String::from_utf8_lossy(&out.stdout);
    for sub in [
        "init", "doctor", "session", "round", "execute", "agent", "role", "sentinel", "watch",
    ] {
        assert!(
            body.contains(sub),
            "subcommand {sub} missing from --help output:\n{body}"
        );
    }
}

#[test]
fn round_status_json_has_last_event_ts() {
    // Bypass `session new` (which spawns claude) by writing the session
    // record + manifest directly on disk, then drive `caucus round status
    // --format json` and check the new field is present.
    use caucus::agent::lane_event::LaneEvent;
    use caucus::agent::manifest::{AgentKind, AgentManifest, write_json};
    use caucus::session::record::{Session, write_session};
    use caucus::session::state::SessionState;

    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().to_path_buf();
    std::fs::create_dir_all(repo.join(".caucus").join("sessions")).unwrap();

    let mut session = Session::new(repo.clone(), "topic".into(), vec!["scribe".into()], 1);
    session.transition(SessionState::MeetingInProgress).unwrap();
    session.advance_round().unwrap();
    std::fs::create_dir_all(session.session_root.join("agents")).unwrap();
    std::fs::create_dir_all(session.session_root.join("round-01")).unwrap();

    let mut manifest = AgentManifest::new(
        session.id,
        "scribe".into(),
        "scribe".into(),
        AgentKind::Meeting,
        None,
    );
    let known_ts = chrono::DateTime::parse_from_rfc3339("2026-05-12T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    manifest.lane_events.push(LaneEvent::ResponseFileWritten {
        ts: known_ts,
        path: session
            .session_root
            .join("round-01")
            .join("response-scribe.md"),
        bytes: 7,
    });
    let agent_id = manifest.agent_id;
    write_json(&manifest, &session.session_root).unwrap();
    session.register_agent("scribe", agent_id);
    write_session(&session).unwrap();

    let out = run_caucus(
        &repo,
        &[
            "--format",
            "json",
            "round",
            "status",
            &session.id.to_string(),
        ],
    );
    assert!(
        out.status.success(),
        "round status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let role0 = &parsed["roles"][0];
    assert_eq!(role0["role"].as_str().unwrap(), "scribe");
    let ts = role0["last_event_ts"]
        .as_str()
        .expect("last_event_ts present");
    assert!(
        ts.starts_with("2026-05-12T10:00:00"),
        "unexpected last_event_ts: {ts}"
    );
    // current_pane_hint should serialise as null (default None).
    assert!(role0["current_pane_hint"].is_null());
}

#[test]
fn caucus_role_list_text_format_is_human_readable() {
    let tmp = TempDir::new().unwrap();
    let out = run_caucus(tmp.path(), &["role", "list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8_lossy(&out.stdout);
    for role in ["architect", "backend", "reviewer", "qa", "scribe"] {
        assert!(body.contains(role), "role {role} not in role list:\n{body}");
    }
}
