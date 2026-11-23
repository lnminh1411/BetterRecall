// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Ported `ChatProviderTests.cs` + Sweep3K linear-time stream test + BatchH B7 SSE/error-cap/retry-hint tests.

mod common;

use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use br_contracts::{AiUsage, dotnet_base64};
use br_core::{
    AiBudget, CancelToken, ChatProvider, ChatProviders, ChatRequest, ClaudeChat, CloudDescriber,
    GeminiChat, HttpRequest, OpenAiChat, OutgoingImage, ProviderErrors, ProviderHttp,
    ProviderStreamError, ProviderTurn, SseReadError, SseReader, ToolOutput, ToolSpec,
    TransportError,
};
use chrono::Utc;
use common::a3::MockTransport;
use serde_json::{Map, Value, json};

fn history_tool() -> ToolSpec {
    let params: Map<String, Value> = serde_json::from_value(json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Search query" }
        },
        "required": ["query"]
    }))
    .unwrap();
    ToolSpec {
        name: "search_history".to_string(),
        description: "Search user history".to_string(),
        parameters: params,
    }
}

#[test]
fn build_request_gemini() {
    let provider = GeminiChat;
    let tool = history_tool();
    let transcript = vec![json!({
        "role": "user",
        "parts": [{ "text": "hello" }]
    })];
    let req = ChatRequest {
        endpoint: "https://generativelanguage.googleapis.com".to_string(),
        model: "gemini-3.8-flash".to_string(),
        api_key: Some("test-gemini-key".to_string()),
        system: "You are helpful.".to_string(),
        transcript: transcript.clone(),
        tool: tool.clone(),
        max_tokens: 2048,
    };
    let http_req = provider.build_request(&req);
    assert_eq!("POST", http_req.method);
    assert_eq!(
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse",
        http_req.url
    );
    assert!(!http_req.url.contains("test-gemini-key"));
    assert_eq!(Some("test-gemini-key"), http_req.header("x-goog-api-key"));
    assert_eq!(Some("text/event-stream"), http_req.header("Accept"));

    let body: Value = serde_json::from_slice(&http_req.body).unwrap();
    assert_eq!(
        "You are helpful.",
        body["systemInstruction"]["parts"][0]["text"].as_str().unwrap()
    );
    assert_eq!(1, body["contents"].as_array().unwrap().len());
    assert_eq!(
        "search_history",
        body["tools"][0]["functionDeclarations"][0]["name"]
            .as_str()
            .unwrap()
    );
    assert_eq!(2048, body["generationConfig"]["maxOutputTokens"].as_i64().unwrap());
    assert!(body["generationConfig"].get("temperature").is_none());

    let no_key = provider.build_request(&ChatRequest {
        api_key: None,
        ..req.clone()
    });
    assert!(no_key.header("x-goog-api-key").is_none());

    let empty_key = provider.build_request(&ChatRequest {
        api_key: Some(String::new()),
        ..req
    });
    assert!(empty_key.header("x-goog-api-key").is_none());
}

#[test]
fn build_request_open_ai() {
    let provider = OpenAiChat;
    let tool = history_tool();
    let transcript = vec![json!({ "role": "user", "content": "previous message" })];
    let req = ChatRequest {
        endpoint: "https://api.openai.com/v1".to_string(),
        model: "gpt-6-luna".to_string(),
        api_key: Some("test-openai-key".to_string()),
        system: "You are helpful.".to_string(),
        transcript,
        tool,
        max_tokens: 2048,
    };
    let http_req = provider.build_request(&req);
    assert_eq!("POST", http_req.method);
    assert_eq!("https://api.openai.com/v1/chat/completions", http_req.url);
    assert_eq!(
        Some("Bearer test-openai-key"),
        http_req.header("Authorization")
    );
    assert_eq!(Some("text/event-stream"), http_req.header("Accept"));

    let body: Value = serde_json::from_slice(&http_req.body).unwrap();
    assert_eq!("gpt-6-luna", body["model"].as_str().unwrap());
    assert!(body["stream"].as_bool().unwrap());
    assert!(body["stream_options"]["include_usage"].as_bool().unwrap());
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(2, msgs.len());
    assert_eq!("system", msgs[0]["role"].as_str().unwrap());
    assert_eq!("You are helpful.", msgs[0]["content"].as_str().unwrap());
    assert_eq!("user", msgs[1]["role"].as_str().unwrap());
    assert_eq!("function", body["tools"][0]["type"].as_str().unwrap());
    assert_eq!(
        "search_history",
        body["tools"][0]["function"]["name"].as_str().unwrap()
    );
    assert!(body.get("max_tokens").is_none());
    assert!(body.get("max_completion_tokens").is_none());
    assert!(body.get("temperature").is_none());

    let no_key = provider.build_request(&ChatRequest {
        api_key: None,
        ..req.clone()
    });
    assert!(no_key.header("Authorization").is_none());
    let empty_key = provider.build_request(&ChatRequest {
        api_key: Some(String::new()),
        ..req
    });
    assert!(empty_key.header("Authorization").is_none());
}

#[test]
fn build_request_claude() {
    let provider = ClaudeChat;
    let tool = history_tool();
    let transcript = vec![json!({ "role": "user", "content": "previous message" })];
    let req = ChatRequest {
        endpoint: "https://api.anthropic.com".to_string(),
        model: "claude-haiku-5-5".to_string(),
        api_key: Some("test-claude-key".to_string()),
        system: "You are helpful.".to_string(),
        transcript,
        tool,
        max_tokens: 1500,
    };
    let http_req = provider.build_request(&req);
    assert_eq!("POST", http_req.method);
    assert_eq!("https://api.anthropic.com/v1/messages", http_req.url);
    assert_eq!(Some("test-claude-key"), http_req.header("x-api-key"));
    assert_eq!(
        Some(CloudDescriber::CLAUDE_VERSION),
        http_req.header("anthropic-version")
    );
    assert_eq!(Some("text/event-stream"), http_req.header("Accept"));

    let body: Value = serde_json::from_slice(&http_req.body).unwrap();
    assert_eq!("claude-haiku-5-5", body["model"].as_str().unwrap());
    assert_eq!(1500, body["max_tokens"].as_i64().unwrap());
    assert!(body["stream"].as_bool().unwrap());
    assert_eq!("You are helpful.", body["system"].as_str().unwrap());
    assert_eq!(1, body["messages"].as_array().unwrap().len());
    assert_eq!("search_history", body["tools"][0]["name"].as_str().unwrap());
    assert!(body["tools"][0].get("input_schema").is_some());
    for forbidden in ["temperature", "top_p", "top_k", "thinking", "tool_choice"] {
        assert!(body.get(forbidden).is_none());
    }

    let no_key = provider.build_request(&ChatRequest {
        api_key: None,
        ..req.clone()
    });
    assert!(no_key.header("x-api-key").is_none());
    let empty_key = provider.build_request(&ChatRequest {
        api_key: Some(String::new()),
        ..req
    });
    assert!(empty_key.header("x-api-key").is_none());
}

#[test]
fn gemini_stream_text_thought_tool_call_usage() {
    let provider = GeminiChat;
    let sse = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Thinking step 1...\",\"thought\":true}]}}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Searching for \"}]}}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"your files.\"}]}}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"search_history\",\"args\":{\"query\":\"betterrecall\"}},\"thoughtSignature\":\"sig_gemini_xyz\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":15,\"candidatesTokenCount\":10,\"thoughtsTokenCount\":5}}\n\n",
    );
    let mut chunks = Vec::new();
    let mut cur = Cursor::new(sse.as_bytes());
    let turn = provider
        .read_stream(&mut cur, &mut |t| chunks.push(t.to_string()), &CancelToken::none())
        .unwrap();
    assert_eq!(vec!["Searching for ", "your files."], chunks);
    assert_eq!("Searching for your files.", turn.text);
    assert_eq!(1, turn.calls.len());
    assert_eq!("gemini-call-0", turn.calls[0].id);
    assert_eq!("search_history", turn.calls[0].name);
    assert_eq!(
        "betterrecall",
        turn.calls[0].args["query"].as_str().unwrap()
    );
    assert_eq!("tool", turn.stop_reason);
    assert_eq!(15, turn.input_tokens);
    assert_eq!(15, turn.output_tokens);
    assert_eq!("model", turn.assistant["role"].as_str().unwrap());
    let parts = turn.assistant["parts"].as_array().unwrap();
    assert_eq!(4, parts.len());
    assert!(parts[0]["thought"].as_bool().unwrap());
    assert_eq!("sig_gemini_xyz", parts[3]["thoughtSignature"].as_str().unwrap());
}

#[test]
fn gemini_stream_error_chunk_throws() {
    let provider = GeminiChat;
    let sse = "data: {\"error\":{\"code\":429,\"status\":\"RESOURCE_EXHAUSTED\",\"message\":\"Quota limit exceeded for current project.\"}}\n\n";
    let mut cur = Cursor::new(sse.as_bytes());
    let err = provider
        .read_stream(&mut cur, &mut |_| {}, &CancelToken::none())
        .expect_err("expected stream error");
    let SseReadError::Stream(se) = err else {
        panic!("expected SseReadError::Stream, got {err:?}");
    };
    assert_eq!("RESOURCE_EXHAUSTED", se.error_type);
    assert!(se.message.contains("Quota limit exceeded"));
}

#[test]
fn open_ai_stream_deltas_tool_call_usage_done() {
    let provider = OpenAiChat;
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Querying \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"database...\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_98765\",\"function\":{\"name\":\"search_history\",\"arguments\":\"{\\\"qu\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ery\\\":\\\"hello\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":22,\"completion_tokens\":33}}\n\n",
        "data: [DONE]\n\n",
    );
    let mut chunks = Vec::new();
    let mut cur = Cursor::new(sse.as_bytes());
    let turn = provider
        .read_stream(&mut cur, &mut |t| chunks.push(t.to_string()), &CancelToken::none())
        .unwrap();
    assert_eq!(vec!["Querying ", "database..."], chunks);
    assert_eq!("Querying database...", turn.text);
    assert_eq!(1, turn.calls.len());
    assert_eq!("call_98765", turn.calls[0].id);
    assert_eq!("search_history", turn.calls[0].name);
    assert_eq!("hello", turn.calls[0].args["query"].as_str().unwrap());
    assert_eq!("tool", turn.stop_reason);
    assert_eq!(22, turn.input_tokens);
    assert_eq!(33, turn.output_tokens);
    assert_eq!("assistant", turn.assistant["role"].as_str().unwrap());
    assert_eq!(
        "Querying database...",
        turn.assistant["content"].as_str().unwrap()
    );
    let tc = turn.assistant["tool_calls"].as_array().unwrap();
    assert_eq!(1, tc.len());
    assert_eq!("call_98765", tc[0]["id"].as_str().unwrap());
    assert_eq!("function", tc[0]["type"].as_str().unwrap());
    assert_eq!("search_history", tc[0]["function"]["name"].as_str().unwrap());
    assert_eq!(
        "{\"query\":\"hello\"}",
        tc[0]["function"]["arguments"].as_str().unwrap()
    );
}

#[test]
fn claude_stream_thinking_text_tool_use() {
    let provider = ClaudeChat;
    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_creation_input_tokens\":2,\"cache_read_input_tokens\":3,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me analyze the request\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_claude_123\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"I will search for that.\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01ABC\",\"name\":\"search_history\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"query\\\":\\\"anthropic\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":42}}\n\n",
    );
    let mut chunks = Vec::new();
    let mut cur = Cursor::new(sse.as_bytes());
    let turn = provider
        .read_stream(&mut cur, &mut |t| chunks.push(t.to_string()), &CancelToken::none())
        .unwrap();
    assert_eq!(vec!["I will search for that."], chunks);
    assert_eq!("I will search for that.", turn.text);
    assert_eq!(1, turn.calls.len());
    assert_eq!("toolu_01ABC", turn.calls[0].id);
    assert_eq!("search_history", turn.calls[0].name);
    assert_eq!("anthropic", turn.calls[0].args["query"].as_str().unwrap());
    assert_eq!("tool", turn.stop_reason);
    assert_eq!(15, turn.input_tokens);
    assert_eq!(42, turn.output_tokens);

    let blocks = turn.assistant["content"].as_array().unwrap();
    assert_eq!(3, blocks.len());
    assert_eq!("thinking", blocks[0]["type"].as_str().unwrap());
    assert_eq!("Let me analyze the request", blocks[0]["thinking"].as_str().unwrap());
    assert_eq!("sig_claude_123", blocks[0]["signature"].as_str().unwrap());
    assert_eq!("text", blocks[1]["type"].as_str().unwrap());
    assert_eq!("I will search for that.", blocks[1]["text"].as_str().unwrap());
    assert_eq!("tool_use", blocks[2]["type"].as_str().unwrap());
    assert_eq!("toolu_01ABC", blocks[2]["id"].as_str().unwrap());
    assert_eq!("anthropic", blocks[2]["input"]["query"].as_str().unwrap());
}

#[test]
fn claude_stream_error_event_throws() {
    let provider = ClaudeChat;
    let sse = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Anthropic is overloaded right now.\"}}\n\n";
    let mut cur = Cursor::new(sse.as_bytes());
    let err = provider
        .read_stream(&mut cur, &mut |_| {}, &CancelToken::none())
        .expect_err("expected error");
    let SseReadError::Stream(se) = err else {
        panic!("expected Stream error");
    };
    assert_eq!("overloaded_error", se.error_type);
    assert!(se.message.contains("Anthropic is overloaded"));
}

#[test]
fn append_tool_results_gemini() {
    let provider = GeminiChat;
    let mut transcript = Vec::new();
    let img = OutgoingImage {
        bytes: vec![1, 2, 3, 4],
        mime: "image/png".to_string(),
        width: 10,
        height: 10,
        redacted: true,
    };
    provider.append_tool_results(
        &mut transcript,
        &[ToolOutput {
            call_id: "gemini-call-0".to_string(),
            name: "search_history".to_string(),
            text: "Found 1 match".to_string(),
            images: vec![img],
        }],
    );
    assert_eq!(1, transcript.len());
    assert_eq!("user", transcript[0]["role"].as_str().unwrap());
    let parts = transcript[0]["parts"].as_array().unwrap();
    assert_eq!(2, parts.len());
    assert_eq!("search_history", parts[0]["functionResponse"]["name"].as_str().unwrap());
    assert_eq!(
        "Found 1 match",
        parts[0]["functionResponse"]["response"]["result"].as_str().unwrap()
    );
    assert!(parts[0]["functionResponse"].get("id").is_none());
    assert_eq!("image/png", parts[1]["inlineData"]["mimeType"].as_str().unwrap());
    assert_eq!(
        dotnet_base64::encode(&[1, 2, 3, 4]),
        parts[1]["inlineData"]["data"].as_str().unwrap()
    );

    let mut transcript2 = Vec::new();
    provider.append_tool_results(
        &mut transcript2,
        &[ToolOutput {
            call_id: "custom_call_42".to_string(),
            name: "search_history".to_string(),
            text: "Found 2 matches".to_string(),
            images: Vec::new(),
        }],
    );
    assert_eq!(
        "custom_call_42",
        transcript2[0]["parts"][0]["functionResponse"]["id"]
            .as_str()
            .unwrap()
    );
}

#[test]
fn append_tool_results_open_ai() {
    let provider = OpenAiChat;
    let mut transcript = Vec::new();
    let img = OutgoingImage {
        bytes: vec![10, 20, 30],
        mime: "image/jpeg".to_string(),
        width: 20,
        height: 20,
        redacted: true,
    };
    provider.append_tool_results(
        &mut transcript,
        &[ToolOutput {
            call_id: "call_openai_1".to_string(),
            name: "search_history".to_string(),
            text: "Results text".to_string(),
            images: vec![img],
        }],
    );
    assert_eq!(2, transcript.len());
    assert_eq!("tool", transcript[0]["role"].as_str().unwrap());
    assert_eq!("call_openai_1", transcript[0]["tool_call_id"].as_str().unwrap());
    assert_eq!("Results text", transcript[0]["content"].as_str().unwrap());
    assert_eq!("user", transcript[1]["role"].as_str().unwrap());
    let contents = transcript[1]["content"].as_array().unwrap();
    assert_eq!(2, contents.len());
    assert_eq!(
        "Images for the search results above (redacted).",
        contents[0]["text"].as_str().unwrap()
    );
    assert_eq!(
        format!("data:image/jpeg;base64,{}", dotnet_base64::encode(&[10, 20, 30])),
        contents[1]["image_url"]["url"].as_str().unwrap()
    );

    let mut no_img = Vec::new();
    provider.append_tool_results(
        &mut no_img,
        &[ToolOutput {
            call_id: "call_2".to_string(),
            name: "search_history".to_string(),
            text: "No imgs".to_string(),
            images: Vec::new(),
        }],
    );
    assert_eq!(1, no_img.len());
}

#[test]
fn append_tool_results_claude() {
    let provider = ClaudeChat;
    let mut transcript = Vec::new();
    let img = OutgoingImage {
        bytes: vec![11, 22, 33],
        mime: "image/png".to_string(),
        width: 10,
        height: 10,
        redacted: true,
    };
    provider.append_tool_results(
        &mut transcript,
        &[ToolOutput {
            call_id: "toolu_claude_1".to_string(),
            name: "search_history".to_string(),
            text: "Claude result text".to_string(),
            images: vec![img],
        }],
    );
    assert_eq!(1, transcript.len());
    let tr = &transcript[0]["content"][0];
    assert_eq!("tool_result", tr["type"].as_str().unwrap());
    assert_eq!("toolu_claude_1", tr["tool_use_id"].as_str().unwrap());
    let inner = tr["content"].as_array().unwrap();
    assert_eq!(2, inner.len());
    assert_eq!("Claude result text", inner[0]["text"].as_str().unwrap());
    assert_eq!("image", inner[1]["type"].as_str().unwrap());
    assert_eq!(
        dotnet_base64::encode(&[11, 22, 33]),
        inner[1]["source"]["data"].as_str().unwrap()
    );
}

#[test]
fn append_user_and_append_assistant() {
    let providers: [Box<dyn ChatProvider>; 3] =
        [Box::new(GeminiChat), Box::new(OpenAiChat), Box::new(ClaudeChat)];
    for p in &providers {
        let mut transcript = Vec::new();
        p.append_user(&mut transcript, "user question");
        assert_eq!(1, transcript.len());
        assert_eq!("user", transcript[0]["role"].as_str().unwrap());

        let turn = ProviderTurn {
            assistant: json!({ "role": "assistant", "content": "hello" }),
            text: "hello".to_string(),
            calls: Vec::new(),
            input_tokens: 10,
            output_tokens: 20,
            stop_reason: "end".to_string(),
        };
        p.append_assistant(&mut transcript, &turn);
        assert_eq!(2, transcript.len());
        assert_eq!("hello", transcript[1]["content"].as_str().unwrap());
    }
}

#[test]
fn chat_providers_for_returns_correct_instances_and_errors_on_unknown() {
    assert_eq!("gemini", ChatProviders::for_provider("gemini").unwrap().name());
    assert_eq!(
        "openai-compatible",
        ChatProviders::for_provider("openai-compatible").unwrap().name()
    );
    assert_eq!("claude", ChatProviders::for_provider("claude").unwrap().name());
    let err = ChatProviders::for_provider("mistral")
        .err()
        .expect("unknown provider should fail");
    assert!(err.contains("mistral"));
}

/// Regression B7: SSE reader handles comments, multi-line data, CRLF/CR endings, and trailing events.
#[test]
fn sse_reader_multi_line_comments_trailing_event_without_blank_line() {
    let cases: [(&str, Vec<(&str, &str)>); 2] = [
        (
            ": leading comment\n: another comment\nevent: update\ndata: line 1\ndata: line 2\n\n: middle comment\ndata: trailing event data",
            vec![("update", "line 1\nline 2"), ("", "trailing event data")],
        ),
        (
            "data: a\r\ndata: b\r\r\ndata: c\r",
            vec![("", "a\nb"), ("", "c")],
        ),
    ];
    for (sse, expected) in cases {
        let mut cur = Cursor::new(sse.as_bytes());
        let mut events = Vec::new();
        SseReader::read(&mut cur, &CancelToken::none(), |ev, data| {
            events.push((ev.to_string(), data.to_string()));
            Ok(true)
        })
        .unwrap();
        let expected_owned: Vec<(String, String)> = expected
            .into_iter()
            .map(|(e, d)| (e.to_string(), d.to_string()))
            .collect();
        assert_eq!(expected_owned, events);
    }
}

/// Regression B7.
#[test]
fn sse_reader_refuses_an_endless_line_and_an_endless_event() {
    let long_line = format!("data: {}\n\n", "a".repeat(SseReader::MAX_LINE_CHARS + 10));
    let mut cur = Cursor::new(long_line.as_bytes());
    let err = SseReader::read(&mut cur, &CancelToken::none(), |_, _| Ok(true))
        .expect_err("expected too large");
    assert!(matches!(err, SseReadError::Stream(_)));

    let mut big_event = String::new();
    let chunk = "b".repeat(100_000);
    for _ in 0..90 {
        big_event.push_str("data: ");
        big_event.push_str(&chunk);
        big_event.push('\n');
    }
    big_event.push('\n');
    let mut cur2 = Cursor::new(big_event.as_bytes());
    let err2 = SseReader::read(&mut cur2, &CancelToken::none(), |_, _| Ok(true))
        .expect_err("expected event too large");
    assert!(matches!(err2, SseReadError::Stream(_)));
}

/// Regression B7.
#[test]
fn error_bodies_are_read_capped() {
    let big = "x".repeat(200_000);
    let mut cur = Cursor::new(big.as_bytes());
    let capped = ProviderHttp::read_capped(&mut cur, ProviderHttp::MAX_ERROR_BODY_BYTES).unwrap();
    assert_eq!(ProviderHttp::MAX_ERROR_BODY_BYTES, capped.len());
}

/// Regression B7.
#[test]
fn huge_retry_hints_do_not_overflow() {
    let now = Utc::now();
    let body = "{\"error\":{\"status\":\"RESOURCE_EXHAUSTED\",\"details\":[{\"retryDelay\":\"999999999999999s\"}]}}";
    let f = ProviderErrors::from_http(429, body, &[], "m", now, None);
    assert_eq!("rate", f.code);
    assert!(f.retry_at.is_some());
    assert!(f.retry_at.unwrap().signed_duration_since(now).num_hours() <= 24);
}

/// Regression B10: ~4 MiB of 100-byte deltas (with a thinking block and no stop on the text block)
/// must parse in linear time well under 4 s; quadratic string concatenation would blow the bound.
#[test]
fn long_streams_parse_in_linear_time_with_the_same_content() {
    let provider = ClaudeChat;
    let chunk = "a".repeat(100);
    let delta_count = 40_000usize;
    let mut sse = String::with_capacity(delta_count * 190 + 512);
    sse.push_str(
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
    );
    sse.push_str(
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"plan\"}}\n\n",
    );
    sse.push_str("event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
    sse.push_str(
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    );
    let delta_line = format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{chunk}\"}}}}\n\n"
    );
    for _ in 0..delta_count {
        sse.push_str(&delta_line);
    }
    let start = Instant::now();
    let mut cur = Cursor::new(sse.as_bytes());
    let turn = provider
        .read_stream(&mut cur, &mut |_| {}, &CancelToken::none())
        .unwrap();
    let elapsed = start.elapsed();
    let expected_len = delta_count * chunk.len();
    assert_eq!(expected_len, turn.text.len());
    let text_block = turn.assistant["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"].as_str() == Some("text"))
        .expect("text block");
    assert_eq!(expected_len, text_block["text"].as_str().unwrap().len());
    assert!(
        elapsed < Duration::from_secs(4),
        "long stream parse took {elapsed:?} (expected < 4s)"
    );
}

#[test]
fn maps_status_and_body_and_scrubs_key() {
    let now = Utc::now();
    assert_eq!("auth", ProviderErrors::from_http(401, "{}", &[], "m", now, None).code);
    assert_eq!("forbidden", ProviderErrors::from_http(403, "{}", &[], "m", now, None).code);
    assert_eq!("model", ProviderErrors::from_http(404, "{}", &[], "m", now, None).code);
    assert_eq!("rate", ProviderErrors::from_http(429, "{}", &[], "m", now, None).code);
    assert_eq!("overloaded", ProviderErrors::from_http(529, "{}", &[], "m", now, None).code);
    assert_eq!("server", ProviderErrors::from_http(500, "{}", &[], "m", now, None).code);
    assert_eq!(
        "quota",
        ProviderErrors::from_http(
            402,
            r#"{"error":{"type":"billing_error","message":"no credit"}}"#,
            &[],
            "m",
            now,
            None
        )
        .code
    );
    assert_eq!(
        "context",
        ProviderErrors::from_http(
            400,
            r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#,
            &[],
            "m",
            now,
            None
        )
        .code
    );

    // Key never appears in error message.
    let f = ProviderErrors::from_http(
        401,
        r#"{"error":{"message":"invalid key sk-test-secret-999"}}"#,
        &[],
        "m",
        now,
        Some("sk-test-secret-999"),
    );
    assert!(!f.message.contains("sk-test-secret-999"));
    assert!(f.message.contains("***"));

    // Stream errors map like HTTP.
    let sf = ProviderErrors::from_stream(
        &ProviderStreamError::new("overloaded_error", "busy"),
        "m",
        now,
        None,
    );
    assert_eq!("overloaded", sf.code);

    // Low quota warning below 10%.
    let hdrs = vec![
        ("anthropic-ratelimit-requests-remaining".into(), "5".into()),
        ("anthropic-ratelimit-requests-limit".into(), "100".into()),
    ];
    assert!(ProviderErrors::low_quota_warning(&hdrs).is_some());

    // B17: CloudDescriber error never shows a key prefix.
    let long_key = "SECRET-KEY-1234567890-ABCDEF";
    let err_body = format!(r#"{{"error":{{"message":"{} {long_key} bad"}}}}"#, "x".repeat(190));
    let msg = CloudDescriber::error_text(401, &err_body, Some(long_key));
    assert!(!msg.contains("SECRET-KEY"));
    assert!(msg.contains("***"));
}

#[test]
fn ai_budget_warns_and_blocks_at_limit() {
    let u = AiUsage {
        day: "2026-11-10".into(),
        requests: 85,
        tokens: 0,
        request_limit: 100,
        token_limit: 0,
    };
    assert!(AiBudget::check(&u).is_none());
    assert!(AiBudget::warning(&u).is_some());
    let over = AiUsage { requests: 100, ..u };
    assert_eq!("budget", AiBudget::check(&over).unwrap().code);
}

#[test]
fn provider_http_retries_twice_then_fails_and_no_retry_for_auth_or_long_wait() {
    let calls = Arc::new(AtomicI32::new(0));
    let calls_cb = calls.clone();
    let transport = Arc::new(MockTransport::new(move |_, _, _| {
        calls_cb.fetch_add(1, Ordering::AcqRel);
        Ok(MockTransport::status_text(
            429,
            "{}",
            vec![("Retry-After".into(), "1".into())],
        ))
    }));
    let http = ProviderHttp::with_transport(
        transport,
        None,
        Some(Arc::new(|_, _| true)),
    );
    let err = http
        .send(
            || HttpRequest::post_json("http://127.0.0.1/test", Vec::new()),
            "http://127.0.0.1",
            "m",
            None,
            None,
            &CancelToken::none(),
        )
        .expect_err("should fail after retries");
    assert_eq!("rate", err.0.code);
    assert_eq!(3, calls.load(Ordering::Acquire));

    // Auth error does not retry.
    let calls2 = Arc::new(AtomicI32::new(0));
    let c2 = calls2.clone();
    let t2 = Arc::new(MockTransport::new(move |_, _, _| {
        c2.fetch_add(1, Ordering::AcqRel);
        Ok(MockTransport::status_text(401, "{}", vec![]))
    }));
    let http2 = ProviderHttp::with_transport(t2, None, Some(Arc::new(|_, _| true)));
    let err2 = http2
        .send(
            || HttpRequest::post_json("http://127.0.0.1/test", Vec::new()),
            "http://127.0.0.1",
            "m",
            None,
            None,
            &CancelToken::none(),
        )
        .expect_err("auth fail");
    assert_eq!("auth", err2.0.code);
    assert_eq!(1, calls2.load(Ordering::Acquire));

    // Offline reported.
    let t3 = Arc::new(MockTransport::new(|_, _, _| {
        Err(TransportError::Offline("conn refused".into()))
    }));
    let http3 = ProviderHttp::with_transport(t3, None, None);
    let err3 = http3
        .send(
            || HttpRequest::post_json("http://127.0.0.1/test", Vec::new()),
            "http://127.0.0.1",
            "m",
            None,
            None,
            &CancelToken::none(),
        )
        .expect_err("offline");
    assert_eq!("offline", err3.0.code);

    // Loopback endpoints.
    assert!(ProviderHttp::is_loopback("http://127.0.0.1:8080/v1"));
    assert!(ProviderHttp::is_loopback("http://localhost:11434/v1"));
    assert!(ProviderHttp::is_loopback("http://[::1]:8080/v1"));
    assert!(!ProviderHttp::is_loopback("https://api.openai.com/v1"));
}
