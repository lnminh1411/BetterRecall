// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! BetterRecall Agent host (stage A4: CLI args, single-instance mutex, message window, tray,
//! hotkeys, hardware probe, governance, memory isolation, worker supervision, embedding scheduler,
//! guardrails, and seams for A5/A6).

pub mod guardrails;
pub mod host;
pub mod native;
pub mod scheduling;
pub mod seams;
pub mod security;
pub mod util;
pub mod worker;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{CoreHostOptions, DataPaths, InstallLayout, PipeNames, full_path};
use br_core::CoreHost;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex};
use windows::core::PCWSTR;

use crate::host::{AgentHost, AgentOptions, Diagnostics, StartupRegistration};
use crate::native::to_wide_z;
use crate::util::AgentLog;

struct OwnedMutex(HANDLE);

impl Drop for OwnedMutex {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            unsafe {
                let _ = ReleaseMutex(self.0);
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Process entry point for `BetterRecall.Agent.exe` (`Program.cs`).
pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    run_with_args(&args)
}

pub fn run_with_args(args: &[String]) -> i32 {
    if let Some(first) = args.first()
        && matches!(
            first.as_str(),
            "--selftest-replay"
                | "--selftest-keyframe"
                | "--selftest-bench"
                | "--selftest-codec"
                | "--selftest-keyframe-budget"
        )
    {
        eprintln!("Self-test '{first}' is not available yet.");
        return 2;
    }

    let mut install_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let mut autostart = true;
    let mut exit_after_s: i32 = 0;
    let mut allow_default_home = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--install-dir" if i + 1 < args.len() => {
                i += 1;
                install_dir = PathBuf::from(&args[i]);
            }
            "--no-autostart" => autostart = false,
            "--unregister" => {
                StartupRegistration::unregister();
                return 0;
            }
            "--console" => AgentLog::set_also_console(true),
            "--exit-after" if i + 1 < args.len() => {
                i += 1;
                match args[i].parse::<i32>() {
                    Ok(v) => exit_after_s = v,
                    Err(_) => {
                        eprintln!("Invalid --exit-after value.");
                        return 2;
                    }
                }
            }
            "--dev-replay-at" | "--dev-export" | "--dev-bench" if i + 1 < args.len() => {
                i += 1; // parsed and ignored until A6/A7
            }
            flag if flag == InstallLayout::ALLOW_DEFAULT_HOME_FLAG => allow_default_home = true,
            _ => {}
        }
        i += 1;
    }

    install_dir = full_path(&install_dir).unwrap_or(install_dir);
    let trimmed = install_dir
        .to_string_lossy()
        .trim_end_matches('\\')
        .to_string();
    install_dir = PathBuf::from(trimmed);

    let home = DataPaths::home();
    if InstallLayout::refuses_home(&install_dir, &home, allow_default_home) {
        eprintln!(
            "Dev build: set {} to a scratch folder (or pass {}).",
            DataPaths::HOME_ENV,
            InstallLayout::ALLOW_DEFAULT_HOME_FLAG
        );
        return 3;
    }

    let mutex_name = PipeNames::agent_mutex_for(&home);
    let mutex_w = to_wide_z(&mutex_name);
    let handle = match unsafe { CreateMutexW(None, true, PCWSTR(mutex_w.as_ptr())) } {
        Ok(h) => h,
        Err(_) => return 1,
    };
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return 0; // single instance per data folder
    }
    let _mutex_guard = OwnedMutex(handle);

    let _ = fs::create_dir_all(&home);
    let pid_file = DataPaths::agent_pid_file(&home);
    let pid_str = std::process::id().to_string();
    let _ = fs::write(&pid_file, &pid_str);

    AgentLog::init(DataPaths::logs_dir(&home), "info");
    AgentLog::info(&format!("agent start, pid {pid_str}"));

    let core = CoreHost::create(CoreHostOptions {
        home: home.clone(),
        install_dir: install_dir.clone(),
    });

    let host = AgentHost::new(
        core.clone(),
        AgentOptions {
            install_dir,
            home,
            autostart,
        },
    );

    let exit_stop = Arc::new((Mutex::new(false), Condvar::new()));
    let exit_thread = if exit_after_s > 0 {
        let host_bg = host.clone();
        let stop_bg = exit_stop.clone();
        let secs = exit_after_s as u64;
        thread::Builder::new()
            .name("BetterRecall.ExitAfter".to_string())
            .spawn(move || {
                let (lock, cv) = &*stop_bg;
                let g = lock.lock().unwrap_or_else(|e| e.into_inner());
                if *g {
                    return;
                }
                let (g2, _) = cv
                    .wait_timeout(g, Duration::from_secs(secs))
                    .unwrap_or_else(|e| e.into_inner());
                if !*g2 {
                    Diagnostics::dump_and_quit(&host_bg);
                }
            })
            .ok()
    } else {
        None
    };

    if let Err(e) = host.run() {
        AgentLog::error_err("agent failed", &e);
    }

    {
        let (lock, cv) = &*exit_stop;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }
    if let Some(h) = exit_thread {
        let _ = h.join();
    }

    let sw = Instant::now();
    host.shutdown();
    core.shutdown();
    AgentLog::info(&format!("agent stopped in {} ms", sw.elapsed().as_millis()));
    AgentLog::flush();

    cleanup_pid_file(&pid_file, &pid_str);
    0
}

fn cleanup_pid_file(pid_file: &Path, own_pid: &str) {
    if let Ok(text) = fs::read_to_string(pid_file)
        && text.trim() == own_pid
    {
        let _ = fs::remove_file(pid_file);
    }
}
