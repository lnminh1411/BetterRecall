// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Instant;

use br_contracts::{BinaryFrame, WhisperLanguages, WorkerRequest, WorkerResponse};

use crate::engine::{IWorkerEngine, ImageInput, TextKind};
use crate::hosting::{Log, WorkerSettings};
use crate::protocol::errors::{WorkerError, WorkerErrors};
use crate::vectors::VectorMath;

/// One decoded response: header plus optional payload.
#[derive(Debug, Clone)]
pub struct WorkerReply {
    pub header: WorkerResponse,
    pub payload: Vec<u8>,
    pub shutdown: bool,
}

/// Validates one request, calls the engine, and maps errors to the section 4 error strings.
pub struct RequestHandler<'a> {
    engine: &'a dyn IWorkerEngine,
    settings: Box<dyn Fn() -> WorkerSettings + Send + Sync + 'a>,
    missing_logged: Mutex<HashSet<String>>,
}

impl<'a> RequestHandler<'a> {
    pub const IMAGE_FORMATS: [&'static str; 4] = ["webp", "jpeg", "png", "bgra8"];
    pub const MAX_IMAGE_SIDE: i32 = 16_384;

    pub fn new<F>(engine: &'a dyn IWorkerEngine, settings: F) -> Self
    where
        F: Fn() -> WorkerSettings + Send + Sync + 'a,
    {
        Self {
            engine,
            settings: Box::new(settings),
            missing_logged: Mutex::new(HashSet::new()),
        }
    }

    pub fn handle(&self, header_bytes: &[u8], payload: &[u8]) -> WorkerReply {
        let req: WorkerRequest = match serde_json::from_slice(header_bytes) {
            Ok(r) => r,
            Err(_) => {
                return fail(
                    Self::try_read_id(header_bytes),
                    WorkerErrors::BAD_REQUEST,
                    "header json",
                );
            }
        };
        if req.op.is_empty() {
            return fail(
                Self::try_read_id(header_bytes),
                WorkerErrors::BAD_REQUEST,
                "no op",
            );
        }

        let sw = Instant::now();
        match self.dispatch(&req, payload, sw) {
            Ok(reply) => {
                Log::debug(&format!(
                    "op={} id={} ok={} in={}B ms={:.1}",
                    req.op,
                    req.id,
                    reply.header.ok,
                    payload.len(),
                    sw.elapsed().as_secs_f64() * 1000.0
                ));
                reply
            }
            Err(WorkerError::Internal(msg)) => {
                Log::error(&format!("op={} id={} internal: {msg}", req.op, req.id));
                WorkerReply {
                    header: WorkerResponse::err(req.id, WorkerErrors::INTERNAL),
                    payload: Vec::new(),
                    shutdown: false,
                }
            }
            Err(e) => {
                let wire = e.wire_message();
                if self.should_log_failure(&req.op, &wire) {
                    let suffix = e
                        .bad_request_why()
                        .map(|w| format!(" ({w})"))
                        .unwrap_or_default();
                    Log::info(&format!(
                        "op={} id={} failed: {wire}{suffix}",
                        req.op, req.id
                    ));
                }
                WorkerReply {
                    header: WorkerResponse::err(req.id, wire),
                    payload: Vec::new(),
                    shutdown: false,
                }
            }
        }
    }

    /// A missing model fails every request of its op (one per keyframe for ocr): logged once per op and model
    /// per worker start, not once per request (sweep 3 D19). Other failures are always logged.
    pub fn should_log_failure(&self, op: &str, message: &str) -> bool {
        if !message.starts_with("Model not installed") {
            return true;
        }
        let mut set = self.missing_logged.lock().expect("missing_logged lock");
        set.insert(format!("{op}|{message}"))
    }

    fn dispatch(
        &self,
        r: &WorkerRequest,
        payload: &[u8],
        sw: Instant,
    ) -> Result<WorkerReply, WorkerError> {
        let s = (self.settings)();
        match r.op.as_str() {
            "hello" => {
                let mut resp = WorkerResponse::ok(r.id);
                resp.hello = Some(self.engine.hello(s.dim)?);
                Ok(WorkerReply {
                    header: resp,
                    payload: Vec::new(),
                    shutdown: false,
                })
            }
            "embed_image" => {
                let image = Self::validate_image(r, payload)?;
                if let Some(t) = r.nsfw
                    && (t.is_nan() || t <= 0.0 || t > 1.0)
                {
                    return Err(WorkerError::bad_request("nsfw range"));
                }
                if let Some(g) = r.nsfw_grid
                    && !(1..=3).contains(&g)
                {
                    return Err(WorkerError::bad_request("nsfw grid"));
                }
                if let Some(dt) = r.nsfw_detector
                    && (dt.is_nan() || dt <= 0.0 || dt > 1.0)
                {
                    return Err(WorkerError::bad_request("nsfw detector range"));
                }
                let result = self.engine.embed_image(
                    &image,
                    r.nsfw,
                    r.nsfw_grid.unwrap_or(1),
                    r.nsfw_detector,
                )?;
                if let Some(dropped) = result.dropped {
                    let mut resp = WorkerResponse::ok(r.id);
                    resp.ms = Some(ms(sw));
                    resp.dropped = Some(dropped);
                    return Ok(WorkerReply {
                        header: self.served(resp),
                        payload: Vec::new(),
                        shutdown: false,
                    });
                }
                let vec_full = result.vector.unwrap_or_default();
                let v = finish(&vec_full, s.dim)?;
                let mut resp = WorkerResponse::ok(r.id);
                resp.dim = Some(v.len() as i32);
                resp.ms = Some(ms(sw));
                Ok(WorkerReply {
                    header: self.served(resp),
                    payload: BinaryFrame::floats_to_bytes(&v),
                    shutdown: false,
                })
            }
            "embed_text" => {
                let text = r
                    .text
                    .as_deref()
                    .ok_or_else(|| WorkerError::bad_request("text missing"))?;
                let kind = match r.kind.as_deref() {
                    Some("query") => TextKind::Query,
                    Some("document") => TextKind::Document,
                    _ => return Err(WorkerError::bad_request("kind")),
                };
                let full = self.engine.embed_text(text, kind)?;
                let v = finish(&full, s.dim)?;
                let mut resp = WorkerResponse::ok(r.id);
                resp.dim = Some(v.len() as i32);
                resp.ms = Some(ms(sw));
                Ok(WorkerReply {
                    header: self.served(resp),
                    payload: BinaryFrame::floats_to_bytes(&v),
                    shutdown: false,
                })
            }
            "transcribe" => {
                if r.sample_rate != Some(16_000)
                    || r.channels != Some(1)
                    || r.format.as_deref() != Some("f32")
                {
                    return Err(WorkerError::bad_request("audio format"));
                }
                let source = match r.source.as_deref() {
                    Some(src @ ("system" | "mic")) => src,
                    _ => return Err(WorkerError::bad_request("source")),
                };
                if payload.is_empty() || !payload.len().is_multiple_of(4) {
                    return Err(WorkerError::bad_request("pcm length"));
                }
                let lang = match r.language.as_deref() {
                    Some(l) if !l.trim().is_empty() => l.trim().to_ascii_lowercase(),
                    _ => s.audio_language.clone(),
                };
                if !Self::is_language(&lang) {
                    return Err(WorkerError::bad_request("language"));
                }
                // E2: zeroize decoded PCM buffer on drop (including early error returns).
                let mut pcm = crate::ZeroOnDrop::new(Vec::with_capacity(payload.len() / 4));
                for chunk in payload.chunks_exact(4) {
                    let x = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    if !x.is_finite() {
                        return Err(WorkerError::bad_request("pcm not finite"));
                    }
                    pcm.push(x);
                }
                let t = self.engine.transcribe(&pcm, source, &lang)?;
                let mut resp = WorkerResponse::ok(r.id);
                resp.ms = Some(ms(sw));
                resp.transcript = Some(t);
                Ok(WorkerReply {
                    header: resp,
                    payload: Vec::new(),
                    shutdown: false,
                })
            }
            "ocr" => {
                let image = Self::validate_image(r, payload)?;
                let mut result = self.engine.ocr(&image, r.entities == Some(true))?;
                result.ms = ms(sw);
                let mut resp = WorkerResponse::ok(r.id);
                resp.ms = Some(ms(sw));
                resp.ocr = Some(result);
                Ok(WorkerReply {
                    header: resp,
                    payload: Vec::new(),
                    shutdown: false,
                })
            }
            "bench" => {
                let b = self.engine.bench()?;
                let mut resp = WorkerResponse::ok(r.id);
                resp.ms = Some(ms(sw));
                resp.bench = Some(b);
                Ok(WorkerReply {
                    header: self.served(resp),
                    payload: Vec::new(),
                    shutdown: false,
                })
            }
            "shutdown" => Ok(WorkerReply {
                header: WorkerResponse::ok(r.id),
                payload: Vec::new(),
                shutdown: true,
            }),
            _ => Err(WorkerError::bad_request("op")),
        }
    }

    /// Stamps the embedder that served the request (A-M3): the agent files the vector under it
    /// and follows a model or device switch that came without a new hello.
    fn served(&self, mut r: WorkerResponse) -> WorkerResponse {
        if let Some(e) = self.engine.embedder() {
            r.model = Some(e.model);
            r.revision = Some(e.revision);
            r.device = Some(e.device);
        }
        r
    }

    pub fn validate_image<'p>(
        r: &WorkerRequest,
        payload: &'p [u8],
    ) -> Result<ImageInput<'p>, WorkerError> {
        let Some(ref fmt) = r.format else {
            return Err(WorkerError::bad_request("format"));
        };
        if !Self::IMAGE_FORMATS.contains(&fmt.as_str()) {
            return Err(WorkerError::bad_request("format"));
        }
        if payload.is_empty() {
            return Err(WorkerError::bad_request("empty image"));
        }
        if fmt == "bgra8" {
            let (Some(w), Some(h), Some(stride)) = (r.width, r.height, r.stride) else {
                return Err(WorkerError::bad_request("bgra8 shape"));
            };
            if w <= 0 || h <= 0 || w > Self::MAX_IMAGE_SIDE || h > Self::MAX_IMAGE_SIDE {
                return Err(WorkerError::bad_request("bgra8 size"));
            }
            if (stride as i64) < (w as i64) * 4 {
                return Err(WorkerError::bad_request("stride"));
            }
            let need = (stride as i64) * (h as i64 - 1) + (w as i64) * 4;
            if (payload.len() as i64) < need {
                return Err(WorkerError::bad_request("bgra8 payload"));
            }
            return Ok(ImageInput {
                format: "bgra8",
                width: w,
                height: h,
                stride,
                bytes: payload,
            });
        }
        let fmt_static = Self::IMAGE_FORMATS
            .iter()
            .copied()
            .find(|&f| f == fmt)
            .unwrap_or("png");
        Ok(ImageInput {
            format: fmt_static,
            width: r.width.unwrap_or(0),
            height: r.height.unwrap_or(0),
            stride: 0,
            bytes: payload,
        })
    }

    /// `"auto"`, or a code Whisper has a language token for (`WhisperLanguages`):
    /// an unknown code is a bad request, not a `"Device failed"` from the Whisper runtime.
    pub fn is_language(lang: &str) -> bool {
        WhisperLanguages::is_valid(lang)
    }

    /// Best-effort `id` from a JSON header that failed to deserialize as a whole.
    pub fn try_read_id(header: &[u8]) -> i64 {
        let mut p = 0usize;
        skip_ws(header, &mut p);
        if header.get(p) != Some(&b'{') {
            return 0;
        }
        p += 1;
        loop {
            skip_ws(header, &mut p);
            if p >= header.len() || header[p] == b'}' {
                return 0;
            }
            if header[p] == b',' {
                p += 1;
                continue;
            }
            let Some(key) = read_json_string(header, &mut p) else {
                return 0;
            };
            skip_ws(header, &mut p);
            if header.get(p) != Some(&b':') {
                return 0;
            }
            p += 1;
            skip_ws(header, &mut p);
            if key == "id" {
                return read_json_i64(header, &mut p).unwrap_or(0);
            }
            if !skip_json_value(header, &mut p) {
                return 0;
            }
        }
    }
}

fn finish(full: &[f32], dim: i32) -> Result<Vec<f32>, WorkerError> {
    if VectorMath::has_non_finite(full) {
        return Err(WorkerError::DeviceFailed(
            "model output contains NaN".to_string(),
        ));
    }
    if dim <= 0 || (full.len() as i32) < dim {
        return Err(WorkerError::DeviceFailed(format!(
            "model dimension {} below index.dim {dim}",
            full.len()
        )));
    }
    VectorMath::matryoshka(full, dim as usize).map_err(WorkerError::DeviceFailed)
}

fn ms(sw: Instant) -> f64 {
    let raw = sw.elapsed().as_secs_f64() * 1000.0;
    (raw * 100.0).round() / 100.0
}

fn fail(id: i64, error: &str, why: &str) -> WorkerReply {
    Log::info(&format!("request rejected: {why}"));
    WorkerReply {
        header: WorkerResponse::err(id, error),
        payload: Vec::new(),
        shutdown: false,
    }
}

fn skip_ws(b: &[u8], p: &mut usize) {
    while *p < b.len() && matches!(b[*p], b' ' | b'\t' | b'\r' | b'\n') {
        *p += 1;
    }
}

fn read_json_string(b: &[u8], p: &mut usize) -> Option<String> {
    if b.get(*p) != Some(&b'"') {
        return None;
    }
    *p += 1;
    let start = *p;
    let mut has_escape = false;
    while *p < b.len() {
        match b[*p] {
            b'\\' => {
                has_escape = true;
                *p += 1;
                if *p >= b.len() {
                    return None;
                }
                *p += 1;
            }
            b'"' => {
                let slice = &b[start..*p];
                *p += 1;
                if !has_escape {
                    return std::str::from_utf8(slice).ok().map(str::to_string);
                }
                return serde_json::from_slice::<String>(&b[start - 1..*p]).ok();
            }
            _ => *p += 1,
        }
    }
    None
}

fn read_json_i64(b: &[u8], p: &mut usize) -> Option<i64> {
    let start = *p;
    if b.get(*p) == Some(&b'-') {
        *p += 1;
    }
    let digit_start = *p;
    while *p < b.len() && b[*p].is_ascii_digit() {
        *p += 1;
    }
    if *p == digit_start {
        return None;
    }
    if *p < b.len() && matches!(b[*p], b'.' | b'e' | b'E') {
        return None;
    }
    std::str::from_utf8(&b[start..*p]).ok()?.parse::<i64>().ok()
}

fn skip_json_value(b: &[u8], p: &mut usize) -> bool {
    skip_ws(b, p);
    if *p >= b.len() {
        return false;
    }
    match b[*p] {
        b'"' => read_json_string(b, p).is_some(),
        b'{' | b'[' => {
            let mut depth = 0usize;
            while *p < b.len() {
                match b[*p] {
                    b'"' => {
                        if read_json_string(b, p).is_none() {
                            return false;
                        }
                    }
                    b'{' | b'[' => {
                        depth += 1;
                        *p += 1;
                    }
                    b'}' | b']' => {
                        depth -= 1;
                        *p += 1;
                        if depth == 0 {
                            return true;
                        }
                    }
                    _ => *p += 1,
                }
            }
            false
        }
        _ => {
            let start = *p;
            while *p < b.len()
                && !matches!(
                    b[*p],
                    b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n'
                )
            {
                *p += 1;
            }
            *p > start
        }
    }
}
