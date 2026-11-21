// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Measurement dump for dev runs (`--exit-after N`, `Diagnostics.cs`).

use std::fmt::Write;
use std::fs;
use std::sync::Mutex;
use std::time::Instant;

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use crate::host::agent_host::AgentHost;
use crate::native::filetime_to_u64;
use crate::security::MemoryIsolation;
use crate::util::AgentLog;

static MARK: Mutex<Option<(u64, Instant)>> = Mutex::new(None);

pub struct Diagnostics;

impl Diagnostics {
    /// Starts a CPU measurement window (called after warm-up).
    pub fn mark() {
        let cpu = process_cpu_100ns();
        *MARK.lock().unwrap_or_else(|e| e.into_inner()) = Some((cpu, Instant::now()));
    }

    pub fn report(host: &AgentHost) -> String {
        let mut sb = String::new();
        if let Some((cpu_start, wall_start)) = *MARK.lock().unwrap_or_else(|e| e.into_inner()) {
            let d_cpu_ms = process_cpu_100ns().saturating_sub(cpu_start) as f64 / 10_000.0;
            let wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
            if wall_ms > 0.0 {
                let _ = writeln!(
                    sb,
                    "cpu {:.2}% of one core over {:.0} s",
                    d_cpu_ms / wall_ms * 100.0,
                    wall_ms / 1000.0
                );
            }
        }
        let (ws_mb, priv_mb) = process_memory_mb();
        let _ = writeln!(sb, "working set {ws_mb} MB, private {priv_mb} MB");

        if let Some(cap_diag) = host.capture_diagnostics() {
            sb.push_str(&cap_diag);
        }
        if let Some(kf_diag) = host.keyframe_diagnostics() {
            sb.push_str(&kf_diag);
        }
        if let Some(sc) = host.scheduler() {
            let _ = writeln!(
                sb,
                "scheduler: {}, processed {}, nsfw {}, queued {}",
                sc.last_decision(),
                sc.processed(),
                sc.dropped(),
                sc.queued()
            );
        }
        if let Some(g) = host.guard() {
            let _ = writeln!(
                sb,
                "guard scan {:.2} ms, badge {:.1} ms, password fields {:.1} ms (failed {})",
                g.last_scan_ms(),
                g.last_badge_ms(),
                g.last_password_ms(),
                g.password_probe_failures()
            );
        }
        let sup = host.supervisor();
        let nsfw_dev = sup
            .hello()
            .and_then(|h| h.nsfw_device)
            .unwrap_or_else(|| "not loaded".to_string());
        let _ = writeln!(
            sb,
            "worker running {}, failures {}, nsfw device {nsfw_dev}",
            sup.is_running(),
            sup.failures()
        );
        let _ = writeln!(
            sb,
            "isolation applied: {}; skipped: {}",
            MemoryIsolation::applied().join(", "),
            MemoryIsolation::skipped().join(", ")
        );
        sb
    }

    pub fn dump_and_quit(host: &AgentHost) {
        let report = Self::report(host);
        AgentLog::info(&format!("diagnostics\n{report}"));
        if let Ok(path) = std::env::var("BETTERRECALL_DIAG_FILE")
            && !path.is_empty()
        {
            let _ = fs::write(path, report);
        }
        host.request_quit();
    }
}

fn process_cpu_100ns() -> u64 {
    let mut c = FILETIME::default();
    let mut e = FILETIME::default();
    let mut k = FILETIME::default();
    let mut u = FILETIME::default();
    if unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }.is_ok() {
        filetime_to_u64(k) + filetime_to_u64(u)
    } else {
        0
    }
}

fn process_memory_mb() -> (usize, usize) {
    let mut pmc = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&raw mut pmc).cast(),
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    if ok.is_ok() {
        (pmc.WorkingSetSize / 1_048_576, pmc.PrivateUsage / 1_048_576)
    } else {
        (0, 0)
    }
}
