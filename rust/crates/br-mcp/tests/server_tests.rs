// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::cell::RefCell;
use std::io::{self, BufReader, Cursor, Read};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use br_contracts::InstallLayout;
use br_mcp::{HostOptions, McpHost, McpPipe, McpServer, PipeAnswer, StaleState, is_server_role};
use serde_json::Value;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::PCWSTR;

static MUTEX_SEQ: AtomicU64 = AtomicU64::new(1);

struct TestMutex {
    name: String,
    handle: HANDLE,
}

impl TestMutex {
    fn create() -> Self {
        let seq = MUTEX_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            r"Local\BetterRecall.Test.McpSetup.{}.{seq}",
            std::process::id()
        );
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { CreateMutexW(None, true, PCWSTR(wide.as_ptr())) }
            .expect("CreateMutexW failed");
        Self { name, handle }
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for TestMutex {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

fn parse(line: Option<String>) -> Value {
    let s = line.expect("expected response line");
    serde_json::from_str(&s).expect("valid JSON response")
}

fn make_server(
    ok: bool,
    text: &str,
    sent: Rc<RefCell<Vec<String>>>,
) -> McpServer<impl FnMut(&str) -> PipeAnswer> {
    let text = text.to_string();
    McpServer::new(move |line: &str| {
        sent.borrow_mut().push(line.to_string());
        PipeAnswer {
            ok,
            text: text.clone(),
        }
    })
}

#[test]
fn legacy_initialize_negotiates() {
    for (asked, got) in [
        ("2025-11-25", "2025-11-25"),
        ("2025-06-18", "2025-06-18"),
        ("2024-11-05", "2024-11-05"),
        ("2099-01-01", "2025-11-25"),
    ] {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let mut s = make_server(true, r#"{"pong":true}"#, sent);
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{asked}","capabilities":{{}},"clientInfo":{{"name":"t","version":"1"}}}}}}"#
        );
        let r = parse(s.handle(&req));
        assert_eq!(r["result"]["protocolVersion"].as_str(), Some(got));
        assert_eq!(
            r["result"]["capabilities"]["tools"]["listChanged"].as_bool(),
            Some(false)
        );
        assert_eq!(
            r["result"]["serverInfo"]["name"].as_str(),
            Some("betterrecall")
        );
    }
}

#[test]
fn notifications_get_no_reply_and_parse_errors_do() {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut s = make_server(true, r#"{"pong":true}"#, sent);
    assert!(
        s.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none()
    );
    assert_eq!(parse(s.handle("{nope"))["error"]["code"].as_i64(), Some(-32700));
    assert_eq!(
        parse(s.handle(r#"{"jsonrpc":"2.0","id":"a","method":"resources/list"}"#))["error"]["code"]
            .as_i64(),
        Some(-32601)
    );
    assert_eq!(parse(s.handle("[1,2,3]"))["error"]["code"].as_i64(), Some(-32600));
}

#[test]
fn tools_list_is_one_line_with_three_tools() {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut s = make_server(true, r#"{"pong":true}"#, sent);
    let line = s
        .handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
        .expect("tools/list reply");
    assert!(!line.contains('\n'));
    let v: Value = serde_json::from_str(&line).expect("valid JSON");
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(names, vec!["search_history", "get_capture", "recall_status"]);
}

#[test]
fn tool_calls_map_to_pipe_verbs() {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut s = make_server(true, r#"{"count":0}"#, Rc::clone(&sent));

    let r = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_history","arguments":{"q":"acme","limit":5,"evil":"x"}}}"#,
    ));
    let sent_search = sent.borrow().last().expect("search call").clone();
    let sent_json: Value =
        serde_json::from_str(sent_search.strip_prefix("search ").expect("search verb"))
            .expect("valid search json");
    assert_eq!(sent_json["q"].as_str(), Some("acme"));
    assert_eq!(sent_json["limit"].as_u64(), Some(5));
    assert!(sent_json.get("evil").is_none());
    assert!(sent_json.get("offset").is_none());
    assert!(sent_json["to"].as_str().is_some_and(|s| s.ends_with('Z')));
    assert_eq!(r["result"]["isError"].as_bool(), Some(false));
    assert_eq!(
        r["result"]["content"][0]["text"].as_str(),
        Some(r#"{"count":0}"#)
    );

    let _ = s.handle(
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_capture","arguments":{"id":"s_01ABC"}}}"#,
    );
    assert_eq!(sent.borrow().last().map(String::as_str), Some("get s_01ABC"));

    let _ = s.handle(
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"recall_status"}}"#,
    );
    assert_eq!(sent.borrow().last().map(String::as_str), Some("status"));

    // An id with a space could smuggle a second verb argument: refused before the pipe.
    let bad = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"get_capture","arguments":{"id":"a b"}}}"#,
    ));
    assert_eq!(bad["result"]["isError"].as_bool(), Some(true));
    assert_eq!(sent.borrow().len(), 3);

    // Missing q
    let missing_q = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"search_history","arguments":{}}}"#,
    ));
    assert_eq!(missing_q["result"]["isError"].as_bool(), Some(true));
    assert_eq!(
        missing_q["result"]["content"][0]["text"].as_str(),
        Some("Missing q.")
    );

    // Unknown tool
    let unknown = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"purge"}}"#,
    ));
    assert_eq!(unknown["error"]["code"].as_i64(), Some(-32602));
}

#[test]
fn pipe_errors_become_tool_errors() {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut s = make_server(false, "Store is locked.", sent);
    let r = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall_status","arguments":{}}}"#,
    ));
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));
    assert_eq!(
        r["result"]["content"][0]["text"].as_str(),
        Some("Store is locked.")
    );
}

#[test]
fn modern_requests_and_unsupported_version() {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut s = make_server(true, r#"{"pong":true}"#, sent);
    let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}"#;

    let d = parse(s.handle(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{{{meta}}}}}"#
    )));
    assert_eq!(d["result"]["resultType"].as_str(), Some("complete"));
    let supported: Vec<&str> = d["result"]["supportedVersions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(supported.contains(&"2026-07-28"));

    let call = parse(s.handle(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"recall_status",{meta}}}}}"#
    )));
    assert_eq!(call["result"]["resultType"].as_str(), Some("complete"));

    let e = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"1900-01-01"}}}"#,
    ));
    assert_eq!(e["error"]["code"].as_i64(), Some(-32022));
    assert_eq!(e["error"]["data"]["requested"].as_str(), Some("1900-01-01"));
}

#[test]
fn cjk_query_of_1024_characters_reaches_the_pipe_unescaped() {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut s = make_server(true, "{}", Rc::clone(&sent));
    let q: String = std::iter::repeat_n('検', 1024).collect();
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"search_history","arguments":{{"q":"{q}"}}}}}}"#
    );
    let reply = s.handle(&req).expect("reply");
    assert!(!reply.contains("Request too long"));
    let sent_ref = sent.borrow();
    assert_eq!(sent_ref.len(), 1);
    assert!(sent_ref[0].contains(&q));
    assert!(sent_ref[0].len() < 4000);
}

#[test]
fn panicking_pipe_call_is_answered_with_internal_error() {
    struct NeverStale;
    impl StaleState for NeverStale {
        fn is_stale_now(&self) -> bool {
            false
        }
        fn marked_stale(&self) -> bool {
            false
        }
        fn relayed(&self) -> bool {
            true
        }
    }

    let mut server = McpServer::new(|_| -> PipeAnswer { panic!("simulated pipe error") });
    let input = Cursor::new(
        b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"recall_status\"}}\n"
            .to_vec(),
    );
    let mut out = Vec::new();
    let code = McpHost::run(
        BufReader::new(input),
        &mut out,
        &mut server,
        &NeverStale,
        HostOptions::default(),
    );
    assert_eq!(code, 0);
    let v: Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["id"].as_i64(), Some(2));
    assert_eq!(v["error"]["code"].as_i64(), Some(-32603));
    assert!(
        McpHost::internal_error(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none()
    );
}

#[test]
fn stale_server_with_setup_mutex_relayed_exits_75_without_answering() {
    let mutex = TestMutex::create();
    let pipe = McpPipe::new(
        "BRtest.unused.pipe",
        r"C:\unused\BetterRecall.Agent.exe",
        mutex.name(),
        true,
    );
    assert!(pipe.is_stale());

    let pipe_ref = &pipe;
    let mut server = McpServer::new(|line| pipe_ref.call(line));
    let input = Cursor::new(
        b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"recall_status\"}}\n"
            .to_vec(),
    );
    let mut out = Vec::new();
    let code = McpHost::run(
        BufReader::new(input),
        &mut out,
        &mut server,
        &pipe,
        HostOptions::default(),
    );
    assert_eq!(code, InstallLayout::MCP_RESTART_EXIT_CODE);
    assert!(out.is_empty());
}

#[test]
fn stale_server_not_relayed_answers_updated_and_exits_75() {
    let mutex = TestMutex::create();
    let pipe = McpPipe::new(
        "BRtest.unused.pipe",
        r"C:\unused\BetterRecall.Agent.exe",
        mutex.name(),
        false,
    );

    let pipe_ref = &pipe;
    let mut server = McpServer::new(|line| pipe_ref.call(line));
    let line =
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"recall_status\"}}\n";
    let input = Cursor::new(format!("{line}{line}").into_bytes());
    let mut out = Vec::new();
    let code = McpHost::run(
        BufReader::new(input),
        &mut out,
        &mut server,
        &pipe,
        HostOptions::default(),
    );
    assert_eq!(code, InstallLayout::MCP_RESTART_EXIT_CODE);
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1);
    let v: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["result"]["isError"].as_bool(), Some(true));
    assert_eq!(
        v["result"]["content"][0]["text"].as_str(),
        Some(McpPipe::UPDATED)
    );
}

#[test]
fn stale_server_relayed_exits_75_while_idle() {
    struct BlockingReader {
        rx: mpsc::Receiver<Vec<u8>>,
        entered: Arc<AtomicBool>,
    }
    impl Read for BlockingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.entered.store(true, Ordering::Release);
            match self.rx.recv() {
                Ok(data) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Ok(n)
                }
                Err(_) => Ok(0),
            }
        }
    }

    let stale_flag = Arc::new(AtomicBool::new(false));
    let reader_entered = Arc::new(AtomicBool::new(false));
    struct ToggleStale(Arc<AtomicBool>);
    impl StaleState for ToggleStale {
        fn is_stale_now(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
        fn marked_stale(&self) -> bool {
            false
        }
        fn relayed(&self) -> bool {
            true
        }
    }

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let flag_bg = Arc::clone(&stale_flag);
    let entered_bg = Arc::clone(&reader_entered);
    let bg = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !entered_bg.load(Ordering::Acquire) {
            if std::time::Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        flag_bg.store(true, Ordering::Release);
        // Keep stdin open until the test finishes (10 s watchdog prevents hang on failure).
        let _ = done_rx.recv_timeout(Duration::from_secs(10));
        drop(tx);
    });

    let mut server = McpServer::new(|_| PipeAnswer::ok("{}"));
    let mut out = Vec::new();
    let code = McpHost::run(
        BufReader::new(BlockingReader {
            rx,
            entered: reader_entered,
        }),
        &mut out,
        &mut server,
        &ToggleStale(stale_flag),
        HostOptions {
            poll_interval: Duration::from_millis(25),
        },
    );
    let _ = done_tx.send(());
    let _ = bg.join();
    assert_eq!(code, InstallLayout::MCP_RESTART_EXIT_CODE);
    assert!(out.is_empty());
}

#[test]
fn is_server_role_detects_agent_exe() {
    // Same fallback order as br-core's test_tmp_root; never the system temp folder.
    let base = std::env::var("BR_TEST_TMP")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("CARGO_TARGET_DIR")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|d| std::path::PathBuf::from(d).join("test-tmp"))
        })
        .unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().and_then(|d| d.parent()).map(|d| d.join("test-tmp")))
                .expect("unable to determine cargo target dir for test temp folder")
        });
    let dir = base.join(format!(
        "mcp-role-{}-{}",
        std::process::id(),
        MUTEX_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!is_server_role(&dir));
    std::fs::write(dir.join(InstallLayout::AGENT_EXE), b"").unwrap();
    assert!(is_server_role(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_utf8_line_does_not_end_mcp_session_f3() {
    struct NeverStale;
    impl StaleState for NeverStale {
        fn is_stale_now(&self) -> bool {
            false
        }
        fn marked_stale(&self) -> bool {
            false
        }
        fn relayed(&self) -> bool {
            true
        }
    }

    let mut server = McpServer::new(|_| PipeAnswer::ok(r#"{"items":[]}"#));
    let mut raw_input = Vec::new();
    // Raw 0xE9 byte inside a JSON-RPC request line (Latin-1 'é', invalid UTF-8).
    raw_input.extend_from_slice(
        br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_history","arguments":{"q":"caf"#,
    );
    raw_input.push(0xE9);
    raw_input.extend_from_slice(b"\"}}}\n");
    // Malformed JSON line with raw 0xE9 byte -> should get parse error (-32700), not close session.
    raw_input.extend_from_slice(b"{not-json-");
    raw_input.push(0xE9);
    raw_input.extend_from_slice(b"\n");
    // Valid ping after the invalid UTF-8 lines must still be answered.
    raw_input.extend_from_slice(br#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#);
    raw_input.push(b'\n');

    let mut out = Vec::new();
    let code = McpHost::run(
        BufReader::new(Cursor::new(raw_input)),
        &mut out,
        &mut server,
        &NeverStale,
        HostOptions::default(),
    );
    assert_eq!(code, 0);
    let text = String::from_utf8(out).expect("UTF-8 output");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 replies, got: {lines:?}");

    let r0: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(r0["id"].as_i64(), Some(2));

    let r1: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(r1["error"]["code"].as_i64(), Some(-32700));

    let r2: Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(r2["id"].as_i64(), Some(3));
    assert_eq!(r2["result"], serde_json::json!({}));
}

fn make_search_pipe_server(
    total: usize,
    sent: Rc<RefCell<Vec<String>>>,
) -> McpServer<impl FnMut(&str) -> PipeAnswer> {
    McpServer::new(move |line: &str| {
        sent.borrow_mut().push(line.to_string());
        let json_part = line.strip_prefix("search ").expect("search verb");
        let req: Value = serde_json::from_str(json_part).expect("valid search json");
        let limit = req.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
        let offset = req.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let start = offset.min(total);
        let end = offset.saturating_add(limit).min(total);
        let results: Vec<Value> = (start..end)
            .map(|i| {
                serde_json::json!({
                    "id": format!("s_{i:03}"),
                    "ts": "2026-11-21T09:30:00Z",
                    "kind": "screen",
                    "app": "chrome.exe",
                    "title": format!("Hit {i}"),
                    "score": 0.9,
                    "snippet": format!("snippet {i}"),
                    "hasThumb": true,
                    "site": ""
                })
            })
            .collect();
        let has_more = end < total;
        let mut body = serde_json::json!({
            "count": results.len(),
            "truncated": false,
            "tookMs": 7,
            "results": results
        });
        if has_more {
            body.as_object_mut()
                .unwrap()
                .insert("hasMore".to_string(), Value::Bool(true));
        }
        PipeAnswer::ok(serde_json::to_string(&body).unwrap())
    })
}

#[test]
fn tools_list_output_schema_depends_on_protocol_version() {
    // 2025-06-18 and 2025-11-25 include outputSchema for all three tools.
    for ver in ["2025-06-18", "2025-11-25"] {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let mut s = make_server(true, "{}", sent);
        let _ = parse(s.handle(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{ver}","capabilities":{{}},"clientInfo":{{"name":"t","version":"1"}}}}}}"#
        )));
        let list = parse(s.handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#));
        let tools = list["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 3);
        for t in tools {
            assert_eq!(
                t["outputSchema"]["type"].as_str(),
                Some("object"),
                "missing outputSchema for {} under {ver}",
                t["name"]
            );
        }
    }

    // Modern (2026-07-28) includes outputSchema for all three tools.
    {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let mut s = make_server(true, "{}", sent);
        let list = parse(s.handle(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#,
        ));
        let tools = list["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 3);
        for t in tools {
            assert_eq!(t["outputSchema"]["type"].as_str(), Some("object"));
        }
    }

    // 2024-11-05 and 2025-03-26 omit outputSchema.
    for ver in ["2024-11-05", "2025-03-26"] {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let mut s = make_server(true, "{}", sent);
        let _ = parse(s.handle(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{ver}","capabilities":{{}},"clientInfo":{{"name":"t","version":"1"}}}}}}"#
        )));
        let list = parse(s.handle(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#));
        let tools = list["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 3);
        for t in tools {
            assert!(
                t.get("outputSchema").is_none(),
                "unexpected outputSchema for {} under {ver}",
                t["name"]
            );
        }
    }
}

#[test]
fn search_history_paging_and_structured_content() {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut s = make_search_pipe_server(5, Rc::clone(&sent));

    // Page 1: limit 2 -> forwards limit 2 (no offset), returns [s_000, s_001] + nextCursor.
    let p1 = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_history","arguments":{"q":"invoice","limit":2}}}"#,
    ));
    assert_eq!(p1["result"]["isError"].as_bool(), Some(false));
    let p1_text: Value =
        serde_json::from_str(p1["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(p1["result"]["structuredContent"], p1_text);
    let ids1: Vec<&str> = p1_text["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids1, vec!["s_000", "s_001"]);
    assert_eq!(p1_text["count"].as_u64(), Some(2));
    assert_eq!(p1_text["hasMore"].as_bool(), Some(true));
    let c1 = p1_text["nextCursor"]
        .as_str()
        .expect("nextCursor on page 1")
        .to_string();

    // Pipe request never contains cursor, forwards client limit 2, and sets anchor `to`.
    let req1_json: Value =
        serde_json::from_str(sent.borrow()[0].strip_prefix("search ").unwrap()).unwrap();
    assert!(req1_json.get("cursor").is_none());
    assert!(req1_json.get("offset").is_none());
    assert_eq!(req1_json["limit"].as_u64(), Some(2));
    let anchor_to = req1_json["to"]
        .as_str()
        .expect("anchor to on page 1")
        .to_string();
    assert!(anchor_to.ends_with('Z'));

    // Page 2: follow cursor -> forwards offset 2, limit 2, and same anchor `to`.
    let p2 = parse(s.handle(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"search_history","arguments":{{"q":"invoice","limit":2,"cursor":"{c1}"}}}}}}"#
    )));
    assert_eq!(p2["result"]["isError"].as_bool(), Some(false));
    let p2_text: Value =
        serde_json::from_str(p2["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(p2["result"]["structuredContent"], p2_text);
    let ids2: Vec<&str> = p2_text["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids2, vec!["s_002", "s_003"]);
    assert_eq!(p2_text["count"].as_u64(), Some(2));
    let c2 = p2_text["nextCursor"]
        .as_str()
        .expect("nextCursor on page 2")
        .to_string();

    let req2_json: Value =
        serde_json::from_str(sent.borrow()[1].strip_prefix("search ").unwrap()).unwrap();
    assert!(req2_json.get("cursor").is_none());
    assert_eq!(req2_json["offset"].as_u64(), Some(2));
    assert_eq!(req2_json["limit"].as_u64(), Some(2));
    assert_eq!(req2_json["to"].as_str(), Some(anchor_to.as_str()));

    // Page 3 (last page): follow cursor -> forwards offset 4, limit 2, returns [s_004], no nextCursor.
    let p3 = parse(s.handle(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"search_history","arguments":{{"q":"invoice","limit":2,"cursor":"{c2}"}}}}}}"#
    )));
    assert_eq!(p3["result"]["isError"].as_bool(), Some(false));
    let p3_text: Value =
        serde_json::from_str(p3["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(p3["result"]["structuredContent"], p3_text);
    let ids3: Vec<&str> = p3_text["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids3, vec!["s_004"]);
    assert_eq!(p3_text["count"].as_u64(), Some(1));
    assert!(p3_text.get("nextCursor").is_none());

    let req3_json: Value =
        serde_json::from_str(sent.borrow()[2].strip_prefix("search ").unwrap()).unwrap();
    assert!(req3_json.get("cursor").is_none());
    assert_eq!(req3_json["offset"].as_u64(), Some(4));
    assert_eq!(req3_json["limit"].as_u64(), Some(2));
    assert_eq!(req3_json["to"].as_str(), Some(anchor_to.as_str()));
}

#[test]
fn search_history_omits_limit_by_default_and_reuses_anchor_to() {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut s = make_search_pipe_server(25, Rc::clone(&sent));

    // No limit supplied -> pipe request must omit `limit` so agent's ipc.defaultResults applies.
    let p1 = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_history","arguments":{"q":"invoice"}}}"#,
    ));
    assert_eq!(p1["result"]["isError"].as_bool(), Some(false));
    let req1: Value =
        serde_json::from_str(sent.borrow()[0].strip_prefix("search ").unwrap()).unwrap();
    assert!(req1.get("limit").is_none(), "limit must not be defaulted: {req1}");
    let anchor = req1["to"].as_str().expect("anchor to").to_string();

    let c1 = p1["result"]["structuredContent"]["nextCursor"]
        .as_str()
        .expect("nextCursor when hasMore is true")
        .to_string();

    let p2 = parse(s.handle(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"search_history","arguments":{{"q":"invoice","cursor":"{c1}"}}}}}}"#
    )));
    assert_eq!(p2["result"]["isError"].as_bool(), Some(false));
    let req2: Value =
        serde_json::from_str(sent.borrow()[1].strip_prefix("search ").unwrap()).unwrap();
    assert!(req2.get("limit").is_none());
    assert_eq!(req2["offset"].as_u64(), Some(20));
    assert_eq!(req2["to"].as_str(), Some(anchor.as_str()));
    assert!(
        p2["result"]["structuredContent"].get("nextCursor").is_none(),
        "no nextCursor when hasMore is false"
    );
}

#[test]
fn search_history_degraded_change_between_pages_returns_error() {
    let call_num = Rc::new(RefCell::new(0usize));
    let call_num_c = Rc::clone(&call_num);
    let mut s = McpServer::new(move |_line: &str| {
        let mut n = call_num_c.borrow_mut();
        *n += 1;
        let body = if *n == 1 {
            serde_json::json!({
                "count": 2,
                "truncated": false,
                "tookMs": 5,
                "results": [{"id": "s_1"}, {"id": "s_2"}],
                "degraded": "vector",
                "hasMore": true
            })
        } else {
            serde_json::json!({
                "count": 2,
                "truncated": false,
                "tookMs": 5,
                "results": [{"id": "s_3"}, {"id": "s_4"}],
                "hasMore": false
            })
        };
        PipeAnswer::ok(serde_json::to_string(&body).unwrap())
    });

    let p1 = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_history","arguments":{"q":"invoice","limit":2}}}"#,
    ));
    assert_eq!(p1["result"]["isError"].as_bool(), Some(false));
    let cursor = p1["result"]["structuredContent"]["nextCursor"]
        .as_str()
        .expect("nextCursor")
        .to_string();

    let p2 = parse(s.handle(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"search_history","arguments":{{"q":"invoice","limit":2,"cursor":"{cursor}"}}}}}}"#
    )));
    assert_eq!(p2["result"]["isError"].as_bool(), Some(true));
    assert_eq!(
        p2["result"]["content"][0]["text"].as_str(),
        Some("Search results changed; search again without cursor.")
    );
    assert!(p2["result"].get("structuredContent").is_none());
}

#[test]
fn invalid_and_mismatched_cursors_return_tool_error() {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut s = make_search_pipe_server(5, Rc::clone(&sent));

    let p1 = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_history","arguments":{"q":"invoice","limit":2}}}"#,
    ));
    let cursor = p1["result"]["structuredContent"]["nextCursor"]
        .as_str()
        .expect("nextCursor")
        .to_string();

    // Reusing cursor with a different query q -> Invalid cursor.
    let changed_q = parse(s.handle(&format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"search_history","arguments":{{"q":"other","limit":2,"cursor":"{cursor}"}}}}}}"#
    )));
    assert_eq!(changed_q["result"]["isError"].as_bool(), Some(true));
    assert_eq!(
        changed_q["result"]["content"][0]["text"].as_str(),
        Some("Invalid cursor.")
    );
    assert!(changed_q["result"].get("structuredContent").is_none());

    // Garbage cursor -> Invalid cursor.
    let garbage = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_history","arguments":{"q":"invoice","cursor":"not-a-valid-cursor!"}}}"#,
    ));
    assert_eq!(garbage["result"]["isError"].as_bool(), Some(true));
    assert_eq!(
        garbage["result"]["content"][0]["text"].as_str(),
        Some("Invalid cursor.")
    );
    assert!(garbage["result"].get("structuredContent").is_none());

    // Only the first valid call hit the pipe.
    assert_eq!(sent.borrow().len(), 1);
}

#[test]
fn agent_error_has_no_structured_content() {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut s = make_server(false, "Store is locked.", sent);
    let r = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall_status","arguments":{}}}"#,
    ));
    assert_eq!(r["result"]["isError"].as_bool(), Some(true));
    assert!(r["result"].get("structuredContent").is_none());
}

#[test]
fn legacy_2024_11_05_call_omits_structured_content() {
    let sent = Rc::new(RefCell::new(Vec::new()));
    let mut s = make_search_pipe_server(3, sent);
    let _ = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
    ));
    let r = parse(s.handle(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_history","arguments":{"q":"invoice","limit":2}}}"#,
    ));
    assert_eq!(r["result"]["isError"].as_bool(), Some(false));
    assert!(r["result"].get("structuredContent").is_none());
    let text_json: Value =
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text_json["count"].as_u64(), Some(2));
}
