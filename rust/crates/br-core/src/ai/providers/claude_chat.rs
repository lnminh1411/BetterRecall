// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Claude Messages API streaming chat provider (`ClaudeChat.cs`).

use std::collections::{BTreeMap, HashMap};
use std::io::Read;

use br_contracts::{AiDefaults, dotnet_base64};
use serde_json::{Map, Value, json};

use crate::ai::chat_provider::{ChatProvider, ChatRequest, ProviderTurn, ToolCall, ToolOutput};
use crate::ai::cloud_describer::CloudDescriber;
use crate::ai::provider_http::{HttpRequest, ProviderStreamError};
use crate::ai::providers::sse_reader::{SseReadError, SseReader};
use crate::cancel::CancelToken;

pub struct ClaudeChat;

impl ChatProvider for ClaudeChat {
    fn name(&self) -> &'static str {
        AiDefaults::CLAUDE
    }

    fn build_request(&self, request: &ChatRequest) -> HttpRequest {
        let ep = request.endpoint.trim_end_matches('/');
        let url = format!("{ep}/v1/messages");
        let root = json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "stream": true,
            "system": request.system,
            "messages": request.transcript,
            "tools": [{
                "name": request.tool.name,
                "description": request.tool.description,
                "input_schema": Value::Object(request.tool.parameters.clone()),
            }]
        });
        let mut req = HttpRequest::post_json(url, root.to_string().into_bytes());
        req.headers
            .push(("Accept".to_string(), "text/event-stream".to_string()));
        if let Some(ref k) = request.api_key
            && !k.is_empty()
        {
            req.headers.push(("x-api-key".to_string(), k.clone()));
        }
        req.headers.push((
            "anthropic-version".to_string(),
            CloudDescriber::CLAUDE_VERSION.to_string(),
        ));
        req
    }

    fn read_stream(
        &self,
        body: &mut dyn Read,
        on_text: &mut dyn FnMut(&str),
        ct: &CancelToken,
    ) -> Result<ProviderTurn, SseReadError> {
        let mut sb_text = String::new();
        let mut blocks: BTreeMap<i64, Map<String, Value>> = BTreeMap::new();
        let mut json_buffers: HashMap<i64, String> = HashMap::new();
        let mut field_buffers: HashMap<(i64, &'static str), String> = HashMap::new();
        let mut stop_reason_raw: Option<String> = None;
        let mut input_tokens = 0i64;
        let mut output_tokens = 0i64;

        let flush_block = |idx: i64,
                           blocks: &mut BTreeMap<i64, Map<String, Value>>,
                           field_buffers: &mut HashMap<(i64, &'static str), String>| {
            if let Some(block) = blocks.get_mut(&idx) {
                for f in ["text", "thinking"] {
                    if let Some(buf) = field_buffers.remove(&(idx, f)) {
                        block.insert(f.to_string(), Value::String(buf));
                    }
                }
            }
        };

        let mut reader = body;
        SseReader::read(&mut reader, ct, |ev, data| {
            if data.trim().is_empty() {
                return Ok(true);
            }
            let parsed: Value = serde_json::from_str(data)
                .map_err(|e| SseReadError::InvalidData(e.to_string()))?;
            let Some(root) = parsed.as_object() else {
                return Ok(true);
            };
            let ty = root.get("type").and_then(Value::as_str);

            if ev == "error" || ty == Some("error") || root.contains_key("error") {
                let err_obj = root.get("error").and_then(Value::as_object);
                let mut err_type = err_obj
                    .and_then(|o| o.get("type"))
                    .and_then(Value::as_str)
                    .or(ty)
                    .unwrap_or("api_error");
                if err_type == "error"
                    && let Some(sub) = err_obj.and_then(|o| o.get("type")).and_then(Value::as_str)
                {
                    err_type = sub;
                }
                let err_msg = err_obj
                    .and_then(|o| o.get("message"))
                    .and_then(Value::as_str)
                    .or_else(|| root.get("message").and_then(Value::as_str))
                    .unwrap_or("");
                return Err(SseReadError::Stream(ProviderStreamError::new(
                    err_type, err_msg,
                )));
            }

            let Some(ty) = ty else {
                return Ok(true);
            };
            if ty == "ping" {
                return Ok(true);
            }

            match ty {
                "message_start" => {
                    if let Some(usage) = root
                        .get("message")
                        .and_then(|m| m.get("usage"))
                        .and_then(Value::as_object)
                    {
                        input_tokens = usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
                        input_tokens += usage
                            .get("cache_creation_input_tokens")
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        input_tokens += usage
                            .get("cache_read_input_tokens")
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        if let Some(out) = usage.get("output_tokens").and_then(Value::as_i64) {
                            output_tokens = out;
                        }
                    }
                }
                "content_block_start" => {
                    if let (Some(idx), Some(cb)) = (
                        root.get("index").and_then(Value::as_i64),
                        root.get("content_block").and_then(Value::as_object),
                    ) {
                        blocks.insert(idx, cb.clone());
                        field_buffers.remove(&(idx, "text"));
                        field_buffers.remove(&(idx, "thinking"));
                    }
                }
                "content_block_delta" => {
                    if let (Some(idx), Some(delta)) = (
                        root.get("index").and_then(Value::as_i64),
                        root.get("delta").and_then(Value::as_object),
                    ) {
                        match delta.get("type").and_then(Value::as_str) {
                            Some("text_delta") => {
                                let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                                if let Some(block) = blocks.get(&idx) {
                                    let entry =
                                        field_buffers.entry((idx, "text")).or_insert_with(|| {
                                            block
                                                .get("text")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .to_string()
                                        });
                                    entry.push_str(text);
                                }
                                on_text(text);
                                sb_text.push_str(text);
                            }
                            Some("input_json_delta") => {
                                let partial = delta
                                    .get("partial_json")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                json_buffers.entry(idx).or_default().push_str(partial);
                            }
                            Some("thinking_delta") => {
                                let thinking = delta
                                    .get("thinking")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if let Some(block) = blocks.get(&idx) {
                                    let entry = field_buffers
                                        .entry((idx, "thinking"))
                                        .or_insert_with(|| {
                                            block
                                                .get("thinking")
                                                .and_then(Value::as_str)
                                                .unwrap_or("")
                                                .to_string()
                                        });
                                    entry.push_str(thinking);
                                }
                            }
                            Some("signature_delta") => {
                                let sig = delta
                                    .get("signature")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if let Some(block) = blocks.get_mut(&idx) {
                                    block.insert(
                                        "signature".to_string(),
                                        Value::String(sig.to_string()),
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_stop" => {
                    if let Some(idx) = root.get("index").and_then(Value::as_i64) {
                        flush_block(idx, &mut blocks, &mut field_buffers);
                        if let Some(block) = blocks.get_mut(&idx)
                            && block.get("type").and_then(Value::as_str) == Some("tool_use")
                        {
                            let raw = json_buffers.get(&idx).map_or("", String::as_str);
                            let parsed_input = if raw.trim().is_empty() {
                                Map::new()
                            } else {
                                serde_json::from_str::<Value>(raw)
                                    .ok()
                                    .and_then(|v| v.as_object().cloned())
                                    .unwrap_or_default()
                            };
                            block.insert("input".to_string(), Value::Object(parsed_input));
                        }
                    }
                }
                "message_delta" => {
                    if let Some(sr) = root
                        .get("delta")
                        .and_then(|d| d.get("stop_reason"))
                        .and_then(Value::as_str)
                    {
                        stop_reason_raw = Some(sr.to_string());
                    }
                    if let Some(out) = root
                        .get("usage")
                        .and_then(|u| u.get("output_tokens"))
                        .and_then(Value::as_i64)
                    {
                        output_tokens = out;
                    }
                }
                _ => {}
            }
            Ok(true)
        })?;

        let keys: Vec<i64> = blocks.keys().copied().collect();
        for idx in keys {
            flush_block(idx, &mut blocks, &mut field_buffers);
        }

        let mut calls = Vec::new();
        for block in blocks.values() {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = block
                    .get("input")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                calls.push(ToolCall { id, name, args });
            }
        }

        let stop_reason = match stop_reason_raw.as_deref() {
            Some("end_turn" | "stop_sequence") => "end",
            Some("tool_use") => "tool",
            Some("max_tokens") => "max_tokens",
            Some("refusal") => "refusal",
            Some("model_context_window_exceeded") => "context",
            _ => "other",
        }
        .to_string();

        let content: Vec<Value> = blocks.into_values().map(Value::Object).collect();
        let assistant = json!({
            "role": "assistant",
            "content": content
        });

        Ok(ProviderTurn {
            assistant,
            text: sb_text,
            calls,
            input_tokens,
            output_tokens,
            stop_reason,
        })
    }

    fn append_user(&self, transcript: &mut Vec<Value>, text: &str) {
        transcript.push(json!({
            "role": "user",
            "content": text
        }));
    }

    fn append_assistant(&self, transcript: &mut Vec<Value>, turn: &ProviderTurn) {
        transcript.push(turn.assistant.clone());
    }

    fn append_tool_results(&self, transcript: &mut Vec<Value>, results: &[ToolOutput]) {
        let mut content = Vec::with_capacity(results.len());
        for r in results {
            let mut inner = vec![json!({
                "type": "text",
                "text": r.text
            })];
            for img in &r.images {
                inner.push(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": img.mime,
                        "data": dotnet_base64::encode(&img.bytes)
                    }
                }));
            }
            content.push(json!({
                "type": "tool_result",
                "tool_use_id": r.call_id,
                "content": inner
            }));
        }
        transcript.push(json!({
            "role": "user",
            "content": content
        }));
    }
}
