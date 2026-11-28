// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Integration tests for signed auto-update (`br-agent::host::update`, stage I5)
//! against a local loopback HTTP server and throwaway P-256 keys.
//! No test ever contacts the internet or spawns an installer process.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{DataPaths, UpdateProgress};
use br_core::{AgentControl, AgentError, CancelSource, CancelToken, CommandProcessor};
use br_fetch::{Catalog, CatalogFile, Component};
use br_integrity::{UPDATE_JSON, UPDATE_SIG, UpdateManifest, sign_manifest, sign_update_manifest};
use chrono::{TimeZone, Utc};
use p256::ecdsa::SigningKey;
use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rand_core::OsRng;

use br_agent::host::{AutoStepOutcome, UpdateTestConfig};
use common::{LocalHttpServer, TempDir, make_test_host, sha256_hex, until};

fn gen_keypair() -> (String, String) {
    let sk = SigningKey::random(&mut OsRng);
    let vk = sk.verifying_key();
    let priv_pem = sk.to_pkcs8_pem(LineEnding::LF).expect("priv pem").to_string();
    let pub_pem = vk.to_public_key_pem(LineEnding::LF).expect("pub pem");
    (priv_pem, pub_pem)
}

struct SignedReleaseFixture {
    installer_name: String,
    installer_bytes: Vec<u8>,
    manifest_bytes: Vec<u8>,
    sig_bytes: Vec<u8>,
}

fn make_signed_release(
    version: &str,
    installer_name: &str,
    payload: Vec<u8>,
    priv_pem: &str,
) -> SignedReleaseFixture {
    let manifest = UpdateManifest {
        format: br_integrity::MANIFEST_FORMAT,
        version: version.to_string(),
        installer: installer_name.to_string(),
        size: payload.len() as u64,
        sha256: sha256_hex(&payload),
    };
    let (manifest_bytes, sig) = sign_update_manifest(&manifest, priv_pem).expect("sign manifest");
    SignedReleaseFixture {
        installer_name: installer_name.to_string(),
        installer_bytes: payload,
        manifest_bytes,
        sig_bytes: sig.to_vec(),
    }
}

#[derive(Default)]
struct ReleaseOpts {
    draft: bool,
    prerelease: bool,
    include_sig: bool,
    asset_size_override: Option<u64>,
    installer_url_override: Option<String>,
}

impl ReleaseOpts {
    fn signed() -> Self {
        Self {
            include_sig: true,
            ..Default::default()
        }
    }
}

fn populate_release(
    server: &LocalHttpServer,
    tag: &str,
    rel: &SignedReleaseFixture,
    opts: ReleaseOpts,
) -> serde_json::Value {
    let ver_path = format!("/releases/download/{tag}");
    let json_path = format!("{ver_path}/{UPDATE_JSON}");
    let sig_path = format!("{ver_path}/{UPDATE_SIG}");
    let exe_path = format!("{ver_path}/{}", rel.installer_name);

    server.set_file(&json_path, rel.manifest_bytes.clone());
    if opts.include_sig {
        server.set_file(&sig_path, rel.sig_bytes.clone());
    }
    server.set_file(&exe_path, rel.installer_bytes.clone());

    let mut assets = vec![
        serde_json::json!({
            "name": UPDATE_JSON,
            "size": rel.manifest_bytes.len(),
            "browser_download_url": server.asset_url(&json_path),
        }),
        serde_json::json!({
            "name": rel.installer_name,
            "size": opts.asset_size_override.unwrap_or(rel.installer_bytes.len() as u64),
            "browser_download_url": opts.installer_url_override.unwrap_or_else(|| server.asset_url(&exe_path)),
        }),
    ];
    if opts.include_sig {
        assets.push(serde_json::json!({
            "name": UPDATE_SIG,
            "size": rel.sig_bytes.len(),
            "browser_download_url": server.asset_url(&sig_path),
        }));
    }

    serde_json::json!({
        "tag_name": tag,
        "draft": opts.draft,
        "prerelease": opts.prerelease,
        "html_url": format!("{}tag/{tag}", server.release_prefix()),
        "published_at": "2026-11-22T10:00:00Z",
        "assets": assets,
    })
}

type LaunchLog = Arc<Mutex<Vec<(PathBuf, Vec<String>)>>>;

fn make_launcher() -> (LaunchLog, br_agent::host::InstallerLauncher) {
    let log: LaunchLog = Arc::new(Mutex::new(Vec::new()));
    let log_c = log.clone();
    let hook: br_agent::host::InstallerLauncher = Arc::new(move |path, args| {
        log_c
            .lock()
            .unwrap()
            .push((path.to_path_buf(), args.to_vec()));
        Ok(None)
    });
    (log, hook)
}

// 1. Newer signed release -> check, download, apply calls launcher hook once with installer path and /S.
#[test]
fn newer_signed_release_check_download_and_apply() {
    let dir = TempDir::new("upd-happy");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel = make_signed_release(
        "0.3.0",
        "BetterRecall-Setup-0.3.0.exe",
        vec![0x5Au8; 8192],
        &priv_pem,
    );
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());

    let (launches, hook) = make_launcher();
    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.3.0-dev".to_string()),
        fast_retries: true,
        launcher: Some(hook),
    }));

    let st = host.update_check(&CancelToken::none()).expect("check ok");
    assert!(st.available);
    assert!(st.can_install);
    assert_eq!(st.latest_version.as_deref(), Some("0.3.0"));
    assert_eq!(st.installer_bytes, Some(8192));
    assert!(st.notes_url.as_deref().unwrap().ends_with("/tag/v0.3.0"));
    assert_eq!(st.state, "idle");

    let mut events: Vec<UpdateProgress> = Vec::new();
    host.update_download(&mut |p| events.push(p), &CancelToken::none())
        .expect("download ok");
    assert_eq!(events.first().map(|e| e.phase.as_str()), Some("checking"));
    assert!(events.iter().any(|e| e.phase == "downloading"));
    assert!(events.iter().any(|e| e.phase == "verifying"));
    assert_eq!(events.last().map(|e| e.phase.as_str()), Some("done"));

    let ver_dir = DataPaths::updates_dir(&dir.path().join("home")).join("0.3.0");
    let expected_exe = ver_dir.join("BetterRecall-Setup-0.3.0.exe");
    assert!(ver_dir.join(UPDATE_JSON).exists());
    assert!(ver_dir.join(UPDATE_SIG).exists());
    assert!(expected_exe.exists());

    let applied = host.update_apply().expect("apply ok");
    assert_eq!(applied.state, "applying");
    let calls = launches.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, expected_exe);
    assert_eq!(calls[0].1, vec!["/S".to_string()]);

    host.shutdown();
}

// 2. Rejections: missing sig, flipped sig, wrong key, replay (older/equal), tag mismatch,
//    size mismatch, disallowed asset URL, installer name with path separator.
#[test]
fn update_rejects_all_invalid_or_untrusted_releases() {
    let (priv_pem, pub_pem) = gen_keypair();
    let (other_priv, _) = gen_keypair();

    let assert_rejected = |label: &str,
                           current_ver: &str,
                           build_feed: &dyn Fn(&LocalHttpServer)| {
        let dir = TempDir::new(&format!("upd-rej-{label}"));
        let host = make_test_host(&dir);
        let server = LocalHttpServer::start(HashMap::new());
        build_feed(&server);
        let (launches, hook) = make_launcher();
        host.set_update_test_config(Some(UpdateTestConfig {
            feed_url: server.feed_url(),
            allowed_prefix: server.release_prefix(),
            public_key_pem: Some(pub_pem.clone()),
            current_version: Some(current_ver.to_string()),
            fast_retries: true,
            launcher: Some(hook),
        }));

        let st = host
            .update_check(&CancelToken::none())
            .expect("check returns status");
        assert!(!st.available, "case={label}: available must be false");

        let mut events: Vec<UpdateProgress> = Vec::new();
        let _ = host.update_download(&mut |p| events.push(p), &CancelToken::none());
        assert_eq!(
            events.last().map(|e| e.phase.as_str()),
            Some("failed"),
            "case={label}: download must fail, got {events:?}"
        );
        assert!(host.update_apply().is_err(), "case={label}: apply must refuse");
        assert!(launches.lock().unwrap().is_empty(), "case={label}: no launch");
        host.shutdown();
    };

    assert_rejected("missing-sig", "0.3.0-dev", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abc".to_vec(), &priv_pem);
        let entry = populate_release(
            server,
            "v0.3.0",
            &rel,
            ReleaseOpts {
                include_sig: false,
                ..ReleaseOpts::signed()
            },
        );
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    assert_rejected("flipped-sig", "0.3.0-dev", &|server| {
        let mut rel = make_signed_release("0.3.0", "Setup.exe", b"abc".to_vec(), &priv_pem);
        rel.sig_bytes[0] ^= 0x01;
        let entry = populate_release(server, "v0.3.0", &rel, ReleaseOpts::signed());
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    assert_rejected("wrong-key", "0.3.0-dev", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abc".to_vec(), &other_priv);
        let entry = populate_release(server, "v0.3.0", &rel, ReleaseOpts::signed());
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    assert_rejected("replay-equal", "0.3.0", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abc".to_vec(), &priv_pem);
        let entry = populate_release(server, "v0.3.0", &rel, ReleaseOpts::signed());
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    assert_rejected("replay-older", "0.4.0", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abc".to_vec(), &priv_pem);
        let entry = populate_release(server, "v0.3.0", &rel, ReleaseOpts::signed());
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    assert_rejected("tag-mismatch", "0.3.0-dev", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abc".to_vec(), &priv_pem);
        let entry = populate_release(server, "v0.3.1", &rel, ReleaseOpts::signed());
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    assert_rejected("size-mismatch", "0.3.0-dev", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abcdef".to_vec(), &priv_pem);
        let entry = populate_release(
            server,
            "v0.3.0",
            &rel,
            ReleaseOpts {
                asset_size_override: Some(999),
                ..ReleaseOpts::signed()
            },
        );
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    assert_rejected("bad-url", "0.3.0-dev", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abcdef".to_vec(), &priv_pem);
        let entry = populate_release(
            server,
            "v0.3.0",
            &rel,
            ReleaseOpts {
                installer_url_override: Some(format!("{}other/Setup.exe", server.base_url())),
                ..ReleaseOpts::signed()
            },
        );
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    assert_rejected("path-sep", "0.3.0-dev", &|server| {
        let payload = b"abcdef".to_vec();
        let bad_json = format!(
            r#"{{"format":1,"version":"0.3.0","installer":"..\\evil.exe","size":{},"sha256":"{}"}}"#,
            payload.len(),
            sha256_hex(&payload)
        )
        .into_bytes();
        let sig = sign_manifest(&bad_json, &priv_pem).unwrap().to_vec();
        let rel = SignedReleaseFixture {
            installer_name: "..\\evil.exe".to_string(),
            installer_bytes: payload,
            manifest_bytes: bad_json,
            sig_bytes: sig,
        };
        let entry = populate_release(server, "v0.3.0", &rel, ReleaseOpts::signed());
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });
}

// 3. Installer bytes mismatch manifest SHA-256 -> failed, no launch, no completed installer file left.
#[test]
fn corrupted_installer_bytes_fail_and_leave_no_completed_file() {
    let dir = TempDir::new("upd-corrupt");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel = make_signed_release(
        "0.3.0",
        "Setup.exe",
        b"good-installer-bytes".to_vec(),
        &priv_pem,
    );
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file(
        "/releases/download/v0.3.0/Setup.exe",
        b"BAD!-installer-bytes".to_vec(),
    );
    server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());

    let (launches, hook) = make_launcher();
    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.3.0-dev".to_string()),
        fast_retries: true,
        launcher: Some(hook),
    }));

    let mut events = Vec::new();
    host.update_download(&mut |p| events.push(p), &CancelToken::none())
        .expect("stream ends cleanly");
    assert_eq!(events.last().map(|e| e.phase.as_str()), Some("failed"));
    let exe = DataPaths::updates_dir(&dir.path().join("home"))
        .join("0.3.0")
        .join("Setup.exe");
    assert!(!exe.exists(), "corrupted installer must not be left at final path");
    assert!(host.update_apply().is_err());
    assert!(launches.lock().unwrap().is_empty());

    host.shutdown();
}

// 4. Installer modified on disk between download and apply -> apply refuses.
#[test]
fn installer_tampered_after_download_is_refused_by_apply() {
    let dir = TempDir::new("upd-tamper");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel = make_signed_release(
        "0.3.0",
        "Setup.exe",
        b"clean-installer-data".to_vec(),
        &priv_pem,
    );
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());

    let (launches, hook) = make_launcher();
    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.3.0-dev".to_string()),
        fast_retries: true,
        launcher: Some(hook),
    }));

    let mut events = Vec::new();
    host.update_download(&mut |p| events.push(p), &CancelToken::none())
        .unwrap();
    assert_eq!(events.last().map(|e| e.phase.as_str()), Some("done"));

    let exe = DataPaths::updates_dir(&dir.path().join("home"))
        .join("0.3.0")
        .join("Setup.exe");
    fs::write(&exe, b"TAMPERED-installer!!").unwrap();

    let err = host.update_apply().unwrap_err();
    assert!(matches!(err, AgentError::InvalidOperation(_)));
    assert!(launches.lock().unwrap().is_empty());

    host.shutdown();
}

// 5. Drafts skipped; prerelease picked only when running version is a prerelease.
#[test]
fn drafts_skipped_and_prerelease_only_for_prerelease_running_version() {
    let dir = TempDir::new("upd-pre");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());

    let draft_rel = make_signed_release("0.5.0", "Setup-0.5.0.exe", b"draft".to_vec(), &priv_pem);
    let pre_rel = make_signed_release("0.4.0-rc.1", "Setup-0.4.0-rc.1.exe", b"pre".to_vec(), &priv_pem);
    let stable_rel = make_signed_release("0.3.1", "Setup-0.3.1.exe", b"stable".to_vec(), &priv_pem);

    let feed = vec![
        populate_release(
            &server,
            "v0.5.0",
            &draft_rel,
            ReleaseOpts {
                draft: true,
                ..ReleaseOpts::signed()
            },
        ),
        populate_release(
            &server,
            "v0.4.0-rc.1",
            &pre_rel,
            ReleaseOpts {
                prerelease: true,
                ..ReleaseOpts::signed()
            },
        ),
        populate_release(&server, "v0.3.1", &stable_rel, ReleaseOpts::signed()),
    ];
    server.set_file("/releases", serde_json::to_vec(&feed).unwrap());

    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem.clone()),
        current_version: Some("0.3.0".to_string()),
        fast_retries: true,
        launcher: None,
    }));
    let st_stable = host.update_check(&CancelToken::none()).unwrap();
    assert!(st_stable.available);
    assert_eq!(st_stable.latest_version.as_deref(), Some("0.3.1"));

    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.3.0-dev".to_string()),
        fast_retries: true,
        launcher: None,
    }));
    let st_pre = host.update_check(&CancelToken::none()).unwrap();
    assert!(st_pre.available);
    assert_eq!(st_pre.latest_version.as_deref(), Some("0.4.0-rc.1"));

    host.shutdown();
}

// 6. Cancel mid-download keeps .part, and a second download resumes from it (CC P10).
#[test]
fn cancel_mid_download_keeps_part_and_second_download_resumes() {
    let dir = TempDir::new("upd-resume");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let payload: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();
    let rel = make_signed_release("0.3.0", "Setup.exe", payload.clone(), &priv_pem);
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    let exe_path = "/releases/download/v0.3.0/Setup.exe";
    server.set_slow(exe_path, 4096, Duration::from_millis(1500));

    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.3.0-dev".to_string()),
        fast_retries: true,
        launcher: None,
    }));

    let ver_dir = DataPaths::updates_dir(&dir.path().join("home")).join("0.3.0");
    let part = ver_dir.join("Setup.exe.part");

    let cts = CancelSource::new();
    let token = cts.token();
    let mut events1 = Vec::new();
    thread::scope(|s| {
        s.spawn(|| {
            assert!(
                until(
                    || server.requests().iter().any(|r| r.path == exe_path)
                        && fs::metadata(&part).is_ok_and(|m| m.len() > 0),
                    Duration::from_secs(5),
                ),
                "installer request or .part file never appeared before cancel"
            );
            cts.cancel();
        });
        host.update_download(&mut |p| events1.push(p), &token)
            .unwrap();
    });
    assert_eq!(events1.last().map(|e| e.phase.as_str()), Some("cancelled"));
    assert!(part.exists(), ".part file must remain after cancel");
    assert!(fs::metadata(&part).unwrap().len() > 0);

    // Second download resumes via Range header and completes.
    server.clear_slow();
    let mut events2 = Vec::new();
    host.update_download(&mut |p| events2.push(p), &CancelToken::none())
        .unwrap();
    assert_eq!(events2.last().map(|e| e.phase.as_str()), Some("done"));
    assert_eq!(fs::read(ver_dir.join("Setup.exe")).unwrap(), payload);
    assert!(
        server
            .requests()
            .iter()
            .any(|r| r.path == exe_path && r.range.is_some()),
        "second download should have sent a Range header to resume"
    );

    host.shutdown();
}

// 7. No key (dev build) -> check reports latest version with canInstall=false; download/apply refuse.
#[test]
fn no_key_build_reports_available_but_refuses_download_and_apply() {
    let dir = TempDir::new("upd-nokey");
    let host = make_test_host(&dir);
    let (priv_pem, _) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel = make_signed_release("0.3.0", "Setup.exe", b"payload".to_vec(), &priv_pem);
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());

    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: None,
        current_version: Some("0.3.0-dev".to_string()),
        fast_retries: true,
        launcher: None,
    }));

    let st = host.update_check(&CancelToken::none()).unwrap();
    assert!(st.available);
    assert!(!st.can_install);
    assert_eq!(st.latest_version.as_deref(), Some("0.3.0"));
    assert_eq!(
        st.reason.as_deref(),
        Some(br_agent::host::update::NO_KEY_REASON)
    );

    let dl_err = host
        .update_download(&mut |_| {}, &CancelToken::none())
        .unwrap_err();
    match dl_err {
        AgentError::InvalidOperation(msg) => assert_eq!(msg, br_agent::host::update::NO_KEY_REASON),
        other => panic!("expected InvalidOperation, got {other:?}"),
    }

    let apply_err = host.update_apply().unwrap_err();
    match apply_err {
        AgentError::InvalidOperation(msg) => assert_eq!(msg, br_agent::host::update::NO_KEY_REASON),
        other => panic!("expected InvalidOperation, got {other:?}"),
    }

    host.shutdown();
}

// 8. Auto-update scheduler: auto=false makes 0 requests; auto=true downloads but defers when not idle;
//    auto=true + idle >= 10 min launches once.
#[test]
fn auto_update_scheduler_respects_setting_and_idle_gate() {
    let dir = TempDir::new("upd-auto");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel = make_signed_release("0.3.0", "Setup.exe", b"auto-installer".to_vec(), &priv_pem);
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());

    let (launches, hook) = make_launcher();
    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.3.0-dev".to_string()),
        fast_retries: true,
        launcher: Some(hook),
    }));

    let now = Utc.with_ymd_and_hms(2026, 11, 22, 12, 0, 0).unwrap();

    let out_off = host.updater().auto_step(
        now,
        br_agent::host::update::AUTO_IDLE_THRESHOLD_MS + 1000,
        false,
        false,
        &CancelToken::none(),
    );
    assert_eq!(out_off, AutoStepOutcome::Disabled);
    assert!(server.requests().is_empty());
    assert!(launches.lock().unwrap().is_empty());

    let out_active = host
        .updater()
        .auto_step(now, 60_000, false, true, &CancelToken::none());
    assert_eq!(out_active, AutoStepOutcome::Downloaded);
    assert!(!server.requests().is_empty());
    assert!(launches.lock().unwrap().is_empty());

    let out_busy = host.updater().auto_step(
        now + chrono::Duration::minutes(15),
        br_agent::host::update::AUTO_IDLE_THRESHOLD_MS,
        true,
        true,
        &CancelToken::none(),
    );
    assert_eq!(out_busy, AutoStepOutcome::Deferred);
    assert!(launches.lock().unwrap().is_empty());

    let out_idle = host.updater().auto_step(
        now + chrono::Duration::minutes(30),
        br_agent::host::update::AUTO_IDLE_THRESHOLD_MS,
        false,
        true,
        &CancelToken::none(),
    );
    assert_eq!(out_idle, AutoStepOutcome::Applied);
    assert_eq!(launches.lock().unwrap().len(), 1);

    host.shutdown();
}

// 9. Agent start cleans a downloaded folder whose version is not newer than the running version.
#[test]
fn cleanup_on_start_removes_stale_downloaded_folder() {
    let dir = TempDir::new("upd-clean");
    let home = dir.path().join("home");
    let updates_dir = DataPaths::updates_dir(&home);
    let stale_dir = updates_dir.join("0.3.0");
    fs::create_dir_all(&stale_dir).unwrap();
    fs::write(stale_dir.join("Setup.exe"), b"old-installer").unwrap();
    fs::write(
        updates_dir.join("state.json"),
        r#"{"lastCheck":"2026-11-22T10:00:00Z","latest":null,"downloaded":"0.3.0"}"#,
    )
    .unwrap();

    let host = make_test_host(&dir);
    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: "http://127.0.0.1:1/releases".to_string(),
        allowed_prefix: "http://127.0.0.1:1/releases/".to_string(),
        public_key_pem: None,
        current_version: Some("0.3.0".to_string()),
        fast_retries: true,
        launcher: None,
    }));

    host.updater().cleanup_on_start();
    assert!(!stale_dir.exists(), "stale downloaded folder must be deleted");
    let st = host.update_status().unwrap();
    assert_eq!(st.downloaded_version, None);
    assert_eq!(st.state, "idle");

    host.shutdown();
}

// 10. F2: update download rejects concurrent update (ERR_UPDATE_BUSY) and refuses while models
//     download is running (ERR_MODELS_RUNNING_FOR_UPDATE) in src\host\update.rs (~358-375).
#[test]
fn update_download_rejects_concurrent_update_and_models_running() {
    let dir = TempDir::new("upd-busy");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let payload = vec![0x5Au8; 16384];
    let rel = make_signed_release("0.3.0", "Setup.exe", payload, &priv_pem);
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    let exe_path = "/releases/download/v0.3.0/Setup.exe";
    server.set_slow(exe_path, 512, Duration::from_millis(2000));

    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.3.0-dev".to_string()),
        fast_retries: true,
        launcher: None,
    }));

    // (a) Concurrent update_download -> ERR_UPDATE_BUSY, and is_busy_for_auto_update() == true.
    let cts1 = CancelSource::new();
    let tok1 = cts1.token();
    let host_bg = host.clone();
    let upd = thread::spawn(move || host_bg.update_download(&mut |_| {}, &tok1));

    assert!(
        until(
            || server.request_paths().iter().any(|p| p == exe_path),
            Duration::from_secs(5),
        ),
        "first update download never reached server"
    );
    assert!(host.is_busy_for_auto_update());
    let busy_err = host
        .update_download(&mut |_| {}, &CancelToken::none())
        .expect_err("concurrent update_download must fail");
    match busy_err {
        AgentError::InvalidOperation(msg) => assert_eq!(msg, CommandProcessor::ERR_UPDATE_BUSY),
        other => panic!("expected InvalidOperation ERR_UPDATE_BUSY, got {other:?}"),
    }
    cts1.cancel();
    upd.join().unwrap().expect("cancelled update returns Ok");
    assert!(!host.is_busy_for_auto_update());

    // (b) update_download while models_download is running -> ERR_MODELS_RUNNING_FOR_UPDATE.
    let weights = vec![0xABu8; 8192];
    let manifest = br#"{"id":"silero-vad","version":1}"#.to_vec();
    server.set_file("/silero-vad/silero_vad.onnx", weights.clone());
    server.set_file("/silero-vad/manifest.json", manifest.clone());
    server.set_slow("/silero-vad/silero_vad.onnx", 512, Duration::from_millis(2000));
    let rev = "0123456789abcdef0123456789abcdef01234567";
    let cat = Catalog {
        repo: "test/models".to_string(),
        revision: rev.to_string(),
        base_url: format!("https://huggingface.co/test/models/resolve/{rev}/"),
        components: vec![Component {
            id: "silero-vad".to_string(),
            sets: vec!["full".to_string()],
            bytes: (weights.len() + manifest.len()) as u64,
            files: vec![
                CatalogFile {
                    path: "manifest.json".to_string(),
                    size: manifest.len() as u64,
                    sha256: sha256_hex(&manifest),
                },
                CatalogFile {
                    path: "silero_vad.onnx".to_string(),
                    size: weights.len() as u64,
                    sha256: sha256_hex(&weights),
                },
            ],
        }],
    };
    cat.validate().unwrap();
    host.set_catalog_for_test(Some(cat.with_base_url_for_test(server.base_url())));

    let cts2 = CancelSource::new();
    let tok2 = cts2.token();
    let host_bg2 = host.clone();
    let mdl = thread::spawn(move || {
        host_bg2.models_download("silero-vad", &mut |_| {}, &tok2)
    });

    assert!(
        until(
            || server
                .request_paths()
                .iter()
                .any(|p| p == "/silero-vad/silero_vad.onnx"),
            Duration::from_secs(5),
        ),
        "model download never reached server"
    );
    let excl_err = host
        .update_download(&mut |_| {}, &CancelToken::none())
        .expect_err("update_download must refuse while models_download runs");
    match excl_err {
        AgentError::InvalidOperation(msg) => {
            assert_eq!(msg, CommandProcessor::ERR_MODELS_RUNNING_FOR_UPDATE);
        }
        other => panic!("expected InvalidOperation ERR_MODELS_RUNNING_FOR_UPDATE, got {other:?}"),
    }

    cts2.cancel();
    mdl.join().unwrap().expect("cancelled models returns Ok");
    host.shutdown();
}

// C1: Watcher thread observes non-zero exit code (3), transitions state to error and records failure.
#[test]
fn apply_watcher_detects_exit_code_3_and_records_failure() {
    use std::os::windows::io::IntoRawHandle;
    use windows::Win32::Foundation::HANDLE;

    let dir = TempDir::new("upd-watcher-exit3");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel = make_signed_release(
        "0.3.0",
        "BetterRecall-Setup-0.3.0.exe",
        vec![0x42u8; 1024],
        &priv_pem,
    );
    let gh_rel = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![gh_rel]).unwrap());

    let hook: br_agent::host::InstallerLauncher = Arc::new(move |_path, _args| {
        let child = std::process::Command::new("cmd")
            .args(["/c", "exit", "3"])
            .spawn()?;
        let raw = child.into_raw_handle();
        Ok(Some(HANDLE(raw as *mut _)))
    });

    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.2.0".to_string()),
        fast_retries: true,
        launcher: Some(hook),
    }));

    let status = host
        .update_check(&CancelToken::none())
        .expect("check ok");
    assert!(status.available);

    host.update_download(&mut |_| {}, &CancelToken::none())
        .expect("download ok");

    let applied = host.update_apply().expect("apply ok");
    assert_eq!(applied.state, "applying");

    assert!(
        until(
            || host.update_status().unwrap().state == "error",
            Duration::from_secs(5),
        ),
        "state never transitioned to error"
    );

    let final_status = host.update_status().unwrap();
    assert_eq!(final_status.state, "error");
    assert!(
        final_status
            .error
            .as_deref()
            .unwrap_or("")
            .contains("setup exit code 3"),
        "error was {:?}",
        final_status.error
    );

    let state_path = DataPaths::updates_dir(&dir.path().join("home")).join("state.json");
    let state_raw = fs::read_to_string(&state_path).expect("read state.json");
    let state_json: serde_json::Value = serde_json::from_str(&state_raw).expect("parse state.json");
    assert_eq!(state_json["failedVersion"], "0.3.0");
    assert_eq!(state_json["failedCount"], 1);
    assert!(state_json["failedAt"].is_string());
}

// C1: Manager start reads last-install.json, starts in error if exit code != 0 and records failure.
#[test]
fn manager_start_reads_last_install_json_error_and_records_failure() {
    let dir = TempDir::new("upd-last-install");
    let updates_dir = DataPaths::updates_dir(&dir.path().join("home"));
    fs::create_dir_all(&updates_dir).unwrap();

    let state_path = updates_dir.join("state.json");
    let state_data = serde_json::json!({
        "downloaded": "0.3.0",
        "lastCheck": "2026-11-23T10:00:00Z",
    });
    fs::write(&state_path, serde_json::to_vec(&state_data).unwrap()).unwrap();

    let last_install_path = updates_dir.join("last-install.json");
    let last_install_data = serde_json::json!({
        "version": "0.3.0",
        "exitCode": 3,
        "utc": "2026-11-23T11:00:00Z"
    });
    fs::write(&last_install_path, serde_json::to_vec(&last_install_data).unwrap()).unwrap();

    let host = make_test_host(&dir);
    let status = host.update_status().unwrap();
    assert_eq!(status.state, "error");
    assert!(
        status
            .error
            .as_deref()
            .unwrap_or("")
            .contains("setup exit code 3"),
        "error was {:?}",
        status.error
    );

    let state_raw = fs::read_to_string(&state_path).expect("read state.json");
    let state_json: serde_json::Value = serde_json::from_str(&state_raw).expect("parse state.json");
    assert_eq!(state_json["failedVersion"], "0.3.0");
    assert_eq!(state_json["failedCount"], 1);
    assert_eq!(state_json["lastInstallHandledUtc"], "2026-11-23T11:00:00Z");

    // Second start does not increment failedCount when utc is same
    let _host2 = make_test_host(&dir);
    let state_raw2 = fs::read_to_string(&state_path).expect("read state.json");
    let state_json2: serde_json::Value = serde_json::from_str(&state_raw2).expect("parse state.json");
    assert_eq!(state_json2["failedCount"], 1);
}

// C1: auto_step skips during backoff and resumes after.
#[test]
fn auto_step_skips_during_backoff_and_resumes_after() {
    let dir = TempDir::new("upd-backoff");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel = make_signed_release(
        "0.3.0",
        "BetterRecall-Setup-0.3.0.exe",
        vec![0x42u8; 1024],
        &priv_pem,
    );
    let gh_rel = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![gh_rel]).unwrap());

    let (launches, hook) = make_launcher();
    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem.clone()),
        current_version: Some("0.2.0".to_string()),
        fast_retries: true,
        launcher: Some(hook),
    }));

    host.update_check(&CancelToken::none()).unwrap();
    host.update_download(&mut |_| {}, &CancelToken::none()).unwrap();

    let updates_dir = DataPaths::updates_dir(&dir.path().join("home"));
    let state_path = updates_dir.join("state.json");

    let t0 = Utc.with_ymd_and_hms(2026, 11, 23, 10, 0, 0).unwrap();
    let state_data = serde_json::json!({
        "downloaded": "0.3.0",
        "lastCheck": "2026-11-23T10:00:00Z",
        "latest": {
            "version": "0.3.0",
            "installerBytes": 1024
        },
        "failedVersion": "0.3.0",
        "failedCount": 1,
        "failedAt": "2026-11-23T10:00:00Z"
    });
    fs::write(&state_path, serde_json::to_vec(&state_data).unwrap()).unwrap();

    let host_new = make_test_host(&dir);
    let launches_clone = Arc::clone(&launches);
    host_new.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.2.0".to_string()),
        fast_retries: true,
        launcher: Some(Arc::new(move |_p, _a| {
            launches_clone.lock().unwrap().push((_p.to_path_buf(), _a.to_vec()));
            Ok(None)
        })),
    }));

    let idle_ms = 15 * 60 * 1000;

    // At t0 + 30m: within 1 h backoff -> skipped (Deferred)
    let t_30m = t0 + chrono::Duration::minutes(30);
    let outcome = host_new.updater().auto_step(t_30m, idle_ms, false, true, &CancelToken::none());
    assert_eq!(outcome, AutoStepOutcome::Deferred);

    // At t0 + 65m: past 1 h backoff -> resumes and applies
    let t_65m = t0 + chrono::Duration::minutes(65);
    let outcome2 = host_new.updater().auto_step(t_65m, idle_ms, false, true, &CancelToken::none());
    assert_eq!(outcome2, AutoStepOutcome::Applied);
}

// C1: state.json without the new fields still loads successfully.
#[test]
fn state_json_without_new_c1_fields_loads_successfully() {
    let dir = TempDir::new("upd-compat");
    let updates_dir = DataPaths::updates_dir(&dir.path().join("home"));
    fs::create_dir_all(&updates_dir).unwrap();

    let state_path = updates_dir.join("state.json");
    let legacy_json = r#"{
        "lastCheck": "2026-11-22T10:00:00Z",
        "latest": {
            "version": "9.0.0",
            "installerBytes": 1024
        },
        "downloaded": "9.0.0"
    }"#;
    fs::write(&state_path, legacy_json).unwrap();

    let host = make_test_host(&dir);
    let status = host.update_status().unwrap();
    assert_eq!(status.state, "ready");
    assert_eq!(status.downloaded_version.as_deref(), Some("9.0.0"));
}

// C1: apply refuses with an InvalidOperation while state is applying.
#[test]
fn apply_refuses_while_already_applying() {
    let dir = TempDir::new("upd-applying-refuse");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel = make_signed_release(
        "0.3.0",
        "BetterRecall-Setup-0.3.0.exe",
        vec![0x42u8; 1024],
        &priv_pem,
    );
    let gh_rel = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![gh_rel]).unwrap());

    let (_launches, hook) = make_launcher();
    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.2.0".to_string()),
        fast_retries: true,
        launcher: Some(hook),
    }));

    host.update_check(&CancelToken::none()).unwrap();
    host.update_download(&mut |_| {}, &CancelToken::none()).unwrap();

    let st = host.update_apply().expect("first apply ok");
    assert_eq!(st.state, "applying");

    let err = host.update_apply().expect_err("second apply while applying must refuse");
    assert!(matches!(err, AgentError::InvalidOperation(_)), "got {err:?}");
}

// C7: apply refuses while models downloading.
#[test]
fn apply_refuses_while_models_downloading() {
    let dir = TempDir::new("upd-c7-apply");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel = make_signed_release(
        "0.3.0",
        "BetterRecall-Setup-0.3.0.exe",
        vec![0x42u8; 1024],
        &priv_pem,
    );
    let gh_rel = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![gh_rel]).unwrap());

    let (_launches, hook) = make_launcher();
    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.2.0".to_string()),
        fast_retries: true,
        launcher: Some(hook),
    }));

    host.update_check(&CancelToken::none()).unwrap();
    host.update_download(&mut |_| {}, &CancelToken::none()).unwrap();

    let weights = vec![0xABu8; 8192];
    let manifest = br#"{"id":"silero-vad","version":1}"#.to_vec();
    server.set_file("/silero-vad/silero_vad.onnx", weights.clone());
    server.set_file("/silero-vad/manifest.json", manifest.clone());
    server.set_slow("/silero-vad/silero_vad.onnx", 512, Duration::from_millis(2000));
    let rev = "0123456789abcdef0123456789abcdef01234567";
    let cat = Catalog {
        repo: "test/models".to_string(),
        revision: rev.to_string(),
        base_url: format!("https://huggingface.co/test/models/resolve/{rev}/"),
        components: vec![Component {
            id: "silero-vad".to_string(),
            sets: vec!["full".to_string()],
            bytes: (weights.len() + manifest.len()) as u64,
            files: vec![
                CatalogFile {
                    path: "manifest.json".to_string(),
                    size: manifest.len() as u64,
                    sha256: sha256_hex(&manifest),
                },
                CatalogFile {
                    path: "silero_vad.onnx".to_string(),
                    size: weights.len() as u64,
                    sha256: sha256_hex(&weights),
                },
            ],
        }],
    };
    cat.validate().unwrap();
    host.set_catalog_for_test(Some(cat.with_base_url_for_test(server.base_url())));

    let cts = CancelSource::new();
    let tok = cts.token();
    let host_bg = host.clone();
    let mdl = thread::spawn(move || {
        host_bg.models_download("silero-vad", &mut |_| {}, &tok)
    });

    assert!(
        until(
            || server
                .request_paths()
                .iter()
                .any(|p| p == "/silero-vad/silero_vad.onnx"),
            Duration::from_secs(5),
        ),
        "model download never reached server"
    );

    let err = host.update_apply().expect_err("update_apply must refuse while models_download runs");
    match err {
        AgentError::InvalidOperation(msg) => {
            assert_eq!(msg, CommandProcessor::ERR_MODELS_RUNNING_FOR_UPDATE);
        }
        other => panic!("expected InvalidOperation ERR_MODELS_RUNNING_FOR_UPDATE, got {other:?}"),
    }

    cts.cancel();
    mdl.join().unwrap().expect("cancelled models returns Ok");
    host.shutdown();
}

// C4: Cancelled update check leaves state as it was before the check.
#[test]
fn check_cancellation_leaves_state_as_before() {
    let dir = TempDir::new("upd-cancel-check");
    let host = make_test_host(&dir);
    let (_priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());

    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.2.0".to_string()),
        fast_retries: true,
        launcher: None,
    }));

    let initial_status = host.update_status().unwrap();
    assert_eq!(initial_status.state, "idle");

    let cts = CancelSource::new();
    cts.cancel();
    let res = host.update_check(&cts.token());
    assert!(matches!(res, Err(AgentError::Cancelled)));

    let after_status = host.update_status().unwrap();
    assert_eq!(after_status.state, initial_status.state);
    assert_eq!(after_status.last_check, initial_status.last_check);
}

// C3: Watcher records failure and marks last-install.json handled; restart does not double-count.
#[test]
fn c3_watcher_marks_last_install_handled_and_restart_does_not_double_count() {
    use std::os::windows::io::IntoRawHandle;
    use std::sync::atomic::AtomicBool;
    use windows::Win32::Foundation::HANDLE;

    let dir = TempDir::new("upd-c3-dedupe");
    let home = dir.path().join("home");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel = make_signed_release(
        "0.3.0",
        "BetterRecall-Setup-0.3.0.exe",
        vec![0x42u8; 1024],
        &priv_pem,
    );
    let gh_rel = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![gh_rel]).unwrap());

    let report_utc = "2026-11-24T15:12:12.148Z";
    let updates_dir = DataPaths::updates_dir(&home);
    let updates_dir_c = updates_dir.clone();
    let hook: br_agent::host::InstallerLauncher = Arc::new(move |_path, _args| {
        let report = serde_json::json!({
            "version": "0.3.0",
            "exitCode": 5,
            "utc": report_utc,
        });
        fs::write(
            updates_dir_c.join("last-install.json"),
            serde_json::to_vec(&report).unwrap(),
        )?;
        let child = std::process::Command::new("cmd")
            .args(["/c", "exit", "5"])
            .spawn()?;
        let raw = child.into_raw_handle();
        Ok(Some(HANDLE(raw as *mut _)))
    });

    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.2.0".to_string()),
        fast_retries: true,
        launcher: Some(hook),
    }));

    host.update_check(&CancelToken::none()).expect("check ok");
    host.update_download(&mut |_| {}, &CancelToken::none())
        .expect("download ok");
    host.update_apply().expect("apply ok");

    assert!(
        until(
            || host.update_status().unwrap().state == "error",
            Duration::from_secs(5),
        ),
        "watcher never transitioned state to error"
    );

    let state_path = updates_dir.join("state.json");
    let state_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(state_json["failedVersion"], "0.3.0");
    assert_eq!(state_json["failedCount"], 1);
    assert_eq!(state_json["lastInstallHandledUtc"], report_utc);

    // New manager on the same home must not count the same report again.
    let mgr2 = br_agent::host::UpdateManager::new_with_version(
        home,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        "0.2.0",
    );
    let st2 = mgr2.status(false);
    assert_eq!(st2.state, "ready");
    assert!(st2.error.is_none());

    let state_json2: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(state_json2["failedCount"], 1);

    host.shutdown();
}

// C3: A report whose utc is older than lastInstallHandledUtc (backward clock step) but different
// is still handled once.
#[test]
fn c3_older_but_different_utc_report_is_handled_once() {
    use std::sync::atomic::AtomicBool;

    let dir = TempDir::new("upd-c3-clock-back");
    let home = dir.path().join("home");
    let updates_dir = DataPaths::updates_dir(&home);
    fs::create_dir_all(&updates_dir).unwrap();

    let state_path = updates_dir.join("state.json");
    let state_data = serde_json::json!({
        "downloaded": "0.4.0",
        "failedVersion": "0.4.0",
        "failedCount": 1,
        "failedAt": "2026-11-24T15:00:00Z",
        "lastInstallHandledUtc": "2026-11-24T15:00:00Z"
    });
    fs::write(&state_path, serde_json::to_vec(&state_data).unwrap()).unwrap();

    let older_utc = "2026-11-24T14:00:00Z";
    let report = serde_json::json!({
        "version": "0.4.0",
        "exitCode": 6,
        "utc": older_utc
    });
    fs::write(
        updates_dir.join("last-install.json"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();

    let mgr1 = br_agent::host::UpdateManager::new_with_version(
        home.clone(),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        "0.3.0",
    );
    let st1 = mgr1.status(false);
    assert_eq!(st1.state, "error");
    assert!(
        st1.error
            .as_deref()
            .unwrap_or("")
            .contains("setup exit code 6")
    );

    let state_json1: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(state_json1["failedCount"], 2);
    assert_eq!(state_json1["lastInstallHandledUtc"], older_utc);

    // Second start with the same older utc must not handle it again.
    let mgr2 = br_agent::host::UpdateManager::new_with_version(
        home,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        "0.3.0",
    );
    assert_eq!(mgr2.status(false).state, "ready");
    let state_json2: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(state_json2["failedCount"], 2);
}

// C4: At start, a non-zero report whose version equals the running version does not set error
// or record a failure, and marks the report handled.
#[test]
fn c4_post_swap_failure_matching_running_version_is_ignored_and_handled() {
    use std::sync::atomic::AtomicBool;

    let dir = TempDir::new("upd-c4-post-swap");
    let home = dir.path().join("home");
    let updates_dir = DataPaths::updates_dir(&home);
    fs::create_dir_all(&updates_dir).unwrap();

    let state_path = updates_dir.join("state.json");
    let state_data = serde_json::json!({
        "downloaded": "0.3.0-dev"
    });
    fs::write(&state_path, serde_json::to_vec(&state_data).unwrap()).unwrap();

    let report_utc = "2026-11-24T15:20:00Z";
    let report = serde_json::json!({
        "version": "0.3.0-dev",
        "exitCode": 1,
        "utc": report_utc
    });
    fs::write(
        updates_dir.join("last-install.json"),
        serde_json::to_vec(&report).unwrap(),
    )
    .unwrap();

    let mgr = br_agent::host::UpdateManager::new_with_version(
        home,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        "0.3.0-dev",
    );
    let st = mgr.status(false);
    assert_ne!(st.state, "error");
    assert!(st.error.is_none());

    let state_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    assert!(state_json["failedVersion"].is_null());
    assert_eq!(state_json["failedCount"], 0);
    assert_eq!(state_json["lastInstallHandledUtc"], report_utc);
}

// C4: cleanup_on_start clears install-failure error state once running version >= failed version.
#[test]
fn c4_cleanup_on_start_clears_install_error_when_running_reaches_failed_version() {
    use std::sync::atomic::AtomicBool;

    let dir = TempDir::new("upd-c4-cleanup");
    let home = dir.path().join("home");
    let updates_dir = DataPaths::updates_dir(&home);
    fs::create_dir_all(&updates_dir).unwrap();

    let state_path = updates_dir.join("state.json");
    fs::write(
        &state_path,
        serde_json::to_vec(&serde_json::json!({ "downloaded": "0.3.0" })).unwrap(),
    )
    .unwrap();
    fs::write(
        updates_dir.join("last-install.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": "0.3.0",
            "exitCode": 5,
            "utc": "2026-11-24T15:25:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let mgr = br_agent::host::UpdateManager::new_with_version(
        home,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        "0.2.0",
    );
    assert_eq!(mgr.status(false).state, "error");
    assert!(mgr.status(false).error.is_some());

    mgr.set_test_config(Some(UpdateTestConfig {
        feed_url: "http://127.0.0.1:1/releases".to_string(),
        allowed_prefix: "http://127.0.0.1:1/releases/".to_string(),
        public_key_pem: None,
        current_version: Some("0.3.0".to_string()),
        fast_retries: true,
        launcher: None,
    }));
    mgr.cleanup_on_start();

    let st = mgr.status(false);
    assert_eq!(st.state, "idle");
    assert!(st.error.is_none());

    let state_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    assert!(state_json["failedVersion"].is_null());
    assert_eq!(state_json["failedCount"], 0);
}

// D3: check_at keeps an install-failure error while latest is still the failed version,
// and clears it when a newer release appears.
#[test]
fn d3_check_at_keeps_install_error_until_newer_version_appears() {
    let dir = TempDir::new("upd-d3-error");
    let home = dir.path().join("home");
    let updates_dir = DataPaths::updates_dir(&home);
    fs::create_dir_all(&updates_dir).unwrap();

    fs::write(
        updates_dir.join("state.json"),
        serde_json::to_vec(&serde_json::json!({ "downloaded": "0.3.0" })).unwrap(),
    )
    .unwrap();
    fs::write(
        updates_dir.join("last-install.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": "0.3.0",
            "exitCode": 5,
            "utc": "2026-11-24T15:30:00Z"
        }))
        .unwrap(),
    )
    .unwrap();

    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel_030 = make_signed_release(
        "0.3.0",
        "BetterRecall-Setup-0.3.0.exe",
        vec![0x42u8; 1024],
        &priv_pem,
    );
    let entry_030 = populate_release(&server, "v0.3.0", &rel_030, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![entry_030]).unwrap());

    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.2.0".to_string()),
        fast_retries: true,
        launcher: None,
    }));

    let before = host.update_status().unwrap();
    assert_eq!(before.state, "error");
    assert!(
        before
            .error
            .as_deref()
            .unwrap_or("")
            .contains("setup exit code 5")
    );

    // Check against the same failed version -> install error is kept.
    let after_same = host.update_check(&CancelToken::none()).unwrap();
    assert_eq!(after_same.state, "error");
    assert_eq!(after_same.error, before.error);

    // Newer version appears -> install error is cleared.
    let rel_040 = make_signed_release(
        "0.4.0",
        "BetterRecall-Setup-0.4.0.exe",
        vec![0x43u8; 1024],
        &priv_pem,
    );
    let entry_040 = populate_release(&server, "v0.4.0", &rel_040, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![entry_040]).unwrap());

    let after_newer = host.update_check(&CancelToken::none()).unwrap();
    assert_eq!(after_newer.state, "idle");
    assert!(after_newer.error.is_none());

    host.shutdown();
}

// D3: check_at does not overwrite `applying` state.
#[test]
fn d3_check_at_keeps_applying_state() {
    let dir = TempDir::new("upd-d3-applying");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel = make_signed_release(
        "0.3.0",
        "BetterRecall-Setup-0.3.0.exe",
        vec![0x42u8; 1024],
        &priv_pem,
    );
    let gh_rel = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![gh_rel]).unwrap());

    let (_launches, hook) = make_launcher();
    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.2.0".to_string()),
        fast_retries: true,
        launcher: Some(hook),
    }));

    host.update_check(&CancelToken::none()).unwrap();
    host.update_download(&mut |_| {}, &CancelToken::none())
        .unwrap();
    let applied = host.update_apply().unwrap();
    assert_eq!(applied.state, "applying");

    let after_check = host.update_check(&CancelToken::none()).unwrap();
    assert_eq!(after_check.state, "applying");
    assert!(after_check.error.is_none());

    host.shutdown();
}

// D3: Two concurrent apply calls with the launcher seam produce exactly one launch.
#[test]
fn d3_concurrent_apply_calls_launch_once() {
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = TempDir::new("upd-d3-concurrent");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = LocalHttpServer::start(HashMap::new());
    let rel = make_signed_release(
        "0.3.0",
        "BetterRecall-Setup-0.3.0.exe",
        vec![0x42u8; 1024],
        &priv_pem,
    );
    let gh_rel = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::signed());
    server.set_file("/releases", serde_json::to_vec(&vec![gh_rel]).unwrap());

    let launch_count = Arc::new(AtomicUsize::new(0));
    let launch_count_c = Arc::clone(&launch_count);
    let hook: br_agent::host::InstallerLauncher = Arc::new(move |_path, _args| {
        launch_count_c.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(150));
        Ok(None)
    });

    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.2.0".to_string()),
        fast_retries: true,
        launcher: Some(hook),
    }));

    host.update_check(&CancelToken::none()).unwrap();
    host.update_download(&mut |_| {}, &CancelToken::none())
        .unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let (r1, r2) = thread::scope(|s| {
        let b1 = Arc::clone(&barrier);
        let h1 = host.clone();
        let t1 = s.spawn(move || {
            b1.wait();
            h1.update_apply()
        });
        let b2 = Arc::clone(&barrier);
        let h2 = host.clone();
        let t2 = s.spawn(move || {
            b2.wait();
            h2.update_apply()
        });
        (t1.join().unwrap(), t2.join().unwrap())
    });

    let ok_count = usize::from(r1.is_ok()) + usize::from(r2.is_ok());
    let err_count = usize::from(matches!(r1, Err(AgentError::InvalidOperation(_))))
        + usize::from(matches!(r2, Err(AgentError::InvalidOperation(_))));
    assert_eq!(ok_count, 1, "expected exactly one Ok, got r1={r1:?} r2={r2:?}");
    assert_eq!(
        err_count, 1,
        "expected exactly one InvalidOperation, got r1={r1:?} r2={r2:?}"
    );
    assert_eq!(launch_count.load(Ordering::SeqCst), 1);

    host.shutdown();
}
