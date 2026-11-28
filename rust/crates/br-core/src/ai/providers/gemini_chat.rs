// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Gemini `streamGenerateContent` SSE chat provider (`GeminiChat.cs`).

use std::io::Read;

use br_contracts::{AiDefaults, dotnet_base64};
use serde_json::{Map, Value, json};

use crate::ai::chat_provider::{ChatProvider, ChatRequest, ProviderTurn, ToolCall, ToolOutput};
use crate::ai::provider_http::{HttpRequest, ProviderStreamError};
use crate::ai::providers::sse_reader::{SseReadError, SseReader};
use crate::cancel::CancelToken;

pub struct GeminiChat;

impl ChatProvider for GeminiChat {
    fn name(&self) -> &'static str {
        AiDefaults::GEMINI
    }

    fn build_request(&self, request: &ChatRequest) -> HttpRequest {
        let ep = request.endpoint.trim_end_matches('/');
        let url = format!("{ep}/v1beta/models/{}:streamGenerateContent?alt=sse", request.model);
        let root = json!({
            "systemInstruction": {
                "parts": [{ "text": request.system }]
            },
            "contents": request.transcript,
            "tools": [{
                "functionDeclarations": [{
                    "name": request.tool.name,
                    "description": request.tool.description,
                    "parameters": Value::Object(request.tool.parameters.clone())
                }]
            }],
            "generationConfig": {
                "maxOutputTokens": request.max_tokens
            }
        });
        let mut req = HttpRequest::post_json(url, root.to_string().into_bytes());
        req.headers
            .push(("Accept".to_string(), "text/event-stream".to_string()));
        if let Some(ref k) = request.api_key
            && !k.is_empty()
        {
            req.headers.push(("x-goog-api-key".to_string(), k.clone()));
        }
        req
    }

    fn read_stream(
        &self,
        body: &mut dyn Read,
        on_text: &mut dyn FnMut(&str),
        ct: &CancelToken,
    ) -> Result<ProviderTurn, SseReadError> {
        let mut sb_text = String::new();
        let mut kept_parts: Vec<Value> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut tool_call_index = 0usize;
        let mut finish_reason: Option<String> = None;
        let mut input_tokens = 0i64;
        let mut output_tokens = 0i64;

        let mut reader = body;
        SseReader::read(&mut reader, ct, |_ev, data| {
            if data.trim().is_empty() {
                return Ok(true);
            }
            let parsed: Value = serde_json::from_str(data)
                .map_err(|e| SseReadError::InvalidData(e.to_string()))?;
            let Some(chunk) = parsed.as_object() else {
                return Ok(true);
            };

            if let Some(error) = chunk.get("error").and_then(Value::as_object) {
                let status = error
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("INTERNAL");
                let message = error.get("message").and_then(Value::as_str).unwrap_or("");
                return Err(SseReadError::Stream(ProviderStreamError::new(
                    status, message,
                )));
            }

            if let Some(usage) = chunk.get("usageMetadata").and_then(Value::as_object) {
                input_tokens = usage
                    .get("promptTokenCount")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let cand = usage
                    .get("candidatesTokenCount")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let thoughts = usage
                    .get("thoughtsTokenCount")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                output_tokens = cand + thoughts;
            }

            if let Some(cand0) = chunk
                .get("candidates")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_object)
            {
                if let Some(fr) = cand0.get("finishReason").and_then(Value::as_str) {
                    finish_reason = Some(fr.to_string());
                }
                if let Some(parts) = cand0
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(Value::as_array)
                {
                    for part in parts {
                        if part.is_null() {
                            continue;
                        }
                        kept_parts.push(part.clone());
                        if let Some(part_obj) = part.as_object() {
                            let is_thought = part_obj
                                .get("thought")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            if !is_thought
                                && let Some(text) = part_obj.get("text").and_then(Value::as_str)
                            {
                                on_text(text);
                                sb_text.push_str(text);
                            }
                            if let Some(fc) = part_obj.get("functionCall").and_then(Value::as_object)
                            {
                                let id = fc
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .filter(|s| !s.is_empty())
                                    .map(String::from)
                                    .unwrap_or_else(|| {
                                        let id = format!("gemini-call-{tool_call_index}");
                                        tool_call_index += 1;
                                        id
                                    });
                                let name = fc
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                let args = fc
                                    .get("args")
                                    .and_then(Value::as_object)
                                    .cloned()
                                    .unwrap_or_else(Map::new);
                                tool_calls.push(ToolCall { id, name, args });
                            }
                        }
                    }
                }
            }
            Ok(true)
        })?;

        let stop_reason = if !tool_calls.is_empty() {
            "tool"
        } else {
            match finish_reason.as_deref() {
                Some("STOP") => "end",
                Some("MAX_TOKENS") => "max_tokens",
                Some(
                    "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII"
                    | "IMAGE_SAFETY",
                ) => "refusal",
                None | Some("") if !sb_text.is_empty() => "end",
                _ => "other",
            }
        }
        .to_string();

        let assistant = json!({
            "role": "model",
            "parts": kept_parts
        });

        Ok(ProviderTurn {
            assistant,
            text: sb_text,
            calls: tool_calls,
            input_tokens,
            output_tokens,
            stop_reason,
        })
    }

    fn append_user(&self, transcript: &mut Vec<Value>, text: &str) {
        transcript.push(json!({
            "role": "user",
            "parts": [{ "text": text }]
        }));
    }

    fn append_assistant(&self, transcript: &mut Vec<Value>, turn: &ProviderTurn) {
        transcript.push(turn.assistant.clone());
    }

    fn append_tool_results(&self, transcript: &mut Vec<Value>, results: &[ToolOutput]) {
        let mut parts = Vec::new();
        for r in results {
            let mut fr = Map::new();
            fr.insert("name".to_string(), Value::String(r.name.clone()));
            fr.insert("response".to_string(), json!({ "result": r.text }));
            if !r.call_id.starts_with("gemini-call-") {
                fr.insert("id".to_string(), Value::String(r.call_id.clone()));
            }
            parts.push(json!({ "functionResponse": Value::Object(fr) }));
        }
        for r in results {
            for img in &r.images {
                parts.push(json!({
                    "inlineData": {
                        "mimeType": img.mime,
                        "data": dotnet_base64::encode(&img.bytes)
                    }
                }));
            }
        }
        transcript.push(json!({
            "role": "user",
            "parts": parts
        }));
    }
}
