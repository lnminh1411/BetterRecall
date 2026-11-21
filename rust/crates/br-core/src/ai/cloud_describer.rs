// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Cloud screenshot describer and image redaction hook (`CloudDescriber.cs`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use br_contracts::{AiDefaults, AiSettings, DescribeResult, dotnet_base64};
use serde_json::{Value, json};
use thiserror::Error;

use crate::ai::provider_http::{HttpRequest, HttpTransport, ProviderHttp, TransportError, UreqTransport};
use crate::cancel::CancelToken;
use crate::log::Log;

/// Image about to leave the PC. `redacted` says whether a redactor changed or approved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingImage {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub width: i32,
    pub height: i32,
    pub redacted: bool,
}

/// The one hook cloud describe calls before a preview is shown and sent (`IDescribeRedactor`).
pub trait DescribeRedactor: Send + Sync {
    fn redact(
        &self,
        capture_id: &str,
        image: OutgoingImage,
        ct: &CancelToken,
    ) -> Result<OutgoingImage, DescribeError>;

    /// True only for a redactor that really blacks out sensitive areas.
    fn redacts_images(&self) -> bool {
        false
    }
}

/// Default pass-through redactor: marks `redacted = false`, so chat sends no images.
pub struct PassThroughRedactor;

impl DescribeRedactor for PassThroughRedactor {
    fn redact(
        &self,
        _capture_id: &str,
        mut image: OutgoingImage,
        _ct: &CancelToken,
    ) -> Result<OutgoingImage, DescribeError> {
        image.redacted = false;
        Ok(image)
    }
}

/// Provider or redaction failure with a short message safe to show.
#[derive(Debug, Clone, Error)]
pub enum DescribeError {
    #[error("{0}")]
    Message(String),
    #[error("Provider unreachable.")]
    Unreachable,
    #[error("Provider timed out.")]
    Timeout,
    #[error("Cancelled")]
    Cancelled,
}

/// Sends one image to the configured provider and returns its text (`CloudDescriber`).
pub struct CloudDescriber {
    transport: Arc<dyn HttpTransport>,
}

impl Default for CloudDescriber {
    fn default() -> Self {
        Self::new()
    }
}

impl CloudDescriber {
    pub const PROMPT: &'static str = "Describe this screenshot in a few sentences: which app or website is shown and what the user is doing. Reply in plain text.";
    pub const CLAUDE_VERSION: &'static str = "2026-10-21";
    pub const MAX_TEXT_CHARS: usize = 8000;
    const TIMEOUT: Duration = Duration::from_secs(90);

    pub fn new() -> Self {
        Self {
            transport: Arc::new(UreqTransport),
        }
    }

    pub fn with_transport(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }

    pub fn describe(
        &self,
        ai: &AiSettings,
        api_key: Option<&str>,
        image: &OutgoingImage,
        ct: &CancelToken,
    ) -> Result<DescribeResult, DescribeError> {
        let provider = ai.provider.as_str();
        let model = AiDefaults::effective_model(ai);
        let endpoint = AiDefaults::effective_endpoint(ai);
        let req = Self::build_request(provider, &endpoint, &model, api_key, image, Self::PROMPT);
        let started = Instant::now();
        let mut resp = match self.transport.send(req, Self::TIMEOUT, ct) {
            Ok(r) => r,
            Err(TransportError::Timeout) => return Err(DescribeError::Timeout),
            Err(TransportError::Cancelled) => return Err(DescribeError::Cancelled),
            Err(TransportError::Offline(msg)) => {
                Log::warn(&format!("cloud describe: {msg}"));
                return Err(DescribeError::Unreachable);
            }
        };
        let cap = if resp.is_success() {
            ProviderHttp::MAX_BUFFERED_BYTES
        } else {
            ProviderHttp::MAX_ERROR_BODY_BYTES
        };
        let body = ProviderHttp::read_capped(&mut resp.body, cap).map_err(|_| {
            if ct.is_cancelled() {
                DescribeError::Cancelled
            } else {
                DescribeError::Unreachable
            }
        })?;
        let elapsed_ms = started.elapsed().as_millis() as i64;
        Log::info(&format!(
            "cloud describe: {provider} {model}, {} KiB sent, HTTP {}, {elapsed_ms} ms",
            image.bytes.len() / 1024,
            resp.status
        ));
        if !resp.is_success() {
            let mut err = Self::error_text(resp.status, &body, api_key);
            if let Some(k) = api_key
                && !k.is_empty()
            {
                err = err.replace(k, "***");
            }
            return Err(DescribeError::Message(err));
        }
        let Some(raw_text) = Self::parse_text(provider, &body) else {
            return Err(DescribeError::Message(
                "The provider returned no text.".to_string(),
            ));
        };
        let trimmed = raw_text.trim();
        if trimmed.is_empty() {
            return Err(DescribeError::Message(
                "The provider returned no text.".to_string(),
            ));
        }
        let text: String = if trimmed.chars().count() > Self::MAX_TEXT_CHARS {
            trimmed.chars().take(Self::MAX_TEXT_CHARS).collect()
        } else {
            trimmed.to_string()
        };
        Ok(DescribeResult {
            text,
            provider: provider.to_string(),
            model,
            took_ms: elapsed_ms,
        })
    }

    pub fn build_request(
        provider: &str,
        endpoint: &str,
        model: &str,
        api_key: Option<&str>,
        image: &OutgoingImage,
        prompt: &str,
    ) -> HttpRequest {
        let b64 = dotnet_base64::encode(&image.bytes);
        let ep = endpoint.trim_end_matches('/');
        match provider {
            AiDefaults::OPEN_AI => {
                let body = json!({
                    "model": model,
                    "messages": [{
                        "role": "user",
                        "content": [
                            { "type": "text", "text": prompt },
                            { "type": "image_url", "image_url": { "url": format!("data:{};base64,{b64}", image.mime) } }
                        ]
                    }]
                });
                let mut req = HttpRequest::post_json(
                    format!("{ep}/chat/completions"),
                    body.to_string().into_bytes(),
                );
                if let Some(k) = api_key
                    && !k.is_empty()
                {
                    req.headers
                        .push(("Authorization".to_string(), format!("Bearer {k}")));
                }
                req
            }
            AiDefaults::CLAUDE => {
                let body = json!({
                    "model": model,
                    "max_tokens": 1024,
                    "messages": [{
                        "role": "user",
                        "content": [
                            {
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": image.mime,
                                    "data": b64
                                }
                            },
                            { "type": "text", "text": prompt }
                        ]
                    }]
                });
                let mut req = HttpRequest::post_json(
                    format!("{ep}/v1/messages"),
                    body.to_string().into_bytes(),
                );
                req.headers
                    .push(("x-api-key".to_string(), api_key.unwrap_or("").to_string()));
                req.headers.push((
                    "anthropic-version".to_string(),
                    Self::CLAUDE_VERSION.to_string(),
                ));
                req
            }
            _ => {
                let path = if model.starts_with("models/") {
                    model.to_string()
                } else {
                    format!("models/{model}")
                };
                let body = json!({
                    "contents": [{
                        "role": "user",
                        "parts": [
                            { "text": prompt },
                            { "inlineData": { "mimeType": image.mime, "data": b64 } }
                        ]
                    }]
                });
                let mut req = HttpRequest::post_json(
                    format!("{ep}/v1beta/{path}:generateContent"),
                    body.to_string().into_bytes(),
                );
                req.headers.push((
                    "x-goog-api-key".to_string(),
                    api_key.unwrap_or("").to_string(),
                ));
                req
            }
        }
    }

    /// Text of a success body, or `None` when the shape is unknown.
    pub fn parse_text(provider: &str, body: &str) -> Option<String> {
        let root: Value = serde_json::from_str(body).ok()?;
        let mut out = String::new();
        match provider {
            AiDefaults::OPEN_AI => {
                let choices = root.get("choices")?.as_array()?;
                let content = choices.first()?.get("message")?.get("content")?;
                if let Some(s) = content.as_str() {
                    return Some(s.to_string());
                }
                if let Some(parts) = content.as_array() {
                    for part in parts {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            out.push_str(t);
                        }
                    }
                }
                Some(out)
            }
            AiDefaults::CLAUDE => {
                let blocks = root.get("content")?.as_array()?;
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(t) = block.get("text").and_then(Value::as_str)
                    {
                        out.push_str(t);
                    }
                }
                Some(out)
            }
            _ => {
                let cands = root.get("candidates")?.as_array()?;
                let parts = cands.first()?.get("content")?.get("parts")?.as_array()?;
                for part in parts {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                    }
                }
                Some(out)
            }
        }
    }

    /// `"Provider error (HTTP 401): message"` with the provider's own message scrubbed of `api_key`
    /// and cut to 200 non-control characters.
    pub fn error_text(status: u16, body: &str, api_key: Option<&str>) -> String {
        let mut message: Option<String> = None;
        if let Ok(mut root) = serde_json::from_str::<Value>(body) {
            if let Value::Array(arr) = root {
                if let Some(first) = arr.into_iter().next() {
                    root = first;
                } else {
                    root = Value::Null;
                }
            }
            if let Some(e) = root.get("error") {
                if let Some(s) = e.as_str() {
                    message = Some(s.to_string());
                } else if let Some(m) = e.get("message").and_then(Value::as_str) {
                    message = Some(m.to_string());
                }
            }
        }
        if let Some(ref mut m) = message
            && let Some(k) = api_key
            && !k.is_empty()
        {
            *m = m.replace(k, "***");
        }
        let suffix = match message {
            Some(m) if !m.trim().is_empty() => {
                let clean: String = m.chars().filter(|c| !c.is_control()).take(200).collect();
                format!(": {clean}")
            }
            _ => String::new(),
        };
        format!("Provider error (HTTP {status}){suffix}")
    }
}
