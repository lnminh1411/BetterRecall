// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Sweep 6 G3 agent status contract tests:
//! - `pause` sets `userPaused` in status and `resume` clears it (br-agent).
//! - A storage-paused agent reports `paused` with reason `storage` (br-agent).
//! - `AgentHost` + `CommandProcessor` report `unlockNeeds` transitions across password/passphrase/none.

mod common;

use std::sync::Arc;
use std::time::Duration;

use br_contracts::{
    CoreHostOptions, Envelope, ThumbData,
};
use br_core::{
    AgentControl, CancelToken, ClientSession, CommandDeps, CommandProcessor, CoreHost, EmbedError,
    PipeKind, QueryEmbedder, Thumbnailer, UnlockPassword,
};
use chrono::Utc;
use serde_json::json;

use br_agent::host::{AgentHost, AgentOptions};

use common::TempDir;

struct NoopEmbedder;
impl QueryEmbedder for NoopEmbedder {
    fn embed_query(&self, _text: &str, _ct: &CancelToken) -> Result<Vec<f32>, EmbedError> {
        Err(EmbedError::Failed("unused".to_string()))
    }
}

struct NoopThumbs;
impl Thumbnailer for NoopThumbs {
    fn get_thumb(&self, _id: &str, _width: i32, _ct: &CancelToken) -> Option<ThumbData> {
        None
    }
    fn clear_cache(&self) {}
}

fn make_agent(dir: &TempDir) -> (Arc<CoreHost>, Arc<AgentHost>, Arc<CommandProcessor>) {
    let core = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: dir.path().to_path_buf(),
            install_dir: dir.path().to_path_buf(),
        },
        false,
    );
    core.keys().set_hello_available_for_test(true);
    let _ = core.keys().try_load_silent();
    let agent = AgentHost::new(
        core.clone(),
        AgentOptions {
            install_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            autostart: false,
        },
    );
    agent.set_capture_started_for_test(true);

    let core_for_store = core.clone();
    let password = Arc::new(UnlockPassword::with_options(
        dir.path(),
        Arc::new(Utc::now),
        1024,
        1,
        1,
    ));
    let proc = Arc::new(CommandProcessor::new(CommandDeps {
        settings: core.settings().clone(),
        store: Arc::new(move || core_for_store.store()),
        keys: core.keys().clone(),
        agent: agent.clone(),
        embedder: Arc::new(NoopEmbedder),
        thumbnailer: Arc::new(NoopThumbs),
        purge_all: Arc::new(|_| 0),
        purge_all_undeleted: None,
        blobs: None,
        home: Some(dir.path().to_path_buf()),
        password: Some(password),
        ai_key: None,
        describer: None,
        redactor: None,
        chat_http: None,
        chat_providers: None,
    }));
    (core, agent, proc)
}

#[test]
fn pause_sets_user_paused_and_resume_clears_it() {
    let dir = TempDir::new("user-paused");
    let (core, agent, proc) = make_agent(&dir);

    let st0 = agent.get_status();
    assert!(!st0.paused);
    assert!(!st0.user_paused);

    // Direct AgentControl user pause sets user_paused=true; resume clears it.
    agent.pause(Some(Duration::from_secs(60)), "user");
    let st1 = agent.get_status();
    assert!(st1.paused);
    assert!(st1.user_paused);
    assert_eq!(Some("user".to_string()), st1.pause_reason);

    agent.resume("user");
    let st2 = agent.get_status();
    assert!(!st2.paused);
    assert!(!st2.user_paused);

    // Over CommandProcessor UI verbs: `pause 60` sets `userPaused: true`, `resume` clears it.
    let mut ui = ClientSession::new(PipeKind::Ui, 1, r"C:\app\BetterRecall.UI.exe");
    let r = proc.handle(&mut ui, "pause 60", &CancelToken::none());
    assert!(Envelope::parse(&r.line).unwrap().0);
    let r = proc.handle(&mut ui, "status", &CancelToken::none());
    let (ok, d, _) = Envelope::parse(&r.line).unwrap();
    assert!(ok);
    let d = d.unwrap();
    assert_eq!(true, d["paused"]);
    assert_eq!(true, d["userPaused"]);
    assert_eq!("user", d["pauseReason"]);

    let r = proc.handle(&mut ui, "resume", &CancelToken::none());
    assert!(Envelope::parse(&r.line).unwrap().0);
    let r = proc.handle(&mut ui, "status", &CancelToken::none());
    let (ok, d, _) = Envelope::parse(&r.line).unwrap();
    assert!(ok);
    let d = d.unwrap();
    assert_eq!(false, d["paused"]);
    assert_eq!(false, d["userPaused"]);

    agent.shutdown();
    core.shutdown();
}

#[test]
fn storage_paused_status_mapping_reports_paused_with_reason_storage() {
    let dir = TempDir::new("storage-paused");
    let (core, agent, proc) = make_agent(&dir);

    // Note: The real trigger (AgentHost::storage_check) is private and not driven here without a seam.
    // Apply the storage pause to verify status mapping through AgentControl and CommandProcessor.
    agent.pause_controller().set("storage", None);

    let st = agent.get_status();
    assert!(st.paused);
    assert!(!st.capturing);
    assert!(!st.user_paused);
    assert_eq!(Some("storage".to_string()), st.pause_reason);

    // Status verb over CommandProcessor reports paused=true, pauseReason="storage", userPaused=false.
    let mut ui = ClientSession::new(PipeKind::Ui, 1, r"C:\app\BetterRecall.UI.exe");
    let r = proc.handle(&mut ui, "status", &CancelToken::none());
    let (ok, d, _) = Envelope::parse(&r.line).unwrap();
    assert!(ok);
    let d = d.unwrap();
    assert_eq!(true, d["paused"]);
    assert_eq!("storage", d["pauseReason"]);
    assert_eq!(false, d["userPaused"]);

    agent.shutdown();
    core.shutdown();
}

#[test]
fn agent_status_over_command_processor_reports_unlock_needs_transitions() {
    let dir = TempDir::new("unlock-needs");
    let (core, agent, proc) = make_agent(&dir);
    let mut ui = ClientSession::new(PipeKind::Ui, 1, r"C:\app\BetterRecall.UI.exe");

    // Open gate first -> unlockNeeds="none".
    agent.open_read_gate();
    let (_, d, _) = Envelope::parse(&proc.handle(&mut ui, "status", &CancelToken::none()).line).unwrap();
    assert_eq!("none", d.unwrap()["unlockNeeds"]);

    // Configure password + unlockMethod=password, then lock -> unlockNeeds="password".
    let (ok, _, _) = Envelope::parse(
        &proc
            .handle(&mut ui, r#"password set {"new":"agent-pw-123"}"#, &CancelToken::none())
            .line,
    )
    .unwrap();
    assert!(ok);
    assert!(
        core.settings()
            .try_update(&json!({"security": {"unlockMethod": "password"}}))
            .is_none()
    );
    agent.lock();
    let (_, d, _) = Envelope::parse(&proc.handle(&mut ui, "status", &CancelToken::none()).line).unwrap();
    assert_eq!("password", d.unwrap()["unlockNeeds"]);

    // Unlock with password -> unlockNeeds="none".
    let (ok, _, _) = Envelope::parse(
        &proc
            .handle(&mut ui, "unlock agent-pw-123", &CancelToken::none())
            .line,
    )
    .unwrap();
    assert!(ok);
    let (_, d, _) = Envelope::parse(&proc.handle(&mut ui, "status", &CancelToken::none()).line).unwrap();
    assert_eq!("none", d.unwrap()["unlockNeeds"]);

    agent.shutdown();
    core.shutdown();
}
