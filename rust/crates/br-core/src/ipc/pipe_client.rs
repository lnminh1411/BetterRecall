// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Client side of the line protocol with trust checks (`PipeClient.cs`).

use std::io;
use std::time::{Duration, Instant};

use br_contracts::{LineError, PipeNames};
use chrono::{DateTime, Utc};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, OPEN_EXISTING,
    SECURITY_ANONYMOUS, SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::Pipes::{
    GetNamedPipeServerProcessId, GetNamedPipeServerSessionId, WaitNamedPipeW,
};
use windows::core::PCWSTR;

use crate::cancel::CancelToken;
use crate::ipc::pipe_stream::{PipeLineReader, PipeStream};
use crate::ipc::process_info::ProcessInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeConnectResult {
    Ok,
    NotRunning,
    NotTrusted,
}

/// Client side of the line protocol with research 7.4 client checks:
/// anonymous impersonation level, 1 s default connect timeout, server image path must equal the
/// expected agent path, same session, and PID-reuse check.
pub struct PipeClient {
    stream: PipeStream,
    reader: PipeLineReader,
    server_pid: i32,
}

impl PipeClient {
    pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

    pub fn server_pid(&self) -> i32 {
        self.server_pid
    }

    /// Connects and runs the trust check. Client is `None` unless the result is `Ok`.
    pub fn connect(
        pipe_name: &str,
        expected_server_image: &str,
        timeout_ms: u32,
        ct: &CancelToken,
    ) -> (PipeConnectResult, Option<Self>) {
        Self::connect_with_clock(pipe_name, expected_server_image, timeout_ms, Utc::now, ct)
    }

    /// `wall_clock`: tests step the clock without touching the PC's.
    pub fn connect_with_clock(
        pipe_name: &str,
        expected_server_image: &str,
        timeout_ms: u32,
        wall_clock: impl FnOnce() -> DateTime<Utc>,
        ct: &CancelToken,
    ) -> (PipeConnectResult, Option<Self>) {
        let base = PipeNames::base(pipe_name);
        let path = format!(r"\\.\pipe\{base}");
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let flags = FILE_FLAGS_AND_ATTRIBUTES(
            FILE_FLAG_OVERLAPPED.0 | SECURITY_SQOS_PRESENT.0 | SECURITY_ANONYMOUS.0,
        );
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);

        let handle = loop {
            if ct.is_cancelled() {
                return (PipeConnectResult::NotRunning, None);
            }
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
                Ok(h) => break h,
                Err(e) => {
                    if e.code() == ERROR_ACCESS_DENIED.to_hresult() {
                        return (PipeConnectResult::NotTrusted, None);
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        return (PipeConnectResult::NotRunning, None);
                    }
                    if e.code() == ERROR_PIPE_BUSY.to_hresult() {
                        let rem_ms = (deadline - now)
                            .as_millis()
                            .clamp(1, (u32::MAX - 1) as u128) as u32;
                        let _ = unsafe { WaitNamedPipeW(PCWSTR(wide.as_ptr()), rem_ms) };
                    } else if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() {
                        std::thread::sleep(Duration::from_millis(20).min(deadline - now));
                    } else {
                        return (PipeConnectResult::NotRunning, None);
                    }
                }
            }
        };

        let Ok(stream) = (unsafe { PipeStream::from_raw_handle(handle) }) else {
            return (PipeConnectResult::NotRunning, None);
        };
        let connected_utc = ProcessInfo::start_bound(wall_clock());

        let mut pid = 0u32;
        let mut session = 0u32;
        let ok_ids = unsafe {
            GetNamedPipeServerProcessId(stream.raw_handle(), &mut pid).is_ok()
                && GetNamedPipeServerSessionId(stream.raw_handle(), &mut session).is_ok()
        };
        if !ok_ids
            || session != ProcessInfo::current_session()
            || ProcessInfo::session_of(pid as i32) != ProcessInfo::current_session()
            || !ProcessInfo::same_path(
                ProcessInfo::image_path_if_started_before(pid as i32, connected_utc).as_deref(),
                Some(expected_server_image),
            )
        {
            return (PipeConnectResult::NotTrusted, None);
        }

        let reader = PipeLineReader::new(stream.clone(), 65_536);
        (
            PipeConnectResult::Ok,
            Some(Self {
                stream,
                reader,
                server_pid: pid as i32,
            }),
        )
    }

    /// Connects without server image trust checks (test helper for raw pipe/wire tests).
    pub fn connect_raw(pipe_name: &str, timeout_ms: u32) -> io::Result<PipeStream> {
        let base = PipeNames::base(pipe_name);
        let path = format!(r"\\.\pipe\{base}");
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let flags = FILE_FLAGS_AND_ATTRIBUTES(
            FILE_FLAG_OVERLAPPED.0 | SECURITY_SQOS_PRESENT.0 | SECURITY_ANONYMOUS.0,
        );
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
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
                Ok(h) => return unsafe { PipeStream::from_raw_handle(h) },
                Err(e) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(crate::win32::win32_to_io(e));
                    }
                    if e.code() == ERROR_PIPE_BUSY.to_hresult() {
                        let rem_ms = (deadline - now)
                            .as_millis()
                            .clamp(1, (u32::MAX - 1) as u128) as u32;
                        let _ = unsafe { WaitNamedPipeW(PCWSTR(wide.as_ptr()), rem_ms) };
                    } else if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() {
                        std::thread::sleep(Duration::from_millis(20).min(deadline - now));
                    } else {
                        return Err(crate::win32::win32_to_io(e));
                    }
                }
            }
        }
    }

    /// Sends one request line and reads one response line. `Ok(None)` when the server closed the pipe.
    pub fn request(&mut self, line: &str, ct: &CancelToken) -> Result<Option<String>, LineError> {
        let clean = line.replace(['\r', '\n'], " ");
        self.stream.write_line(&clean, None, ct)?;
        self.read_line(ct)
    }

    /// Sends one request line with a read/write timeout.
    pub fn request_timeout(
        &mut self,
        line: &str,
        timeout: Duration,
        ct: &CancelToken,
    ) -> Result<Option<String>, LineError> {
        let clean = line.replace(['\r', '\n'], " ");
        self.stream.write_line(&clean, Some(timeout), ct)?;
        self.reader.read_line(Self::MAX_RESPONSE_BYTES, Some(timeout), ct)
    }

    /// Buffered line read, up to 16 MiB. `Ok(None)` at end of stream.
    pub fn read_line(&mut self, ct: &CancelToken) -> Result<Option<String>, LineError> {
        self.reader.read_line(Self::MAX_RESPONSE_BYTES, None, ct)
    }

    /// Buffered line read with timeout.
    pub fn read_line_timeout(
        &mut self,
        timeout: Duration,
        ct: &CancelToken,
    ) -> Result<Option<String>, LineError> {
        self.reader.read_line(Self::MAX_RESPONSE_BYTES, Some(timeout), ct)
    }

    pub fn close(&self) {
        self.stream.close();
    }
}
