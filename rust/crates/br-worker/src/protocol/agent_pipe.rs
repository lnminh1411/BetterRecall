// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use br_contracts::{BinaryFrame, FrameError, InstallLayout, PipeNames, full_path};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_HANDLE_EOF, ERROR_IO_PENDING,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_MODE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, GetFinalPathNameByHandleW, OPEN_EXISTING, ReadFile,
    SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{GetNamedPipeServerProcessId, PeekNamedPipe, WaitNamedPipeW};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use crate::protocol::worker_loop::{FrameRead, PipeIo};

/// Owned Win32 handle wrapper that closes on drop and is safe to share for overlapped pipe I/O.
pub struct PipeHandle(HANDLE);

unsafe impl Send for PipeHandle {}
unsafe impl Sync for PipeHandle {}

impl PipeHandle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct EventHandle(HANDLE);

impl EventHandle {
    fn new_manual() -> io::Result<Self> {
        let h = unsafe { CreateEventW(None, true, false, None) }.map_err(win32_to_io)?;
        Ok(Self(h))
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Result of verifying the pipe server process image against the expected agent binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerVerification {
    pub trusted: bool,
    pub pid: u32,
    pub actual: Option<String>,
}

enum ReadStart {
    Bytes(usize),
    Eof,
    TimedOut,
}

/// Client side of the private worker pipe. Connects anonymously (`TokenImpersonationLevel.Anonymous`)
/// and verifies that the server process is `<install>\BetterRecall.Agent.exe`.
pub struct AgentPipe {
    handle: Arc<PipeHandle>,
}

impl AgentPipe {
    /// Accepts `"BetterRecall.worker.<32 hex>"` with or without the `\\.\pipe\` prefix. `None` when invalid.
    pub fn normalize_name(configured: Option<&str>) -> Option<String> {
        let raw = configured?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        let base = PipeNames::base(trimmed);
        let suffix = base.strip_prefix("BetterRecall.worker.")?;
        if suffix.len() == 32 && suffix.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Some(base.to_string())
        } else {
            None
        }
    }

    /// Install dir = parent of the directory holding the worker exe.
    pub fn install_dir(worker_base_dir: &Path) -> PathBuf {
        let s = worker_base_dir.to_string_lossy();
        let trimmed = s.trim_end_matches(['\\', '/']);
        let parent_join = Path::new(trimmed).join("..");
        full_path(&parent_join).unwrap_or(parent_join)
    }

    /// Expected agent exe path `<install>\BetterRecall.Agent.exe`.
    pub fn expected_agent_image(worker_base_dir: &Path) -> PathBuf {
        InstallLayout::full(&Self::install_dir(worker_base_dir), InstallLayout::AGENT_EXE)
    }

    /// Connects to `\\.\pipe\<name>` with overlapped I/O and anonymous impersonation level.
    pub fn connect(name: &str, timeout: Duration) -> io::Result<Self> {
        let base = PipeNames::base(name);
        let pipe_path = format!(r"\\.\pipe\{base}");
        let wide: Vec<u16> = pipe_path.encode_utf16().chain(Some(0)).collect();
        let flags = FILE_FLAGS_AND_ATTRIBUTES(
            FILE_FLAG_OVERLAPPED.0 | SECURITY_SQOS_PRESENT.0 | SECURITY_ANONYMOUS.0,
        );
        let deadline = Instant::now() + timeout;

        loop {
            match unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    flags,
                    None,
                )
            } {
                Ok(h) => {
                    return Ok(Self {
                        handle: Arc::new(PipeHandle(h)),
                    });
                }
                Err(e) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(win32_to_io(e));
                    }
                    if e.code() == ERROR_PIPE_BUSY.to_hresult() {
                        let rem_ms = (deadline - now)
                            .as_millis()
                            .clamp(1, (u32::MAX - 1) as u128)
                            as u32;
                        let _ = unsafe { WaitNamedPipeW(PCWSTR(wide.as_ptr()), rem_ms) };
                    } else if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() {
                        std::thread::sleep(Duration::from_millis(20).min(deadline - now));
                    } else {
                        return Err(win32_to_io(e));
                    }
                }
            }
        }
    }

    /// Wraps an already-opened overlapped pipe handle (used by tests).
    pub fn from_handle(handle: HANDLE) -> Self {
        Self {
            handle: Arc::new(PipeHandle(handle)),
        }
    }

    /// Returns the pipe server process ID (`GetNamedPipeServerProcessId`).
    pub fn server_process_id(&self) -> io::Result<u32> {
        let mut pid = 0u32;
        unsafe { GetNamedPipeServerProcessId(self.handle.0, &mut pid) }.map_err(win32_to_io)?;
        Ok(pid)
    }

    /// Full image path of a process (`QueryFullProcessImageNameW`), or `None`.
    pub fn image_path(pid: u32) -> Option<String> {
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let _guard = PipeHandle(h);
        let mut buf = vec![0u16; 32_768];
        let mut size = buf.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                h,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut size,
            )
        }
        .ok()?;
        Some(OsString::from_wide(&buf[..size as usize]).to_string_lossy().into_owned())
    }

    /// Resolves junctions/symlinks/SUBST via `GetFinalPathNameByHandleW`, stripping `\\?\` / `\\?\UNC\`.
    /// Falls back to `full_path` if the file cannot be opened.
    pub fn final_path(path: &Path) -> PathBuf {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let Ok(h) = (unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }) else {
            return full_path(path).unwrap_or_else(|_| path.to_path_buf());
        };
        let _guard = PipeHandle(h);

        let mut buf = vec![0u16; 1024];
        let mut ret = unsafe { GetFinalPathNameByHandleW(h, &mut buf, FILE_NAME_NORMALIZED) };
        if ret as usize >= buf.len() {
            buf.resize(ret as usize + 1, 0);
            ret = unsafe { GetFinalPathNameByHandleW(h, &mut buf, FILE_NAME_NORMALIZED) };
        }
        if ret == 0 || ret as usize >= buf.len() {
            return full_path(path).unwrap_or_else(|_| path.to_path_buf());
        }
        let s = OsString::from_wide(&buf[..ret as usize])
            .to_string_lossy()
            .into_owned();
        if s.len() >= 8 && s[..8].eq_ignore_ascii_case(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{}", &s[8..]))
        } else if s.len() >= 4 && s[..4].eq_ignore_ascii_case(r"\\?\") {
            PathBuf::from(&s[4..])
        } else {
            PathBuf::from(s)
        }
    }

    /// Verifies that the pipe server process image matches `expected_image` (case-insensitive, link-resolved).
    pub fn verify_server(&self, expected_image: &Path) -> io::Result<ServerVerification> {
        let pid = self.server_process_id()?;
        let actual = Self::image_path(pid);
        let Some(ref actual_str) = actual else {
            return Ok(ServerVerification {
                trusted: false,
                pid,
                actual: None,
            });
        };
        let a_final = Self::final_path(Path::new(actual_str));
        let e_final = Self::final_path(expected_image);
        let trusted = ordinal_eq_ignore_case(&a_final, &e_final);
        Ok(ServerVerification {
            trusted,
            pid,
            actual,
        })
    }

    /// True when the agent has written bytes this end has not read yet (idle-exit race check).
    pub fn has_pending_input(&self) -> bool {
        let mut available = 0u32;
        unsafe { PeekNamedPipe(self.handle.0, None, 0, None, Some(&mut available), None) }.is_ok()
            && available > 0
    }

    fn read_first_with_timeout(
        &self,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<ReadStart, FrameError> {
        let ev = EventHandle::new_manual().map_err(FrameError::Io)?;
        let mut ov = OVERLAPPED {
            hEvent: ev.0,
            ..Default::default()
        };
        let res = unsafe { ReadFile(self.handle.0, Some(buf), None, Some(&mut ov)) };
        match res {
            Ok(()) => self.finish_first_overlapped(&ov),
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                let ms = timeout.as_millis().min((u32::MAX - 1) as u128) as u32;
                let wait = unsafe { WaitForSingleObject(ev.0, ms) };
                if wait == WAIT_OBJECT_0 {
                    self.finish_first_overlapped(&ov)
                } else if wait == WAIT_TIMEOUT {
                    unsafe {
                        let _ = CancelIoEx(self.handle.0, Some(&raw const ov));
                    }
                    let mut n = 0u32;
                    match unsafe { GetOverlappedResult(self.handle.0, &ov, &mut n, true) } {
                        Ok(()) => {
                            if n == 0 {
                                Ok(ReadStart::Eof)
                            } else {
                                Ok(ReadStart::Bytes(n as usize))
                            }
                        }
                        Err(ce) if ce.code() == ERROR_OPERATION_ABORTED.to_hresult() => {
                            Ok(ReadStart::TimedOut)
                        }
                        Err(ce) if is_eof_win32(&ce) => Ok(ReadStart::Eof),
                        Err(ce) => Err(FrameError::Io(win32_to_io(ce))),
                    }
                } else {
                    unsafe {
                        let _ = CancelIoEx(self.handle.0, Some(&raw const ov));
                    }
                    let mut n = 0u32;
                    let _ = unsafe { GetOverlappedResult(self.handle.0, &ov, &mut n, true) };
                    Err(FrameError::Io(io::Error::last_os_error()))
                }
            }
            Err(e) if is_eof_win32(&e) => Ok(ReadStart::Eof),
            Err(e) => Err(FrameError::Io(win32_to_io(e))),
        }
    }

    fn finish_first_overlapped(&self, ov: &OVERLAPPED) -> Result<ReadStart, FrameError> {
        let mut n = 0u32;
        match unsafe { GetOverlappedResult(self.handle.0, ov, &mut n, true) } {
            Ok(()) => {
                if n == 0 {
                    Ok(ReadStart::Eof)
                } else {
                    Ok(ReadStart::Bytes(n as usize))
                }
            }
            Err(e) if is_eof_win32(&e) => Ok(ReadStart::Eof),
            Err(e) => Err(FrameError::Io(win32_to_io(e))),
        }
    }

    fn read_exact_wait(&self, mut buf: &mut [u8]) -> Result<(), FrameError> {
        while !buf.is_empty() {
            let ev = EventHandle::new_manual().map_err(FrameError::Io)?;
            let mut ov = OVERLAPPED {
                hEvent: ev.0,
                ..Default::default()
            };
            let res = unsafe { ReadFile(self.handle.0, Some(buf), None, Some(&mut ov)) };
            match res {
                Ok(()) => {}
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {}
                Err(e) if is_eof_win32(&e) => return Err(FrameError::UnexpectedEof),
                Err(e) => return Err(FrameError::Io(win32_to_io(e))),
            }
            let mut n = 0u32;
            match unsafe { GetOverlappedResult(self.handle.0, &ov, &mut n, true) } {
                Ok(()) => {
                    if n == 0 {
                        return Err(FrameError::UnexpectedEof);
                    }
                    buf = &mut buf[n as usize..];
                }
                Err(e) if is_eof_win32(&e) => return Err(FrameError::UnexpectedEof),
                Err(e) => return Err(FrameError::Io(win32_to_io(e))),
            }
        }
        Ok(())
    }

    fn write_all_wait(&self, mut buf: &[u8]) -> Result<(), FrameError> {
        while !buf.is_empty() {
            let ev = EventHandle::new_manual().map_err(FrameError::Io)?;
            let mut ov = OVERLAPPED {
                hEvent: ev.0,
                ..Default::default()
            };
            let res = unsafe { WriteFile(self.handle.0, Some(buf), None, Some(&mut ov)) };
            match res {
                Ok(()) => {}
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {}
                Err(e) => return Err(FrameError::Io(win32_to_io(e))),
            }
            let mut n = 0u32;
            unsafe { GetOverlappedResult(self.handle.0, &ov, &mut n, true) }
                .map_err(|e| FrameError::Io(win32_to_io(e)))?;
            if n == 0 {
                return Err(FrameError::Io(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pipe write returned 0 bytes",
                )));
            }
            buf = &buf[n as usize..];
        }
        Ok(())
    }
}

impl PipeIo for AgentPipe {
    fn read_frame_idle(&self, idle_exit: Duration) -> Result<FrameRead, FrameError> {
        let mut len_buf = [0u8; 4];
        match self.read_first_with_timeout(&mut len_buf, idle_exit)? {
            ReadStart::TimedOut => Ok(FrameRead::TimedOut),
            ReadStart::Eof => Ok(FrameRead::Eof),
            ReadStart::Bytes(first) => {
                if first < 4 {
                    self.read_exact_wait(&mut len_buf[first..])?;
                }
                let header_len = BinaryFrame::validate_header_len(u32::from_le_bytes(len_buf))?;
                let mut header = vec![0u8; header_len];
                self.read_exact_wait(&mut header)?;
                self.read_exact_wait(&mut len_buf)?;
                let payload_len = BinaryFrame::validate_payload_len(u32::from_le_bytes(len_buf))?;
                let mut payload = vec![0u8; payload_len];
                self.read_exact_wait(&mut payload)?;
                Ok(FrameRead::Frame(header, payload))
            }
        }
    }

    fn has_pending_input(&self) -> bool {
        AgentPipe::has_pending_input(self)
    }

    fn write_frame(&self, header: &[u8], payload: &[u8]) -> Result<(), FrameError> {
        let head = BinaryFrame::build_head(header, payload.len())?;
        self.write_all_wait(&head)?;
        if !payload.is_empty() {
            self.write_all_wait(payload)?;
        }
        Ok(())
    }
}

fn ordinal_eq_ignore_case(a: &Path, b: &Path) -> bool {
    let aw: Vec<u16> = a.as_os_str().encode_wide().collect();
    let bw: Vec<u16> = b.as_os_str().encode_wide().collect();
    unsafe { CompareStringOrdinal(&aw, &bw, true) == CSTR_EQUAL }
}

fn is_eof_win32(e: &windows::core::Error) -> bool {
    let c = e.code();
    c == ERROR_BROKEN_PIPE.to_hresult()
        || c == ERROR_PIPE_NOT_CONNECTED.to_hresult()
        || c == ERROR_HANDLE_EOF.to_hresult()
}

pub fn win32_to_io(e: windows::core::Error) -> io::Error {
    let raw = e.code().0 as u32;
    if (raw & 0xFFFF_0000) == 0x8007_0000 {
        io::Error::from_raw_os_error((raw & 0xFFFF) as i32)
    } else {
        io::Error::other(e.to_string())
    }
}
