//! `caucus host doctor` as a main's `caucus doctor` runs it over `ssh`: one
//! report line a main reads.

use std::process::Command;

use caucus::doctor::{HostReport, Severity};
use caucus::host::proto::PROTO_VERSION;

#[test]
fn host_doctor_reports_this_host_on_one_line() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_caucus"))
        .args(["host", "doctor"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let line = stdout.lines().last().unwrap();
    let report: HostReport = serde_json::from_str(line).unwrap();
    assert_eq!(report.proto, PROTO_VERSION);
    assert_eq!(report.version, env!("CARGO_PKG_VERSION"));
    // A home with no Claude settings has no hook to run.
    let hook = report
        .checks
        .iter()
        .find(|c| c.name == "claude-stop-hook")
        .unwrap();
    assert_eq!(hook.severity, Severity::Warn, "{}", hook.detail);
}
