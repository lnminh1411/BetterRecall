// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! OpenAI-compatible `/chat/completions` streaming chat provider (`OpenAiChat.cs`).

use std::collections::BTreeMap;
use std::io::Read;

use br_contracts::{AiDefaults, dotnet_base64};
use serde_json::{Map, Value, json};

use crate::ai::chat_provider::{ChatProvider, ChatRequest, ProviderTurn, ToolCall, ToolOutput};
use crate::ai::provider_http::{HttpRequest, ProviderStreamError};
use crate::ai::providers::sse_reader::{SseReadError, SseReader};
use crate::cancel::CancelToken;

pub struct OpenAiChat;

struct AccumulatedToolCall {
    index: i64,
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ChatProvider for OpenAiChat {
    fn name(&self) -> &'static str {
        AiDefaults::OPEN_AI
    }

    fn build_request(&self, request: &ChatRequest) -> HttpRequest {
        let ep = request.endpoint.trim_end_matches('/');
        let url = format!("{ep}/chat/completions");
        let mut messages = Vec::with_capacity(request.transcript.len() + 1);
        messages.push(json!({
            "role": "system",
            "content": request.system
        }));
        messages.extend(request.transcript.iter().cloned());

        let root = json!({
            "model": request.model,
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": messages,
            "tools": [{
                "type": "function",
                "function": {
                    "name": request.tool.name,
                    "description": request.tool.description,
                    "parameters": Value::Object(request.tool.parameters.clone())
                }
            }]
        });
        let mut req = HttpRequest::post_json(url, root.to_string().into_bytes());
        req.headers
            .push(("Accept".to_string(), "text/event-stream".to_string()));
        if let Some(ref k) = request.api_key
            && !k.is_empty()
        {
            req.headers
                .push(("Authorization".to_string(), format!("Bearer {k}")));
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
        let mut tool_call_map: BTreeMap<i64, AccumulatedToolCall> = BTreeMap::new();
        let mut finish_reason: Option<String> = None;
        let mut input_tokens = 0i64;
        let mut output_tokens = 0i64;

        let mut reader = body;
        SseReader::read(&mut reader, ct, |_ev, data| {
            if data.trim().is_empty() {
                return Ok(true);
            }
            if data.trim() == "[DONE]" {
                return Ok(false);
            }
            let parsed: Value = serde_json::from_str(data)
                .map_err(|e| SseReadError::InvalidData(e.to_string()))?;
            let Some(chunk) = parsed.as_object() else {
                return Ok(true);
            };

            if let Some(error) = chunk.get("error").and_then(Value::as_object) {
                let code = error.get("code").and_then(|v| {
                    if let Some(s) = v.as_str() {
                        Some(s.to_string())
                    } else if v.is_number() {
                        Some(v.to_string())
                    } else {
                        None
                    }
                });
                let err_type = error
                    .get("type")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or(code)
                    .unwrap_or_else(|| "server_error".to_string());
                let err_msg = error.get("message").and_then(Value::as_str).unwrap_or("");
                return Err(SseReadError::Stream(ProviderStreamError::new(
                    err_type, err_msg,
                )));
            }

            if let Some(usage) = chunk.get("usage").and_then(Value::as_object) {
                if let Some(p) = usage.get("prompt_tokens").and_then(Value::as_i64) {
                    input_tokens = p;
                }
                if let Some(c) = usage.get("completion_tokens").and_then(Value::as_i64) {
                    output_tokens = c;
                }
            }

            if let Some(choice0) = chunk
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_object)
            {
                if let Some(fr) = choice0.get("finish_reason").and_then(Value::as_str) {
                    finish_reason = Some(fr.to_string());
                }
                if let Some(delta) = choice0.get("delta").and_then(Value::as_object) {
                    if let Some(content) = delta.get("content").and_then(Value::as_str) {
                        on_text(content);
                        sb_text.push_str(content);
                    }
                    if let Some(tc_arr) = delta.get("tool_calls").and_then(Value::as_array) {
                        for tc_node in tc_arr {
                            let Some(tc) = tc_node.as_object() else {
                                continue;
                            };
                            let idx = tc.get("index").and_then(Value::as_i64).unwrap_or(0);
                            let acc = tool_call_map.entry(idx).or_insert_with(|| {
                                AccumulatedToolCall {
                                    index: idx,
                                    id: None,
                                    name: None,
                                    arguments: String::new(),
                                }
                            });
                            if acc.id.is_none()
                                && let Some(id) = tc.get("id").and_then(Value::as_str)
                            {
                                acc.id = Some(id.to_string());
                            }
                            if let Some(f) = tc.get("function").and_then(Value::as_object) {
                                if acc.name.is_none()
                                    && let Some(n) = f.get("name").and_then(Value::as_str)
                                {
                                    acc.name = Some(n.to_string());
                                }
                                if let Some(frag) = f.get("arguments").and_then(Value::as_str) {
                                    acc.arguments.push_str(frag);
                                }
                            }
                        }
                    }
                }
            }
            Ok(true)
        })?;

        let mut calls = Vec::new();
        for acc in tool_call_map.values() {
            let id = acc
                .id
                .clone()
                .unwrap_or_else(|| format!("call_{}", acc.index));
            let name = acc.name.clone().unwrap_or_default();
            let args = if acc.arguments.trim().is_empty() {
                Map::new()
            } else {
                serde_json::from_str::<Value>(&acc.arguments)
                    .ok()
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default()
            };
            calls.push(ToolCall { id, name, args });
        }

        let stop_reason = if !calls.is_empty() {
            "tool"
        } else {
            match finish_reason.as_deref() {
                Some("stop") => "end",
                Some("length") => "max_tokens",
                Some("content_filter") => "refusal",
                None | Some("") if !sb_text.is_empty() => "end",
                _ => "other",
            }
        }
        .to_string();

        let mut assistant = Map::new();
        assistant.insert("role".to_string(), Value::String("assistant".to_string()));
        assistant.insert(
            "content".to_string(),
            if sb_text.is_empty() {
                Value::Null
            } else {
                Value::String(sb_text.clone())
            },
        );
        if !tool_call_map.is_empty() {
            let tc_arr: Vec<Value> = tool_call_map
                .values()
                .map(|acc| {
                    json!({
                        "id": acc.id.clone().unwrap_or_else(|| format!("call_{}", acc.index)),
                        "type": "function",
                        "function": {
                            "name": acc.name.clone().unwrap_or_default(),
                            "arguments": acc.arguments
                        }
                    })
                })
                .collect();
            assistant.insert("tool_calls".to_string(), Value::Array(tc_arr));
        }

        Ok(ProviderTurn {
            assistant: Value::Object(assistant),
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
        for r in results {
            transcript.push(json!({
                "role": "tool",
                "tool_call_id": r.call_id,
                "content": r.text
            }));
        }
        let has_images = results.iter().any(|r| !r.images.is_empty());
        if has_images {
            let mut content = vec![json!({
                "type": "text",
                "text": "Images for the search results above (redacted)."
            })];
            for r in results {
                for img in &r.images {
                    content.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", img.mime, dotnet_base64::encode(&img.bytes))
                        }
                    }));
                }
            }
            transcript.push(json!({
                "role": "user",
                "content": content
            }));
        }
    }
}
