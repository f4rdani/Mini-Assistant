use serde::{Deserialize, Serialize};
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub ram_mb: f64,
    pub ram_str: String,
    pub cpu_percent: f64,
    pub cpu_str: String,
    pub uptime_secs: u64,
}

static PREV_CPU_TIME: AtomicU64 = AtomicU64::new(0);
static PREV_MEASURE_TIME: AtomicU64 = AtomicU64::new(0);

fn filetime_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

pub fn get_live_metrics() -> SystemMetrics {
    // No invented fallbacks: when a measurement fails the caller gets "—"
    // instead of a plausible-looking number that never changes.
    let mut ram_mb = 0.0;
    let mut ram_str = "—".to_string();
    let mut cpu_percent = 0.0;
    let mut cpu_str = "—".to_string();

    unsafe {
        let handle = GetCurrentProcess();

        // 1. Live Process Working Set (RAM)
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        let size = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if GetProcessMemoryInfo(handle, &mut counters, size) != 0 {
            ram_mb = counters.WorkingSetSize as f64 / (1024.0 * 1024.0);
            ram_str = format!("{ram_mb:.1} MB");
        }

        // 2. Process CPU calculation
        let mut creation = std::mem::zeroed();
        let mut exit = std::mem::zeroed();
        let mut kernel = std::mem::zeroed();
        let mut user = std::mem::zeroed();

        if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0 {
            let kernel_u64 = filetime_to_u64(kernel);
            let user_u64 = filetime_to_u64(user);
            let total_process_time = kernel_u64 + user_u64;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;

            let prev_total = PREV_CPU_TIME.swap(total_process_time, Ordering::SeqCst);
            let prev_time = PREV_MEASURE_TIME.swap(now, Ordering::SeqCst);

            if prev_time > 0 && now > prev_time && prev_total > 0 && total_process_time >= prev_total {
                let diff_time = (now - prev_time) * 10_000; // convert ms to 100ns units
                let diff_cpu = total_process_time - prev_total;
                if diff_time > 0 {
                    let num_cores = num_cpus();
                    let raw_pct = (diff_cpu as f64 / diff_time as f64) * 100.0 / (num_cores as f64);
                    cpu_percent = raw_pct.clamp(0.0, 99.9);
                    cpu_str = format!("{cpu_percent:.2}%");
                }
            }
        }
    }

    SystemMetrics {
        ram_mb: (ram_mb * 10.0).round() / 10.0,
        ram_str,
        cpu_percent: (cpu_percent * 100.0).round() / 100.0,
        cpu_str,
        uptime_secs: 0,
    }
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}
