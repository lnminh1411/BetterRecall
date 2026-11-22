// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::json::to_json_string;

/// Error returned when a line protocol message exceeds its byte limit.
#[derive(Debug, Error)]
#[error("Line exceeds {max} bytes.")]
pub struct LineTooLongError {
    pub max: usize,
}

/// Errors returned while reading or writing a line protocol message.
#[derive(Debug, Error)]
pub enum LineError {
    #[error("Line exceeds {0} bytes.")]
    TooLong(usize),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Line protocol for the UI, public and MCP pipes: UTF-8, one request per `\n`, one JSON line back.
pub struct LineProtocol;

impl LineProtocol {
    pub const MAX_REQUEST_BYTES: usize = 4096;
    /// UI pipe only: chat questions (4000 characters, escaped in JSON) and settings arrays exceed 4 KiB.
    pub const MAX_UI_REQUEST_BYTES: usize = 64 * 1024;
    pub const TOO_LONG_ERROR: &'static str = "Line exceeds 4096 bytes.";
    pub const TOO_LONG_UI_ERROR: &'static str = "Line exceeds 65536 bytes.";

    /// Reads one line. Strips `\r` and a leading UTF-8 BOM. `Ok(None)` at end of stream.
    pub fn read_line<R: Read>(stream: &mut R, max_bytes: usize) -> Result<Option<String>, LineError> {
        let mut buf = Vec::new();
        let mut one = [0u8; 1];
        loop {
            match stream.read(&mut one) {
                Ok(0) => {
                    return if buf.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(decode_line(&buf)))
                    };
                }
                Ok(_) => {
                    if one[0] == b'\n' {
                        return Ok(Some(decode_line(&buf)));
                    }
                    if buf.len() >= max_bytes {
                        return Err(LineError::TooLong(max_bytes));
                    }
                    buf.push(one[0]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(LineError::Io(e)),
            }
        }
    }

    pub fn write_line<W: Write>(stream: &mut W, line: &str) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        stream.write_all(&bytes)?;
        stream.flush()
    }
}

/// Buffered line reader for one stream. Use one instance per connection.
pub struct LineReader<R> {
    stream: R,
    buf: Vec<u8>,
    start: usize,
    end: usize,
}

impl<R: Read> LineReader<R> {
    pub fn new(stream: R) -> Self {
        Self::with_capacity(stream, 65_536)
    }

    pub fn with_capacity(stream: R, buffer_size: usize) -> Self {
        Self {
            stream,
            buf: vec![0u8; buffer_size.max(1)],
            start: 0,
            end: 0,
        }
    }

    /// Reads one line of at most `max_bytes`. Strips `\r` and a leading BOM. `Ok(None)` at end of stream.
    pub fn read_line(&mut self, max_bytes: usize) -> Result<Option<String>, LineError> {
        let mut acc = Vec::new();
        loop {
            if self.start == self.end {
                self.start = 0;
                loop {
                    match self.stream.read(&mut self.buf) {
                        Ok(0) => {
                            self.end = 0;
                            return if acc.is_empty() {
                                Ok(None)
                            } else {
                                Ok(Some(decode_line(&acc)))
                            };
                        }
                        Ok(n) => {
                            self.end = n;
                            break;
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(LineError::Io(e)),
                    }
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

    pub fn into_inner(self) -> R {
        self.stream
    }
}

fn decode_line(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let trimmed = s.strip_prefix('\u{FEFF}').unwrap_or(&s);
    trimmed.replace('\r', "")
}

/// Typed `{"ok":false,"error":"..."}` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorReply {
    pub ok: bool,
    pub error: String,
}

impl ErrorReply {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: message.into(),
        }
    }
}

/// Typed `{"ok":true,"data":...}` / `{"ok":false,"error":"..."}` envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PipeEnvelope<T = Value> {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `{"ok":true,"data":...}` / `{"ok":false,"error":"..."}` line helpers.
pub struct Envelope;

impl Envelope {
    /// `data_json` must already be valid compact JSON (`"null"` allowed).
    pub fn ok_raw(data_json: &str) -> String {
        format!("{{\"ok\":true,\"data\":{data_json}}}")
    }

    pub fn ok<T: Serialize>(data: &T) -> serde_json::Result<String> {
        Ok(Self::ok_raw(&to_json_string(data)?))
    }

    pub fn error(message: &str) -> String {
        let encoded = to_json_string(&message).expect("string serialization cannot fail");
        format!("{{\"ok\":false,\"error\":{encoded}}}")
    }

    /// Parses a response line into `(ok, data, error)`.
    pub fn parse(line: &str) -> serde_json::Result<(bool, Option<Value>, Option<String>)> {
        let v: Value = serde_json::from_str(line)?;
        let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let data = v.get("data").cloned();
        let error = v.get("error").and_then(Value::as_str).map(String::from);
        Ok((ok, data, error))
    }

    /// Like [`Envelope::parse`], but `data` is returned as its raw JSON text exactly as received
    /// (key order and number formatting kept), as C# `JsonElement.GetRawText()` does.
    pub fn parse_raw(line: &str) -> serde_json::Result<(bool, Option<String>, Option<String>)> {
        let map: std::collections::HashMap<String, Box<serde_json::value::RawValue>> =
            serde_json::from_str(line)?;
        let ok = map
            .get("ok")
            .and_then(|r| serde_json::from_str::<Value>(r.get()).ok())
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let data = map.get("data").map(|r| r.get().to_string());
        let error = map
            .get("error")
            .and_then(|r| serde_json::from_str::<Value>(r.get()).ok())
            .and_then(|v| v.as_str().map(String::from));
        Ok((ok, data, error))
    }
}

/// Errors returned while reading or writing a binary worker pipe frame.
#[derive(Debug, Error)]
pub enum FrameError {
    #[error("Frame header too large.")]
    HeaderTooLarge,
    #[error("Frame payload too large.")]
    PayloadTooLarge,
    #[error("Pipe closed mid-frame.")]
    UnexpectedEof,
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Worker pipe frame: u32 LE headerLen, JSON header, u32 LE payloadLen, payload.
/// One frame as read: (header JSON bytes, payload bytes).
pub type Frame = (Vec<u8>, Vec<u8>);

pub struct BinaryFrame;

impl BinaryFrame {
    /// OCR and transcript replies travel in the header: a dense 4K or CJK screen is several hundred KB of JSON
    /// (C1: 64 KiB killed the worker on one code screen). Both ends read this constant.
    pub const MAX_HEADER_BYTES: usize = 4 * 1024 * 1024;
    pub const MAX_PAYLOAD_BYTES: usize = 67_108_864;

    /// Validates sizes and builds `[header_len_le, header, payload_len_le]` so lengths and header
    /// go out in a single write on 0-byte-buffer pipes.
    pub fn build_head(header: &[u8], payload_len: usize) -> Result<Vec<u8>, FrameError> {
        if header.len() > Self::MAX_HEADER_BYTES {
            return Err(FrameError::HeaderTooLarge);
        }
        if payload_len > Self::MAX_PAYLOAD_BYTES {
            return Err(FrameError::PayloadTooLarge);
        }
        let mut head = Vec::with_capacity(8 + header.len());
        head.extend_from_slice(&(header.len() as u32).to_le_bytes());
        head.extend_from_slice(header);
        head.extend_from_slice(&(payload_len as u32).to_le_bytes());
        Ok(head)
    }

    /// Validates header length and returns the parsed usize length.
    pub fn validate_header_len(len: u32) -> Result<usize, FrameError> {
        let n = len as usize;
        if n > Self::MAX_HEADER_BYTES {
            return Err(FrameError::HeaderTooLarge);
        }
        Ok(n)
    }

    /// Validates payload length and returns the parsed usize length.
    pub fn validate_payload_len(len: u32) -> Result<usize, FrameError> {
        let n = len as usize;
        if n > Self::MAX_PAYLOAD_BYTES {
            return Err(FrameError::PayloadTooLarge);
        }
        Ok(n)
    }

    /// Writes one frame (header + payload) to `stream` and flushes.
    pub fn write<W: Write>(
        stream: &mut W,
        header: &[u8],
        payload: &[u8],
    ) -> Result<(), FrameError> {
        let head = Self::build_head(header, payload.len())?;
        // Lengths and header in one write (the payload follows): on the 0-byte-buffer worker pipe every write waits for
        // the reader, so fewer writes means fewer points where a frame sits half sent.
        stream.write_all(&head)?;
        if !payload.is_empty() {
            stream.write_all(payload)?;
        }
        stream.flush()?;
        Ok(())
    }

    /// Reads one frame from `stream`. Returns `Ok(None)` at clean EOF before any byte is read.
    pub fn read<R: Read>(stream: &mut R) -> Result<Option<Frame>, FrameError> {
        let mut len_buf = [0u8; 4];
        let first = read_at_least_one_or_eof(stream, &mut len_buf)?;
        if first == 0 {
            return Ok(None);
        }
        if first < 4 {
            fill_exact(stream, &mut len_buf[first..])?;
        }
        let header_len = Self::validate_header_len(u32::from_le_bytes(len_buf))?;
        let mut header = vec![0u8; header_len];
        fill_exact(stream, &mut header)?;
        fill_exact(stream, &mut len_buf)?;
        let payload_len = Self::validate_payload_len(u32::from_le_bytes(len_buf))?;
        let mut payload = vec![0u8; payload_len];
        fill_exact(stream, &mut payload)?;
        Ok(Some((header, payload)))
    }

    /// Encodes `f32` samples as little-endian bytes.
    pub fn floats_to_bytes(v: &[f32]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(v.len() * 4);
        for &x in v {
            bytes.extend_from_slice(&x.to_le_bytes());
        }
        bytes
    }

    /// Decodes little-endian `f32` samples from bytes (truncating any trailing partial float, matching C#).
    pub fn bytes_to_floats(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }
}

fn read_at_least_one_or_eof<R: Read>(stream: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match stream.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn fill_exact<R: Read>(stream: &mut R, mut buf: &mut [u8]) -> Result<(), FrameError> {
    while !buf.is_empty() {
        match stream.read(buf) {
            Ok(0) => return Err(FrameError::UnexpectedEof),
            Ok(n) => buf = &mut buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    Ok(())
}
