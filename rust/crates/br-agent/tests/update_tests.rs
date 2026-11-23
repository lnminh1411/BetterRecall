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
        Ok(())
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
    host.set_catalog_for_test(Some(cat.with_base_url_for_test(&server.base_url())));

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
