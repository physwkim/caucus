//! End-to-end test of `doctor::signal_selftest` against a real hook script
//! that execs the freshly built caucus binary — the exact production
//! `turn-signal` shape (`caucus init` writes the same script with a bare
//! `caucus` name). This is the delivery path a broken machine setup kills:
//! Stop hook → script → `caucus signal post` → unix socket.

use caucus::doctor::{Severity, signal_selftest};

/// A hook script in the production `turn-signal` shape, but exec'ing the
/// test-built binary by absolute path so the test does not depend on a
/// `caucus` install on `PATH`.
fn write_turn_signal_script(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("turn-signal");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec \"{}\" signal post --sock \"$CAUCUS_SOCK\" \
             --session \"$CAUCUS_SESSION_ID\" --panel \"$CAUCUS_PANEL_ID\" \
             --kind stop\n",
            env!("CARGO_BIN_EXE_caucus"),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[test]
fn selftest_passes_through_a_real_turn_signal_script() {
    let tmp = tempfile::TempDir::new().unwrap();
    let script = write_turn_signal_script(tmp.path());
    let check = signal_selftest(script.to_str().unwrap());
    assert_eq!(check.severity, Severity::Ok, "detail: {}", check.detail);
}

#[test]
fn selftest_warns_when_the_script_is_not_executable() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::TempDir::new().unwrap();
    let script = write_turn_signal_script(tmp.path());
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
    let check = signal_selftest(script.to_str().unwrap());
    assert_eq!(check.severity, Severity::Warn, "detail: {}", check.detail);
}
