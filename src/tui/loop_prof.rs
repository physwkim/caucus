//! `CAUCUS_PROF=1`: per-pass timing of the event loop, logged to
//! `.caucus/caucus.log` once a second. `begin` marks the start of a turn's
//! work (after the input wait) and each `lap` charges the time since the
//! previous mark to the named pass, so the passes of a turn sum to its busy
//! time. Disabled, every call is one branch.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Stats {
    count: u32,
    total: Duration,
    max: Duration,
}

struct Window {
    started: Instant,
    mark: Instant,
    turns: u32,
    /// In first-lap order, which is loop order.
    passes: Vec<(&'static str, Stats)>,
}

impl Window {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            mark: now,
            turns: 0,
            passes: Vec::new(),
        }
    }

    fn record(&mut self, pass: &'static str, elapsed: Duration) {
        let index = match self.passes.iter().position(|(name, _)| *name == pass) {
            Some(index) => index,
            None => {
                self.passes.push((pass, Stats::default()));
                self.passes.len() - 1
            }
        };
        let stats = &mut self.passes[index].1;
        stats.count += 1;
        stats.total += elapsed;
        stats.max = stats.max.max(elapsed);
    }

    fn report(&self) -> String {
        let busy: Duration = self.passes.iter().map(|(_, s)| s.total).sum();
        let mut out = format!("turns={} busy_us={}", self.turns, busy.as_micros());
        for (name, s) in &self.passes {
            let avg_us = s.total.as_secs_f64() * 1e6 / f64::from(s.count);
            let _ = write!(
                out,
                " {name}=n:{},avg_us:{avg_us:.1},max_us:{}",
                s.count,
                s.max.as_micros()
            );
        }
        out
    }
}

pub(super) struct LoopProf(Option<Window>);

impl LoopProf {
    pub(super) fn from_env() -> Self {
        let on = std::env::var("CAUCUS_PROF").is_ok_and(|v| matches!(v.as_str(), "1" | "true"));
        Self(on.then(|| Window::new(Instant::now())))
    }

    pub(super) fn begin(&mut self) {
        if let Some(window) = &mut self.0 {
            window.turns += 1;
            window.mark = Instant::now();
        }
    }

    pub(super) fn lap(&mut self, pass: &'static str) {
        if let Some(window) = &mut self.0 {
            let now = Instant::now();
            let elapsed = now - window.mark;
            window.mark = now;
            window.record(pass, elapsed);
        }
    }

    pub(super) fn flush_if_due(&mut self) {
        let Some(window) = &mut self.0 else { return };
        let now = Instant::now();
        let elapsed = now - window.started;
        if elapsed < WINDOW {
            return;
        }
        tracing::info!(
            window_ms = elapsed.as_millis() as u64,
            report = %window.report(),
            "event loop profile"
        );
        *window = Window::new(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_lists_passes_in_loop_order_with_count_mean_and_max() {
        let mut window = Window::new(Instant::now());
        window.turns = 2;
        window.record("pump_all", Duration::from_micros(10));
        window.record("draw", Duration::from_micros(300));
        window.record("pump_all", Duration::from_micros(30));
        assert_eq!(
            window.report(),
            "turns=2 busy_us=340 pump_all=n:2,avg_us:20.0,max_us:30 \
             draw=n:1,avg_us:300.0,max_us:300"
        );
    }

    #[test]
    fn laps_charge_each_pass_once_per_turn() {
        let mut prof = LoopProf(Some(Window::new(Instant::now())));
        for _ in 0..3 {
            prof.begin();
            prof.lap("signals");
            prof.lap("pump_all");
        }
        let window = prof.0.as_ref().unwrap();
        assert_eq!(window.turns, 3);
        let counts: Vec<_> = window.passes.iter().map(|(n, s)| (*n, s.count)).collect();
        assert_eq!(counts, [("signals", 3), ("pump_all", 3)]);
    }
}
