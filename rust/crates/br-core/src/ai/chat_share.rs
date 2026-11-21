// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Conservative text redactor and `ChatShareGate` (`ChatShare.cs`).

use br_contracts::{
    AiShare, CaptureDetail, CaptureKind, ChatApprovalItem, ChatSource, SearchHit, ThumbData,
};
use std::sync::OnceLock;

use chrono::Local;
use fancy_regex::{Captures, Regex};

use crate::ai::cloud_describer::{DescribeRedactor, OutgoingImage};
use crate::cancel::CancelToken;
use crate::log::Log;

/// Scrubs text that may leave the PC in a chat (`ITextRedactor`).
pub trait TextRedactor: Send + Sync {
    fn redact(&self, text: &str) -> String;
}

/// Conservative pattern scrubber: labelled secrets, e-mails, URL paths/queries, IBANs,
/// runs of 8+ digits (except dates), and 24+ char mixed alphanumeric tokens.
pub struct BasicTextRedactor;

impl BasicTextRedactor {
    pub const MASK: &'static str = "[redacted]";
}

impl TextRedactor for BasicTextRedactor {
    fn redact(&self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let p = patterns();
        let t = p
            .labelled
            .replace_all(text, |c: &Captures| format!("{}{}", &c[1], Self::MASK))
            .into_owned();
        let t = p.email.replace_all(&t, Self::MASK).into_owned();
        let t = p
            .url
            .replace_all(&t, |c: &Captures| {
                let tail = c.get(2).map_or(0, |m| m.as_str().chars().count());
                if tail > 1 {
                    format!("{}/{}", &c[1], Self::MASK)
                } else {
                    c[1].to_string()
                }
            })
            .into_owned();
        let t = p.iban.replace_all(&t, Self::MASK).into_owned();
        let t = p
            .digits
            .replace_all(&t, |c: &Captures| {
                let m = &c[0];
                if is_date(p, m) || m.chars().filter(char::is_ascii_digit).count() < 8 {
                    m.to_string()
                } else {
                    Self::MASK.to_string()
                }
            })
            .into_owned();
        p.token_like.replace_all(&t, Self::MASK).into_owned()
    }
}

/// The `BasicTextRedactor` patterns from `ChatShare.cs`, verbatim. As in .NET, `\d`, `\s` and `\b`
/// are Unicode-aware, so non-ASCII digits are caught too.
struct Patterns {
    labelled: Regex,
    email: Regex,
    url: Regex,
    iban: Regex,
    digits: Regex,
    date_like: Regex,
    token_like: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let r = |s: &str| Regex::new(s).expect("static redactor pattern");
        Patterns {
            labelled: r(
                r"(?i)\b((?:pass(?:word|code|phrase)?|pwd|pin|otp|cvv|cvc|token|secret|api[ _-]?key)\s*[:=]\s*)\S+",
            ),
            email: r(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"),
            url: r(r"(?i)\b((?:https?|ftp)://[^\s/?#]+)([/?#][^\s]*)?"),
            iban: r(r"\b[A-Z]{2}\d{2}(?:[ ]?[A-Z0-9]{4}){2,7}(?:[ ]?[A-Z0-9]{1,4})?\b"),
            digits: r(r"\+?\(?\d(?:[\d]|[ \-./()](?=\d))*\d"),
            date_like: r(r"^(?:\d{4}[-./]\d{1,2}[-./]\d{1,2}|\d{1,2}[-./]\d{1,2}[-./]\d{4})$"),
            token_like: r(r"\b(?=[A-Za-z0-9_\-]*\d)(?=[A-Za-z0-9_\-]*[A-Za-z])[A-Za-z0-9_\-]{24,}\b"),
        }
    })
}

fn is_date(p: &Patterns, s: &str) -> bool {
    p.date_like.is_match(s.trim()).unwrap_or(false)
}

/// One capture prepared for a chat: the exact text and optional redacted image that may be sent.
#[derive(Debug, Clone)]
pub struct SharedHit {
    pub ref_id: String,
    pub hit: SearchHit,
    pub text: String,
    pub image: Option<OutgoingImage>,
}

/// What one search tool call would send.
#[derive(Debug, Clone)]
pub struct SharePlan {
    pub share: String,
    pub hits: Vec<SharedHit>,
    pub images_unavailable: bool,
}

/// Arguments for [`ChatShareGate::build`].
pub struct ShareGateInput<'a> {
    pub hits: &'a [SearchHit],
    pub share: &'a str,
    pub max_hits: i32,
    pub max_images: i32,
    pub detail: &'a dyn Fn(&str) -> Option<CaptureDetail>,
    pub thumb: &'a dyn Fn(&str, i32, &CancelToken) -> Option<ThumbData>,
    pub image_redactor: Option<&'a dyn DescribeRedactor>,
    pub text_redactor: &'a dyn TextRedactor,
    pub ct: &'a CancelToken,
}

/// The only place that decides what a chat sends about captures (`ChatShareGate`).
pub struct ChatShareGate;

impl ChatShareGate {
    pub const SNIPPET_CHARS: usize = 400;
    pub const IMAGE_WIDTH: i32 = 768;
    pub const IMAGES_UNAVAILABLE_NOTE: &'static str = "Some images were not shared: images are sent only after on-screen text reading has checked them, and it is off, not installed or failed. Only text was sent for those captures. Turn on on-screen text reading (Settings > OCR) to share images.";

    pub fn build(input: ShareGateInput<'_>) -> SharePlan {
        let rank = AiShare::rank(Some(input.share)).max(0);
        let level = AiShare::ALL
            .into_iter()
            .find(|&a| AiShare::rank(Some(a)) == rank)
            .unwrap_or(AiShare::METADATA);
        let want_images = rank >= 2 && input.max_images > 0;
        let can_images =
            want_images && input.image_redactor.is_some_and(|r| r.redacts_images());
        let mut list = Vec::new();
        let mut images = 0i32;
        let mut unchecked = want_images && !can_images;

        let take = input.max_hits.clamp(1, 20) as usize;
        for h in input.hits.iter().take(take) {
            let ref_id = format!("S{}", list.len() + 1);
            let d = if rank >= 1 { (input.detail)(&h.id) } else { None };
            let text = Self::describe(&ref_id, h, d.as_ref(), rank, input.text_redactor);
            let mut image = None;
            if can_images && images < input.max_images && h.kind != CaptureKind::Audio
                && let Some(t) = (input.thumb)(&h.id, Self::IMAGE_WIDTH, input.ct)
                    && !t.bytes.is_empty()
                {
                    let red = input.image_redactor.and_then(|r| {
                        match r.redact(
                            &h.id,
                            OutgoingImage {
                                bytes: t.bytes,
                                mime: t.mime,
                                width: t.width,
                                height: t.height,
                                redacted: false,
                            },
                            input.ct,
                        ) {
                            Ok(img) => Some(img),
                            Err(e) => {
                                Log::warn(&format!(
                                    "chat image redaction failed, sending text only ({e})"
                                ));
                                None
                            }
                        }
                    });
                    if let Some(img) = red
                        && img.redacted
                    {
                        image = Some(img);
                        images += 1;
                    } else {
                        unchecked = true;
                    }
                }
            list.push(SharedHit {
                ref_id,
                hit: h.clone(),
                text,
                image,
            });
        }

        SharePlan {
            share: level.to_string(),
            hits: list,
            images_unavailable: unchecked,
        }
    }

    pub fn describe(
        ref_id: &str,
        h: &SearchHit,
        d: Option<&CaptureDetail>,
        rank: i32,
        r: &dyn TextRedactor,
    ) -> String {
        let local = h.ts.with_timezone(&Local);
        let mut sb = format!(
            "[{ref_id}] {} | {} | app: {}",
            local.format("%Y-%m-%d %H:%M %:z"),
            kind_name(h.kind),
            h.app
        );
        if !h.site.is_empty() {
            sb.push_str(" | site: ");
            sb.push_str(&h.site);
        }
        if let Some(off) = h.replay_offset_ms {
            let total_s = (off.max(0) as i64) / 1000;
            let mins = total_s / 60;
            let secs = total_s % 60;
            sb.push_str(&format!(" | replay at {mins}:{secs:02}"));
        }
        if rank >= 1 {
            let title = r.redact(&clean(&h.title));
            if !title.is_empty() {
                sb.push_str("\n  title: ");
                sb.push_str(&cut(&title, 200));
            }
            let body_raw = match d {
                Some(det) if !det.text.is_empty() && det.text != h.title => det.text.as_str(),
                _ => h.snippet.as_str(),
            };
            let body = r.redact(&clean(body_raw));
            if !body.is_empty() {
                sb.push_str("\n  text: ");
                sb.push_str(&cut(&body, Self::SNIPPET_CHARS));
            }
        }
        sb
    }

    pub fn tool_text(plan: &SharePlan, declined: bool) -> String {
        if declined {
            return "The user did not allow sharing captures for this question. Answer without them, or say what you would need.".to_string();
        }
        if plan.hits.is_empty() {
            return "No matching captures were found (or none were shared).".to_string();
        }
        let mut sb = format!(
            "Captures from the user's history (share level {}). Cite them as [S1], [S2]:\n",
            plan.share
        );
        for h in &plan.hits {
            sb.push_str(&h.text);
            sb.push('\n');
        }
        if plan.images_unavailable {
            sb.push_str(Self::IMAGES_UNAVAILABLE_NOTE);
            sb.push('\n');
        }
        sb.trim_end().to_string()
    }

    pub fn source(h: &SearchHit) -> ChatSource {
        ChatSource {
            id: h.id.clone(),
            ts: h.ts,
            kind: h.kind,
            app: h.app.clone(),
            title: h.title.clone(),
            site: h.site.clone(),
            replay_id: h.replay_id.clone(),
            replay_offset_ms: h.replay_offset_ms,
        }
    }

    pub fn item(s: &SharedHit, rank: i32, r: &dyn TextRedactor) -> ChatApprovalItem {
        ChatApprovalItem {
            id: s.hit.id.clone(),
            ts: s.hit.ts,
            kind: s.hit.kind,
            app: s.hit.app.clone(),
            title: if rank >= 1 {
                r.redact(&clean(&s.hit.title))
            } else {
                String::new()
            },
            site: s.hit.site.clone(),
            text: s.text.clone(),
            image_mime: s.image.as_ref().map(|i| i.mime.clone()),
            image: s.image.as_ref().map(|i| i.bytes.clone()),
        }
    }
}

fn kind_name(k: CaptureKind) -> &'static str {
    match k {
        CaptureKind::Audio => "speech transcript",
        CaptureKind::Replay => "replay frame",
        CaptureKind::Screen => "screen",
    }
}

fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let prefix: String = s.chars().take(n).collect();
        format!("{}...", prefix.trim_end())
    }
}
