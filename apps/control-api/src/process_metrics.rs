//! Lightweight process/host metrics for system-instance heartbeats.
//! Best-effort across platforms; no sysinfo dependency.

use serde_json::{Value as JsonValue, json};

/// Snapshot of local process + host resource usage.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProcessMetrics {
    pub process_rss_bytes: Option<u64>,
    pub host_memory_total: Option<u64>,
    pub host_memory_used:  Option<u64>,
    pub host_cpu_percent:  Option<f64>,
    pub storage_total:     Option<u64>,
    pub storage_used:      Option<u64>,
    pub storage_free:      Option<u64>,
}

impl ProcessMetrics {
    pub(crate) fn collect() -> Self {
        let mut metrics = Self {
            process_rss_bytes: process_rss_bytes(),
            host_cpu_percent: host_cpu_percent(),
            ..Self::default()
        };
        if let Some((total, used, _free)) = host_memory() {
            metrics.host_memory_total = Some(total);
            metrics.host_memory_used = Some(used);
        }
        if let Some((total, used, free)) = host_storage() {
            metrics.storage_total = Some(total);
            metrics.storage_used = Some(used);
            metrics.storage_free = Some(free);
        }
        metrics
    }

    pub(crate) fn to_resources_json(&self, process: &str) -> JsonValue {
        let process_rss = self.process_rss_bytes.unwrap_or(0);
        let mem_percent = match (self.process_rss_bytes, self.host_memory_total) {
            (Some(rss), Some(total)) if total > 0 => Some((rss as f64 / total as f64) * 100.0),
            _ => None,
        };
        let storage_percent = match (self.storage_used, self.storage_total) {
            (Some(used), Some(total)) if total > 0 => Some((used as f64 / total as f64) * 100.0),
            _ => None,
        };
        json!({
            "process": process,
            "cpu": {
                "usage_percent": self.host_cpu_percent,
                "scope": "host",
            },
            "memory": {
                "usage_percent": mem_percent,
                "used_bytes": process_rss,
                "process_rss_bytes": process_rss,
                "host_total_bytes": self.host_memory_total,
                "host_used_bytes": self.host_memory_used,
                "scope": "process",
            },
            "storage": {
                "total_bytes": self.storage_total,
                "used_bytes": self.storage_used,
                "free_bytes": self.storage_free,
                "used_percent": storage_percent,
                "scope": "host",
            }
        })
    }
}

fn process_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(kb.saturating_mul(1024));
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        // best-effort via `ps`
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let kb: u64 = text.trim().parse().ok()?;
        Some(kb.saturating_mul(1024))
    }
    #[cfg(windows)]
    {
        // Working set via PowerShell is too heavy; leave unset on Windows for now.
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}

fn host_memory() -> Option<(u64, u64, u64)> {
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let mut total_kb: Option<u64> = None;
        let mut available_kb: Option<u64> = None;
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                total_kb = rest.split_whitespace().next()?.parse::<u64>().ok();
            } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
                available_kb = rest.split_whitespace().next()?.parse::<u64>().ok();
            }
        }
        let total = total_kb? * 1024;
        let available = available_kb.unwrap_or(0) * 1024;
        let used = total.saturating_sub(available);
        Some((total, used, available))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn host_cpu_percent() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        fn read_idle_total() -> Option<(u64, u64)> {
            let stat = std::fs::read_to_string("/proc/stat").ok()?;
            let line = stat.lines().find(|l| l.starts_with("cpu "))?;
            let mut parts = line.split_whitespace().skip(1);
            let mut values = [0u64; 7];
            for slot in values.iter_mut() {
                *slot = parts.next()?.parse().ok()?;
            }
            // user nice system idle iowait irq softirq
            let idle = values[3].saturating_add(values[4]);
            let total: u64 = values.iter().sum();
            Some((idle, total))
        }
        let (idle1, total1) = read_idle_total()?;
        std::thread::sleep(std::time::Duration::from_millis(120));
        let (idle2, total2) = read_idle_total()?;
        let idle_delta = idle2.saturating_sub(idle1) as f64;
        let total_delta = total2.saturating_sub(total1) as f64;
        if total_delta <= 0.0 {
            return None;
        }
        let usage = (1.0 - idle_delta / total_delta) * 100.0;
        Some(usage.clamp(0.0, 100.0))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn host_storage() -> Option<(u64, u64, u64)> {
    #[cfg(target_os = "linux")]
    {
        // Prefer root filesystem via `statvfs`-like reading of /proc/self/mounts is complex;
        // use `df -B1 /` when available.
        let output = std::process::Command::new("df")
            .args(["-B1", "/"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        // Filesystem 1B-blocks Used Available Use% Mounted
        let line = text.lines().nth(1)?;
        let mut cols = line.split_whitespace();
        let _fs = cols.next()?;
        let total: u64 = cols.next()?.parse().ok()?;
        let used: u64 = cols.next()?.parse().ok()?;
        let free: u64 = cols.next()?.parse().ok()?;
        Some((total, used, free))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
