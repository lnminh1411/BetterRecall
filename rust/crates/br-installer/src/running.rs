// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Closing a running BetterRecall instance for a specific data folder and install folder.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{DataPaths, PipeNames, full_path};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    EVENT_MODIFY_STATE, GetCurrentProcessId, OpenEventW, OpenMutexW, OpenProcess,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, TerminateProcess,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use crate::InstallError;
use crate::ctx::Ctx;
use crate::log::log_line;

const AGENT_QUIT_WAIT: Duration = Duration::from_secs(10);
const UI_QUIT_WAIT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const TERMINATE_WAIT_MS: u32 = 3000;

/// Snapshot of a single running process used by [`pick_processes_to_end`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub session: u32,
    pub image: Option<PathBuf>,
}

/// Pure selection of process IDs to terminate after graceful quit signaling.
///
/// - Only processes in `session` whose image path is strictly under `install_dir`
///   (case-insensitive normalized path with a trailing separator check).
/// - When `agent_pid` is `Some(root)`, restricts further to processes belonging to `root`'s
///   process tree (`root` itself and its descendants in `session`).
/// - When `agent_pid` is `None`, ends nothing by tree: only processes whose image is under
///   `install_dir` and in `session`.
pub fn pick_processes_to_end(
    procs: &[ProcInfo],
    agent_pid: Option<u32>,
    install_dir: &Path,
    session: u32,
) -> Vec<u32> {
    let dir_prefix = normalized_dir_prefix(install_dir);

    let tree: Option<HashSet<u32>> = agent_pid.map(|root| {
        let mut set = HashSet::new();
        set.insert(root);
        loop {
            let mut added = false;
            for p in procs {
                if p.session == session && set.contains(&p.parent_pid) && set.insert(p.pid) {
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        set
    });

    procs
        .iter()
        .filter(|p| {
            if p.session != session {
                return false;
            }
            if let Some(ref set) = tree
                && !set.contains(&p.pid)
            {
                return false;
            }
            p.image
                .as_deref()
                .is_some_and(|img| path_is_under_prefix(img, &dir_prefix))
        })
        .map(|p| p.pid)
        .collect()
}

/// Signals the agent and UI quit events for `ctx.home`, waits for them to exit,
/// then terminates any remaining processes selected by [`pick_processes_to_end`].
pub fn close_running(ctx: &Ctx) -> Result<(), InstallError> {
    let agent_pid = read_agent_pid(&ctx.home);

    let agent_quit = PipeNames::agent_quit_event_for(&ctx.home);
    if signal_named_event(&agent_quit) {
        log_line(&ctx.home, "signaled agent quit event");
        let agent_mutex = PipeNames::agent_mutex_for(&ctx.home);
        wait_until_gone(AGENT_QUIT_WAIT, || named_mutex_exists(&agent_mutex));
    }

    let ui_quit = PipeNames::ui_quit_event_for(&ctx.home);
    if signal_named_event(&ui_quit) {
        log_line(&ctx.home, "signaled UI quit event");
        wait_until_gone(UI_QUIT_WAIT, || named_event_exists(&ui_quit));
    }

    let my_pid = unsafe { GetCurrentProcessId() };
    let my_session = process_session_id(my_pid).unwrap_or(0);
    let procs = snapshot_processes();
    let targets = pick_processes_to_end(&procs, agent_pid, &ctx.install_dir, my_session);

    for pid in targets {
        if pid == my_pid {
            continue;
        }
        terminate_pid(pid, &ctx.home);
    }

    Ok(())
}

/// Normalizes a directory path to a full Windows path ending with `\`.
pub(crate) fn normalized_dir_prefix(dir: &Path) -> String {
    let full = full_path(dir).unwrap_or_else(|_| dir.to_path_buf());
    let s = full.to_string_lossy().replace('/', "\\");
    let trimmed = s.trim_end_matches('\\');
    format!("{trimmed}\\")
}

/// True when `path` (after full normalization) lies strictly inside `dir_prefix`
/// (which must end with `\`), case-insensitively.
pub(crate) fn path_is_under_prefix(path: &Path, dir_prefix: &str) -> bool {
    let full = full_path(path).unwrap_or_else(|_| path.to_path_buf());
    let s = full.to_string_lossy().replace('/', "\\");
    s.len() > dir_prefix.len()
        && s.is_char_boundary(dir_prefix.len())
        && s[..dir_prefix.len()].eq_ignore_ascii_case(dir_prefix)
}

fn read_agent_pid(home: &Path) -> Option<u32> {
    let text = fs::read_to_string(DataPaths::agent_pid_file(home)).ok()?;
    text.trim().parse::<u32>().ok()
}

fn signal_named_event(name: &str) -> bool {
    let wide = to_wide_nul(name);
    let Ok(handle) = (unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(wide.as_ptr())) })
    else {
        return false;
    };
    let _guard = OwnedHandle(handle);
    unsafe { SetEvent(handle) }.is_ok()
}

fn named_event_exists(name: &str) -> bool {
    let wide = to_wide_nul(name);
    let Ok(handle) =
        (unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wide.as_ptr())) })
    else {
        return false;
    };
    let _guard = OwnedHandle(handle);
    true
}

fn named_mutex_exists(name: &str) -> bool {
    let wide = to_wide_nul(name);
    let Ok(handle) =
        (unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wide.as_ptr())) })
    else {
        return false;
    };
    let _guard = OwnedHandle(handle);
    true
}

fn wait_until_gone<F: Fn() -> bool>(timeout: Duration, still_present: F) {
    let deadline = Instant::now() + timeout;
    while still_present() {
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn process_session_id(pid: u32) -> Option<u32> {
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(pid, &mut session) }
        .ok()
        .map(|()| session)
}

fn snapshot_processes() -> Vec<ProcInfo> {
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    let _guard = OwnedHandle(snap);

    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    if unsafe { Process32FirstW(snap, &mut entry) }.is_err() {
        return out;
    }
    loop {
        let pid = entry.th32ProcessID;
        let parent_pid = entry.th32ParentProcessID;
        if let Some(session) = process_session_id(pid) {
            let image = query_process_image(pid);
            out.push(ProcInfo {
                pid,
                parent_pid,
                session,
                image,
            });
        }
        if unsafe { Process32NextW(snap, &mut entry) }.is_err() {
            break;
        }
    }
    out
}

fn query_process_image(pid: u32) -> Option<PathBuf> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let _guard = OwnedHandle(handle);
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
    }
    .ok()?;
    Some(PathBuf::from(OsString::from_wide(&buf[..len as usize])))
}

fn terminate_pid(pid: u32, home: &Path) {
    let Ok(handle) =
        (unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) })
    else {
        return;
    };
    let _guard = OwnedHandle(handle);
    log_line(home, &format!("terminating remaining process pid={pid}"));
    if unsafe { TerminateProcess(handle, 1) }.is_ok() {
        unsafe {
            let _ = WaitForSingleObject(handle, TERMINATE_WAIT_MS);
        }
    }
}

fn to_wide_nul(s: &str) -> Vec<u16> {
    OsString::from(s).encode_wide().chain(Some(0)).collect()
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}
