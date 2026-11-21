// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Chat provider trait and wire-format DTOs (`ChatProvider.cs`).

use std::io::Read;

use serde_json::{Map, Value};

use crate::ai::cloud_describer::OutgoingImage;
use crate::ai::provider_http::HttpRequest;
use crate::ai::providers::sse_reader::SseReadError;
use crate::cancel::CancelToken;

/// The one tool a chat model gets: search the user's history.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Map<String, Value>,
}

/// One provider request. `transcript` is the provider-native message list (append-only).
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub endpoint: String,
    pub model: String,
    pub api_key: Option<String>,
    pub system: String,
    pub transcript: Vec<Value>,
    pub tool: ToolSpec,
    pub max_tokens: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Map<String, Value>,
}

/// What one tool call returns to the model. Images are already redacted (`ChatShareGate`).
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub call_id: String,
    pub name: String,
    pub text: String,
    pub images: Vec<OutgoingImage>,
}

/// One assistant turn read from a stream.
/// `stop_reason`: `end` | `tool` | `max_tokens` | `refusal` | `context` | `other`.
#[derive(Debug, Clone)]
pub struct ProviderTurn {
    pub assistant: Value,
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub stop_reason: String,
}

/// Wire format of one provider: request building, SSE parsing, transcript appends.
pub trait ChatProvider: Send + Sync {
    fn name(&self) -> &'static str;

    /// A streaming POST with the key in the provider's header (none when `api_key` is None/empty).
    fn build_request(&self, request: &ChatRequest) -> HttpRequest;

    /// Reads the SSE body to the end; `on_text` gets each visible text delta.
    fn read_stream(
        &self,
        body: &mut dyn Read,
        on_text: &mut dyn FnMut(&str),
        ct: &CancelToken,
    ) -> Result<ProviderTurn, SseReadError>;

    fn append_user(&self, transcript: &mut Vec<Value>, text: &str);
    fn append_assistant(&self, transcript: &mut Vec<Value>, turn: &ProviderTurn);
    fn append_tool_results(&self, transcript: &mut Vec<Value>, results: &[ToolOutput]);
}
