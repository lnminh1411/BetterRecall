// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Cloud describe image redaction hook (`Privacy\DescribeRedactor.cs`).

use std::sync::Arc;
use std::time::Duration;

use br_contracts::Settings;
use br_core::{CancelSource, CancelToken, DescribeError, DescribeRedactor, OutgoingImage};
use zeroize::Zeroize;

use crate::capture::image_codec::ImageCodec;
use crate::capture::keyframe_pipeline::{KeyframePipeline, KeyframeReader};
use crate::privacy::redaction_masks::RedactionMasks;
use crate::privacy::redactor::{PixelBox, RedactionOptions, Redactor};
use crate::thumbs::KeyframeBlobStore;
use crate::util::AgentLog;
use crate::worker::WorkerError;

type SettingsProvider = Arc<dyn Fn() -> Settings + Send + Sync>;
type BlobStoreProvider = Arc<dyn Fn() -> Option<Arc<dyn KeyframeBlobStore>> + Send + Sync>;
type OcrWaitProvider = Arc<dyn Fn() -> Duration + Send + Sync>;

/// Runs OCR and `privacy.redact*` detectors on an image about to leave the PC for cloud describe/chat.
pub struct AgentDescribeRedactor {
    reader: Arc<dyn KeyframeReader>,
    settings: SettingsProvider,
    store: Option<BlobStoreProvider>,
    ocr_wait: Option<OcrWaitProvider>,
    min_wait: Duration,
    start_wait: Duration,
}

impl AgentDescribeRedactor {
    const QUALITY: i32 = 90;

    pub const TIMED_OUT: &'static str =
        "Text recognition timed out, so the image was not checked and is not sent. Try again.";
    pub const BUSY: &'static str = "Text recognition is busy with other work, so the image was not checked and is not sent. Try again in a minute.";
    pub const STARTING: &'static str = "Text recognition is starting, so the image was not checked and is not sent. Try again in a minute.";

    pub fn new(
        reader: Arc<dyn KeyframeReader>,
        settings: SettingsProvider,
        store: Option<BlobStoreProvider>,
        ocr_wait: Option<OcrWaitProvider>,
    ) -> Self {
        Self {
            reader,
            settings,
            store,
            ocr_wait,
            min_wait: KeyframePipeline::OCR_TIMEOUT,
            start_wait: Duration::from_secs(60),
        }
    }

    pub fn with_min_wait(mut self, wait: Duration) -> Self {
        self.min_wait = wait;
        self
    }

    pub fn with_start_wait(mut self, wait: Duration) -> Self {
        self.start_wait = wait;
        self
    }

    /// Scales boxes on a `fw x fh` image onto a `w x h` copy, rounded outwards and grown by the
    /// resize filter's reach (`ceil(1 / scale)` px, at least 1; S12), clipped to the image. Pure.
    pub fn scale_boxes(boxes: &[PixelBox], fw: i32, fh: i32, w: i32, h: i32) -> Vec<PixelBox> {
        let sx = w as f64 / fw as f64;
        let sy = h as f64 / fh as f64;
        let gx = ((1.0 / sx).ceil() as i32).max(1);
        let gy = ((1.0 / sy).ceil() as i32).max(1);
        let mut out = Vec::with_capacity(boxes.len());
        for b in boxes {
            let x0 = ((b.x as f64 * sx).floor() as i32 - gx).max(0);
            let y0 = ((b.y as f64 * sy).floor() as i32 - gy).max(0);
            let x1 = ((b.right() as f64 * sx).ceil() as i32 + gx).min(w);
            let y1 = ((b.bottom() as f64 * sy).ceil() as i32 + gy).min(h);
            if x1 > x0 && y1 > y0 {
                out.push(PixelBox::new(x0, y0, x1 - x0, y1 - y0));
            }
        }
        out
    }

    /// Returns `(Some(bgra), fw, fh, true)` when the stored keyframe is larger than `w x h`,
    /// `(None, 0, 0, true)` when not larger or absent, or `(None, 0, 0, false)` when unreadable.
    fn full_size(&self, capture_id: &str, w: i32, h: i32) -> (Option<Vec<u8>>, i32, i32, bool) {
        let Some(ref store_fn) = self.store else {
            return (None, 0, 0, true);
        };
        let Some(store) = store_fn() else {
            return (None, 0, 0, true);
        };
        let kf = match store.get_keyframe(capture_id) {
            Ok(Some(k)) => k,
            Ok(None) => return (None, 0, 0, true),
            Err(e) => {
                AgentLog::debug(&format!("describe redaction: keyframe read: {e}"));
                return (None, 0, 0, false);
            }
        };
        let mut kf = kf;
        if kf.width <= w && kf.height <= h {
            kf.bytes.zeroize();
            return (None, 0, 0, true);
        }
        let decoded = ImageCodec::decode_bgra(&kf.bytes, &kf.mime);
        let mime = kf.mime.clone();
        kf.bytes.zeroize();
        match decoded {
            Ok((bgra, fw, fh)) => (Some(bgra), fw, fh, true),
            Err(e) => {
                AgentLog::warn_err(&format!("describe redaction: keyframe decode ({mime})"), &e);
                (None, 0, 0, false)
            }
        }
    }
}

impl DescribeRedactor for AgentDescribeRedactor {
    fn redacts_images(&self) -> bool {
        true
    }

    fn redact(
        &self,
        capture_id: &str,
        image: OutgoingImage,
        ct: &CancelToken,
    ) -> Result<OutgoingImage, DescribeError> {
        let s = (self.settings)();
        let red = RedactionOptions::from_privacy(&s.privacy);
        // Own a fresh copy before any work/early return (A2).
        let mut owned = image.bytes.clone();
        if !red.any_text() {
            return Ok(OutgoingImage {
                bytes: owned,
                redacted: true,
                ..image
            });
        }
        if !s.ocr.enabled {
            return Ok(OutgoingImage {
                bytes: owned,
                redacted: false,
                ..image
            });
        }

        let (mut pixels, w, h) = match ImageCodec::decode_bgra(&owned, &image.mime) {
            Ok(v) => v,
            Err(_) => {
                owned.zeroize();
                return Err(DescribeError::Message(
                    "The image could not be read.".to_string(),
                ));
            }
        };
        let stride = w * 4;
        let (mut full, fw, fh, full_read) = self.full_size(capture_id, w, h);

        let wait = self
            .ocr_wait
            .as_ref()
            .map(|f| f())
            .filter(|&aw| aw > self.min_wait)
            .unwrap_or(self.min_wait);

        // Start the worker first, outside the OCR wait (TH2 / S9).
        let ahead = match ready_with_timeout(&*self.reader, self.start_wait, ct) {
            Ok(a) => a,
            Err(ReadyErr::Cancelled) => {
                pixels.zeroize();
                if let Some(ref mut f) = full {
                    f.zeroize();
                }
                owned.zeroize();
                return Err(DescribeError::Cancelled);
            }
            Err(ReadyErr::TimedOut) => {
                pixels.zeroize();
                if let Some(ref mut f) = full {
                    f.zeroize();
                }
                owned.zeroize();
                return Err(DescribeError::Message(Self::STARTING.to_string()));
            }
            Err(ReadyErr::Worker(e)) => {
                pixels.zeroize();
                if let Some(ref mut f) = full {
                    f.zeroize();
                }
                if e.message().starts_with("Model not installed") {
                    return Ok(OutgoingImage {
                        bytes: owned,
                        redacted: false,
                        ..image
                    });
                }
                owned.zeroize();
                AgentLog::warn(&format!("describe redaction: ocr failed ({})", e.message()));
                return Err(DescribeError::Message(
                    "Text recognition failed, so the image was not checked and is not sent. Try again."
                        .to_string(),
                ));
            }
        };

        let ocr_res = if let Some(ref f) = full {
            read_with_timeout(&*self.reader, (f, fw, fh, fw * 4), red.entities(), wait, ct)
        } else {
            read_with_timeout(
                &*self.reader,
                (&pixels, w, h, stride),
                red.entities(),
                wait,
                ct,
            )
        };

        let ocr = match ocr_res {
            Ok(r) => r,
            Err(ReadErr::Cancelled) => {
                pixels.zeroize();
                if let Some(ref mut f) = full {
                    f.zeroize();
                }
                owned.zeroize();
                return Err(DescribeError::Cancelled);
            }
            Err(ReadErr::TimedOut) => {
                pixels.zeroize();
                if let Some(ref mut f) = full {
                    f.zeroize();
                }
                owned.zeroize();
                AgentLog::info(&format!(
                    "describe redaction: ocr took over {:.0} s ({ahead} worker requests ahead of it)",
                    wait.as_secs_f64()
                ));
                return Err(DescribeError::Message(if ahead > 0 {
                    Self::BUSY.to_string()
                } else {
                    Self::TIMED_OUT.to_string()
                }));
            }
            Err(ReadErr::Worker(e)) => {
                pixels.zeroize();
                if let Some(ref mut f) = full {
                    f.zeroize();
                }
                if e.message().starts_with("Model not installed") {
                    return Ok(OutgoingImage {
                        bytes: owned,
                        redacted: false,
                        ..image
                    });
                }
                owned.zeroize();
                AgentLog::warn(&format!("describe redaction: ocr failed ({})", e.message()));
                return Err(DescribeError::Message(
                    "Text recognition failed, so the image was not checked and is not sent. Try again."
                        .to_string(),
                ));
            }
        };

        let checked_all = !(red.entities() && ocr.entities_missing) && full_read;
        if !checked_all {
            AgentLog::warn(&format!(
                "describe redaction: {}, image marked not checked",
                if full_read {
                    "name and address detector unavailable"
                } else {
                    "stored keyframe unreadable, small text unchecked"
                }
            ));
        }

        let r = Redactor::apply(&ocr.lines, &[], red, "");
        if r.boxes.is_empty() {
            pixels.zeroize();
            if let Some(ref mut f) = full {
                f.zeroize();
            }
            return Ok(OutgoingImage {
                bytes: owned,
                redacted: checked_all,
                ..image
            });
        }

        let boxes = if full.is_some() {
            Self::scale_boxes(&r.boxes, fw, fh, w, h)
        } else {
            r.boxes
        };
        unsafe {
            RedactionMasks::fill(pixels.as_mut_ptr(), w, h, stride, &boxes);
        }
        let encoded = unsafe { ImageCodec::encode_bgra(pixels.as_ptr(), w, h, stride, "webp", Self::QUALITY) };
        pixels.zeroize();
        if let Some(ref mut f) = full {
            f.zeroize();
        }
        owned.zeroize();
        match encoded {
            Ok(bytes) => Ok(OutgoingImage {
                bytes,
                mime: ImageCodec::mime("webp").to_string(),
                width: w,
                height: h,
                redacted: checked_all,
            }),
            Err(e) => Err(DescribeError::Message(e)),
        }
    }
}

enum ReadyErr {
    Cancelled,
    TimedOut,
    Worker(WorkerError),
}

enum ReadErr {
    Cancelled,
    TimedOut,
    Worker(WorkerError),
}

fn linked_timeout_source(
    parent: &CancelToken,
    timeout: Duration,
) -> (Arc<CancelSource>, br_core::CancelReg) {
    let cs = Arc::new(CancelSource::new());
    let cs_for_parent = cs.clone();
    let guard = parent.on_cancel(move || cs_for_parent.cancel());
    let cs_for_timer = cs.clone();
    let _ = std::thread::Builder::new()
        .name("BetterRecall.OcrTimeout".to_string())
        .spawn(move || {
            let tok = cs_for_timer.token();
            if !tok.wait_timeout(timeout) {
                cs_for_timer.cancel();
            }
        });
    (cs, guard)
}

fn ready_with_timeout(
    reader: &dyn KeyframeReader,
    timeout: Duration,
    ct: &CancelToken,
) -> Result<i32, ReadyErr> {
    let (cs, _guard) = linked_timeout_source(ct, timeout);
    let tok = cs.token();
    let res = reader.ready(&tok);
    cs.cancel();
    match res {
        Ok(ahead) => Ok(ahead),
        Err(WorkerError::Cancelled) if ct.is_cancelled() => Err(ReadyErr::Cancelled),
        Err(WorkerError::Cancelled) => Err(ReadyErr::TimedOut),
        Err(e) => Err(ReadyErr::Worker(e)),
    }
}

fn read_with_timeout(
    reader: &dyn KeyframeReader,
    frame: (&[u8], i32, i32, i32),
    entities: bool,
    timeout: Duration,
    ct: &CancelToken,
) -> Result<br_contracts::OcrResult, ReadErr> {
    let (bgra, width, height, stride) = frame;
    let (cs, _guard) = linked_timeout_source(ct, timeout);
    let tok = cs.token();
    let res = reader.read(bgra, width, height, stride, entities, &tok);
    cs.cancel();
    match res {
        Ok(r) => Ok(r),
        Err(WorkerError::Cancelled) if ct.is_cancelled() => Err(ReadErr::Cancelled),
        Err(WorkerError::Cancelled) => Err(ReadErr::TimedOut),
        Err(e) => Err(ReadErr::Worker(e)),
    }
}
