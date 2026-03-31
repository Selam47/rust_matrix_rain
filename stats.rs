// ─────────────────────────────────────────────────────────────────────────────
// stats.rs — System telemetry layer
//
// Design decision: we keep a single `System` instance alive for the entire
// process lifetime.  sysinfo internally caches file descriptors (Linux: /proc,
// macOS: Mach ports) so re-creating it every frame would thrash the OS.
//
// Refresh cadence is deliberately coarse (caller controls it) so we pay the
// syscall cost only once every N frames, not 60× per second.
// ─────────────────────────────────────────────────────────────────────────────

use sysinfo::System;

pub struct SystemStats {
    sys: System,

    // Pre-computed percentages, updated on each `refresh()` call.
    // Stored as plain f32 so the render thread can read without locking.
    pub ram_percent: f32,
    pub cpu_percent: f32,
}

impl SystemStats {
    /// First call does a full refresh to warm up sysinfo's internal caches.
    pub fn new() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();
        let ram_percent = Self::calc_ram(&sys);
        let cpu_percent = Self::calc_cpu(&sys);
        Self { sys, ram_percent, cpu_percent }
    }

    /// Incremental refresh — only touches memory and CPU pages in /proc.
    /// Called every STATS_REFRESH_INTERVAL frames (≈ every 500 ms at 60 fps).
    pub fn refresh(&mut self) {
        // refresh_memory() reads /proc/meminfo (Linux) or vm_stat (macOS)
        self.sys.refresh_memory();
        // refresh_cpu_usage() updates per-core jiffies delta since last call
        self.sys.refresh_cpu_usage();
        self.ram_percent = Self::calc_ram(&self.sys);
        self.cpu_percent = Self::calc_cpu(&self.sys);
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn calc_ram(sys: &System) -> f32 {
        let total = sys.total_memory();
        if total == 0 {
            return 0.0;
        }
        // sysinfo returns values in bytes (0.31+)
        sys.used_memory() as f32 / total as f32 * 100.0
    }

    fn calc_cpu(sys: &System) -> f32 {
        let cpus = sys.cpus();
        if cpus.is_empty() {
            return 0.0;
        }
        // Average across all logical cores for a single representative value
        let total: f32 = cpus.iter().map(|c| c.cpu_usage()).sum();
        total / cpus.len() as f32
    }
}
