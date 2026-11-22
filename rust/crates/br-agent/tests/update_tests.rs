// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Integration tests for signed auto-update (`br-agent::host::update`, stage I5)
//! against a local loopback HTTP server and throwaway P-256 keys.
//! No test ever contacts the internet or spawns an installer process.

mod common;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use br_contracts::{CoreHostOptions, DataPaths, UpdateProgress};
use br_core::{AgentControl, AgentError, CancelSource, CancelToken, CoreHost};
use br_integrity::{UPDATE_JSON, UPDATE_SIG, UpdateManifest, sign_manifest, sign_update_manifest};
use chrono::{TimeZone, Utc};
use p256::ecdsa::SigningKey;
use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rand_core::OsRng;
use sha2::{Digest, Sha256};

use br_agent::host::{AgentHost, AgentOptions, AutoStepOutcome, UpdateTestConfig};
use common::TempDir;

fn gen_keypair() -> (String, String) {
    let sk = SigningKey::random(&mut OsRng);
    let vk = sk.verifying_key();
    let priv_pem = sk.to_pkcs8_pem(LineEnding::LF).expect("priv pem").to_string();
    let pub_pem = vk.to_public_key_pem(LineEnding::LF).expect("pub pem");
    (priv_pem, pub_pem)
}

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

#[derive(Debug, Clone)]
struct RecordedReq {
    path: String,
    range: Option<String>,
}

#[derive(Default)]
struct ServerState {
    files: HashMap<String, Vec<u8>>,
    /// Optional slow-drip path: sends `first_chunk` bytes then sleeps `delay` before remaining bytes.
    slow_path: Option<(String, usize, Duration)>,
    requests: Vec<RecordedReq>,
}

struct FakeReleaseServer {
    addr: String,
    state: Arc<Mutex<ServerState>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FakeReleaseServer {
    fn start(files: HashMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let addr = format!("127.0.0.1:{port}");
        let state = Arc::new(Mutex::new(ServerState {
            files,
            slow_path: None,
            requests: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let st_c = state.clone();
        let stop_c = stop.clone();
        let handle = thread::spawn(move || {
            for incoming in listener.incoming() {
                if stop_c.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(stream) = incoming {
                    let st_req = st_c.clone();
                    thread::spawn(move || serve_one(stream, &st_req));
                }
            }
        });
        Self {
            addr,
            state,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    fn feed_url(&self) -> String {
        format!("http://{}/releases", self.addr)
    }

    fn release_prefix(&self) -> String {
        format!("http://{}/releases/", self.addr)
    }

    fn asset_url(&self, path: &str) -> String {
        let clean = path.trim_start_matches('/');
        format!("http://{}/{clean}", self.addr)
    }

    fn set_file(&self, path: &str, data: Vec<u8>) {
        self.state
            .lock()
            .unwrap()
            .files
            .insert(path.to_string(), data);
    }

    fn set_slow(&self, path: &str, first_chunk: usize, delay: Duration) {
        self.state.lock().unwrap().slow_path = Some((path.to_string(), first_chunk, delay));
    }

    fn clear_slow(&self) {
        self.state.lock().unwrap().slow_path = None;
    }

    fn requests(&self) -> Vec<RecordedReq> {
        self.state.lock().unwrap().requests.clone()
    }
}

impl Drop for FakeReleaseServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve_one(mut stream: TcpStream, state: &Arc<Mutex<ServerState>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut req_line = String::new();
    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
        return;
    }
    let path = req_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();

    let mut range: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':')
            && k.eq_ignore_ascii_case("range")
        {
            range = Some(v.trim().to_string());
        }
    }

    let (data_opt, slow) = {
        let mut st = state.lock().unwrap();
        st.requests.push(RecordedReq {
            path: path.clone(),
            range: range.clone(),
        });
        let slow = st
            .slow_path
            .as_ref()
            .filter(|(p, _, _)| p == &path)
            .map(|(_, chunk, delay)| (*chunk, *delay));
        (st.files.get(&path).cloned(), slow)
    };

    let Some(data) = data_opt else {
        let _ = write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    };

    let (status_line, slice, extra) = if let Some(ref r) = range
        && let Some(start_str) = r.strip_prefix("bytes=").and_then(|s| s.strip_suffix('-'))
        && let Ok(start) = start_str.parse::<usize>()
        && start < data.len()
    {
        let end = data.len() - 1;
        (
            "HTTP/1.1 206 Partial Content",
            &data[start..],
            format!("Content-Range: bytes {start}-{end}/{}\r\n", data.len()),
        )
    } else {
        ("HTTP/1.1 200 OK", data.as_slice(), String::new())
    };

    let _ = write!(
        stream,
        "{status_line}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        slice.len()
    );
    if let Some((first_chunk, delay)) = slow {
        let split = first_chunk.min(slice.len());
        let _ = stream.write_all(&slice[..split]);
        let _ = stream.flush();
        thread::sleep(delay);
        let _ = stream.write_all(&slice[split..]);
    } else {
        let _ = stream.write_all(slice);
    }
    let _ = stream.flush();
}

fn make_test_host(dir: &TempDir) -> Arc<AgentHost> {
    let home = dir.path().join("home");
    let install = dir.path().join("install");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&install).unwrap();
    let core = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: home.clone(),
            install_dir: install.clone(),
        },
        false,
    );
    AgentHost::new(
        core,
        AgentOptions {
            install_dir: install,
            home,
            autostart: false,
        },
    )
}

struct SignedReleaseFixture {
    installer_name: String,
    installer_bytes: Vec<u8>,
    manifest_bytes: Vec<u8>,
    sig_bytes: Vec<u8>,
}

fn make_signed_release(version: &str, installer_name: &str, payload: Vec<u8>, priv_pem: &str) -> SignedReleaseFixture {
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

struct ReleaseOpts {
    draft: bool,
    prerelease: bool,
    include_sig: bool,
    asset_size_override: Option<u64>,
    installer_url_override: Option<String>,
}

impl Default for ReleaseOpts {
    fn default() -> Self {
        Self {
            draft: false,
            prerelease: false,
            include_sig: true,
            asset_size_override: None,
            installer_url_override: None,
        }
    }
}

fn populate_release(
    server: &FakeReleaseServer,
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
    let server = FakeReleaseServer::start(HashMap::new());
    let rel = make_signed_release(
        "0.3.0",
        "BetterRecall-Setup-0.3.0.exe",
        vec![0x5Au8; 8192],
        &priv_pem,
    );
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::default());
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

    // Helper: run download & verify failed/rejected, no installer stored, no launch.
    let assert_rejected = |label: &str,
                           current_ver: &str,
                           build_feed: &dyn Fn(&FakeReleaseServer)| {
        let dir = TempDir::new(&format!("upd-rej-{label}"));
        let host = make_test_host(&dir);
        let server = FakeReleaseServer::start(HashMap::new());
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

        let st = host.update_check(&CancelToken::none()).expect("check returns status");
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

    // 2a. Missing update.sig -> candidate skipped -> no release.
    assert_rejected("missing-sig", "0.3.0-dev", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abc".to_vec(), &priv_pem);
        let entry = populate_release(
            server,
            "v0.3.0",
            &rel,
            ReleaseOpts {
                include_sig: false,
                ..Default::default()
            },
        );
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    // 2b. Flipped signature byte.
    assert_rejected("flipped-sig", "0.3.0-dev", &|server| {
        let mut rel = make_signed_release("0.3.0", "Setup.exe", b"abc".to_vec(), &priv_pem);
        rel.sig_bytes[0] ^= 0x01;
        let entry = populate_release(server, "v0.3.0", &rel, ReleaseOpts::default());
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    // 2c. Signed by another key.
    assert_rejected("wrong-key", "0.3.0-dev", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abc".to_vec(), &other_priv);
        let entry = populate_release(server, "v0.3.0", &rel, ReleaseOpts::default());
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    // 2d. Replay: equal version.
    assert_rejected("replay-equal", "0.3.0", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abc".to_vec(), &priv_pem);
        let entry = populate_release(server, "v0.3.0", &rel, ReleaseOpts::default());
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    // 2e. Replay: older version.
    assert_rejected("replay-older", "0.4.0", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abc".to_vec(), &priv_pem);
        let entry = populate_release(server, "v0.3.0", &rel, ReleaseOpts::default());
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    // 2f. Manifest version differs from tag.
    assert_rejected("tag-mismatch", "0.3.0-dev", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abc".to_vec(), &priv_pem);
        let entry = populate_release(server, "v0.3.1", &rel, ReleaseOpts::default());
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    // 2g. Installer asset size differs from manifest.
    assert_rejected("size-mismatch", "0.3.0-dev", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abcdef".to_vec(), &priv_pem);
        let entry = populate_release(
            server,
            "v0.3.0",
            &rel,
            ReleaseOpts {
                asset_size_override: Some(999),
                ..Default::default()
            },
        );
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    // 2h. Asset URL outside allowed prefix.
    assert_rejected("bad-url", "0.3.0-dev", &|server| {
        let rel = make_signed_release("0.3.0", "Setup.exe", b"abcdef".to_vec(), &priv_pem);
        let entry = populate_release(
            server,
            "v0.3.0",
            &rel,
            ReleaseOpts {
                installer_url_override: Some(format!("{}other/Setup.exe", server.base_url())),
                ..Default::default()
            },
        );
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });

    // 2i. Installer name with a path separator (manually crafted + signed JSON).
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
        let entry = populate_release(server, "v0.3.0", &rel, ReleaseOpts::default());
        server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    });
}

// 3. Installer bytes mismatch manifest SHA-256 -> failed, no launch, no completed installer file left.
#[test]
fn corrupted_installer_bytes_fail_and_leave_no_completed_file() {
    let dir = TempDir::new("upd-corrupt");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = FakeReleaseServer::start(HashMap::new());
    let rel = make_signed_release("0.3.0", "Setup.exe", b"good-installer-bytes".to_vec(), &priv_pem);
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::default());
    // Corrupt the served installer bytes (same length, different content).
    server.set_file("/releases/download/v0.3.0/Setup.exe", b"BAD!-installer-bytes".to_vec());
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
    let server = FakeReleaseServer::start(HashMap::new());
    let rel = make_signed_release("0.3.0", "Setup.exe", b"clean-installer-data".to_vec(), &priv_pem);
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::default());
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
    let server = FakeReleaseServer::start(HashMap::new());

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
                ..Default::default()
            },
        ),
        populate_release(
            &server,
            "v0.4.0-rc.1",
            &pre_rel,
            ReleaseOpts {
                prerelease: true,
                ..Default::default()
            },
        ),
        populate_release(&server, "v0.3.1", &stable_rel, ReleaseOpts::default()),
    ];
    server.set_file("/releases", serde_json::to_vec(&feed).unwrap());

    // Running stable 0.3.0 -> skips draft 0.5.0 and prerelease 0.4.0-rc.1, picks 0.3.1.
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

    // Running prerelease 0.3.0-dev -> skips draft 0.5.0, picks prerelease 0.4.0-rc.1.
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

// 6. Cancel mid-download keeps .part, and a second download resumes from it.
#[test]
fn cancel_mid_download_keeps_part_and_second_download_resumes() {
    let dir = TempDir::new("upd-resume");
    let host = make_test_host(&dir);
    let (priv_pem, pub_pem) = gen_keypair();
    let server = FakeReleaseServer::start(HashMap::new());
    let payload: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();
    let rel = make_signed_release("0.3.0", "Setup.exe", payload.clone(), &priv_pem);
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::default());
    server.set_file("/releases", serde_json::to_vec(&vec![entry]).unwrap());
    server.set_slow(
        "/releases/download/v0.3.0/Setup.exe",
        4096,
        Duration::from_millis(800),
    );

    host.set_update_test_config(Some(UpdateTestConfig {
        feed_url: server.feed_url(),
        allowed_prefix: server.release_prefix(),
        public_key_pem: Some(pub_pem),
        current_version: Some("0.3.0-dev".to_string()),
        fast_retries: true,
        launcher: None,
    }));

    let cts = CancelSource::new();
    let token = cts.token();
    let mut events1 = Vec::new();
    // Progress lines are throttled to one per 250 ms, so the first 4 KiB chunk may not be
    // reported before the server's 800 ms pause; cancel from a timer while it is paused.
    thread::scope(|s| {
        s.spawn(|| {
            thread::sleep(Duration::from_millis(300));
            cts.cancel();
        });
        host.update_download(&mut |p| events1.push(p), &token)
            .unwrap();
    });
    assert_eq!(events1.last().map(|e| e.phase.as_str()), Some("cancelled"));

    let ver_dir = DataPaths::updates_dir(&dir.path().join("home")).join("0.3.0");
    let part = ver_dir.join("Setup.exe.part");
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
            .any(|r| r.path == "/releases/download/v0.3.0/Setup.exe" && r.range.is_some()),
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
    let server = FakeReleaseServer::start(HashMap::new());
    let rel = make_signed_release("0.3.0", "Setup.exe", b"payload".to_vec(), &priv_pem);
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::default());
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
    let server = FakeReleaseServer::start(HashMap::new());
    let rel = make_signed_release("0.3.0", "Setup.exe", b"auto-installer".to_vec(), &priv_pem);
    let entry = populate_release(&server, "v0.3.0", &rel, ReleaseOpts::default());
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

    // auto=false -> zero requests, no launch.
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

    // auto=true, not idle (< 10 min) -> checks & downloads, does NOT launch.
    let out_active = host.updater().auto_step(now, 60_000, false, true, &CancelToken::none());
    assert_eq!(out_active, AutoStepOutcome::Downloaded);
    assert!(!server.requests().is_empty());
    assert!(launches.lock().unwrap().is_empty());

    // auto=true, idle >= 10 min, busy -> deferred, no launch.
    let out_busy = host.updater().auto_step(
        now + chrono::Duration::minutes(15),
        br_agent::host::update::AUTO_IDLE_THRESHOLD_MS,
        true,
        true,
        &CancelToken::none(),
    );
    assert_eq!(out_busy, AutoStepOutcome::Deferred);
    assert!(launches.lock().unwrap().is_empty());

    // auto=true, idle >= 10 min, not busy -> applies and launches once.
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
