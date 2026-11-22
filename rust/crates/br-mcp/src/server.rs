// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! MCP over stdio, JSON-RPC 2.0, one message per line (`McpServer.cs`).
//!
//! Dual-era:
//! - modern (`2026-07-28`): requests carry `params._meta["io.modelcontextprotocol/protocolVersion"]`,
//!   `server/discover`, results carry `resultType: "complete"`.
//! - legacy (`2025-11-25`, `2025-06-18`, `2025-03-26`, `2024-11-05`): `initialize` handshake.
//!
//! Tools: `search_history`, `get_capture`, `recall_status`. Tool results are text.

use std::sync::OnceLock;

use br_core::CommandProcessor;
use serde::Serialize;
use serde_json::{Map, Value};

/// Result of one agent MCP pipe call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeAnswer {
    pub ok: bool,
    pub text: String,
}

impl PipeAnswer {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            ok: true,
            text: text.into(),
        }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Self {
            ok: false,
            text: text.into(),
        }
    }
}

/// JSON-RPC 2.0 MCP server dispatching tool calls to `pipe`.
pub struct McpServer<F> {
    pipe: F,
    legacy_version: Option<&'static str>,
}

impl<F> McpServer<F>
where
    F: FnMut(&str) -> PipeAnswer,
{
    pub const MODERN: &'static str = "2026-07-28";
    pub const LEGACY_VERSIONS: [&'static str; 4] =
        ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
    const META_VERSION: &'static str = "io.modelcontextprotocol/protocolVersion";
    const INSTRUCTIONS: &'static str = "Searches the user's local BetterRecall history (screen keyframes by window title, app, on-screen text and image meaning; audio transcripts). Results are private data of the user.";

    const TOOLS_JSON: &'static str = r#"[
      {"name":"search_history","title":"Search BetterRecall history","description":"Search what the user saw or heard on this PC. Hybrid keyword and semantic search over window titles, app names, on-screen text, keyframe images and audio transcripts. Returns JSON with results (id, ts, kind, app, title, score, snippet).",
       "inputSchema":{"type":"object","properties":{
         "q":{"type":"string","description":"What to look for, natural language or keywords. 1 to 1024 characters.","minLength":1,"maxLength":1024},
         "limit":{"type":"integer","description":"Maximum results.","minimum":1},
         "from":{"type":"string","format":"date-time","description":"Only results at or after this ISO 8601 time."},
         "to":{"type":"string","format":"date-time","description":"Only results before this ISO 8601 time."},
         "app":{"type":"string","description":"Process image name, for example chrome.exe."},
         "kind":{"type":"array","items":{"type":"string","enum":["screen","audio"]},"description":"Capture kinds to include."},
         "mode":{"type":"string","enum":["hybrid","text","vector"],"description":"Search mode, default hybrid."}},
         "required":["q"],"additionalProperties":false},
       "annotations":{"readOnlyHint":true,"openWorldHint":false}},
      {"name":"get_capture","title":"Get one capture","description":"Full stored text of one search result: the on-screen text (private details removed) for screen captures, or the window title when no text was read; the transcript for audio.",
       "inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Result id from search_history."}},"required":["id"],"additionalProperties":false},
       "annotations":{"readOnlyHint":true,"openWorldHint":false}},
      {"name":"recall_status","title":"BetterRecall status","description":"Whether BetterRecall is capturing, paused or locked, and store statistics.",
       "inputSchema":{"type":"object","additionalProperties":false},
       "annotations":{"readOnlyHint":true,"openWorldHint":false}}
    ]"#;

    pub fn new(pipe: F) -> Self {
        Self {
            pipe,
            legacy_version: None,
        }
    }

    /// Handles one input line. Returns the response line, or `None` for notifications.
    pub fn handle(&mut self, line: &str) -> Option<String> {
        let root: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return Some(error_reply(None, -32700, "Parse error")),
        };
        let Value::Object(obj) = root else {
            return Some(error_reply(None, -32600, "Invalid Request"));
        };

        let id_el = obj.get("id");
        let has_id = matches!(id_el, Some(Value::String(_)) | Some(Value::Number(_)));
        let id = if has_id { id_el } else { None };

        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            return if has_id || id_el.is_none() {
                None
            } else {
                Some(error_reply(None, -32600, "Invalid Request"))
            };
        };

        if !has_id {
            return None; // notification: notifications/initialized, notifications/cancelled, ...
        }

        let params = obj.get("params");
        let modern_version = params
            .and_then(Value::as_object)
            .and_then(|p| p.get("_meta"))
            .and_then(Value::as_object)
            .and_then(|m| m.get(Self::META_VERSION))
            .and_then(Value::as_str);
        let modern = modern_version.is_some();
        if let Some(mv) = modern_version
            && mv != Self::MODERN
        {
            return Some(unsupported_version(id, mv));
        }

        match method {
            "initialize" => Some(self.initialize(id, params)),
            "server/discover" => Some(discover_reply(id)),
            "ping" => Some(ping_reply(id, modern)),
            "tools/list" => Some(tools_list_reply(id, modern)),
            "tools/call" => Some(self.call(id, params, modern)),
            _ => Some(error_reply(id, -32601, "Method not found")),
        }
    }

    fn initialize(&mut self, id: Option<&Value>, params: Option<&Value>) -> String {
        let requested = params
            .and_then(Value::as_object)
            .and_then(|p| p.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let negotiated = Self::LEGACY_VERSIONS
            .iter()
            .copied()
            .find(|&v| v == requested)
            .unwrap_or(Self::LEGACY_VERSIONS[0]);
        self.legacy_version = Some(negotiated);

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct InitResult<'a> {
            protocol_version: &'a str,
            capabilities: Capabilities,
            server_info: ServerInfo<'a>,
            instructions: &'a str,
        }

        result_reply(
            id,
            &InitResult {
                protocol_version: negotiated,
                capabilities: Capabilities::default(),
                server_info: ServerInfo::current(),
                instructions: Self::INSTRUCTIONS,
            },
        )
    }

    fn call(&mut self, id: Option<&Value>, params: Option<&Value>, modern: bool) -> String {
        let Some(p) = params.and_then(Value::as_object) else {
            return error_reply(id, -32602, "Invalid params");
        };
        let Some(name) = p.get("name").and_then(Value::as_str) else {
            return error_reply(id, -32602, "Invalid params");
        };
        let args = p.get("arguments").and_then(Value::as_object);

        let request = match name {
            "search_history" => {
                let Some(args) = args else {
                    return tool_result(id, modern, "Missing q.", true);
                };
                if !args.get("q").is_some_and(Value::is_string) {
                    return tool_result(id, modern, "Missing q.", true);
                }
                format!("search {}", search_json(args))
            }
            "get_capture" => {
                let cid = args.and_then(|a| a.get("id")).and_then(Value::as_str);
                let Some(cid) = cid else {
                    return tool_result(id, modern, "Missing or bad id.", true);
                };
                if cid.trim().is_empty()
                    || cid.chars().any(|c| c.is_whitespace() || c.is_control())
                {
                    return tool_result(id, modern, "Missing or bad id.", true);
                }
                format!("get {cid}")
            }
            "recall_status" => "status".to_string(),
            _ => return error_reply(id, -32602, &format!("Unknown tool: {name}")),
        };

        if request.len() > 4000 {
            return tool_result(id, modern, "Request too long.", true);
        }
        let answer = (self.pipe)(&request);
        tool_result(id, modern, &answer.text, !answer.ok)
    }
}

/// Copies only the known search fields in canonical order, keeping UTF-8 unescaped.
pub fn search_json(args: &Map<String, Value>) -> String {
    let mut out = String::from("{");
    let mut first = true;
    for key in ["q", "limit", "from", "to", "app", "kind", "mode"] {
        if let Some(v) = args.get(key) {
            if !first {
                out.push(',');
            }
            first = false;
            out.push('"');
            out.push_str(key);
            out.push_str("\":");
            let encoded = serde_json::to_string(v).expect("JSON value serialization cannot fail");
            out.push_str(&encoded);
        }
    }
    out.push('}');
    out
}

fn compact_tools_json() -> &'static str {
    static COMPACT: OnceLock<String> = OnceLock::new();
    COMPACT.get_or_init(|| {
        let mut out = String::with_capacity(McpServer::<fn(&str) -> PipeAnswer>::TOOLS_JSON.len());
        let mut in_string = false;
        let mut escaped = false;
        for ch in McpServer::<fn(&str) -> PipeAnswer>::TOOLS_JSON.chars() {
            if in_string {
                out.push(ch);
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
            } else if ch == '"' {
                in_string = true;
                out.push(ch);
            } else if !ch.is_whitespace() {
                out.push(ch);
            }
        }
        out
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolsCapability {
    list_changed: bool,
}

#[derive(Serialize)]
struct Capabilities {
    tools: ToolsCapability,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            tools: ToolsCapability {
                list_changed: false,
            },
        }
    }
}

#[derive(Serialize)]
struct ServerInfo<'a> {
    name: &'a str,
    title: &'a str,
    version: &'a str,
}

impl ServerInfo<'static> {
    fn current() -> Self {
        Self {
            name: "betterrecall",
            title: "BetterRecall",
            version: CommandProcessor::APP_VERSION,
        }
    }
}

fn write_head(out: &mut String, id: Option<&Value>) {
    out.push_str(r#"{"jsonrpc":"2.0","id":"#);
    match id {
        Some(v) => out.push_str(&serde_json::to_string(v).expect("id serialize")),
        None => out.push_str("null"),
    }
}

fn result_reply<T: Serialize>(id: Option<&Value>, result: &T) -> String {
    let mut out = String::new();
    write_head(&mut out, id);
    out.push_str(r#","result":"#);
    out.push_str(&serde_json::to_string(result).expect("result serialize"));
    out.push('}');
    out
}

fn error_reply(id: Option<&Value>, code: i32, message: &str) -> String {
    #[derive(Serialize)]
    struct ErrBody<'a> {
        code: i32,
        message: &'a str,
    }
    let mut out = String::new();
    write_head(&mut out, id);
    out.push_str(r#","error":"#);
    out.push_str(&serde_json::to_string(&ErrBody { code, message }).expect("error serialize"));
    out.push('}');
    out
}

fn unsupported_version(id: Option<&Value>, requested: &str) -> String {
    #[derive(Serialize)]
    struct Data<'a> {
        supported: [&'static str; 5],
        requested: &'a str,
    }
    #[derive(Serialize)]
    struct ErrBody<'a> {
        code: i32,
        message: &'static str,
        data: Data<'a>,
    }
    let supported = [
        McpServer::<fn(&str) -> PipeAnswer>::MODERN,
        McpServer::<fn(&str) -> PipeAnswer>::LEGACY_VERSIONS[0],
        McpServer::<fn(&str) -> PipeAnswer>::LEGACY_VERSIONS[1],
        McpServer::<fn(&str) -> PipeAnswer>::LEGACY_VERSIONS[2],
        McpServer::<fn(&str) -> PipeAnswer>::LEGACY_VERSIONS[3],
    ];
    let mut out = String::new();
    write_head(&mut out, id);
    out.push_str(r#","error":"#);
    out.push_str(
        &serde_json::to_string(&ErrBody {
            code: -32022,
            message: "Unsupported protocol version",
            data: Data {
                supported,
                requested,
            },
        })
        .expect("unsupported version serialize"),
    );
    out.push('}');
    out
}

fn discover_reply(id: Option<&Value>) -> String {
    #[derive(Serialize)]
    struct DiscoverMeta<'a> {
        #[serde(rename = "io.modelcontextprotocol/serverInfo")]
        server_info: ServerInfo<'a>,
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct DiscoverResult<'a> {
        result_type: &'static str,
        supported_versions: [&'static str; 5],
        capabilities: Capabilities,
        #[serde(rename = "_meta")]
        meta: DiscoverMeta<'a>,
        instructions: &'static str,
    }

    let supported_versions = [
        McpServer::<fn(&str) -> PipeAnswer>::MODERN,
        McpServer::<fn(&str) -> PipeAnswer>::LEGACY_VERSIONS[0],
        McpServer::<fn(&str) -> PipeAnswer>::LEGACY_VERSIONS[1],
        McpServer::<fn(&str) -> PipeAnswer>::LEGACY_VERSIONS[2],
        McpServer::<fn(&str) -> PipeAnswer>::LEGACY_VERSIONS[3],
    ];

    result_reply(
        id,
        &DiscoverResult {
            result_type: "complete",
            supported_versions,
            capabilities: Capabilities::default(),
            meta: DiscoverMeta {
                server_info: ServerInfo::current(),
            },
            instructions: McpServer::<fn(&str) -> PipeAnswer>::INSTRUCTIONS,
        },
    )
}

fn ping_reply(id: Option<&Value>, modern: bool) -> String {
    let mut out = String::new();
    write_head(&mut out, id);
    if modern {
        out.push_str(r#","result":{"resultType":"complete"}}"#);
    } else {
        out.push_str(r#","result":{}}"#);
    }
    out
}

fn tools_list_reply(id: Option<&Value>, modern: bool) -> String {
    let mut out = String::new();
    write_head(&mut out, id);
    out.push_str(r#","result":{"#);
    if modern {
        out.push_str(r#""resultType":"complete","#);
    }
    out.push_str(r#""tools":"#);
    out.push_str(compact_tools_json());
    out.push_str("}}");
    out
}

fn tool_result(id: Option<&Value>, modern: bool, text: &str, is_error: bool) -> String {
    #[derive(Serialize)]
    struct TextContent<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        text: &'a str,
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ToolBody<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        result_type: Option<&'static str>,
        content: [TextContent<'a>; 1],
        is_error: bool,
    }

    result_reply(
        id,
        &ToolBody {
            result_type: modern.then_some("complete"),
            content: [TextContent { kind: "text", text }],
            is_error,
        },
    )
}
