//! Loop stall from a burst of control-socket requests. No input is read and
//! no frame is drawn while one `drain_control` call runs. A sample sends
//! `burst` real socket requests, waits until all of them sit in the job
//! channel, then calls `drain_control` once per loop turn until the channel
//! is empty and keeps the longest single call.

use std::path::Path;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::test_support::area;
use super::*;
use crate::bench_support::{PANEL_COUNTS, Summary, cat_panel, paint_screen};
use serde_json::{Value, json};

use crate::mcp::control_client::call;
use crate::role::spec::AgentCli;

const BURSTS: [usize; 3] = [64, 512, 4096];
const SAMPLES: usize = 5;
const QUEUE_TIMEOUT: Duration = Duration::from_secs(30);

/// `n` idle panels with manifests, so `list_panels` takes its real path,
/// plus the control server the loop would drain.
fn build(tmp: &TempDir, n: usize) -> (Multiplexer, ControlServer) {
    let session = Session::new("profile", tmp.path().to_path_buf());
    let config = Config::load(tmp.path()).unwrap();
    let (mut mux, _signal, control) =
        Multiplexer::new(session, config, area(), 'a', LaunchMode::Fresh).unwrap();
    for i in 0..n {
        let panel = cat_panel("worker");
        let manifest = AgentManifest::new(
            mux.session.id,
            panel.id,
            "worker",
            format!("worker-{i}"),
            AgentCli::Claude,
            None,
        );
        mux.manifests.insert(panel.id, manifest);
        mux.panels.push(panel);
    }
    mux.rebuild_layout_tree();
    for (i, panel) in mux.panels.iter_mut().enumerate() {
        paint_screen(&mut panel.grid, i);
    }
    (mux, control)
}

/// Queue `burst` copies of the `method` call through the socket, then drain them the
/// way successive loop turns would and return the longest single turn.
async fn drain_once(
    mux: &mut Multiplexer,
    control: &mut ControlServer,
    sock: &Path,
    method: &'static str,
    params: &Value,
    burst: usize,
) -> Duration {
    let clients: Vec<_> = (0..burst)
        .map(|_| {
            let sock = sock.to_path_buf();
            let params = params.clone();
            tokio::spawn(async move { call(&sock, "profile", method, &params).await })
        })
        .collect();
    let deadline = Instant::now() + QUEUE_TIMEOUT;
    while control.jobs().len() < burst {
        assert!(Instant::now() < deadline, "the burst never fully queued");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let mut stall = Duration::ZERO;
    while !control.jobs().is_empty() {
        let start = Instant::now();
        mux.drain_control(control);
        stall = stall.max(start.elapsed());
    }
    for client in clients {
        client.await.unwrap().unwrap().unwrap();
    }
    stall
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "profile: cargo test --release --lib control_profile -- --ignored --nocapture --test-threads=1"]
async fn control_profile() {
    println!("control_profile: longest drain_control turn, median of {SAMPLES} bursts");
    println!(
        "{:<12} {:>6} {:>6} {:>10} {:>10}",
        "request", "panels", "burst", "stall_ms", "max_ms"
    );
    for n in PANEL_COUNTS {
        let tmp = TempDir::new().unwrap();
        let (mut mux, mut control) = build(&tmp, n);
        let sock = control.sock_path().to_path_buf();
        let requests = [
            ("list_panels", "list_panels", json!({})),
            (
                "read_screen",
                "read_panel",
                json!({"panel": mux.panels[0].id.to_string(), "mode": "screen"}),
            ),
        ];
        for (name, method, params) in &requests {
            for burst in BURSTS {
                let mut samples = Vec::with_capacity(SAMPLES);
                for _ in 0..SAMPLES {
                    samples.push(
                        drain_once(&mut mux, &mut control, &sock, method, params, burst).await,
                    );
                }
                let s = Summary::of(&mut samples);
                println!(
                    "{:<12} {:>6} {:>6} {:>10.2} {:>10.2}",
                    name,
                    n,
                    burst,
                    s.median.as_secs_f64() * 1e3,
                    s.max.as_secs_f64() * 1e3
                );
            }
        }
        mux.shutdown();
    }
}
