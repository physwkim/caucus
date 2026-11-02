//! Per-tick cost of the event loop's passes against panel count. The passes
//! run on every loop turn whether or not anything changed. `quiet` has no
//! panel output; `all_active` changes every grid every tick, which defeats
//! the blocked-prompt scan's generation cache. Grids are written directly,
//! so `pump_all` here is the drain and liveness cost without byte ingest.

use std::hint::black_box;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::Multiplexer;
use super::test_support::mux;
use crate::agent::AgentManifest;
use crate::bench_support::{PANEL_COUNTS, Summary, cat_panel, paint_screen, spin};
use crate::panel::PanelState;
use crate::role::spec::AgentCli;

const WARMUP_TICKS: usize = 50;
const TICKS: usize = 500;

type Pass = (&'static str, fn(&mut Multiplexer));

/// The argument-free passes of one `tui::run` loop turn, in its order, and
/// the redraw gate's signature.
fn passes() -> [Pass; 12] {
    [
        ("poll_pending_spawns", Multiplexer::poll_pending_spawns),
        ("pump_all", Multiplexer::pump_all),
        ("poll_pending_submits", Multiplexer::poll_pending_submits),
        ("poll_resume_notice", Multiplexer::poll_resume_notice),
        ("poll_notifications", Multiplexer::poll_notifications),
        ("poll_lost_turns", Multiplexer::poll_lost_turns),
        (
            "poll_round_blocked_panels",
            Multiplexer::poll_round_blocked_panels,
        ),
        ("poll_worker_watches", Multiplexer::poll_worker_watches),
        ("poll_waits", Multiplexer::poll_waits),
        ("poll_question_notices", Multiplexer::poll_question_notices),
        ("poll_stranded_main", Multiplexer::poll_stranded_main),
        ("render_signature", |m| {
            black_box(m.render_signature());
        }),
    ]
}

#[derive(Clone, Copy)]
enum Activity {
    Quiet,
    AllActive,
}

impl Activity {
    fn label(self) -> &'static str {
        match self {
            Activity::Quiet => "quiet",
            Activity::AllActive => "all_active",
        }
    }
}

/// A working main panel plus `n - 1` working workers, each watched, tiled
/// into the test area with a full screen of text. CLIs alternate so both
/// screen-activity scanners run.
fn build(tmp: &TempDir, n: usize) -> Multiplexer {
    let mut mux = mux(tmp);
    for i in 0..n {
        let role = if i == 0 { "main" } else { "worker" };
        let mut panel = cat_panel(role);
        panel.state = PanelState::Working;
        if i == 0 {
            mux.main_panel_id = Some(panel.id);
        }
        let cli = if i % 2 == 0 {
            AgentCli::Claude
        } else {
            AgentCli::Codex
        };
        let manifest = AgentManifest::new(
            mux.session.id,
            panel.id,
            role,
            format!("{role}-{i}"),
            cli,
            None,
        );
        mux.manifests.insert(panel.id, manifest);
        mux.panels.push(panel);
    }
    mux.rebuild_layout_tree();
    for (i, panel) in mux.panels.iter_mut().enumerate() {
        paint_screen(&mut panel.grid, i);
    }
    let workers: Vec<_> = mux.panels.iter().skip(1).map(|p| p.id).collect();
    for id in workers {
        mux.note_prompt_delivered(id);
    }
    mux
}

/// One summary per pass, then one for the whole turn.
fn profile(n: usize, activity: Activity) -> Vec<Summary> {
    let tmp = TempDir::new().unwrap();
    let mut mux = build(&tmp, n);
    let passes = passes();
    let mut samples = vec![Vec::with_capacity(TICKS); passes.len() + 1];
    for tick in 0..WARMUP_TICKS + TICKS {
        if let Activity::AllActive = activity {
            mux.panels.iter_mut().for_each(|p| spin(&mut p.grid, tick));
        }
        let mut turn = Duration::ZERO;
        for (i, (_, pass)) in passes.iter().enumerate() {
            let start = Instant::now();
            pass(&mut mux);
            let elapsed = start.elapsed();
            turn += elapsed;
            if tick >= WARMUP_TICKS {
                samples[i].push(elapsed);
            }
        }
        if tick >= WARMUP_TICKS {
            samples[passes.len()].push(turn);
        }
    }
    assert_eq!(
        mux.watches.len(),
        n - 1,
        "a watch settled mid-profile, so the later ticks measured fewer watched panels"
    );
    samples.iter_mut().map(|s| Summary::of(s)).collect()
}

#[tokio::test]
#[ignore = "profile: cargo test --release --lib tick_profile -- --ignored --nocapture --test-threads=1"]
async fn tick_profile() {
    println!("tick_profile: 120x40 tiled, {TICKS} ticks after {WARMUP_TICKS} warmup");
    let names: Vec<&str> = passes()
        .iter()
        .map(|(name, _)| *name)
        .chain(["(turn)"])
        .collect();
    let [small, team, stress] = PANEL_COUNTS;
    println!(
        "{:<10} {:<26} {:>9} {:>9} {:>9} {:>9} {:>7}",
        "activity",
        "pass",
        format!("med@{small}"),
        format!("med@{team}"),
        format!("med@{stress}"),
        format!("p95@{stress}"),
        "x50"
    );
    for activity in [Activity::Quiet, Activity::AllActive] {
        let runs: Vec<Vec<Summary>> = PANEL_COUNTS.iter().map(|&n| profile(n, activity)).collect();
        for (i, name) in names.iter().enumerate() {
            let ns = |run: usize| runs[run][i].median.as_nanos();
            println!(
                "{:<10} {:<26} {:>9} {:>9} {:>9} {:>9} {:>7.1}",
                activity.label(),
                name,
                ns(0),
                ns(1),
                ns(2),
                runs[2][i].p95.as_nanos(),
                ns(2) as f64 / ns(0).max(1) as f64
            );
        }
    }
}
