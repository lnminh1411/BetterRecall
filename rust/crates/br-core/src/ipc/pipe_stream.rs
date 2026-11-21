// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Overlapped Win32 named-pipe handle wrapper with instant cancellation, timeouts, and buffered line reading.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use br_contracts::LineError;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED,
    ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::ConnectNamedPipe;
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects,
};

use crate::cancel::CancelToken;
use crate::win32::win32_to_io;

struct EventHandle(HANDLE);

unsafe impl Send for EventHandle {}
unsafe impl Sync for EventHandle {}

impl EventHandle {
    fn new_manual() -> io::Result<Self> {
        let h = unsafe { CreateEventW(None, true, false, None) }.map_err(win32_to_io)?;
        Ok(Self(h))
    }

    fn signal(&self) {
        unsafe {
            let _ = SetEvent(self.0);
        }
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

struct PipeInner {
    handle: HANDLE,
    closed: AtomicBool,
    close_event: EventHandle,
}

unsafe impl Send for PipeInner {}
unsafe impl Sync for PipeInner {}

impl Drop for PipeInner {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // No DisconnectNamedPipe: it discards unread data, so a client would lose the last line
            // ("Client not allowed.", "Line exceeds 4096 bytes."). Closing the handle keeps it readable.
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

/// Shareable overlapped Win32 named-pipe handle.
///
/// Cloning returns a handle to the same underlying pipe so another thread can call [`Self::close`]
/// (e.g. `PipeServer::drop_where` or `PipeServer::stop`) or read concurrently with writes.
#[derive(Clone)]
pub struct PipeStream {
    inner: Arc<PipeInner>,
}

impl PipeStream {
    /// Wraps an already-opened overlapped Win32 pipe handle.
    ///
    /// # Safety
    /// `handle` must be a valid Win32 pipe handle opened with `FILE_FLAG_OVERLAPPED`, owned
    /// exclusively by the caller (it will be closed when the last `PipeStream` clone is dropped).
    pub unsafe fn from_raw_handle(handle: HANDLE) -> io::Result<Self> {
        Ok(Self {
            inner: Arc::new(PipeInner {
                handle,
                closed: AtomicBool::new(false),
                close_event: EventHandle::new_manual()?,
            }),
        })
    }

    pub fn raw_handle(&self) -> HANDLE {
        self.inner.handle
    }

    pub fn id(&self) -> usize {
        Arc::as_ptr(&self.inner) as usize
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Aborts all in-flight overlapped I/O on this pipe and marks it closed.
    pub fn close(&self) {
        if !self.inner.closed.swap(true, Ordering::AcqRel) {
            self.inner.close_event.signal();
            unsafe {
                let _ = CancelIoEx(self.inner.handle, None);
            }
        }
    }

    /// Waits for a client to connect (`ConnectNamedPipe` overlapped).
    pub fn wait_for_connection(&self, ct: &CancelToken) -> io::Result<()> {
        if self.is_closed() || ct.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let io_ev = EventHandle::new_manual()?;
        let mut ov = OVERLAPPED {
            hEvent: io_ev.0,
            ..Default::default()
        };
        match unsafe { ConnectNamedPipe(self.inner.handle, Some(&mut ov)) } {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => Ok(()),
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                self.wait_overlapped(&io_ev, &ov, None, ct).map(|_| ())
            }
            Err(e) => Err(win32_to_io(e)),
        }
    }

    /// Reads up to `buf.len()` bytes with optional timeout and cancellation.
    /// Returns `Ok(0)` on clean EOF / broken pipe.
    pub fn read(&self, buf: &mut [u8], timeout: Option<Duration>, ct: &CancelToken) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.is_closed() || ct.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let io_ev = EventHandle::new_manual()?;
        let mut ov = OVERLAPPED {
            hEvent: io_ev.0,
            ..Default::default()
        };
        match unsafe { ReadFile(self.inner.handle, Some(buf), None, Some(&mut ov)) } {
            Ok(()) => {
                let mut n = 0u32;
                match unsafe { GetOverlappedResult(self.inner.handle, &ov, &mut n, true) } {
                    Ok(()) => Ok(n as usize),
                    Err(e) if is_eof_win32(&e) => Ok(0),
                    Err(e) => Err(win32_to_io(e)),
                }
            }
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                match self.wait_overlapped(&io_ev, &ov, timeout, ct) {
                    Ok(n) => Ok(n as usize),
                    Err(err) if is_eof_io(&err) => Ok(0),
                    Err(err) => Err(err),
                }
            }
            Err(e) if is_eof_win32(&e) => Ok(0),
            Err(e) => Err(win32_to_io(e)),
        }
    }

    /// Writes all bytes in `buf` with optional timeout and cancellation.
    pub fn write_all(&self, mut buf: &[u8], timeout: Option<Duration>, ct: &CancelToken) -> io::Result<()> {
        while !buf.is_empty() {
            if self.is_closed() || ct.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let io_ev = EventHandle::new_manual()?;
            let mut ov = OVERLAPPED {
                hEvent: io_ev.0,
                ..Default::default()
            };
            let written = match unsafe { WriteFile(self.inner.handle, Some(buf), None, Some(&mut ov)) } {
                Ok(()) => {
                    let mut n = 0u32;
                    unsafe { GetOverlappedResult(self.inner.handle, &ov, &mut n, true) }
                        .map_err(win32_to_io)?;
                    n as usize
                }
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                    self.wait_overlapped(&io_ev, &ov, timeout, ct)? as usize
                }
                Err(e) => return Err(win32_to_io(e)),
            };
            if written == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "pipe write returned 0 bytes"));
            }
            buf = &buf[written..];
        }
        Ok(())
    }

    /// Writes one UTF-8 line terminated with `\n`.
    pub fn write_line(&self, line: &str, timeout: Option<Duration>, ct: &CancelToken) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        self.write_all(&bytes, timeout, ct)
    }

    fn wait_overlapped(
        &self,
        io_ev: &EventHandle,
        ov: &OVERLAPPED,
        timeout: Option<Duration>,
        ct: &CancelToken,
    ) -> io::Result<u32> {
        let cancel_ev = Arc::new(EventHandle::new_manual()?);
        let cancel_ev_cb = cancel_ev.clone();
        let _reg = ct.on_cancel(move || cancel_ev_cb.signal());

        let handles = [io_ev.0, self.inner.close_event.0, cancel_ev.0];
        let wait_ms = match timeout {
            Some(d) => d.as_millis().min((u32::MAX - 1) as u128) as u32,
            None => INFINITE,
        };
        let wait = unsafe { WaitForMultipleObjects(&handles, false, wait_ms) };
        if wait == WAIT_OBJECT_0 {
            let mut n = 0u32;
            unsafe { GetOverlappedResult(self.inner.handle, ov, &mut n, true) }
                .map_err(win32_to_io)?;
            Ok(n)
        } else {
            unsafe {
                let _ = CancelIoEx(self.inner.handle, Some(ov));
            }
            let mut n = 0u32;
            match unsafe { GetOverlappedResult(self.inner.handle, ov, &mut n, true) } {
                Ok(()) => Ok(n),
                Err(e) if e.code() == ERROR_OPERATION_ABORTED.to_hresult() => {
                    if wait == WAIT_TIMEOUT {
                        Err(io::Error::new(io::ErrorKind::TimedOut, "pipe operation timed out"))
                    } else {
                        Err(io::Error::new(io::ErrorKind::Interrupted, "pipe operation cancelled"))
                    }
                }
                Err(e) => Err(win32_to_io(e)),
            }
        }
    }
}

/// Buffered line reader over [`PipeStream`] supporting timeouts, cancellation, BOM and `\r` stripping.
pub struct PipeLineReader {
    stream: PipeStream,
    buf: Vec<u8>,
    start: usize,
    end: usize,
}

impl PipeLineReader {
    pub fn new(stream: PipeStream, capacity: usize) -> Self {
        Self {
            stream,
            buf: vec![0u8; capacity.max(1)],
            start: 0,
            end: 0,
        }
    }

    pub fn stream(&self) -> &PipeStream {
        &self.stream
    }

    /// Reads one line of at most `max_bytes`. Strips `\r` and a leading UTF-8 BOM.
    /// Returns `Ok(None)` at clean EOF.
    pub fn read_line(
        &mut self,
        max_bytes: usize,
        timeout: Option<Duration>,
        ct: &CancelToken,
    ) -> Result<Option<String>, LineError> {
        let mut acc = Vec::new();
        loop {
            if self.start == self.end {
                self.start = 0;
                match self.stream.read(&mut self.buf, timeout, ct) {
                    Ok(0) => {
                        self.end = 0;
                        return if acc.is_empty() {
                            Ok(None)
                        } else {
                            Ok(Some(decode_line(&acc)))
                        };
                    }
                    Ok(n) => self.end = n,
                    Err(e) => return Err(LineError::Io(e)),
                }
            }
            let slice = &self.buf[self.start..self.end];
            let nl = slice.iter().position(|&b| b == b'\n');
            let take = nl.unwrap_or(slice.len());
            if acc.len() + take > max_bytes {
                return Err(LineError::TooLong(max_bytes));
            }
            acc.extend_from_slice(&slice[..take]);
            self.start += take;
            if nl.is_some() {
                self.start += 1;
                return Ok(Some(decode_line(&acc)));
            }
        }
    }
}

fn decode_line(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let trimmed = s.strip_prefix('\u{FEFF}').unwrap_or(&s);
    trimmed.replace('\r', "")
}

fn is_eof_win32(e: &windows::core::Error) -> bool {
    let c = e.code();
    c == ERROR_BROKEN_PIPE.to_hresult()
        || c == ERROR_PIPE_NOT_CONNECTED.to_hresult()
        || c == ERROR_HANDLE_EOF.to_hresult()
}

fn is_eof_io(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(109) | Some(233) | Some(38) // ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_HANDLE_EOF
    )
}
