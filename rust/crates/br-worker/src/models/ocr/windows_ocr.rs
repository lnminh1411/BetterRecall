// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use br_contracts::OcrLine;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::System::UserProfile::GlobalizationPreferences;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::core::HSTRING;

use crate::models::image_pipeline::{ImagePipeline, RgbImage};
use crate::protocol::WorkerError;

fn ensure_winrt_mta() {
    thread_local! {
        static INIT: bool = unsafe { RoInitialize(RO_INIT_MULTITHREADED).is_ok() };
    }
    INIT.with(|_| {});
}

/// Returns the Windows user profile language tags (best effort for `ocr.languages = auto`).
pub fn user_profile_languages() -> Result<Vec<String>, String> {
    ensure_winrt_mta();
    let langs = GlobalizationPreferences::Languages().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for s in langs {
        out.push(s.to_string_lossy());
    }
    Ok(out)
}

/// `ocr.engine = windows`: `Windows.Media.Ocr`, built into Windows and offline.
pub struct WindowsOcr {
    engine: OcrEngine,
    pub language: String,
}

impl WindowsOcr {
    /// `None` when no requested language and no profile language has an installed OCR pack.
    pub fn try_create(languages: &[String]) -> Option<Self> {
        ensure_winrt_mta();
        for code in languages {
            let Ok(lang) = Language::CreateLanguage(&HSTRING::from(code.as_str())) else {
                continue;
            };
            if OcrEngine::IsLanguageSupported(&lang).unwrap_or(false)
                && let Ok(engine) = OcrEngine::TryCreateFromLanguage(&lang)
            {
                let tag = engine
                    .RecognizerLanguage()
                    .and_then(|l| l.LanguageTag())
                    .map(|t| t.to_string_lossy())
                    .unwrap_or_else(|_| code.clone());
                return Some(Self {
                    engine,
                    language: tag,
                });
            }
        }
        let engine = OcrEngine::TryCreateFromUserProfileLanguages().ok()?;
        let tag = engine
            .RecognizerLanguage()
            .and_then(|l| l.LanguageTag())
            .map(|t| t.to_string_lossy())
            .unwrap_or_default();
        Some(Self {
            engine,
            language: tag,
        })
    }

    pub fn read(&self, img: &RgbImage) -> Result<Vec<OcrLine>, WorkerError> {
        ensure_winrt_mta();
        let max = OcrEngine::MaxImageDimension()
            .map_err(|e| WorkerError::Internal(format!("Windows OCR max dimension: {e}")))?
            as i32;
        if img.width > max || img.height > max {
            let s = max as f64 / img.width.max(img.height) as f64;
            let dw = 1.max((img.width as f64 * s) as i32);
            let dh = 1.max((img.height as f64 * s) as i32);
            let small = ImagePipeline::resize(img, dw, dh, "bilinear");
            return Ok(scale_lines(self.read(&small)?, 1.0 / s));
        }

        let n = (img.width * img.height) as usize;
        let mut bgra = vec![0u8; n * 4];
        for i in 0..n {
            bgra[i * 4] = img.rgb[i * 3 + 2];
            bgra[i * 4 + 1] = img.rgb[i * 3 + 1];
            bgra[i * 4 + 2] = img.rgb[i * 3];
            bgra[i * 4 + 3] = 255;
        }
        let writer =
            DataWriter::new().map_err(|e| WorkerError::Internal(format!("DataWriter: {e}")))?;
        let ibuf_res = writer
            .WriteBytes(&bgra)
            .and_then(|()| writer.DetachBuffer());
        // Zero raw frame bytes before WinRT OCR returns.
        bgra.fill(0);
        let ibuf = ibuf_res.map_err(|e| WorkerError::Internal(format!("DataWriter buffer: {e}")))?;
        let bmp = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
            &ibuf,
            BitmapPixelFormat::Bgra8,
            img.width,
            img.height,
            BitmapAlphaMode::Premultiplied,
        )
        .map_err(|e| WorkerError::Internal(format!("SoftwareBitmap: {e}")))?;

        let op = self
            .engine
            .RecognizeAsync(&bmp)
            .map_err(|e| WorkerError::Internal(format!("RecognizeAsync: {e}")))?;
        let result = op
            .join()
            .map_err(|e| WorkerError::Internal(format!("RecognizeAsync join: {e}")))?;
        let ocr_lines = result
            .Lines()
            .map_err(|e| WorkerError::Internal(format!("OcrResult Lines: {e}")))?;

        let mut lines = Vec::new();
        for line in ocr_lines {
            let words_view = line
                .Words()
                .map_err(|e| WorkerError::Internal(format!("OcrLine Words: {e}")))?;
            let line_text = line
                .Text()
                .map_err(|e| WorkerError::Internal(format!("OcrLine Text: {e}")))?
                .to_string_lossy();
            let mut words = Vec::new();
            for w in words_view {
                let wt = w
                    .Text()
                    .map_err(|e| WorkerError::Internal(format!("OcrWord Text: {e}")))?
                    .to_string_lossy();
                let rect = w
                    .BoundingRect()
                    .map_err(|e| WorkerError::Internal(format!("BoundingRect: {e}")))?;
                words.push((
                    wt,
                    (rect.X as f64).round_ties_even() as i32,
                    (rect.Y as f64).round_ties_even() as i32,
                    (rect.Width as f64).round_ties_even() as i32,
                    (rect.Height as f64).round_ties_even() as i32,
                ));
            }
            if words.is_empty() {
                continue;
            }
            let word_refs: Vec<(&str, i32, i32, i32, i32)> = words
                .iter()
                .map(|(t, x, y, w, h)| (t.as_str(), *x, *y, *w, *h))
                .collect();
            if let Some(l) = Self::from_words(&line_text, &word_refs) {
                lines.push(l);
            }
        }
        Ok(lines)
    }

    /// Line from Windows words using UTF-16 code unit cut offsets matching Contracts/UI.
    pub fn from_words(text: &str, words: &[(&str, i32, i32, i32, i32)]) -> Option<OcrLine> {
        let u16_len = text.encode_utf16().count();
        if u16_len == 0 || words.is_empty() {
            return None;
        }
        let x0 = words.iter().map(|w| w.1).min().unwrap();
        let y0 = words.iter().map(|w| w.2).min().unwrap();
        let x1 = words.iter().map(|w| w.1 + w.3).max().unwrap();
        let y1 = words.iter().map(|w| w.2 + w.4).max().unwrap();

        let mut cuts = vec![0i32; u16_len + 1];
        let mut byte_pos = 0usize;
        let mut u16_pos = 0usize;
        let mut prev_end = x0;
        let mut ok = true;

        for &(w_text, wx, _wy, ww, _wh) in words {
            if w_text.is_empty() {
                ok = false;
                break;
            }
            let Some(rel_byte) = text[byte_pos..].find(w_text) else {
                ok = false;
                break;
            };
            let at = u16_pos + text[byte_pos..byte_pos + rel_byte].encode_utf16().count();
            let w_u16 = w_text.encode_utf16().count();
            let gap = (at - u16_pos).max(1) as i32;
            for (k, slot) in cuts[u16_pos..at].iter_mut().enumerate() {
                *slot = prev_end + (wx - prev_end) * (k as i32) / gap;
            }
            for (k, slot) in cuts[at..at + w_u16].iter_mut().enumerate() {
                *slot = wx + ww * (k as i32) / (w_u16 as i32);
            }
            byte_pos += rel_byte + w_text.len();
            u16_pos = at + w_u16;
            prev_end = wx + ww;
        }
        if !ok {
            for (i, slot) in cuts.iter_mut().enumerate() {
                *slot = x0 + (x1 - x0) * (i as i32) / (u16_len as i32);
            }
            return Some(OcrLine {
                text: text.to_string(),
                x: x0,
                y: y0,
                w: x1 - x0,
                h: y1 - y0,
                confidence: 1.0,
                cuts,
                entities: None,
            });
        }
        for slot in &mut cuts[u16_pos..=u16_len] {
            *slot = prev_end;
        }
        Some(OcrLine {
            text: text.to_string(),
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
            confidence: 1.0,
            cuts,
            entities: None,
        })
    }
}

fn scale_lines(lines: Vec<OcrLine>, f: f64) -> Vec<OcrLine> {
    lines
        .into_iter()
        .map(|mut l| {
            l.x = (l.x as f64 * f) as i32;
            l.y = (l.y as f64 * f) as i32;
            l.w = (l.w as f64 * f).ceil() as i32;
            l.h = (l.h as f64 * f).ceil() as i32;
            for c in &mut l.cuts {
                *c = (*c as f64 * f) as i32;
            }
            l
        })
        .collect()
}
