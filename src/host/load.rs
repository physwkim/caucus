//! A machine's load, as `list_hosts` reports it for each host.

use serde::{Deserialize, Serialize};

/// What main weighs when it places a panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostLoad {
    /// The 1, 5 and 15 minute load averages.
    pub load_avg: [f64; 3],
    pub cpus: usize,
    /// Memory available for new processes, MiB. `None` off Linux.
    pub mem_available_mib: Option<u64>,
    pub mem_total_mib: Option<u64>,
}

/// Measure this machine now.
pub fn sample() -> HostLoad {
    let mut load_avg = [0f64; 3];
    // SAFETY: `load_avg` has room for the 3 samples requested.
    if unsafe { libc::getloadavg(load_avg.as_mut_ptr(), 3) } != 3 {
        load_avg = [0.0; 3];
    }
    let (mem_available_mib, mem_total_mib) = meminfo();
    HostLoad {
        load_avg,
        cpus: std::thread::available_parallelism().map_or(1, usize::from),
        mem_available_mib,
        mem_total_mib,
    }
}

fn meminfo() -> (Option<u64>, Option<u64>) {
    std::fs::read_to_string("/proc/meminfo").map_or((None, None), |text| {
        (
            meminfo_mib(&text, "MemAvailable:"),
            meminfo_mib(&text, "MemTotal:"),
        )
    })
}

fn meminfo_mib(text: &str, key: &str) -> Option<u64> {
    let line = text.lines().find_map(|l| l.strip_prefix(key))?;
    let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
    Some(kib / 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_values_are_read_in_mib() {
        let text = "MemTotal:       32768000 kB\nMemFree:  1 kB\nMemAvailable:    2048000 kB\n";
        assert_eq!(meminfo_mib(text, "MemTotal:"), Some(32000));
        assert_eq!(meminfo_mib(text, "MemAvailable:"), Some(2000));
        assert_eq!(meminfo_mib(text, "SwapTotal:"), None);
    }

    #[test]
    fn a_sample_counts_at_least_one_cpu() {
        assert!(sample().cpus >= 1);
    }
}
