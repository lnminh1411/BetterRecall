// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! CLI purge confirmation and agent presence checks (`CliPurge`, `AgentPresence`).

mod common;

use br_contracts::PipeNames;
use br_core::{AgentPresence, CliPurge, CliPurgeOptions};

use common::TempHome;

/// Regression B10.
#[test]
fn cli_purge_applies_to_previews_and_takes_yes() {
    assert!(CliPurge::applies(&["purge", "last", "60"]));
    assert!(!CliPurge::applies(&["purge", "confirm", "abc"]));
    assert!(!CliPurge::applies(&["purge"]));
    let mut words: Vec<String> = vec!["purge".into(), "last".into(), "60".into(), "--yes".into()];
    assert!(CliPurge::take_yes(&mut words));
    assert_eq!(vec!["purge", "last", "60"], words);
}

/// Regression B10.
#[test]
fn cli_purge_with_yes_confirms_on_the_same_connection() {
    let mut sent = Vec::new();
    let mut out = Vec::new();
    let mut prompt = Vec::new();
    let code = CliPurge::run(CliPurgeOptions {
        request: |line: &str| {
            sent.push(line.to_string());
            if line.starts_with("purge confirm ") {
                Some("{\"ok\":true,\"data\":{\"purged\":2}}".to_string())
            } else {
                Some("{\"ok\":true,\"data\":{\"confirm\":\"tok1\",\"rows\":2}}".to_string())
            }
        },
        preview_line: "purge last 60",
        yes: true,
        interactive: false,
        read_answer: || None,
        output: &mut out,
        prompt: &mut prompt,
    });
    assert_eq!(0, code);
    assert_eq!(vec!["purge last 60", "purge confirm tok1"], sent);
}

/// Regression B10.
#[test]
fn cli_purge_prompt_yes_deletes_and_no_keeps() {
    let mut sent_yes = Vec::new();
    let mut out = Vec::new();
    let mut prompt = Vec::new();
    let code_yes = CliPurge::run(CliPurgeOptions {
        request: |line: &str| {
            sent_yes.push(line.to_string());
            if line.starts_with("purge confirm ") {
                Some("{\"ok\":true,\"data\":{\"purged\":1}}".to_string())
            } else {
                Some("{\"ok\":true,\"data\":{\"confirm\":\"tok2\",\"rows\":1}}".to_string())
            }
        },
        preview_line: "purge last 60",
        yes: false,
        interactive: true,
        read_answer: || Some("yes".to_string()),
        output: &mut out,
        prompt: &mut prompt,
    });
    assert_eq!(0, code_yes);
    assert_eq!(vec!["purge last 60", "purge confirm tok2"], sent_yes);

    let mut confirmed = false;
    let mut out2 = Vec::new();
    let mut prompt2 = Vec::new();
    let code_no = CliPurge::run(CliPurgeOptions {
        request: |line: &str| {
            if line.starts_with("purge confirm ") {
                confirmed = true;
            }
            Some("{\"ok\":true,\"data\":{\"confirm\":\"tok3\",\"rows\":1}}".to_string())
        },
        preview_line: "purge last 60",
        yes: false,
        interactive: true,
        read_answer: || Some("no".to_string()),
        output: &mut out2,
        prompt: &mut prompt2,
    });
    assert_eq!(1, code_no);
    assert!(!confirmed);
}

#[test]
fn cli_says_ipc_is_off_when_the_agent_mutex_is_held() {
    let home = TempHome::new(Some("dpapi"));
    assert_eq!(
        (AgentPresence::ERR_NOT_RUNNING, 2),
        AgentPresence::not_answering(home.path(), false)
    );
    let mname = PipeNames::agent_mutex_for(home.path());
    let wide: Vec<u16> = mname.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe {
        windows::Win32::System::Threading::CreateMutexW(
            None,
            true,
            windows::core::PCWSTR(wide.as_ptr()),
        )
    }
    .expect("create test mutex");
    assert!(AgentPresence::running(home.path()));
    assert_eq!(
        (AgentPresence::ERR_IPC_OFF, 3),
        AgentPresence::not_answering(home.path(), false)
    );
    assert_eq!(
        (AgentPresence::ERR_NOT_RUNNING, 2),
        AgentPresence::not_answering(home.path(), true)
    );
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
}
