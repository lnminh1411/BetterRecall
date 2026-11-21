// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Blocking HTTP transport (`ureq` 3 + `native-tls`/SChannel, no redirects, 90 s header timeout),
//! error mapping, retry policy, and local daily AI budget (`ProviderHttp.cs`).

use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use br_contracts::{AiSettings, AiUsage};
use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use serde_json::Value;
use thiserror::Error;

use crate::cancel::CancelToken;
use crate::log::Log;
use crate::store::EncryptedStore;

/// Called before each retry with the failure and the wait.
pub type RetryHook<'a> = dyn FnMut(&ChatFailure, Duration) + 'a;

/// A failed cloud request as the user sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatFailure {
    pub code: String,
    pub message: String,
    pub retry_at: Option<DateTime<Utc>>,
}

impl ChatFailure {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        retry_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retry_at,
        }
    }
}

#[derive(Debug, Error)]
#[error("{}", .0.message)]
pub struct ChatFailureError(pub ChatFailure);

/// An error event inside a 200 streaming response (Claude `event: error`, Gemini/OpenAI error chunk).
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct ProviderStreamError {
    pub error_type: String,
    pub message: String,
}

impl ProviderStreamError {
    pub fn new(error_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error_type: error_type.into(),
            message: message.into(),
        }
    }
}

/// Outgoing HTTP request built by a provider or `CloudDescriber`.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
}

impl HttpRequest {
    pub fn post_json(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: "POST".to_string(),
            url: url.into(),
            headers: Vec::new(),
            body,
            content_type: Some("application/json".to_string()),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Streaming HTTP response returned after headers are received.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn Read + Send>,
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Low-level transport failure before a complete HTTP status/header block is received.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("offline: {0}")]
    Offline(String),
    #[error("timeout")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
}

/// Abstract blocking HTTP transport so tests can inject fakes or point at a local server.
pub trait HttpTransport: Send + Sync {
    fn send(
        &self,
        req: HttpRequest,
        header_timeout: Duration,
        ct: &CancelToken,
    ) -> Result<HttpResponse, TransportError>;
}

/// Real blocking HTTP transport backed by `ureq` 3 + `native-tls` (Windows SChannel).
/// No redirects are followed, and non-2xx responses are returned as `Ok(HttpResponse)`.
pub struct UreqTransport;

impl UreqTransport {
    fn build_agent(header_timeout: Duration) -> ureq::Agent {
        let config = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(header_timeout))
            .timeout_send_request(Some(header_timeout))
            .timeout_recv_response(Some(header_timeout))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::NativeTls)
                    .build(),
            )
            .build();
        ureq::Agent::new_with_config(config)
    }
}

impl HttpTransport for UreqTransport {
    fn send(
        &self,
        req: HttpRequest,
        header_timeout: Duration,
        ct: &CancelToken,
    ) -> Result<HttpResponse, TransportError> {
        if ct.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        let agent = Self::build_agent(header_timeout);
        let res = if req.method.eq_ignore_ascii_case("POST") {
            let mut builder = agent.post(&req.url);
            if let Some(ref ct_hdr) = req.content_type {
                builder = builder.header("Content-Type", ct_hdr);
            }
            for (k, v) in &req.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            builder.send(req.body.as_slice())
        } else {
            let mut builder = agent.get(&req.url);
            for (k, v) in &req.headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            builder.call()
        };

        if ct.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        match res {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let headers = resp
                    .headers()
                    .iter()
                    .filter_map(|(k, v)| {
                        v.to_str()
                            .ok()
                            .map(|vs| (k.as_str().to_string(), vs.to_string()))
                    })
                    .collect();
                let reader = resp.into_body().into_reader();
                Ok(HttpResponse {
                    status,
                    headers,
                    body: Box::new(reader),
                })
            }
            Err(ureq::Error::Timeout(_)) => Err(TransportError::Timeout),
            Err(e) => Err(TransportError::Offline(e.to_string())),
        }
    }
}

/// Error mapping and retry policy for every cloud request (`ProviderErrors`).
pub struct ProviderErrors;

impl ProviderErrors {
    pub const MAX_RETRY_WAIT: Duration = Duration::from_secs(20);
    pub const MAX_RETRIES: i32 = 2;

    pub fn from_http(
        status: u16,
        body: &str,
        headers: &[(String, String)],
        model: &str,
        now: DateTime<Utc>,
        api_key: Option<&str>,
    ) -> ChatFailure {
        let parsed = Self::parse_body(body);
        let message = Self::scrub(&parsed.message, api_key);
        let retry_at = Self::retry_after(headers, now)
            .or_else(|| parsed.retry_delay.map(|d| now + Self::clamp_chrono(d)));
        let say = |text: &str| -> String {
            if message.is_empty() {
                text.to_string()
            } else {
                format!("{text} ({message})")
            }
        };

        let msg_lower = message.to_ascii_lowercase();
        let ctx = has(parsed.code.as_deref(), "context_length_exceeded")
            || has(parsed.error_type.as_deref(), "request_too_large")
            || status == 413
            || msg_lower.contains("prompt is too long")
            || msg_lower.contains("context length")
            || msg_lower.contains("context window")
            || msg_lower.contains("input context is too long")
            || msg_lower.contains("too many tokens");

        let quota = status == 402
            || has(parsed.error_type.as_deref(), "insufficient_quota")
            || has(parsed.code.as_deref(), "insufficient_quota")
            || has(parsed.error_type.as_deref(), "billing_error")
            || has(parsed.code.as_deref(), "credit_balance_exhausted")
            || has(parsed.code.as_deref(), "organization_spend_limit_exceeded")
            || has(parsed.code.as_deref(), "project_spend_limit_exceeded")
            || has(parsed.code.as_deref(), "organization_usage_limit_exceeded")
            || has(parsed.reason.as_deref(), "enforced_spend_limit_reached")
            || (has(parsed.gstatus.as_deref(), "RESOURCE_EXHAUSTED")
                && parsed.retry_delay.is_none()
                && (msg_lower.contains("quota") || status != 429));

        if has(parsed.reason.as_deref(), "API_KEY_INVALID")
            || status == 401
            || has(parsed.error_type.as_deref(), "authentication_error")
        {
            return ChatFailure::new(
                "auth",
                say("The provider rejected the API key. Check the key in Settings > AI."),
                None,
            );
        }
        if quota {
            return ChatFailure::new(
                "quota",
                say("Your provider quota or credit is used up. Check the plan or billing with the provider; retrying will not help."),
                retry_at,
            );
        }
        if ctx {
            return ChatFailure::new(
                "context",
                "The conversation is too long for this model. Start a new chat or ask a shorter question.",
                None,
            );
        }
        if status == 403
            || has(parsed.error_type.as_deref(), "permission_error")
            || has(parsed.gstatus.as_deref(), "PERMISSION_DENIED")
        {
            return ChatFailure::new(
                "forbidden",
                say("The provider refused access (HTTP 403). The key may lack access to this model or region."),
                None,
            );
        }
        if status == 404
            || has(parsed.error_type.as_deref(), "not_found_error")
            || has(parsed.code.as_deref(), "model_not_found")
            || has(parsed.gstatus.as_deref(), "NOT_FOUND")
        {
            return ChatFailure::new(
                "model",
                say(&format!(
                    "Unknown model or endpoint: '{model}' (HTTP 404). Check Settings > AI."
                )),
                None,
            );
        }
        if status == 429
            || has(parsed.error_type.as_deref(), "rate_limit_error")
            || has(parsed.gstatus.as_deref(), "RESOURCE_EXHAUSTED")
        {
            return ChatFailure::new(
                "rate",
                say(&format!(
                    "The provider's rate limit was reached.{}",
                    when_text(retry_at, now)
                )),
                retry_at,
            );
        }
        if status == 529
            || status == 503
            || has(parsed.error_type.as_deref(), "overloaded_error")
            || has(parsed.gstatus.as_deref(), "UNAVAILABLE")
        {
            return ChatFailure::new(
                "overloaded",
                say(&format!(
                    "The provider is overloaded right now.{}",
                    when_text(retry_at, now)
                )),
                retry_at,
            );
        }
        if status >= 500 {
            return ChatFailure::new(
                "server",
                say(&format!(
                    "The provider had a server error (HTTP {status}).{}",
                    when_text(retry_at, now)
                )),
                retry_at,
            );
        }
        ChatFailure::new(
            "bad_request",
            say(&format!(
                "The provider refused the request (HTTP {status})."
            )),
            None,
        )
    }

    pub fn from_stream(
        ex: &ProviderStreamError,
        model: &str,
        now: DateTime<Utc>,
        api_key: Option<&str>,
    ) -> ChatFailure {
        let status: u16 = match ex.error_type.as_str() {
            "overloaded_error" | "UNAVAILABLE" | "server_is_overloaded" => 529,
            "rate_limit_error" | "RESOURCE_EXHAUSTED" | "rate_limit_exceeded" => 429,
            "api_error" | "INTERNAL" | "server_error" => 500,
            "authentication_error" => 401,
            "permission_error" => 403,
            "not_found_error" => 404,
            "invalid_request_error" => 400,
            _ => 500,
        };
        let body = serde_json::json!({
            "error": {
                "type": ex.error_type,
                "message": ex.message,
            }
        })
        .to_string();
        Self::from_http(status, &body, &[], model, now, api_key)
    }

    pub fn offline(endpoint: &str) -> ChatFailure {
        ChatFailure::new(
            "offline",
            format!(
                "Can't reach {}. Check the internet connection (or that the local server is running).",
                Self::host_of(endpoint)
            ),
            None,
        )
    }

    pub fn timeout(t: Duration) -> ChatFailure {
        ChatFailure::new(
            "timeout",
            format!(
                "The provider did not answer within {} s. Try again.",
                t.as_secs()
            ),
            None,
        )
    }

    pub fn retryable(f: &ChatFailure) -> bool {
        matches!(f.code.as_str(), "rate" | "overloaded" | "server")
    }

    /// How long to wait before retry `attempt` (0-based), or `None` when not retryable or too long.
    pub fn retry_delay(f: &ChatFailure, attempt: i32, now: DateTime<Utc>) -> Option<Duration> {
        if !Self::retryable(f) || attempt >= Self::MAX_RETRIES {
            return None;
        }
        let wait = match f.retry_at {
            Some(at) => {
                let diff = at.signed_duration_since(now);
                if diff <= ChronoDuration::zero() {
                    Duration::ZERO
                } else {
                    Duration::from_millis(diff.num_milliseconds() as u64)
                }
            }
            None => Duration::from_secs(1u64 << attempt),
        };
        if wait <= Self::MAX_RETRY_WAIT {
            Some(wait)
        } else {
            None
        }
    }

    /// Warning text when a rate-limit header shows <10% of requests or tokens left.
    pub fn low_quota_warning(headers: &[(String, String)]) -> Option<String> {
        const PAIRS: [(&str, &str, &str); 6] = [
            (
                "requests",
                "anthropic-ratelimit-requests-remaining",
                "anthropic-ratelimit-requests-limit",
            ),
            (
                "tokens",
                "anthropic-ratelimit-tokens-remaining",
                "anthropic-ratelimit-tokens-limit",
            ),
            (
                "input tokens",
                "anthropic-ratelimit-input-tokens-remaining",
                "anthropic-ratelimit-input-tokens-limit",
            ),
            (
                "output tokens",
                "anthropic-ratelimit-output-tokens-remaining",
                "anthropic-ratelimit-output-tokens-limit",
            ),
            (
                "requests",
                "x-ratelimit-remaining-requests",
                "x-ratelimit-limit-requests",
            ),
            (
                "tokens",
                "x-ratelimit-remaining-tokens",
                "x-ratelimit-limit-tokens",
            ),
        ];
        for (what, rem_name, lim_name) in PAIRS {
            let (Some(r), Some(l)) = (header_num(headers, rem_name), header_num(headers, lim_name))
            else {
                continue;
            };
            if l <= 0.0 {
                continue;
            }
            if r < l * 0.10 {
                return Some(format!(
                    "Provider rate limit almost used up: {} of {} {what} left in the current window.",
                    format_commas(r.round() as i64),
                    format_commas(l.round() as i64)
                ));
            }
        }
        None
    }

    pub fn clamp(d: Duration) -> Duration {
        let day = Duration::from_secs(86_400);
        if d > day { day } else { d }
    }

    fn clamp_chrono(d: ChronoDuration) -> ChronoDuration {
        let day = ChronoDuration::days(1);
        if d < ChronoDuration::zero() {
            ChronoDuration::zero()
        } else if d > day {
            day
        } else {
            d
        }
    }

    pub fn retry_after(headers: &[(String, String)], now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let val = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
            .map(|(_, v)| v.trim())?;
        if let Ok(secs) = val.parse::<i64>() {
            let clamped = Self::clamp_chrono(ChronoDuration::seconds(secs));
            return Some(now + clamped);
        }
        if let Ok(dt) = DateTime::parse_from_rfc2822(val) {
            return Some(dt.with_timezone(&Utc));
        }
        None
    }

    pub fn host_of(endpoint: &str) -> String {
        extract_host(endpoint).unwrap_or_else(|| "the provider".to_string())
    }

    pub fn scrub(text: &str, key: Option<&str>) -> String {
        if text.is_empty() {
            return String::new();
        }
        let mut s = match key {
            Some(k) if !k.is_empty() => text.replace(k, "***"),
            _ => text.to_string(),
        };
        s = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if s.chars().count() > 300 {
            let cut: String = s.chars().take(300).collect();
            format!("{cut}...")
        } else {
            s
        }
    }

    pub fn parse_body(body: &str) -> ParsedErrorBody {
        let Ok(mut root) = serde_json::from_str::<Value>(body) else {
            return ParsedErrorBody::default();
        };
        if let Value::Array(arr) = root {
            if let Some(first) = arr.into_iter().next() {
                root = first;
            } else {
                return ParsedErrorBody::default();
            }
        }
        let Some(e) = root.get("error").and_then(Value::as_object) else {
            return ParsedErrorBody::default();
        };
        let get_str = |o: &serde_json::Map<String, Value>, k: &str| -> Option<String> {
            o.get(k).and_then(Value::as_str).map(String::from)
        };
        let mut delay: Option<ChronoDuration> = None;
        let mut reason: Option<String> = None;
        if let Some(det) = e.get("details") {
            if let Some(arr) = det.as_array() {
                for d in arr {
                    let Some(obj) = d.as_object() else {
                        continue;
                    };
                    if let Some(rd) = get_str(obj, "retryDelay")
                        && let Some(num) = rd.strip_suffix('s')
                        && let Ok(sec) = num.parse::<f64>()
                        && sec.is_finite()
                    {
                        let clamped = sec.clamp(0.0, 86_400.0);
                        delay = Some(ChronoDuration::milliseconds((clamped * 1000.0) as i64));
                    }
                    if reason.is_none() {
                        reason = get_str(obj, "reason");
                    }
                }
            } else if let Some(obj) = det.as_object() {
                reason = get_str(obj, "error_code");
            }
        }
        ParsedErrorBody {
            error_type: get_str(e, "type"),
            code: get_str(e, "code"),
            gstatus: get_str(e, "status"),
            message: get_str(e, "message").unwrap_or_default(),
            retry_delay: delay,
            reason,
        }
    }
}

#[derive(Debug, Default)]
pub struct ParsedErrorBody {
    pub error_type: Option<String>,
    pub code: Option<String>,
    pub gstatus: Option<String>,
    pub message: String,
    pub retry_delay: Option<ChronoDuration>,
    pub reason: Option<String>,
}

type ClockFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
type DelayFn = Arc<dyn Fn(Duration, &CancelToken) -> bool + Send + Sync>;

/// Sends one cloud request with retry policy, 90 s header timeout, and no redirects.
pub struct ProviderHttp {
    transport: Arc<dyn HttpTransport>,
    now: ClockFn,
    delay: DelayFn,
}

impl Default for ProviderHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderHttp {
    pub const HEADER_TIMEOUT: Duration = Duration::from_secs(90);
    pub const MAX_BUFFERED_BYTES: usize = 4 << 20;
    pub const MAX_ERROR_BODY_BYTES: usize = 64 << 10;

    pub fn new() -> Self {
        Self::with_transport(Arc::new(UreqTransport), None, None)
    }

    pub fn with_transport(
        transport: Arc<dyn HttpTransport>,
        now: Option<ClockFn>,
        delay: Option<DelayFn>,
    ) -> Self {
        Self {
            transport,
            now: now.unwrap_or_else(|| Arc::new(Utc::now)),
            delay: delay.unwrap_or_else(|| Arc::new(|d, ct| !ct.wait_timeout(d))),
        }
    }

    pub fn now(&self) -> DateTime<Utc> {
        (self.now)()
    }

    /// Returns a successful streaming response. Fails with [`ChatFailureError`] on any failure.
    pub fn send(
        &self,
        make: impl Fn() -> HttpRequest,
        endpoint: &str,
        model: &str,
        api_key: Option<&str>,
        mut on_retry: Option<&mut RetryHook<'_>>,
        ct: &CancelToken,
    ) -> Result<HttpResponse, ChatFailureError> {
        let mut attempt = 0i32;
        loop {
            let req = make();
            let resp = match self.transport.send(req, Self::HEADER_TIMEOUT, ct) {
                Ok(r) => r,
                Err(TransportError::Timeout) => {
                    return Err(ChatFailureError(ProviderErrors::timeout(
                        Self::HEADER_TIMEOUT,
                    )));
                }
                Err(TransportError::Offline(_)) => {
                    return Err(ChatFailureError(ProviderErrors::offline(endpoint)));
                }
                Err(TransportError::Cancelled) => {
                    return Err(ChatFailureError(ChatFailure::new(
                        "cancelled",
                        "Cancelled",
                        None,
                    )));
                }
            };
            if resp.is_success() {
                return Ok(resp);
            }
            let status = resp.status;
            let headers = resp.headers;
            let mut body_reader = resp.body;
            let body = Self::read_capped(&mut body_reader, Self::MAX_ERROR_BODY_BYTES)
                .unwrap_or_default();
            let failure =
                ProviderErrors::from_http(status, &body, &headers, model, (self.now)(), api_key);
            Log::info(&format!(
                "cloud request: HTTP {status} -> {} (attempt {})",
                failure.code,
                attempt + 1
            ));
            let Some(wait) = ProviderErrors::retry_delay(&failure, attempt, (self.now)()) else {
                return Err(ChatFailureError(failure));
            };
            if let Some(ref mut cb) = on_retry {
                cb(&failure, wait);
            }
            if !(self.delay)(wait, ct) || ct.is_cancelled() {
                return Err(ChatFailureError(ChatFailure::new(
                    "cancelled",
                    "Cancelled",
                    None,
                )));
            }
            attempt += 1;
        }
    }

    /// UTF-8 text of at most `max_bytes` from the start of `reader`; the rest is not read.
    pub fn read_capped<R: Read + ?Sized>(reader: &mut R, max_bytes: usize) -> io::Result<String> {
        let mut buf = vec![0u8; max_bytes];
        let mut n = 0usize;
        while n < max_bytes {
            match reader.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(r) => n += r,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
    }

    pub fn is_loopback(endpoint: &str) -> bool {
        let Some(host) = extract_host(endpoint) else {
            return false;
        };
        let clean = host.trim_start_matches('[').trim_end_matches(']');
        if clean.eq_ignore_ascii_case("localhost") {
            return true;
        }
        if let Ok(ip) = clean.parse::<std::net::IpAddr>() {
            return ip.is_loopback();
        }
        false
    }
}

/// Local daily budget (`ai.dailyRequests`, `ai.dailyTokens`; `0` = no limit), counted per local day.
pub struct AiBudget;

impl AiBudget {
    pub fn today(now: DateTime<Utc>) -> String {
        now.with_timezone(&Local).format("%Y-%m-%d").to_string()
    }

    pub fn usage(store: &EncryptedStore, ai: &AiSettings, now: DateTime<Utc>) -> AiUsage {
        let day = Self::today(now);
        let (requests, tokens) = store.ai_usage(&day).unwrap_or((0, 0));
        AiUsage {
            day,
            requests,
            tokens,
            request_limit: ai.daily_requests,
            token_limit: ai.daily_tokens,
        }
    }

    /// `None` when one more request is allowed, else the stop message.
    pub fn check(u: &AiUsage) -> Option<ChatFailure> {
        if u.request_limit > 0 && u.requests >= u.request_limit {
            return Some(ChatFailure::new(
                "budget",
                format!(
                    "Today's cloud budget is used up ({} of {} requests). It resets at midnight, or raise the limit in Settings > AI.",
                    format_commas(u.requests as i64),
                    format_commas(u.request_limit as i64),
                ),
                None,
            ));
        }
        if u.token_limit > 0 && u.tokens >= u.token_limit {
            return Some(ChatFailure::new(
                "budget",
                format!(
                    "Today's cloud budget is used up ({} of {} tokens). It resets at midnight, or raise the limit in Settings > AI.",
                    format_commas(u.tokens),
                    format_commas(u.token_limit),
                ),
                None,
            ));
        }
        None
    }

    /// Warning once usage reaches 80% of either limit; `None` below.
    pub fn warning(u: &AiUsage) -> Option<String> {
        if u.request_limit > 0 && (u.requests as f64) >= (u.request_limit as f64) * 0.8 {
            return Some(format!(
                "{} of today's {} cloud requests used.",
                format_commas(u.requests as i64),
                format_commas(u.request_limit as i64),
            ));
        }
        if u.token_limit > 0 && (u.tokens as f64) >= (u.token_limit as f64) * 0.8 {
            return Some(format!(
                "{} of today's {} cloud tokens used.",
                format_commas(u.tokens),
                format_commas(u.token_limit),
            ));
        }
        None
    }
}

fn has(v: Option<&str>, want: &str) -> bool {
    v.is_some_and(|s| s.eq_ignore_ascii_case(want))
}

fn header_num(headers: &[(String, String)], name: &str) -> Option<f64> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| v.trim().parse::<f64>().ok())
}

fn when_text(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(t) = at else {
        return " Try again in a minute.".to_string();
    };
    let wait = t.signed_duration_since(now);
    if wait <= ChronoDuration::seconds(1) {
        return " Try again now.".to_string();
    }
    if wait < ChronoDuration::minutes(2) {
        let secs = (wait.num_milliseconds() as f64 / 1000.0).ceil() as i64;
        return format!(" Try again in {secs} s.");
    }
    let local = t.with_timezone(&Local);
    format!(" Try again after {}.", local.format("%H:%M"))
}

fn extract_host(endpoint: &str) -> Option<String> {
    let (_, rest) = endpoint.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    if host_port.is_empty() {
        return None;
    }
    if host_port.starts_with('[') {
        let end = host_port.find(']')?;
        Some(host_port[..=end].to_string())
    } else {
        Some(host_port.split(':').next()?.to_string())
    }
}

pub(crate) fn format_commas(n: i64) -> String {
    let neg = n < 0;
    let s = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 + 1);
    if neg {
        out.push('-');
    }
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}
