// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! MCP relay (`betterrecall-mcp.exe` when started outside an install folder).
//!
//! Relays JSON-RPC stdio between the MCP client and `<install>\betterrecall-mcp.exe` with
//! `BETTERRECALL_MCP_RELAY=1`. When the installer/updater holds the scoped setup mutex, the
//! installed server exits with code 75 (or is ended by the installer); the relay waits for the
//! mutex to clear, verifies install integrity, restarts the server, and resends every request
//! the old server left unanswered.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::InstallLayout;
use br_integrity::{CheckOutcome, INTEGRITY_EXIT_CODE, check_install};
use serde_json::Value;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW,
};
use windows::core::PCWSTR;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const APP_SUBKEY: &str = r"Software\BetterRecall";
const INSTALL_PATH_VALUE: &str = "InstallPath";

/// Waiter handle for one spawned MCP server child.
pub trait ChildWaiter: Send {
    fn try_wait(&mut self) -> io::Result<Option<i32>>;
}

/// Real child process waiter.
pub struct ProcessWaiter(Child);

impl ChildWaiter for ProcessWaiter {
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        match self.0.try_wait()? {
            Some(status) => Ok(Some(status.code().unwrap_or(1))),
            None => Ok(None),
        }
    }
}

/// Split I/O and exit handles of one MCP server child.
pub struct ChildHandles {
    pub stdin: Box<dyn Write + Send>,
    pub stdout: Box<dyn Read + Send>,
    pub waiter: Box<dyn ChildWaiter>,
}

/// Failure when validating or starting the installed MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    Integrity(String),
    Other(String),
}

/// Tuning parameters for [`run_relay`].
#[derive(Debug, Clone, Copy)]
pub struct RelayConfig {
    pub mutex_poll: Duration,
    pub mutex_max_wait: Duration,
    pub mutex_grace: Duration,
    pub child_poll: Duration,
    pub restart_window: Duration,
    pub max_restarts: usize,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            mutex_poll: Duration::from_millis(500),
            mutex_max_wait: Duration::from_secs(600),
            mutex_grace: Duration::from_secs(5),
            // The relay lives as long as the MCP session: 4 wake-ups a second, not 40.
            child_poll: Duration::from_millis(250),
            restart_window: Duration::from_secs(60),
            max_restarts: 5,
        }
    }
}

/// Final result of [`run_relay`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayOutcome {
    pub exit_code: i32,
    pub restarts: usize,
}

/// Streams and hooks passed into [`run_relay`].
pub struct RelayDeps<R, W, E, M, S> {
    pub client_in: R,
    pub client_out: W,
    pub stderr: E,
    pub install_dir: PathBuf,
    pub mutex_exists: M,
    pub start_child: S,
    pub config: RelayConfig,
}

#[derive(Clone)]
struct Pending {
    key: String,
    raw_id: String,
    line: Vec<u8>,
}

struct ChildStdinState {
    stdin: Option<Box<dyn Write + Send>>,
}

/// Resolves the install folder and remaining args for the relay:
/// `--install <dir>` as first two args, else HKCU `Software\BetterRecall\InstallPath`,
/// else `%LOCALAPPDATA%\Programs\BetterRecall`.
pub fn resolve_install_dir(
    args: &[String],
    reg_reader: impl FnOnce() -> Option<PathBuf>,
    default_dir: impl FnOnce() -> PathBuf,
) -> Result<(PathBuf, Vec<String>), String> {
    if args.first().map(String::as_str) == Some("--install") {
        let Some(dir) = args.get(1).filter(|s| !s.trim().is_empty()) else {
            return Err("--install requires a directory".to_string());
        };
        return Ok((PathBuf::from(dir), args[2..].to_vec()));
    }
    if let Some(reg_dir) = reg_reader()
        && !reg_dir.as_os_str().is_empty()
    {
        return Ok((reg_dir, args.to_vec()));
    }
    Ok((default_dir(), args.to_vec()))
}

/// Reads `HKCU\Software\BetterRecall` `InstallPath` (`REG_SZ`), or `None` if absent.
pub fn read_registry_install_path() -> Option<PathBuf> {
    let subkey: Vec<u16> = OsString::from(APP_SUBKEY)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut hkey = HKEY::default();
    let err = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
    };
    if err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND || err.is_err() {
        return None;
    }
    struct KeyGuard(HKEY);
    impl Drop for KeyGuard {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe {
                    let _ = RegCloseKey(self.0);
                }
            }
        }
    }
    let _guard = KeyGuard(hkey);

    let name: Vec<u16> = OsString::from(INSTALL_PATH_VALUE)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut ty = REG_VALUE_TYPE(0);
    let mut byte_len = 0u32;
    let err = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            None,
            Some(&mut byte_len),
        )
    };
    if err.is_err() || ty != REG_SZ || byte_len == 0 {
        return None;
    }
    let u16_len = (byte_len as usize).div_ceil(2);
    let mut buf = vec![0u16; u16_len];
    let mut actual = (buf.len() * 2) as u32;
    let err = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr().cast::<u8>()),
            Some(&mut actual),
        )
    };
    if err.is_err() || ty != REG_SZ {
        return None;
    }
    let words = (actual as usize) / 2;
    let slice = &buf[..words.min(buf.len())];
    let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    let s = OsString::from_wide(&slice[..end])
        .to_string_lossy()
        .trim()
        .to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Verifies `<install_dir>` has both executables and passes `br_integrity::check_install`, then
/// spawns `<install_dir>\betterrecall-mcp.exe` with `BETTERRECALL_MCP_RELAY=1`.
pub fn start_real_server(install_dir: &Path, args: &[String]) -> Result<ChildHandles, StartError> {
    let agent_exe = install_dir.join(InstallLayout::AGENT_EXE);
    let mcp_exe = install_dir.join(InstallLayout::MCP_EXE);
    if !agent_exe.is_file() || !mcp_exe.is_file() {
        return Err(StartError::Other(format!(
            "BetterRecall installation not found in '{}'",
            install_dir.display()
        )));
    }
    if let CheckOutcome::Failed(err) = check_install(install_dir) {
        return Err(StartError::Integrity(format!(
            "BetterRecall's files were changed or are damaged ({}). Reinstall BetterRecall.",
            err.offending_path()
        )));
    }
    let mut child = Command::new(&mcp_exe)
        .args(args)
        .env(InstallLayout::MCP_RELAY_ENV, "1")
        .current_dir(install_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| StartError::Other(format!("could not start {}: {e}", mcp_exe.display())))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| StartError::Other("missing child stdin".to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| StartError::Other("missing child stdout".to_string()))?;

    Ok(ChildHandles {
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        waiter: Box::new(ProcessWaiter(child)),
    })
}

/// Runs the relay loop until client EOF or fatal server exit.
pub fn run_relay<R, W, E, M, S>(deps: RelayDeps<R, W, E, M, S>) -> RelayOutcome
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    E: Write,
    M: Fn() -> bool,
    S: FnMut(&Path) -> Result<ChildHandles, StartError>,
{
    let RelayDeps {
        client_in,
        client_out,
        mut stderr,
        install_dir,
        mutex_exists,
        mut start_child,
        config,
    } = deps;

    let gate = Arc::new(Mutex::new(ChildStdinState { stdin: None }));
    let pending_gate = Arc::new(Mutex::new(Vec::<Pending>::new()));
    let out_gate = Arc::new(Mutex::new(client_out));
    let client_ended = Arc::new(AtomicBool::new(false));

    // Read client stdin on a dedicated thread so requests keep queueing while waiting for the
    // setup mutex or restarting the server.
    let gate_in = Arc::clone(&gate);
    let pending_in = Arc::clone(&pending_gate);
    let ended_in = Arc::clone(&client_ended);
    thread::spawn(move || {
        let mut splitter = LineSplitter::new(client_in);
        while let Some(line) = splitter.read_line() {
            let id = message_id(&line, true);
            let mut st = gate_in.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((key, raw_id)) = id {
                pending_in
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Pending {
                        key,
                        raw_id,
                        line: line.clone(),
                    });
            }
            if let Some(ref mut stdin) = st.stdin {
                send_line(stdin, &line);
            }
        }
        ended_in.store(true, Ordering::Release);
        let mut st = gate_in.lock().unwrap_or_else(|e| e.into_inner());
        st.stdin = None;
    });

    let mut restarts_log: VecDeque<Instant> = VecDeque::new();
    let mut restarts = 0usize;
    let mut first = true;
    let mut last_exit_code = 1;

    loop {
        if !wait_for_mutex_clear(
            &mutex_exists,
            &client_ended,
            config.mutex_poll,
            config.mutex_max_wait,
        ) {
            if client_ended.load(Ordering::Acquire) {
                return RelayOutcome {
                    exit_code: 0,
                    restarts,
                };
            }
            let _ = writeln!(
                stderr,
                "betterrecall-mcp: timed out waiting for BetterRecall installer"
            );
            fail_pending(
                &pending_gate,
                &out_gate,
                "BetterRecall is still updating: restart your MCP client to reconnect.",
            );
            return RelayOutcome {
                exit_code: if first { 1 } else { last_exit_code },
                restarts,
            };
        }

        if client_ended.load(Ordering::Acquire) {
            return RelayOutcome {
                exit_code: 0,
                restarts,
            };
        }

        let child = match start_child(&install_dir) {
            Ok(c) => c,
            Err(StartError::Integrity(msg)) => {
                let _ = writeln!(stderr, "betterrecall-mcp: {msg}");
                fail_pending(
                    &pending_gate,
                    &out_gate,
                    "BetterRecall's files failed integrity verification: reinstall BetterRecall.",
                );
                return RelayOutcome {
                    exit_code: INTEGRITY_EXIT_CODE,
                    restarts,
                };
            }
            Err(StartError::Other(msg)) => {
                let _ = writeln!(stderr, "betterrecall-mcp: {msg}");
                fail_pending(
                    &pending_gate,
                    &out_gate,
                    "BetterRecall could not restart its MCP server: restart your MCP client.",
                );
                return RelayOutcome {
                    exit_code: 1,
                    restarts,
                };
            }
        };

        let ChildHandles {
            stdin,
            stdout,
            mut waiter,
        } = child;

        {
            let mut st = gate.lock().unwrap_or_else(|e| e.into_inner());
            if client_ended.load(Ordering::Acquire) {
                drop(stdin);
                st.stdin = None;
            } else {
                st.stdin = Some(stdin);
                let again: Vec<Pending> = pending_gate
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some(ref mut s) = st.stdin {
                    for p in &again {
                        send_line(s, &p.line);
                    }
                }
            }
        }

        let pending_pump = Arc::clone(&pending_gate);
        let out_pump = Arc::clone(&out_gate);
        let pump_handle = thread::spawn(move || {
            let mut splitter = LineSplitter::new(stdout);
            while let Some(line) = splitter.read_line() {
                if let Some((key, _)) = message_id(&line, false) {
                    pending_pump
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .retain(|p| p.key != key);
                }
                write_client(&out_pump, &line);
            }
        });

        let mut last_mutex_seen: Option<Instant> = None;
        let code = loop {
            if mutex_exists() {
                last_mutex_seen = Some(Instant::now());
            }
            match waiter.try_wait() {
                Ok(Some(c)) => break c,
                Ok(None) => thread::sleep(config.child_poll),
                Err(_) => break 1,
            }
        };

        // Drain every answer the old server wrote before deciding whether to resend anything.
        let _ = pump_handle.join();
        {
            let mut st = gate.lock().unwrap_or_else(|e| e.into_inner());
            st.stdin = None;
        }

        if client_ended.load(Ordering::Acquire) {
            return RelayOutcome {
                exit_code: 0,
                restarts,
            };
        }

        if mutex_exists() {
            last_mutex_seen = Some(Instant::now());
        }
        let mutex_recent = last_mutex_seen.is_some_and(|t| t.elapsed() <= config.mutex_grace);
        let should_restart = code == InstallLayout::MCP_RESTART_EXIT_CODE || mutex_recent;

        if !should_restart {
            fail_pending(
                &pending_gate,
                &out_gate,
                "BetterRecall's MCP server stopped: restart your MCP client.",
            );
            return RelayOutcome {
                exit_code: code,
                restarts,
            };
        }

        let now = Instant::now();
        while let Some(&front) = restarts_log.front() {
            if now.duration_since(front) > config.restart_window {
                restarts_log.pop_front();
            } else {
                break;
            }
        }
        if restarts_log.len() >= config.max_restarts {
            fail_pending(
                &pending_gate,
                &out_gate,
                "BetterRecall was updated while this MCP session ran: restart your MCP client to reconnect.",
            );
            return RelayOutcome {
                exit_code: code,
                restarts,
            };
        }

        restarts_log.push_back(now);
        restarts += 1;
        last_exit_code = code;
        first = false;
    }
}

fn wait_for_mutex_clear(
    mutex_exists: &dyn Fn() -> bool,
    client_ended: &AtomicBool,
    poll: Duration,
    max_wait: Duration,
) -> bool {
    let deadline = Instant::now() + max_wait;
    while mutex_exists() {
        if client_ended.load(Ordering::Acquire) || Instant::now() >= deadline {
            return false;
        }
        thread::sleep(poll);
    }
    true
}

fn send_line(stdin: &mut dyn Write, line: &[u8]) {
    let _ = stdin.write_all(line);
    let _ = stdin.flush();
}

fn write_client<W: Write>(out_gate: &Mutex<W>, line: &[u8]) {
    let mut out = out_gate.lock().unwrap_or_else(|e| e.into_inner());
    let _ = out.write_all(line);
    let _ = out.flush();
}

fn fail_pending<W: Write>(pending_gate: &Mutex<Vec<Pending>>, out_gate: &Mutex<W>, message: &str) {
    let left: Vec<Pending> = {
        let mut guard = pending_gate.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    };
    let encoded_msg = serde_json::to_string(message).expect("string serialize");
    for p in left {
        let reply = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{{\"code\":-32603,\"message\":{}}}}}\n",
            p.raw_id, encoded_msg
        );
        write_client(out_gate, reply.as_bytes());
    }
}

/// Id of a JSON-RPC request (`is_request == true`: has string `method` and string/number `id`)
/// or response (`is_request == false`: has string/number `id`, no `method`).
/// Key is kind-prefixed (`s:` / `n:`) so `1` and `"1"` differ and escaped strings match.
pub fn message_id(line: &[u8], is_request: bool) -> Option<(String, String)> {
    let json = line.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(line);
    let root: Value = serde_json::from_slice(json).ok()?;
    let obj = root.as_object()?;
    let id = obj.get("id")?;
    let method = obj.get("method");
    if is_request {
        if !method.is_some_and(Value::is_string) {
            return None;
        }
    } else if method.is_some() {
        return None;
    }
    match id {
        Value::String(s) => {
            let raw = serde_json::to_string(s).ok()?;
            Some((format!("s:{s}"), raw))
        }
        Value::Number(n) => {
            let raw = n.to_string();
            Some((format!("n:{raw}"), raw))
        }
        _ => None,
    }
}

/// Splits a byte stream into lines, each with its `'\n'`; the last one may lack it.
/// `None` at end of stream. Lines longer than `MAX_LINE` are yielded in `MAX_LINE` chunks.
pub struct LineSplitter<R> {
    stream: R,
    buf: Vec<u8>,
    start: usize,
    end: usize,
    line: Vec<u8>,
}

impl<R: Read> LineSplitter<R> {
    pub const MAX_LINE: usize = 16 * 1024 * 1024;

    pub fn new(stream: R) -> Self {
        Self {
            stream,
            buf: vec![0u8; 16 * 1024],
            start: 0,
            end: 0,
            line: Vec::new(),
        }
    }

    pub fn read_line(&mut self) -> Option<Vec<u8>> {
        loop {
            if self.start < self.end {
                let slice = &self.buf[self.start..self.end];
                if let Some(rel) = slice.iter().position(|&b| b == b'\n') {
                    self.line.extend_from_slice(&slice[..=rel]);
                    self.start += rel + 1;
                    return Some(std::mem::take(&mut self.line));
                }
                self.line.extend_from_slice(slice);
                self.start = 0;
                self.end = 0;
                if self.line.len() >= Self::MAX_LINE {
                    return Some(std::mem::take(&mut self.line));
                }
            }
            match self.stream.read(&mut self.buf) {
                Ok(0) | Err(_) => {
                    return if self.line.is_empty() {
                        None
                    } else {
                        Some(std::mem::take(&mut self.line))
                    };
                }
                Ok(n) => {
                    self.start = 0;
                    self.end = n;
                }
            }
        }
    }
}
