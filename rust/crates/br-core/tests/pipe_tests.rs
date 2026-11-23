// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Ported `PipeTests.cs` + pipe/trust/throttle/StepClock/AgentPresence/CliPurge tests from
//! BatchHCoreTests, Sweep2Tests, Sweep3KTests, Sweep3LCoreTests, Sweep4LTests, Sweep5LTests.

mod common;

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use br_contracts::{CaptureKind, CaptureMeta, Envelope, LineReader, PipeNames, Settings};
use br_core::{
    AuditThrottle, CancelToken, CommandDeps, CommandProcessor, KeyVault, PeerInfo, PipeClient,
    PipeConnectResult, PipeKind, PipeManager, PipeServer, ProcessInfo, SettingsStore,
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
    embedder: Arc<FakeEmbedder>,
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
        let embedder = Arc::new(FakeEmbedder::new());
        let st_for_proc = store.clone();
        let processor = Arc::new(CommandProcessor::new(CommandDeps {
            settings: settings.clone(),
            store: Arc::new(move || Some(st_for_proc.clone())),
            keys: vault.clone(),
            agent: agent.clone(),
            embedder: embedder.clone(),
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
            embedder,
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
    wait_for(|| {
        f.store
            .read_audit(10)
            .unwrap()
            .iter()
            .any(|a| a.verb == "rejected" && a.pipe == "ui")
    });
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

/// C4 (sweep 5): stopping the pipes at session end runs in parallel, so a slow handler on
/// one pipe does not keep the later pipes serving for its whole wait.
#[test]
fn stop_does_not_wait_for_one_slow_pipe_before_the_others() {
    let f = PipeFixture::new();
    let self_exe = ProcessInfo::current_image().unwrap();
    f.set(serde_json::json!({"mcp": {"enabled": true}}));
    wait_for(|| f.pipes.mcp().is_some());
    let entered = Arc::new(AtomicBool::new(false));
    let e2 = entered.clone();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let r2 = release.clone();
    // A search whose query embedding ignores cancel and blocks on `release` keeps one public handler busy.
    *f.embedder.on_embed.lock().unwrap() = Some(Box::new(move |_q, _ct| {
        e2.store(true, Ordering::Release);
        let (lock, cv) = &*r2;
        let guard = lock.lock().unwrap();
        let _ = cv.wait_timeout_while(guard, Duration::from_secs(10), |done| !*done);
        Err(br_core::EmbedError::Cancelled)
    }));
    let mut c = f.connect(PipeKind::Public);
    let slow = thread::spawn(move || {
        let _ = c.request("search hello", &CancelToken::none());
    });
    wait_for(|| entered.load(Ordering::Acquire));
    let mcp_name = f.name(PipeKind::Mcp);
    assert_eq!(
        PipeConnectResult::Ok,
        PipeClient::connect(&mcp_name, &self_exe, 300, &CancelToken::none()).0
    );
    let started = Instant::now();
    thread::scope(|sc| {
        sc.spawn(|| f.pipes.stop());
        // Poll until MCP pipe stops while the public handler is still blocked on `release`.
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut mcp_stopped = false;
        while Instant::now() < deadline {
            if PipeClient::connect(&mcp_name, &self_exe, 100, &CancelToken::none()).0
                == PipeConnectResult::NotRunning
            {
                mcp_stopped = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        // Release the slow handler before asserting so `pipes.stop()` cannot hang on failure.
        {
            let (lock, cv) = &*release;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        assert!(
            mcp_stopped,
            "MCP pipe still serving while the public pipe waits for its slow handler"
        );
    });
    assert!(started.elapsed() < Duration::from_secs(7));
    let _ = slow.join();
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
    f.pipes
        .set_idle_timeout_for_test(Some(Duration::from_millis(500)));
    let mut c = f.connect(PipeKind::Public);
    let sw = Instant::now();
    let res = c
        .read_line_timeout(Duration::from_secs(5), &CancelToken::none())
        .unwrap();
    assert!(res.is_none());
    let secs = sw.elapsed().as_secs_f64();
    assert!((0.3..=3.0).contains(&secs), "elapsed={secs}");
}

/// Regression B14.
#[test]
fn server_check_rejects_a_peer_that_started_after_the_connection() {
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

/// Regression B14.
#[test]
fn client_image_lookup_refuses_a_process_newer_than_the_connection() {
    let pid = ProcessInfo::current_pid();
    let past = Utc::now() - ChronoDuration::days(365);
    assert!(ProcessInfo::image_path_if_started_before(pid, past).is_none());
    let future = Utc::now() + ChronoDuration::seconds(5);
    assert!(ProcessInfo::image_path_if_started_before(pid, future).is_some());
}

/// Regression B15.
#[test]
fn throttle_merges_repeats_per_key_and_caps_the_total() {
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

/// Regression B21.
#[test]
fn revoked_means_a_permission_turned_off() {
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

/// Regression B21.
#[test]
fn removing_a_client_from_allow_clients_drops_its_live_connection() {
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

/// Regression B21.
#[test]
fn turning_a_permission_off_drops_live_connections() {
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

/// B12 with the 0.3 layout: fixed install folder, shim in `<home>\app\bin`, and 0.2.x
/// versioned entries under `<home>\app\<version>` still name the bundled CLI.
#[test]
fn same_install_file_fixed_install_and_home_shim() {
    let install = Path::new(r"C:\Users\u\AppData\Local\Programs\BetterRecall");
    let cli = Some(r"C:\Users\u\AppData\Local\Programs\BetterRecall\betterrecall.exe");
    let shim = r"C:\Users\u\AppData\Local\BetterRecall\app\bin\betterrecall.cmd";
    assert!(PipeManager::same_install_file(install, cli, shim));
    assert!(PipeManager::same_install_file(
        install,
        cli,
        r"C:\Users\u\AppData\Local\BetterRecall\app\0.2.3-dev-0123456789ab\betterrecall.exe"
    ));
    // The shim names the CLI only, not another program in the install folder.
    assert!(!PipeManager::same_install_file(
        install,
        Some(r"C:\Users\u\AppData\Local\Programs\BetterRecall\other.exe"),
        shim
    ));
    // A client outside the install folder never matches.
    assert!(!PipeManager::same_install_file(install, Some(r"C:\Temp\betterrecall.exe"), shim));
    // A versioned entry for another file does not match the CLI.
    assert!(!PipeManager::same_install_file(
        install,
        cli,
        r"C:\Users\u\AppData\Local\BetterRecall\app\0.2.3-dev-0123456789ab\other.exe"
    ));
    // A shim-named file outside an `app\bin` folder does not match.
    assert!(!PipeManager::same_install_file(install, cli, r"C:\Tools\betterrecall.cmd"));
    // Not a version folder: "tools" is refused.
    assert!(!PipeManager::same_install_file(
        install,
        cli,
        r"C:\Users\u\AppData\Local\BetterRecall\app\tools\betterrecall.exe"
    ));
    // Sub-folder relative image admitted when relative paths match across version folders.
    let ui_client = Some(r"C:\Users\u\AppData\Local\Programs\BetterRecall\ui\BetterRecall.UI.exe");
    assert!(PipeManager::same_install_file(
        install,
        ui_client,
        r"C:\Users\u\AppData\Local\BetterRecall\app\0.2.3-aaaaaaaaaaaa\ui\BetterRecall.UI.exe"
    ));
    // Another root is admitted: only the ...\app\<version>\<file> suffix is checked (kept by design, 2026-11-23).
    assert!(PipeManager::same_install_file(
        install,
        cli,
        r"C:\other\app\0.2.3-dev-0123456789ab\betterrecall.exe"
    ));
}
