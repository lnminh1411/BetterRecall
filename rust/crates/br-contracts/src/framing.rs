// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors

use std::io::{self, Read, Write};

use thiserror::Error;

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
