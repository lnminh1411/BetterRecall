// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Ported `PipeTests.cs` + pipe/trust/throttle/StepClock/AgentPresence/CliPurge tests from
//! BatchHCoreTests, Sweep2Tests, Sweep3KTests, Sweep3LCoreTests, Sweep4LTests, Sweep5LTests.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{CaptureKind, CaptureMeta, Envelope, LineReader, PipeNames, Settings};
use br_core::{
    AgentPresence, AuditThrottle, CancelToken, CliPurge, CliPurgeOptions, CommandDeps,
    CommandProcessor, KeyVault, PeerInfo, PipeClient, PipeConnectResult, PipeKind, PipeManager,
    PipeServer, ProcessInfo, SettingsStore, StepClock,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};

use common::a3::{FakeAgent, FakeEmbedder, FakeThumbs, test_pipe_name};
use common::{TempHome, TempStore};

struct PipeFixture {
    _store_tmp: TempStore,
    store: Arc<br_core::EncryptedStore>,
    _home: TempHome,
    tag: String,
    settings: Arc<SettingsStore>,
    _vault: Arc<KeyVault>,
    agent: Arc<FakeAgent>,
    pipes: PipeManager,
}

impl PipeFixture {
    fn new() -> Self {
        let mut store_tmp = TempStore::new();
        let store = Arc::new(store_tmp.store.take().expect("store"));
        let home = TempHome::new(Some("dpapi"));
        let tag = test_pipe_name("pt");
        let settings = Arc::new(SettingsStore::load(home.path()));
        assert!(
            settings
                .try_update(&serde_json::json!({"ipc": {"enabled": true}}))
                .is_none()
        );
        let s_for_vault = settings.clone();
        let vault = Arc::new(KeyVault::new(
            home.path(),
            move || s_for_vault.current(),
            None,
            false,
        ));
        let agent = Arc::new(FakeAgent::new());
        let st_for_proc = store.clone();
        let processor = Arc::new(CommandProcessor::new(CommandDeps {
            settings: settings.clone(),
            store: Arc::new(move || Some(st_for_proc.clone())),
            keys: vault.clone(),
            agent: agent.clone(),
            embedder: Arc::new(FakeEmbedder::new()),
            thumbnailer: Arc::new(FakeThumbs::new()),
            purge_all: Arc::new(|_| 0),
            purge_all_undeleted: None,
            blobs: None,
            home: None,
            password: None,
            ai_key: None,
            describer: None,
            redactor: None,
            chat_http: None,
            chat_providers: None,
        }));
        let tag_for_names = tag.clone();
        let st_for_pm = store.clone();
        let pipes = PipeManager::new(
            settings.clone(),
            processor,
            Path::new(r"C:\NotTheInstallDir"),
            Arc::new(move || Some(st_for_pm.clone())),
            Some(Arc::new(move |k, s| match k {
                PipeKind::Ui => format!("{tag_for_names}.ui"),
                PipeKind::Mcp => format!("{tag_for_names}.mcp"),
                PipeKind::Public => format!("{tag_for_names}.{}", s.ipc.pipe_name),
            })),
        );
        pipes.start();
        Self {
            _store_tmp: store_tmp,
            store,
            _home: home,
            tag,
            settings,
            _vault: vault,
            agent,
            pipes,
        }
    }

    fn name(&self, k: PipeKind) -> String {
        match k {
            PipeKind::Ui => format!("{}.ui", self.tag),
            PipeKind::Mcp => format!("{}.mcp", self.tag),
            PipeKind::Public => format!("{}.{}", self.tag, self.settings.current().ipc.pipe_name),
        }
    }

    fn set(&self, json: serde_json::Value) {
        assert!(self.settings.try_update(&json).is_none());
    }

    fn connect(&self, k: PipeKind) -> PipeClient {
        let self_exe = ProcessInfo::current_image().expect("current image");
        let (r, c) = PipeClient::connect(&self.name(k), &self_exe, 2000, &CancelToken::none());
        assert_eq!(PipeConnectResult::Ok, r);
        c.expect("client")
    }
}

impl Drop for PipeFixture {
    fn drop(&mut self) {
        self.pipes.stop();
    }
}

fn wait_for(cond: impl Fn() -> bool) {
    for _ in 0..100 {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(cond());
}

#[test]
fn public_pipe_round_trip_with_client_trust_check() {
    let f = PipeFixture::new();
    let mut c = f.connect(PipeKind::Public);
    assert_eq!(ProcessInfo::current_pid(), c.server_pid());
    let line = c.request("ping", &CancelToken::none()).unwrap().unwrap();
    let (ok, d, _) = Envelope::parse(&line).unwrap();
    assert!(ok);
    assert!(d.unwrap()["pong"].as_bool().unwrap());

    let line2 = c.request("VERSION", &CancelToken::none()).unwrap().unwrap();
    assert!(Envelope::parse(&line2).unwrap().0);
    let audit = f.store.read_audit(5).unwrap();
    assert!(audit.iter().any(|a| {
        a.verb == "connect" && a.pipe == "ipc" && a.client_pid == ProcessInfo::current_pid()
    }));
}

#[test]
fn client_rejects_wrong_server_image_and_reports_not_running() {
    let f = PipeFixture::new();
    let self_exe = ProcessInfo::current_image().unwrap();
    let (r, c) = PipeClient::connect(
        &f.name(PipeKind::Public),
        r"C:\Program Files\BetterRecall\BetterRecall.Agent.exe",
        2000,
        &CancelToken::none(),
    );
    assert_eq!(PipeConnectResult::NotTrusted, r);
    assert!(c.is_none());

    let (r, c) = PipeClient::connect(
        &format!("{}.nothing", f.tag),
        &self_exe,
        300,
        &CancelToken::none(),
    );
    assert_eq!(PipeConnectResult::NotRunning, r);
    assert!(c.is_none());
}

#[test]
fn ui_pipe_rejects_other_images() {
    let f = PipeFixture::new();
    let mut c = f.connect(PipeKind::Ui);
    let line = c.read_line(&CancelToken::none()).unwrap().unwrap();
    assert_eq!("{\"ok\":false,\"error\":\"Client not allowed.\"}", line);
    assert!(c.read_line(&CancelToken::none()).unwrap().is_none());
    thread::sleep(Duration::from_millis(100));
    assert!(
        f.store
            .read_audit(10)
            .unwrap()
            .iter()
            .any(|a| a.verb == "rejected" && a.pipe == "ui")
    );
}

#[test]
fn allow_clients_narrows_the_public_pipe() {
    let f = PipeFixture::new();
    let self_exe = ProcessInfo::current_image().unwrap();
    f.set(serde_json::json!({"ipc": {"allowClients": [r"C:\Tools\other.exe"]}}));
    {
        let mut c = f.connect(PipeKind::Public);
        let line = c.read_line(&CancelToken::none()).unwrap().unwrap();
        assert_eq!(
            Some("Client not allowed.".to_string()),
            Envelope::parse(&line).unwrap().2
        );
    }
    f.set(serde_json::json!({"ipc": {"allowClients": [self_exe.to_uppercase()]}}));
    {
        let mut c = f.connect(PipeKind::Public);
        let line = c.request("ping", &CancelToken::none()).unwrap().unwrap();
        assert!(Envelope::parse(&line).unwrap().0);
    }
}

#[test]
fn security_descriptor_has_deny_network_user_only_and_medium_label() {
    let probe = PipeServer::new(
        &test_pipe_name("sddl"),
        PipeKind::Public,
        |_| true,
        |_, _| {},
        None,
    );
    let inst = probe.create_instance(true).expect("create instance");
    let sddl = unsafe { PipeServer::read_sddl(inst.raw_handle()) }.expect("read_sddl");
    let sid = PipeNames::user_sid().unwrap();
    assert!(sddl.contains(&format!("O:{sid}")), "sddl={sddl}");
    assert!(sddl.contains("(D;;FA;;;NU)"), "sddl={sddl}");
    assert!(sddl.contains("D:P"), "sddl={sddl}");
    assert!(sddl.contains("(ML;;NWNR;;;ME)"), "sddl={sddl}");
    assert!(!sddl.contains(";;;SY)"), "sddl={sddl}");
    assert!(!sddl.contains(";;;BA)"), "sddl={sddl}");
}

#[test]
fn first_pipe_instance_flag_refuses_a_second_server() {
    let f = PipeFixture::new();
    let other = PipeServer::new(
        &f.name(PipeKind::Public),
        PipeKind::Public,
        |_| true,
        |_, _| {},
        None,
    );
    other.start().expect_err("second server must fail");
}

#[test]
fn settings_toggle_pipes_live() {
    let f = PipeFixture::new();
    let self_exe = ProcessInfo::current_image().unwrap();
    assert!(f.pipes.mcp().is_none());
    f.set(serde_json::json!({"mcp": {"enabled": true}}));
    wait_for(|| f.pipes.mcp().is_some());
    {
        let mut c = f.connect(PipeKind::Mcp);
        let line = c.read_line(&CancelToken::none()).unwrap().unwrap();
        assert_eq!(
            Some("Client not allowed.".to_string()),
            Envelope::parse(&line).unwrap().2
        );
    }
    let old_name = f.name(PipeKind::Public);
    f.set(serde_json::json!({"ipc": {"pipeName": "Renamed"}}));
    wait_for(|| f.pipes.public().is_some_and(|p| p.name().ends_with("Renamed")));
    assert_eq!(
        PipeConnectResult::NotRunning,
        PipeClient::connect(&old_name, &self_exe, 300, &CancelToken::none()).0
    );
    {
        let mut c = f.connect(PipeKind::Public);
        let line = c.request("ping", &CancelToken::none()).unwrap().unwrap();
        assert!(Envelope::parse(&line).unwrap().0);
    }
    f.set(serde_json::json!({"ipc": {"enabled": false}, "mcp": {"enabled": false}}));
    wait_for(|| f.pipes.public().is_none() && f.pipes.mcp().is_none());
    assert_eq!(
        PipeConnectResult::NotRunning,
        PipeClient::connect(&f.name(PipeKind::Public), &self_exe, 300, &CancelToken::none()).0
    );
}

#[test]
fn four_concurrent_clients() {
    let f = PipeFixture::new();
    let mut clients: Vec<PipeClient> = (0..4).map(|_| f.connect(PipeKind::Public)).collect();
    for c in &mut clients {
        let r = c.request("ping", &CancelToken::none()).unwrap().unwrap();
        assert!(Envelope::parse(&r).unwrap().0);
    }
}

#[test]
fn oversized_line_gets_error_and_close() {
    let f = PipeFixture::new();
    let mut c = f.connect(PipeKind::Public);
    let big = format!("search {}", "x".repeat(5000));
    let line = c.request(&big, &CancelToken::none()).unwrap().unwrap();
    assert_eq!("{\"ok\":false,\"error\":\"Line exceeds 4096 bytes.\"}", line);
    assert!(c.read_line(&CancelToken::none()).unwrap().is_none());
}

#[test]
fn bom_and_crlf_on_the_wire() {
    let f = PipeFixture::new();
    let stream = PipeClient::connect_raw(&f.name(PipeKind::Public), 2000).expect("raw connect");
    let raw = [0xEFu8, 0xBB, 0xBF, b'p', b'i', b'n', b'g', b'\r', b'\n'];
    stream.write_all(&raw, None, &CancelToken::none()).unwrap();
    let mut reader = br_core::PipeLineReader::new(stream, 4096);
    let line = reader
        .read_line(1 << 20, Some(Duration::from_secs(5)), &CancelToken::none())
        .unwrap()
        .unwrap();
    assert!(Envelope::parse(&line).unwrap().0);
    let _ = LineReader::new(std::io::Cursor::new(Vec::<u8>::new()));
}

#[test]
fn subscribe_status_and_captures_streams() {
    let f = PipeFixture::new();
    let mut s = f.connect(PipeKind::Public);
    assert_eq!(
        "{\"ok\":true,\"data\":{\"subscribed\":true}}",
        s.request("subscribe", &CancelToken::none()).unwrap().unwrap()
    );
    f.agent.raise_status();
    let line = s
        .read_line_timeout(Duration::from_secs(5), &CancelToken::none())
        .unwrap()
        .unwrap();
    let (ok, d, _) = Envelope::parse(&line).unwrap();
    assert!(ok);
    assert!(d.unwrap()["capturing"].as_bool().unwrap());

    let mut c = f.connect(PipeKind::Public);
    assert_eq!(
        "{\"ok\":true,\"data\":{\"subscribed\":\"captures\"}}",
        c.request("subscribe captures", &CancelToken::none())
            .unwrap()
            .unwrap()
    );
    let ts = DateTime::parse_from_rfc3339("2026-11-10T07:40:12Z")
        .unwrap()
        .with_timezone(&Utc);
    f.agent.raise_capture(&CaptureMeta {
        id: "s_1".to_string(),
        ts,
        kind: CaptureKind::Screen,
        app: "chrome.exe".to_string(),
        title: "SECRET TITLE".to_string(),
        monitor: 0,
        duration_ms: 0,
        has_thumb: true,
        embedded: true,
        site: String::new(),
        replay_id: None,
        replay_offset_ms: None,
    });
    let ev = c
        .read_line_timeout(Duration::from_secs(5), &CancelToken::none())
        .unwrap()
        .unwrap();
    assert_eq!(
        "{\"ok\":true,\"data\":{\"id\":\"s_1\",\"ts\":\"2026-11-10T07:40:12Z\",\"kind\":\"screen\",\"app\":\"chrome.exe\"}}",
        ev
    );
    assert!(!ev.contains("SECRET"));
}

#[test]
fn slow_subscriber_is_disconnected() {
    let f = PipeFixture::new();
    let mut s = f.connect(PipeKind::Public);
    assert!(
        Envelope::parse(&s.request("subscribe", &CancelToken::none()).unwrap().unwrap())
            .unwrap()
            .0
    );
    for _ in 0..5000 {
        f.agent.raise_status();
    }
    let mut lines = 0;
    while let Ok(Some(_)) = s.read_line_timeout(Duration::from_secs(10), &CancelToken::none()) {
        lines += 1;
        if lines >= 5000 {
            break;
        }
    }
    assert!(lines < 5000, "expected server disconnect, read {lines}");
}

#[test]
fn idle_connection_is_closed() {
    let f = PipeFixture::new();
    f.set(serde_json::json!({"ipc": {"idleTimeoutS": 10}}));
    let mut c = f.connect(PipeKind::Public);
    let sw = Instant::now();
    let res = c
        .read_line_timeout(Duration::from_secs(20), &CancelToken::none())
        .unwrap();
    assert!(res.is_none());
    let secs = sw.elapsed().as_secs_f64();
    assert!((9.0..=15.0).contains(&secs), "elapsed={secs}");
}

#[test]
fn b14_server_check_rejects_a_peer_that_started_after_the_connection() {
    let server = PipeServer::new(
        &test_pipe_name("b14"),
        PipeKind::Public,
        |_| true,
        |_, _| {},
        None,
    );
    let now = Utc::now();
    let peer = PeerInfo {
        pid: ProcessInfo::current_pid(),
        session: ProcessInfo::current_session(),
        sid: Some(ProcessInfo::current_sid().to_string()),
        integrity_rid: 0x2000,
        app_container: false,
        image: ProcessInfo::current_image(),
        created_utc: Some(now + ChronoDuration::seconds(5)),
    };
    assert_eq!(Some("pid reuse"), server.check(&peer, Some(now)));
    let older = PeerInfo {
        created_utc: Some(now - ChronoDuration::seconds(5)),
        ..peer
    };
    assert_eq!(None, server.check(&older, Some(now)));
}

#[test]
fn b14_client_image_lookup_refuses_a_process_newer_than_the_connection() {
    let pid = ProcessInfo::current_pid();
    let past = Utc::now() - ChronoDuration::days(365);
    assert!(ProcessInfo::image_path_if_started_before(pid, past).is_none());
    let future = Utc::now() + ChronoDuration::seconds(5);
    assert!(ProcessInfo::image_path_if_started_before(pid, future).is_some());
}

#[test]
fn b15_throttle_merges_repeats_per_key_and_caps_the_total() {
    let t = AuditThrottle::new(ChronoDuration::minutes(1), 3, 512);
    let t0 = DateTime::parse_from_rfc3339("2026-11-10T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(1, t.admit("a", t0));
    for _ in 0..50 {
        assert_eq!(0, t.admit("a", t0 + ChronoDuration::seconds(1)));
    }
    let t1 = t0 + ChronoDuration::minutes(1);
    assert_eq!(51, t.admit("A", t1)); // next row carries the merged repeats (case-insensitive)
    assert_eq!(1, t.admit("b", t1));
    assert_eq!(1, t.admit("c", t1));
    assert_eq!(0, t.admit("d", t1)); // over the per-window total
    assert_eq!(2, t.admit("d", t0 + ChronoDuration::minutes(3)));
}

#[test]
fn b21_revoked_means_a_permission_turned_off() {
    let on = br_contracts::IpcSettings {
        allow_control: true,
        allow_purge: true,
        allow_thumbnails: true,
        require_unlocked: false,
        ..Default::default()
    };
    assert!(!PipeManager::revoked(&on, &on));
    assert!(PipeManager::revoked(
        &on,
        &br_contracts::IpcSettings {
            allow_control: false,
            ..on.clone()
        }
    ));
    assert!(PipeManager::revoked(
        &on,
        &br_contracts::IpcSettings {
            allow_purge: false,
            ..on.clone()
        }
    ));
    assert!(PipeManager::revoked(
        &on,
        &br_contracts::IpcSettings {
            allow_thumbnails: false,
            ..on.clone()
        }
    ));
    assert!(PipeManager::revoked(
        &on,
        &br_contracts::IpcSettings {
            require_unlocked: true,
            ..on.clone()
        }
    ));
}

#[test]
fn b21_removing_a_client_from_allow_clients_drops_its_live_connection() {
    let f = PipeFixture::new();
    let self_exe = ProcessInfo::current_image().unwrap();
    f.set(serde_json::json!({"ipc": {"allowClients": [self_exe]}}));
    let mut c = f.connect(PipeKind::Public);
    assert!(
        Envelope::parse(&c.request("ping", &CancelToken::none()).unwrap().unwrap())
            .unwrap()
            .0
    );
    f.set(serde_json::json!({"ipc": {"allowClients": [r"C:\Tools\other.exe"]}}));
    let line = c
        .read_line_timeout(Duration::from_secs(5), &CancelToken::none())
        .unwrap();
    assert!(line.is_none());
}

#[test]
fn b21_turning_a_permission_off_drops_live_connections() {
    let f = PipeFixture::new();
    f.set(serde_json::json!({"ipc": {"allowControl": true}}));
    let mut c = f.connect(PipeKind::Public);
    assert!(
        Envelope::parse(&c.request("ping", &CancelToken::none()).unwrap().unwrap())
            .unwrap()
            .0
    );
    f.set(serde_json::json!({"ipc": {"allowControl": false}}));
    let line = c
        .read_line_timeout(Duration::from_secs(5), &CancelToken::none())
        .unwrap();
    assert!(line.is_none());
}

#[test]
fn b10_cli_purge_applies_to_previews_and_takes_yes() {
    assert!(CliPurge::applies(&["purge", "last", "60"]));
    assert!(!CliPurge::applies(&["purge", "confirm", "abc"]));
    assert!(!CliPurge::applies(&["purge"]));
    let mut words: Vec<String> = vec!["purge".into(), "last".into(), "60".into(), "--yes".into()];
    assert!(CliPurge::take_yes(&mut words));
    assert_eq!(vec!["purge", "last", "60"], words);
}

#[test]
fn b10_cli_purge_with_yes_confirms_on_the_same_connection() {
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

#[test]
fn b10_cli_purge_prompt_yes_deletes_and_no_keeps() {
    let mut out = Vec::new();
    let mut prompt = Vec::new();
    let code_yes = CliPurge::run(CliPurgeOptions {
        request: |line: &str| {
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
fn b10_cli_purge_without_a_console_or_yes_does_not_delete() {
    let mut out = Vec::new();
    let mut prompt = Vec::new();
    let code = CliPurge::run(CliPurgeOptions {
        request: |_: &str| {
            Some("{\"ok\":true,\"data\":{\"confirm\":\"tok4\",\"rows\":3}}".to_string())
        },
        preview_line: "purge last 60",
        yes: false,
        interactive: false,
        read_answer: || None,
        output: &mut out,
        prompt: &mut prompt,
    });
    assert_eq!(1, code);
    assert!(String::from_utf8_lossy(&prompt).contains(CliPurge::HINT_YES));
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

#[test]
fn pipe_manager_names_follow_home() {
    let home = TempHome::new(Some("dpapi"));
    let s = Settings::default();
    let ui = PipeManager::default_name_for_home(PipeKind::Ui, &s, home.path());
    let pub_name = PipeManager::default_name_for_home(PipeKind::Public, &s, home.path());
    let mcp = PipeManager::default_name_for_home(PipeKind::Mcp, &s, home.path());
    let sid = PipeNames::user_sid().unwrap();
    assert_eq!(PipeNames::ui(&sid, home.path()), ui);
    assert_eq!(PipeNames::public(&s.ipc.pipe_name, &sid, home.path()), pub_name);
    assert_eq!(PipeNames::mcp(&sid, home.path()), mcp);
}

#[test]
fn t2_start_bound_follows_the_boot_clock_only_after_a_step_back() {
    let boot = DateTime::parse_from_rfc3339("2026-11-10T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let wall = boot + ChronoDuration::hours(2);
    // Normal clock: wall == boot + 2h -> bound is wall.
    assert_eq!(
        wall,
        ProcessInfo::start_bound_fresh(wall, 2 * 3600 * 1000, Some(boot))
    );
    // Clock stepped back 1 hour: boot + 2h is 1h ahead of wall -> bound is boot + 2h.
    let stepped = boot + ChronoDuration::hours(1);
    assert_eq!(
        boot + ChronoDuration::hours(2),
        ProcessInfo::start_bound_fresh(stepped, 2 * 3600 * 1000, Some(boot))
    );
}

#[test]
fn s6_boot_bound_lasts_only_for_the_catch_up_period_and_is_logged_once_per_step() {
    let logs = Arc::new(AtomicI32::new(0));
    let logs_cb = logs.clone();
    let clock = StepClock::new(Some(Box::new(move |_| {
        logs_cb.fetch_add(1, Ordering::AcqRel);
    })));
    let boot = DateTime::parse_from_rfc3339("2026-11-10T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let step_wall = boot + ChronoDuration::minutes(30);
    let b1 = clock.bound(step_wall, 60 * 60 * 1000, Some(boot));
    assert_eq!(boot + ChronoDuration::minutes(60), b1);
    assert_eq!(1, logs.load(Ordering::Acquire));

    // Still inside catch-up: no second log.
    let b2 = clock.bound(
        step_wall + ChronoDuration::minutes(10),
        70 * 60 * 1000,
        Some(boot),
    );
    assert_eq!(boot + ChronoDuration::minutes(60), b2);
    assert_eq!(1, logs.load(Ordering::Acquire));

    // After catch-up: wall clock alone.
    let after = boot + ChronoDuration::minutes(70);
    let b3 = clock.bound(after, 100 * 60 * 1000, Some(boot));
    assert_eq!(after, b3);
    assert_eq!(1, logs.load(Ordering::Acquire));
}

#[test]
fn c3_step_clock_preserves_until_across_multiple_backward_steps_and_sub_tolerance_steps() {
    let clock = StepClock::new(None);
    let boot = DateTime::parse_from_rfc3339("2026-11-10T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let w1 = boot + ChronoDuration::minutes(30);
    let b1 = clock.bound(w1, 60 * 60 * 1000, Some(boot));
    assert_eq!(boot + ChronoDuration::minutes(60), b1);
    // Second smaller step back preserves the later `until`.
    let w2 = boot + ChronoDuration::minutes(20);
    let b2 = clock.bound(w2, 70 * 60 * 1000, Some(boot));
    assert!(b2 >= b1);
}

#[test]
fn b12_same_install_file() {
    let install = Path::new(r"C:\Apps\BR\0.2.0-abcdef012345");
    assert!(PipeManager::same_install_file(
        install,
        Some(r"C:\Apps\BR\0.2.0-abcdef012345\betterrecall.exe"),
        r"C:\Apps\BR\0.1.9-1234567890ab\betterrecall.exe"
    ));
    assert!(PipeManager::same_install_file(
        install,
        Some(r"C:\Apps\BR\0.2.0-abcdef012345\betterrecall.exe"),
        r"C:\Apps\BR\bin\betterrecall.cmd"
    ));
    assert!(!PipeManager::same_install_file(
        install,
        Some(r"C:\Apps\BR\0.2.0-abcdef012345\betterrecall.exe"),
        r"C:\Other\0.1.9-1234567890ab\betterrecall.exe"
    ));
    assert!(!PipeManager::same_install_file(
        install,
        Some(r"C:\Apps\BR\0.2.0-abcdef012345\other.exe"),
        r"C:\Apps\BR\bin\betterrecall.cmd"
    ));
}
