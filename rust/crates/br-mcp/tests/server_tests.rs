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
    assert_eq!(sent.borrow().last().map(String::as_str), Some(r#"search {"q":"acme","limit":5}"#));
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
