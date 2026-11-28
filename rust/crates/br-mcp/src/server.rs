// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! MCP over stdio, JSON-RPC 2.0, one message per line (`McpServer.cs`).
//!
//! Dual-era:
//! - modern (`2026-07-28`): requests carry `params._meta["io.modelcontextprotocol/protocolVersion"]`,
//!   `server/discover`, results carry `resultType: "complete"`.
//! - legacy (`2025-11-25`, `2025-06-18`, `2025-03-26`, `2024-11-05`): `initialize` handshake.
//!
//! Tools: `search_history`, `get_capture`, `recall_status`. Tool results carry both a text block
//! and `structuredContent` (with matching `outputSchema` in `tools/list`) on protocol versions
//! `2025-06-18` and newer; earlier versions (`2024-11-05`, `2025-03-26`) return text only.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use br_core::CommandProcessor;
use serde::{Deserialize, Serialize};
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
    const INSTRUCTIONS: &'static str = "Searches the user's local BetterRecall history (screen keyframes by window title, app, on-screen text and image meaning; audio transcripts). Results are the user's private data and must not be sent to other tools or services unless the user asks. These tools are read-only and nothing leaves this PC through them. Use get_capture for the full text of one result, and pass cursor from nextCursor to search_history for more results.";

    const TOOLS_JSON: &'static str = r#"[
      {"name":"search_history","title":"Search BetterRecall history","description":"Search what the user saw or heard on this PC. Hybrid keyword and semantic search over window titles, app names, on-screen text, keyframe images and audio transcripts. Returns JSON with results (id, ts, kind, app, title, score, snippet) and nextCursor when more results exist (agent limits ipc.defaultResults and ipc.maxResults apply).",
       "inputSchema":{"type":"object","properties":{
         "q":{"type":"string","description":"What to look for, natural language or keywords. 1 to 1024 characters.","minLength":1,"maxLength":1024},
         "limit":{"type":"integer","description":"Maximum results per page (defaults to ipc.defaultResults; agent maximum ipc.maxResults applies).","minimum":1},
         "from":{"type":"string","format":"date-time","description":"Only results at or after this ISO 8601 time."},
         "to":{"type":"string","format":"date-time","description":"Only results before this ISO 8601 time."},
         "app":{"type":"string","description":"Process image name, for example chrome.exe."},
         "kind":{"type":"array","items":{"type":"string","enum":["screen","audio"]},"description":"Capture kinds to include."},
         "mode":{"type":"string","enum":["hybrid","text","vector"],"description":"Search mode, default hybrid."},
         "cursor":{"type":"string","description":"Opaque pagination cursor from a previous search_history nextCursor."}},
         "required":["q"],"additionalProperties":false},
       "outputSchema":{"type":"object","properties":{
         "count":{"type":"integer"},
         "truncated":{"type":"boolean"},
         "tookMs":{"type":"integer"},
         "results":{"type":"array","items":{"type":"object"}},
         "degraded":{"type":"string"},
         "hasMore":{"type":"boolean"},
         "nextCursor":{"type":"string"}},
         "required":["count","truncated","tookMs","results"]},
       "annotations":{"readOnlyHint":true,"openWorldHint":false}},
      {"name":"get_capture","title":"Get one capture","description":"Full stored text of one search result: the on-screen text (private details removed) for screen captures, or the window title when no text was read; the transcript for audio.",
       "inputSchema":{"type":"object","properties":{"id":{"type":"string","description":"Result id from search_history."}},"required":["id"],"additionalProperties":false},
       "outputSchema":{"type":"object","properties":{
         "id":{"type":"string"},
         "ts":{"type":"string","format":"date-time"},
         "kind":{"type":"string","enum":["screen","audio","replay"]},
         "app":{"type":"string"},
         "title":{"type":"string"},
         "text":{"type":"string"},
         "hasThumb":{"type":"boolean"},
         "durationS":{"type":"integer"},
         "site":{"type":"string"},
         "replayId":{"type":"string"},
         "replayOffsetMs":{"type":"integer"}},
         "required":["id","ts","kind","app","title","text","hasThumb","site"]},
       "annotations":{"readOnlyHint":true,"openWorldHint":false}},
      {"name":"recall_status","title":"BetterRecall status","description":"Whether BetterRecall is capturing, paused or locked, and store statistics.",
       "inputSchema":{"type":"object","additionalProperties":false},
       "outputSchema":{"type":"object","properties":{
         "capturing":{"type":"boolean"},
         "paused":{"type":"boolean"},
         "pausedUntil":{"type":"string","format":"date-time"},
         "pauseReason":{"type":"string"},
         "locked":{"type":"boolean"},
         "unlockedUntil":{"type":"string","format":"date-time"},
         "audio":{"type":"object"},
         "queue":{"type":"object"},
         "device":{"type":"string"},
         "store":{"type":"object"},
         "retentionDays":{"type":"integer"},
         "hotkeysFailed":{"type":"array","items":{"type":"string"}},
         "warnings":{"type":"array","items":{"type":"string"}},
         "userPaused":{"type":"boolean"}},
         "required":["capturing","paused","locked","audio","queue","device","store","retentionDays","userPaused"]},
       "annotations":{"readOnlyHint":true,"openWorldHint":false}}
    ]"#;

    pub fn new(pipe: F) -> Self {
        Self {
            pipe,
            legacy_version: None,
        }
    }

    fn supports_structured(&self, modern: bool) -> bool {
        modern || !matches!(self.legacy_version, Some("2024-11-05" | "2025-03-26"))
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
            "tools/list" => Some(tools_list_reply(
                id,
                modern,
                self.supports_structured(modern),
            )),
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
        let structured = self.supports_structured(modern);

        let mut search_page: Option<SearchPage> = None;
        let request = match name {
            "search_history" => {
                let Some(args) = args else {
                    return tool_result(id, modern, "Missing q.", None, true);
                };
                if !args.get("q").is_some_and(Value::is_string) {
                    return tool_result(id, modern, "Missing q.", None, true);
                }
                let client_limit = match args.get("limit") {
                    None => None,
                    Some(v) => match v.as_i64() {
                        Some(n) if n >= 1 => Some(n),
                        _ => return tool_result(id, modern, "Invalid limit.", None, true),
                    },
                };
                let client_to = match args.get("to") {
                    None => None,
                    Some(v) => match v.as_str() {
                        Some(s) => Some(s.to_string()),
                        None => return tool_result(id, modern, "Invalid to.", None, true),
                    },
                };
                let (offset, anchor_to, query_hash, expected_degraded) = match args.get("cursor") {
                    None => {
                        let anchor = client_to.unwrap_or_else(now_utc_iso8601);
                        let qh = search_query_hash(args, &anchor);
                        (0usize, anchor, qh, None)
                    }
                    Some(v) => {
                        let Some(cur) = v.as_str().and_then(decode_cursor) else {
                            return tool_result(id, modern, "Invalid cursor.", None, true);
                        };
                        if let Some(ref ct) = client_to
                            && ct != &cur.t
                        {
                            return tool_result(id, modern, "Invalid cursor.", None, true);
                        }
                        let qh = search_query_hash(args, &cur.t);
                        if cur.h != qh[..CURSOR_HASH_HEX_LEN] {
                            return tool_result(id, modern, "Invalid cursor.", None, true);
                        }
                        (cur.o as usize, cur.t, qh, Some(cur.d))
                    }
                };
                let mut forwarded = args.clone();
                forwarded.remove("cursor");
                match client_limit {
                    Some(n) => {
                        forwarded.insert("limit".to_string(), Value::from(n));
                    }
                    None => {
                        forwarded.remove("limit");
                    }
                }
                if offset > 0 {
                    forwarded.insert("offset".to_string(), Value::from(offset as u64));
                } else {
                    forwarded.remove("offset");
                }
                forwarded.insert("to".to_string(), Value::String(anchor_to.clone()));
                search_page = Some(SearchPage {
                    offset,
                    anchor_to,
                    query_hash,
                    expected_degraded,
                });
                format!("search {}", search_json(&forwarded))
            }
            "get_capture" => {
                let cid = args.and_then(|a| a.get("id")).and_then(Value::as_str);
                let Some(cid) = cid else {
                    return tool_result(id, modern, "Missing or bad id.", None, true);
                };
                if cid.trim().is_empty()
                    || cid.chars().any(|c| c.is_whitespace() || c.is_control())
                {
                    return tool_result(id, modern, "Missing or bad id.", None, true);
                }
                format!("get {cid}")
            }
            "recall_status" => "status".to_string(),
            _ => return error_reply(id, -32602, &format!("Unknown tool: {name}")),
        };

        if request.len() > 4000 {
            return tool_result(id, modern, "Request too long.", None, true);
        }
        let answer = (self.pipe)(&request);
        if !answer.ok {
            return tool_result(id, modern, &answer.text, None, true);
        }
        if let Ok(mut parsed) = serde_json::from_str::<Value>(&answer.text)
            && parsed.is_object()
        {
            if let Some(page) = search_page {
                match apply_search_page(&mut parsed, &page) {
                    Ok(true) => {
                        let text = serde_json::to_string(&parsed).expect("paged search serialize");
                        return tool_result(
                            id,
                            modern,
                            &text,
                            structured.then_some(&parsed),
                            false,
                        );
                    }
                    Ok(false) => {}
                    Err(msg) => return tool_result(id, modern, msg, None, true),
                }
            }
            return tool_result(
                id,
                modern,
                &answer.text,
                structured.then_some(&parsed),
                false,
            );
        }
        tool_result(id, modern, &answer.text, None, false)
    }
}

const CURSOR_HASH_HEX_LEN: usize = 32;
const ERR_RESULTS_CHANGED: &str = "Search results changed; search again without cursor.";

struct SearchPage {
    offset: usize,
    anchor_to: String,
    query_hash: String,
    expected_degraded: Option<Option<String>>,
}

#[derive(Serialize, Deserialize)]
struct CursorData {
    o: u32,
    t: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    d: Option<String>,
    h: String,
}

fn format_unix_secs_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let min = (rem % 3_600) / 60;
    let sec = rem % 60;

    // Howard Hinnant's civil_from_days (days since 1970-01-01).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn now_utc_iso8601() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let base = format_unix_secs_utc(dur.as_secs());
    let ms = dur.subsec_millis();
    format!("{}.{ms:03}Z", &base[..base.len() - 1])
}

fn search_query_hash(args: &Map<String, Value>, anchor_to: &str) -> String {
    let mut out = String::from("{");
    let mut first = true;
    for key in ["q", "from", "to", "app", "kind", "mode"] {
        let encoded = if key == "to" {
            serde_json::to_string(anchor_to).expect("JSON string serialization cannot fail")
        } else if let Some(v) = args.get(key) {
            serde_json::to_string(v).expect("JSON value serialization cannot fail")
        } else {
            continue;
        };
        if !first {
            out.push(',');
        }
        first = false;
        out.push('"');
        out.push_str(key);
        out.push_str("\":");
        out.push_str(&encoded);
    }
    out.push('}');
    CommandProcessor::sha256_hex(&out)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let chunks = bytes.chunks_exact(2);
    if !chunks.remainder().is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in chunks {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn encode_cursor(
    offset: usize,
    anchor_to: &str,
    degraded: Option<&str>,
    query_hash: &str,
) -> String {
    let data = CursorData {
        o: offset as u32,
        t: anchor_to.to_string(),
        d: degraded.map(String::from),
        h: query_hash[..CURSOR_HASH_HEX_LEN].to_string(),
    };
    let json = serde_json::to_vec(&data).expect("cursor serialize");
    hex_encode(&json)
}

fn decode_cursor(cursor: &str) -> Option<CursorData> {
    let raw = hex_decode(cursor)?;
    let data: CursorData = serde_json::from_slice(&raw).ok()?;
    if data.h.len() != CURSOR_HASH_HEX_LEN || data.t.is_empty() {
        return None;
    }
    Some(data)
}

fn apply_search_page(val: &mut Value, page: &SearchPage) -> Result<bool, &'static str> {
    let Some(obj) = val.as_object_mut() else {
        return Ok(false);
    };
    let resp_degraded = obj
        .get("degraded")
        .and_then(Value::as_str)
        .map(String::from);
    if let Some(ref expected) = page.expected_degraded
        && &resp_degraded != expected
    {
        return Err(ERR_RESULTS_CHANGED);
    }

    let has_more = obj
        .get("hasMore")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let count = obj.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;

    if has_more && count > 0 {
        let next_offset = page.offset.saturating_add(count);
        obj.insert(
            "nextCursor".to_string(),
            Value::String(encode_cursor(
                next_offset,
                &page.anchor_to,
                resp_degraded.as_deref(),
                &page.query_hash,
            )),
        );
    } else {
        obj.remove("nextCursor");
    }
    Ok(true)
}

/// Copies only the known search fields in canonical order, keeping UTF-8 unescaped.
pub fn search_json(args: &Map<String, Value>) -> String {
    let mut out = String::from("{");
    let mut first = true;
    for key in ["q", "limit", "offset", "from", "to", "app", "kind", "mode"] {
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

fn compact_tools_json_no_output_schema() -> &'static str {
    static COMPACT_LEGACY: OnceLock<String> = OnceLock::new();
    COMPACT_LEGACY.get_or_init(|| {
        let mut tools: Vec<Value> =
            serde_json::from_str(McpServer::<fn(&str) -> PipeAnswer>::TOOLS_JSON)
                .expect("TOOLS_JSON is valid JSON");
        for tool in &mut tools {
            if let Some(obj) = tool.as_object_mut() {
                obj.remove("outputSchema");
            }
        }
        serde_json::to_string(&tools).expect("legacy tools serialize")
    })
}

fn tools_list_reply(id: Option<&Value>, modern: bool, structured: bool) -> String {
    let mut out = String::new();
    write_head(&mut out, id);
    out.push_str(r#","result":{"#);
    if modern {
        out.push_str(r#""resultType":"complete","#);
    }
    out.push_str(r#""tools":"#);
    out.push_str(if structured {
        compact_tools_json()
    } else {
        compact_tools_json_no_output_schema()
    });
    out.push_str("}}");
    out
}

fn tool_result(
    id: Option<&Value>,
    modern: bool,
    text: &str,
    structured: Option<&Value>,
    is_error: bool,
) -> String {
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
        #[serde(skip_serializing_if = "Option::is_none")]
        structured_content: Option<&'a Value>,
        is_error: bool,
    }

    result_reply(
        id,
        &ToolBody {
            result_type: modern.then_some("complete"),
            content: [TextContent { kind: "text", text }],
            structured_content: structured,
            is_error,
        },
    )
}
