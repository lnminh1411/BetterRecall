// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::VecDeque;
use std::io;
use std::marker::PhantomData;

use br_cli::{CliConnection, CliEnv, ERR_NOT_TRUSTED, USAGE, run_cli};
use br_contracts::LineError;
use br_core::{AgentPresence, CliPurge, PipeConnectResult};

struct FakeConn {
    sent: Vec<String>,
    replies: VecDeque<Result<Option<String>, LineError>>,
    stream_lines: VecDeque<Result<Option<String>, LineError>>,
}

impl FakeConn {
    fn with_replies(replies: Vec<&str>) -> Self {
        Self {
            sent: Vec::new(),
            replies: replies.into_iter().map(|s| Ok(Some(s.to_string()))).collect(),
            stream_lines: VecDeque::new(),
        }
    }
}

impl CliConnection for FakeConn {
    fn request(&mut self, line: &str) -> Result<Option<String>, LineError> {
        self.sent.push(line.to_string());
        self.replies.pop_front().unwrap_or(Ok(None))
    }

    fn read_line(&mut self) -> Result<Option<String>, LineError> {
        self.stream_lines.pop_front().unwrap_or(Ok(None))
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_string()).collect()
}

#[test]
fn usage_on_missing_or_wrong_verb() {
    for args in [s(&[]), s(&["ipc"]), s(&["status", "now"])] {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_cli(
            &args,
            CliEnv {
                connect: || -> (PipeConnectResult, Option<FakeConn>) {
                    panic!("should not connect on bad usage")
                },
                not_answering: || (AgentPresence::ERR_NOT_RUNNING, 2),
                interactive: false,
                read_answer: || None,
                stdout: &mut out,
                stderr: &mut err,
                _marker: PhantomData,
            },
        );
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert_eq!(String::from_utf8(err).unwrap().trim(), USAGE);
    }
}

#[test]
fn not_running_and_ipc_off_exit_codes() {
    for (msg, expected_exit) in [
        (AgentPresence::ERR_NOT_RUNNING, 2),
        (AgentPresence::ERR_IPC_OFF, 3),
    ] {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_cli(
            &s(&["ipc", "ping"]),
            CliEnv {
                connect: || -> (PipeConnectResult, Option<FakeConn>) {
                    (PipeConnectResult::NotRunning, None)
                },
                not_answering: || (msg, expected_exit),
                interactive: false,
                read_answer: || None,
                stdout: &mut out,
                stderr: &mut err,
                _marker: PhantomData,
            },
        );
        assert_eq!(code, expected_exit);
        assert_eq!(String::from_utf8(out).unwrap().trim(), msg);
        assert!(err.is_empty());
    }
}

#[test]
fn not_trusted_prints_error_and_exits_2() {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_cli(
        &s(&["ipc", "ping"]),
        CliEnv {
            connect: || -> (PipeConnectResult, Option<FakeConn>) {
                (PipeConnectResult::NotTrusted, None)
            },
            not_answering: || (AgentPresence::ERR_NOT_RUNNING, 2),
            interactive: false,
            read_answer: || None,
            stdout: &mut out,
            stderr: &mut err,
            _marker: PhantomData,
        },
    );
    assert_eq!(code, 2);
    assert_eq!(String::from_utf8(out).unwrap().trim(), ERR_NOT_TRUSTED);
}

#[test]
fn ok_and_error_envelopes_map_to_exit_0_and_1() {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_cli(
        &s(&["IPC", "ping"]),
        CliEnv {
            connect: || {
                (
                    PipeConnectResult::Ok,
                    Some(FakeConn::with_replies(vec![r#"{"ok":true,"data":{"pong":true}}"#])),
                )
            },
            not_answering: || (AgentPresence::ERR_NOT_RUNNING, 2),
            interactive: false,
            read_answer: || None,
            stdout: &mut out,
            stderr: &mut err,
            _marker: PhantomData,
        },
    );
    assert_eq!(code, 0);
    assert_eq!(
        String::from_utf8(out).unwrap().trim(),
        r#"{"ok":true,"data":{"pong":true}}"#
    );
    assert!(err.is_empty());

    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_cli(
        &s(&["ipc", "get", "missing"]),
        CliEnv {
            connect: || {
                (
                    PipeConnectResult::Ok,
                    Some(FakeConn::with_replies(vec![r#"{"ok":false,"error":"Not found."}"#])),
                )
            },
            not_answering: || (AgentPresence::ERR_NOT_RUNNING, 2),
            interactive: false,
            read_answer: || None,
            stdout: &mut out,
            stderr: &mut err,
            _marker: PhantomData,
        },
    );
    assert_eq!(code, 1);
    assert_eq!(
        String::from_utf8(out).unwrap().trim(),
        r#"{"ok":false,"error":"Not found."}"#
    );
    assert!(err.is_empty());
}

#[test]
fn failed_purge_confirm_prints_hint_to_stderr() {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_cli(
        &s(&["ipc", "purge", "confirm", "badtoken"]),
        CliEnv {
            connect: || {
                (
                    PipeConnectResult::Ok,
                    Some(FakeConn::with_replies(vec![r#"{"ok":false,"error":"Invalid token."}"#])),
                )
            },
            not_answering: || (AgentPresence::ERR_NOT_RUNNING, 2),
            interactive: false,
            read_answer: || None,
            stdout: &mut out,
            stderr: &mut err,
            _marker: PhantomData,
        },
    );
    assert_eq!(code, 1);
    assert_eq!(String::from_utf8(err).unwrap().trim(), CliPurge::HINT_CONFIRM);
}

#[test]
fn io_error_or_invalid_json_returns_1() {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_cli(
        &s(&["ipc", "ping"]),
        CliEnv {
            connect: || {
                let mut conn = FakeConn::with_replies(vec![]);
                conn.replies
                    .push_back(Err(LineError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))));
                (PipeConnectResult::Ok, Some(conn))
            },
            not_answering: || (AgentPresence::ERR_NOT_RUNNING, 2),
            interactive: false,
            read_answer: || None,
            stdout: &mut out,
            stderr: &mut err,
            _marker: PhantomData,
        },
    );
    assert_eq!(code, 1);

    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_cli(
        &s(&["ipc", "ping"]),
        CliEnv {
            connect: || (PipeConnectResult::Ok, Some(FakeConn::with_replies(vec!["not-json"]))),
            not_answering: || (AgentPresence::ERR_NOT_RUNNING, 2),
            interactive: false,
            read_answer: || None,
            stdout: &mut out,
            stderr: &mut err,
            _marker: PhantomData,
        },
    );
    assert_eq!(code, 1);
    assert_eq!(String::from_utf8(out).unwrap().trim(), "not-json");
}

#[test]
fn subscribe_streams_until_eof() {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_cli(
        &s(&["ipc", "subscribe", "captures"]),
        CliEnv {
            connect: || {
                let mut conn = FakeConn::with_replies(vec![r#"{"ok":true,"data":{"subscribed":"captures"}}"#]);
                conn.stream_lines
                    .push_back(Ok(Some(r#"{"event":"capture","id":"s_1"}"#.to_string())));
                conn.stream_lines
                    .push_back(Ok(Some(r#"{"event":"capture","id":"s_2"}"#.to_string())));
                conn.stream_lines.push_back(Ok(None));
                (PipeConnectResult::Ok, Some(conn))
            },
            not_answering: || (AgentPresence::ERR_NOT_RUNNING, 2),
            interactive: false,
            read_answer: || None,
            stdout: &mut out,
            stderr: &mut err,
            _marker: PhantomData,
        },
    );
    assert_eq!(code, 0);
    let lines: Vec<String> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[1].contains("s_1"));
    assert!(lines[2].contains("s_2"));
}

#[test]
fn purge_with_yes_and_interactive_prompt_flow() {
    // --yes confirms on the same connection
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_cli(
        &s(&["ipc", "purge", "app", "chrome.exe", "--yes"]),
        CliEnv {
            connect: || {
                (
                    PipeConnectResult::Ok,
                    Some(FakeConn::with_replies(vec![
                        r#"{"ok":true,"data":{"confirm":"tok123","rows":2}}"#,
                        r#"{"ok":true,"data":{"purged":2}}"#,
                    ])),
                )
            },
            not_answering: || (AgentPresence::ERR_NOT_RUNNING, 2),
            interactive: false,
            read_answer: || None,
            stdout: &mut out,
            stderr: &mut err,
            _marker: PhantomData,
        },
    );
    assert_eq!(code, 0);
    assert!(String::from_utf8(out).unwrap().contains(r#""purged":2"#));

    // interactive "no" cancels
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_cli(
        &s(&["ipc", "purge", "app", "chrome.exe"]),
        CliEnv {
            connect: || {
                (
                    PipeConnectResult::Ok,
                    Some(FakeConn::with_replies(vec![
                        r#"{"ok":true,"data":{"confirm":"tok123","rows":1}}"#,
                    ])),
                )
            },
            not_answering: || (AgentPresence::ERR_NOT_RUNNING, 2),
            interactive: true,
            read_answer: || Some("no\n".to_string()),
            stdout: &mut out,
            stderr: &mut err,
            _marker: PhantomData,
        },
    );
    assert_eq!(code, 1);
    let err_text = String::from_utf8(err).unwrap();
    assert!(err_text.contains("Delete 1 capture?"));
    assert!(err_text.contains("Cancelled."));

    // non-interactive without --yes prints HintYes
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_cli(
        &s(&["ipc", "purge", "app", "chrome.exe"]),
        CliEnv {
            connect: || {
                (
                    PipeConnectResult::Ok,
                    Some(FakeConn::with_replies(vec![
                        r#"{"ok":true,"data":{"confirm":"tok123","rows":1}}"#,
                    ])),
                )
            },
            not_answering: || (AgentPresence::ERR_NOT_RUNNING, 2),
            interactive: false,
            read_answer: || Some("yes".to_string()),
            stdout: &mut out,
            stderr: &mut err,
            _marker: PhantomData,
        },
    );
    assert_eq!(code, 1);
    assert!(String::from_utf8(err).unwrap().contains(CliPurge::HINT_YES));
}
