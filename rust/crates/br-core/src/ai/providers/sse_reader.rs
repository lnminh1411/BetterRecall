// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Bounded Server-Sent Events (SSE) reader (`SseReader.cs`).

use std::io::Read;

use crate::cancel::CancelToken;
use crate::ai::provider_http::ProviderStreamError;

/// Bounded SSE reader: CR, LF, and CRLF end a line; caps line, event, and whole-stream size.
pub struct SseReader;

impl SseReader {
    /// Longest line accepted (UTF-16 / char count).
    pub const MAX_LINE_CHARS: usize = 1 << 20;
    /// Longest event (data lines joined).
    pub const MAX_EVENT_CHARS: usize = 8 << 20;
    /// Whole stream cap.
    pub const MAX_STREAM_CHARS: usize = 16 << 20;
    pub const TOO_LARGE_MESSAGE: &'static str = "The provider's response is too large.";

    pub fn too_large() -> ProviderStreamError {
        ProviderStreamError::new("invalid_request_error", Self::TOO_LARGE_MESSAGE)
    }

    /// Streams SSE events `(event_name, data)` from `body`, calling `on_event` for each event.
    /// Stops early if `on_event` returns `Ok(false)` (e.g., OpenAI `[DONE]`).
    pub fn read<R: Read, F>(
        body: &mut R,
        ct: &CancelToken,
        mut on_event: F,
    ) -> Result<(), SseReadError>
    where
        F: FnMut(&str, &str) -> Result<bool, SseReadError>,
    {
        let mut lines = BoundedLines::new(body);
        let mut current_event = String::new();
        let mut data = String::new();
        let mut has_data = false;

        while let Some(line) = lines.read_line(ct)? {
            if line.starts_with(':') {
                continue;
            }
            if line.is_empty() {
                if has_data {
                    let cont = on_event(&current_event, &data)?;
                    current_event.clear();
                    data.clear();
                    has_data = false;
                    if !cont {
                        return Ok(());
                    }
                } else {
                    current_event.clear();
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("event:") {
                let val = rest.strip_prefix(' ').unwrap_or(rest);
                current_event.clear();
                current_event.push_str(val);
            } else if let Some(rest) = line.strip_prefix("data:") {
                let val = rest.strip_prefix(' ').unwrap_or(rest);
                if data.len() + val.len() + 1 > Self::MAX_EVENT_CHARS {
                    return Err(SseReadError::Stream(Self::too_large()));
                }
                if has_data {
                    data.push('\n');
                    data.push_str(val);
                } else {
                    data.push_str(val);
                    has_data = true;
                }
            }
        }

        if has_data {
            let _ = on_event(&current_event, &data)?;
        }
        Ok(())
    }
}

/// Errors produced while reading or parsing an SSE stream.
#[derive(Debug, thiserror::Error)]
pub enum SseReadError {
    #[error(transparent)]
    Stream(#[from] ProviderStreamError),
    #[error("Malformed JSON in stream: {0}")]
    InvalidData(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Cancelled")]
    Cancelled,
}

struct BoundedLines<'a, R: Read> {
    reader: &'a mut R,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    total: usize,
    skip_lf: bool,
}

impl<'a, R: Read> BoundedLines<'a, R> {
    fn new(reader: &'a mut R) -> Self {
        Self {
            reader,
            buf: vec![0u8; 4096],
            pos: 0,
            len: 0,
            total: 0,
            skip_lf: false,
        }
    }

    fn read_line(&mut self, ct: &CancelToken) -> Result<Option<String>, SseReadError> {
        let mut acc: Option<Vec<u8>> = None;
        loop {
            if ct.is_cancelled() {
                return Err(SseReadError::Cancelled);
            }
            if self.pos == self.len {
                let n = self.reader.read(&mut self.buf)?;
                self.pos = 0;
                self.len = n;
                if n == 0 {
                    return Ok(acc.map(|b| String::from_utf8_lossy(&b).into_owned()));
                }
                self.total += n;
                if self.total > SseReader::MAX_STREAM_CHARS {
                    return Err(SseReadError::Stream(SseReader::too_large()));
                }
            }
            if self.skip_lf {
                self.skip_lf = false;
                if self.buf[self.pos] == b'\n' {
                    self.pos += 1;
                    continue;
                }
            }
            let start = self.pos;
            while self.pos < self.len && self.buf[self.pos] != b'\r' && self.buf[self.pos] != b'\n'
            {
                self.pos += 1;
            }
            let take = self.pos - start;
            let cur_len = acc.as_ref().map_or(0, Vec::len);
            if cur_len + take > SseReader::MAX_LINE_CHARS {
                return Err(SseReadError::Stream(SseReader::too_large()));
            }
            if self.pos < self.len {
                self.skip_lf = self.buf[self.pos] == b'\r';
                self.pos += 1;
                return match acc {
                    None => Ok(Some(
                        String::from_utf8_lossy(&self.buf[start..start + take]).into_owned(),
                    )),
                    Some(mut v) => {
                        v.extend_from_slice(&self.buf[start..start + take]);
                        Ok(Some(String::from_utf8_lossy(&v).into_owned()))
                    }
                };
            }
            acc.get_or_insert_with(Vec::new)
                .extend_from_slice(&self.buf[start..start + take]);
        }
    }
}
