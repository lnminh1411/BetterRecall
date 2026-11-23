// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Integration tests for `br-fetch` (stage I3) against a local loopback HTTP server.
//! No test ever contacts the internet.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use br_fetch::{
    Catalog, CatalogFile, Component, ComponentState, Downloader, FetchError, FetchProgress,
    InstallOptions, ModelSet, NoFetchProgress, install_components, installed_state,
    remove_components, remove_components_from_catalog,
};
use sha2::{Digest, Sha256};

static NEXT_DIR_ID: AtomicU64 = AtomicU64::new(1);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(label: &str) -> Self {
        let id = NEXT_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let base = std::env::var_os("BR_TEST_TMP")
            .filter(|v| !v.to_string_lossy().trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"K:\scratch\br-rust\test-tmp"));
        let path = base.join(format!("i3-fetch-{label}-{pid}-{id}"));
        fs::create_dir_all(&path).expect("create test dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
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
struct RecordedRequest {
    path: String,
    range: Option<String>,
    user_agent: Option<String>,
}

#[derive(Default)]
struct ServerConfig {
    files: HashMap<String, Vec<u8>>,
    redirects: HashMap<String, String>,
    ignore_range: bool,
    cut_after: Option<usize>,
    fail_500_remaining: usize,
    fail_503_remaining: usize,
    requests: Vec<RecordedRequest>,
}

struct LocalHttpServer {
    addr: String,
    config: Arc<Mutex<ServerConfig>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl LocalHttpServer {
    fn start(files: HashMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let addr = format!("127.0.0.1:{port}");
        let config = Arc::new(Mutex::new(ServerConfig {
            files,
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let cfg_clone = Arc::clone(&config);
        let stop_clone = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            for incoming in listener.incoming() {
                if stop_clone.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(stream) = incoming {
                    handle_conn(stream, &cfg_clone);
                }
            }
        });

        Self {
            addr,
            config,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    fn url(&self, path: &str) -> String {
        let clean = path.trim_start_matches('/');
        format!("http://{}/{clean}", self.addr)
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.config.lock().unwrap().requests.clone()
    }
}

impl Drop for LocalHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn handle_conn(mut stream: TcpStream, config: &Arc<Mutex<ServerConfig>>) {
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
    let mut user_agent: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            if k.eq_ignore_ascii_case("range") {
                range = Some(v.trim().to_string());
            } else if k.eq_ignore_ascii_case("user-agent") {
                user_agent = Some(v.trim().to_string());
            }
        }
    }

    let (redirect, fail_500, fail_503, file_data, ignore_range, cut_after) = {
        let mut cfg = config.lock().unwrap();
        cfg.requests.push(RecordedRequest {
            path: path.clone(),
            range: range.clone(),
            user_agent,
        });
        let fail_500 = if cfg.fail_500_remaining > 0 {
            cfg.fail_500_remaining -= 1;
            true
        } else {
            false
        };
        let fail_503 = if cfg.fail_503_remaining > 0 {
            cfg.fail_503_remaining -= 1;
            true
        } else {
            false
        };
        let redirect = cfg.redirects.get(&path).cloned();
        let file_data = cfg.files.get(&path).cloned();
        (
            redirect,
            fail_500,
            fail_503,
            file_data,
            cfg.ignore_range,
            cfg.cut_after,
        )
    };

    if fail_500 {
        let _ = write!(
            stream,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    }

    if fail_503 {
        let _ = write!(
            stream,
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    }

    if let Some(target) = redirect {
        let _ = write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    }

    let Some(data) = file_data else {
        let _ = write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    };

    let (status_line, slice, extra_header) = if !ignore_range
        && let Some(ref r) = range
        && let Some(start_str) = r.strip_prefix("bytes=").and_then(|s| s.strip_suffix('-'))
        && let Ok(start) = start_str.parse::<usize>()
    {
        if start >= data.len() {
            let _ = write!(
                stream,
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                data.len()
            );
            return;
        }
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
        "{status_line}\r\n{extra_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
        slice.len()
    );
    let to_send = match cut_after {
        Some(n) => &slice[..n.min(slice.len())],
        None => slice,
    };
    let _ = stream.write_all(to_send);
    let _ = stream.flush();
}

struct RecordingProgress {
    total: AtomicU64,
    sum: AtomicU64,
    files: Mutex<Vec<String>>,
}

impl RecordingProgress {
    fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            files: Mutex::new(Vec::new()),
        }
    }
}

impl FetchProgress for RecordingProgress {
    fn total(&self, bytes: u64) {
        self.total.store(bytes, Ordering::SeqCst);
    }
    fn bytes(&self, delta: u64) {
        self.sum.fetch_add(delta, Ordering::SeqCst);
    }
    fn file(&self, rel_path: &str) {
        self.files.lock().unwrap().push(rel_path.to_string());
    }
}

fn make_test_catalog(base_url: &str, components: Vec<Component>) -> Catalog {
    let rev = "0123456789abcdef0123456789abcdef01234567";
    let cat = Catalog {
        repo: "test/models".to_string(),
        revision: rev.to_string(),
        base_url: format!("https://huggingface.co/test/models/resolve/{rev}/"),
        components,
    };
    cat.validate().expect("test catalog must validate");
    cat.with_base_url_for_test(base_url)
}

fn catalog_file(path: &str, data: &[u8]) -> CatalogFile {
    CatalogFile {
        path: path.to_string(),
        size: data.len() as u64,
        sha256: sha256_hex(data),
    }
}

// 1. Embedded catalog parses and validates; minimal=5, full=8; bad catalogs rejected.
#[test]
fn catalog_embedded_validates_and_bad_catalogs_rejected() {
    let cat = Catalog::embedded().expect("embedded catalog must parse");
    assert_eq!(cat.components_for(ModelSet::Minimal).len(), 5);
    assert_eq!(cat.components_for(ModelSet::Full).len(), 8);

    let minimal_ids: Vec<&str> = cat
        .components_for(ModelSet::Minimal)
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(
        minimal_ids,
        vec![
            "embeddinggemma-2",
            "nsfw-classifier",
            "nsfw-detector",
            "ocr",
            "ner"
        ]
    );

    let full_ids: Vec<&str> = cat
        .components_for(ModelSet::Full)
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert!(full_ids.contains(&"siglip2-base"));
    assert!(full_ids.contains(&"whisper-large-v3-turbo"));
    assert!(full_ids.contains(&"silero-vad"));

    let rev = "893eb2c63a11443e9f5061745c69ecbd5e1bd16a";
    let good_hash = sha256_hex(b"{}");

    // Bad: relative `..` path.
    let bad_path = format!(
        r#"{{"repo":"a/b","revision":"{rev}","baseUrl":"https://huggingface.co/a/b/resolve/{rev}/","components":[{{"id":"c","sets":["full"],"bytes":2,"files":[{{"path":"../evil","size":2,"sha256":"{good_hash}"}},{{"path":"manifest.json","size":2,"sha256":"{good_hash}"}}]}}]}}"#
    );
    assert!(matches!(
        Catalog::from_json(&bad_path).expect_err(".. path rejected"),
        FetchError::Catalog(_)
    ));

    // Bad: http baseUrl.
    let bad_base = format!(
        r#"{{"repo":"a/b","revision":"{rev}","baseUrl":"http://huggingface.co/a/b/resolve/{rev}/","components":[{{"id":"c","sets":["full"],"bytes":2,"files":[{{"path":"manifest.json","size":2,"sha256":"{good_hash}"}}]}}]}}"#
    );
    assert!(matches!(
        Catalog::from_json(&bad_base).expect_err("http base rejected"),
        FetchError::Catalog(_)
    ));

    // Bad: short hash.
    let bad_hash = format!(
        r#"{{"repo":"a/b","revision":"{rev}","baseUrl":"https://huggingface.co/a/b/resolve/{rev}/","components":[{{"id":"c","sets":["full"],"bytes":2,"files":[{{"path":"manifest.json","size":2,"sha256":"deadbeef"}}]}}]}}"#
    );
    assert!(matches!(
        Catalog::from_json(&bad_hash).expect_err("short hash rejected"),
        FetchError::Catalog(_)
    ));

    // Bad: missing manifest.json.
    let no_manifest = format!(
        r#"{{"repo":"a/b","revision":"{rev}","baseUrl":"https://huggingface.co/a/b/resolve/{rev}/","components":[{{"id":"c","sets":["full"],"bytes":2,"files":[{{"path":"model.onnx","size":2,"sha256":"{good_hash}"}}]}}]}}"#
    );
    assert!(matches!(
        Catalog::from_json(&no_manifest).expect_err("missing manifest rejected"),
        FetchError::Catalog(_)
    ));
}

// 2. Download verifies and renames; wrong bytes give Verify and no file; wrong size the same.
#[test]
fn download_verifies_renames_and_rejects_bad_bytes_or_size() {
    let dir = TestDir::new("verify");
    let good_bytes = b"hello-betterrecall-model-weights".to_vec();
    let good_hash = sha256_hex(&good_bytes);
    let mut wrong_bytes = good_bytes.clone();
    wrong_bytes[0] ^= 0xFF;

    let mut files = HashMap::new();
    files.insert("/good.bin".to_string(), good_bytes.clone());
    files.insert("/wrong.bin".to_string(), wrong_bytes);
    files.insert("/short.bin".to_string(), b"short".to_vec());
    let server = LocalHttpServer::start(files);
    let dl = Downloader::new().with_retry_delays(vec![Duration::from_millis(10)]);

    // Good file verifies and renames, leaving no .part.
    let dest = dir.path().join("good.bin");
    dl.download_file(
        &server.url("/good.bin"),
        &dest,
        good_bytes.len() as u64,
        &good_hash,
        &NoFetchProgress,
        &|| false,
    )
    .expect("good download");
    assert_eq!(fs::read(&dest).unwrap(), good_bytes);
    assert!(!dir.path().join("good.bin.part").exists());

    // User-Agent is BetterRecall/<version>.
    let reqs = server.requests();
    assert_eq!(
        reqs[0].user_agent.as_deref(),
        Some(format!("BetterRecall/{}", env!("CARGO_PKG_VERSION")).as_str())
    );

    // Wrong bytes -> Verify, neither dest nor .part remains.
    let wrong_dest = dir.path().join("wrong.bin");
    let err = dl
        .download_file(
            &server.url("/wrong.bin"),
            &wrong_dest,
            good_bytes.len() as u64,
            &good_hash,
            &NoFetchProgress,
            &|| false,
        )
        .expect_err("wrong bytes must fail");
    assert!(matches!(err, FetchError::Verify { .. }));
    assert!(!wrong_dest.exists());
    assert!(!dir.path().join("wrong.bin.part").exists());

    // Wrong size -> Verify, neither dest nor .part remains.
    let short_dest = dir.path().join("short.bin");
    let err = dl
        .download_file(
            &server.url("/short.bin"),
            &short_dest,
            good_bytes.len() as u64,
            &good_hash,
            &NoFetchProgress,
            &|| false,
        )
        .expect_err("wrong size must fail");
    assert!(matches!(err, FetchError::Verify { .. }));
    assert!(!short_dest.exists());
    assert!(!dir.path().join("short.bin.part").exists());
}

// 3. Resume: cut after N bytes, second call sends Range and completes;
//    server that ignores Range restarts from zero and still verifies.
#[test]
fn resume_with_range_and_fallback_when_range_ignored() {
    let dir = TestDir::new("resume");
    let payload: Vec<u8> = (0..16_384).map(|i| (i % 251) as u8).collect();
    let hash = sha256_hex(&payload);

    let mut files = HashMap::new();
    files.insert("/model.bin".to_string(), payload.clone());
    let server = LocalHttpServer::start(files);
    server.config.lock().unwrap().cut_after = Some(5_000);

    // First call with 0 retries cuts after 5000 bytes and leaves .part.
    let no_retry = Downloader::new().with_retry_delays(vec![]);
    let dest = dir.path().join("model.bin");
    let err = no_retry
        .download_file(
            &server.url("/model.bin"),
            &dest,
            payload.len() as u64,
            &hash,
            &NoFetchProgress,
            &|| false,
        )
        .expect_err("cut transfer must fail");
    assert!(matches!(err, FetchError::Io { .. }));
    assert!(!dest.exists());
    assert_eq!(
        fs::metadata(dir.path().join("model.bin.part")).unwrap().len(),
        5_000
    );

    // Remove cut; second call resumes with Range: bytes=5000- and completes.
    server.config.lock().unwrap().cut_after = None;
    no_retry
        .download_file(
            &server.url("/model.bin"),
            &dest,
            payload.len() as u64,
            &hash,
            &NoFetchProgress,
            &|| false,
        )
        .expect("resume must succeed");
    assert_eq!(fs::read(&dest).unwrap(), payload);
    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[1].range.as_deref(), Some("bytes=5000-"));

    // Server ignores Range (answers 200) -> restarts from zero and still verifies.
    let dest2 = dir.path().join("model2.bin");
    fs::write(dir.path().join("model2.bin.part"), &payload[..4_000]).unwrap();
    server.config.lock().unwrap().ignore_range = true;
    no_retry
        .download_file(
            &server.url("/model.bin"),
            &dest2,
            payload.len() as u64,
            &hash,
            &NoFetchProgress,
            &|| false,
        )
        .expect("200 fallback must succeed");
    assert_eq!(fs::read(&dest2).unwrap(), payload);
    assert!(!dir.path().join("model2.bin.part").exists());
}

// 4. 500 once then success (retry); 404 fails at once.
#[test]
fn retry_on_500_and_fail_immediately_on_404() {
    let dir = TestDir::new("retry");
    let payload = b"retry-payload-ok".to_vec();
    let hash = sha256_hex(&payload);

    let mut files = HashMap::new();
    files.insert("/ok.bin".to_string(), payload.clone());
    let server = LocalHttpServer::start(files);
    server.config.lock().unwrap().fail_500_remaining = 1;

    let dl = Downloader::new().with_retry_delays(vec![
        Duration::from_millis(10),
        Duration::from_millis(10),
        Duration::from_millis(10),
    ]);

    let dest = dir.path().join("ok.bin");
    dl.download_file(
        &server.url("/ok.bin"),
        &dest,
        payload.len() as u64,
        &hash,
        &NoFetchProgress,
        &|| false,
    )
    .expect("500 once then 200 must succeed");
    assert_eq!(fs::read(&dest).unwrap(), payload);
    assert_eq!(server.requests().len(), 2);

    // 404 fails at once (1 request, no retries).
    let missing_dest = dir.path().join("missing.bin");
    let err = dl
        .download_file(
            &server.url("/does-not-exist"),
            &missing_dest,
            10,
            &hash,
            &NoFetchProgress,
            &|| false,
        )
        .expect_err("404 must fail");
    assert!(matches!(err, FetchError::Http { status: 404, .. }));
    assert_eq!(server.requests().len(), 3);
}

// 5. Redirect to another path followed; redirect to non-loopback http:// refused.
#[test]
fn redirect_followed_and_non_loopback_http_redirect_refused() {
    let dir = TestDir::new("redirect");
    let payload = b"redirected-bytes".to_vec();
    let hash = sha256_hex(&payload);

    let mut files = HashMap::new();
    files.insert("/cdn/file.bin".to_string(), payload.clone());
    let server = LocalHttpServer::start(files);
    {
        let mut cfg = server.config.lock().unwrap();
        cfg.redirects
            .insert("/resolve/file.bin".to_string(), "/cdn/file.bin".to_string());
        cfg.redirects.insert(
            "/evil/file.bin".to_string(),
            "http://example.com/file.bin".to_string(),
        );
    }

    let dl = Downloader::new().with_retry_delays(vec![]);
    let dest = dir.path().join("file.bin");
    dl.download_file(
        &server.url("/resolve/file.bin"),
        &dest,
        payload.len() as u64,
        &hash,
        &NoFetchProgress,
        &|| false,
    )
    .expect("redirect to local path must succeed");
    assert_eq!(fs::read(&dest).unwrap(), payload);

    let bad_dest = dir.path().join("evil.bin");
    let err = dl
        .download_file(
            &server.url("/evil/file.bin"),
            &bad_dest,
            payload.len() as u64,
            &hash,
            &NoFetchProgress,
            &|| false,
        )
        .expect_err("redirect to non-loopback http:// must be refused");
    assert!(matches!(err, FetchError::Http { status: 302, .. }));
    assert!(!bad_dest.exists());
}

// 6. Cancel mid-file leaves .part and returns Cancelled; next call resumes.
#[test]
fn cancel_mid_file_leaves_part_and_next_call_resumes() {
    struct CancelAfterFirstChunk {
        cancel: Arc<AtomicBool>,
    }
    impl FetchProgress for CancelAfterFirstChunk {
        fn total(&self, _bytes: u64) {}
        fn bytes(&self, delta: u64) {
            if delta > 0 {
                self.cancel.store(true, Ordering::SeqCst);
            }
        }
    }

    let dir = TestDir::new("cancel");
    // 2.5 MiB so reading with a 1 MiB buffer takes multiple iterations.
    let payload: Vec<u8> = (0..2_500_000).map(|i| (i % 251) as u8).collect();
    let hash = sha256_hex(&payload);

    let mut files = HashMap::new();
    files.insert("/big.bin".to_string(), payload.clone());
    let server = LocalHttpServer::start(files);
    let dl = Downloader::new().with_retry_delays(vec![]);

    let cancel_flag = Arc::new(AtomicBool::new(false));
    let prog = CancelAfterFirstChunk {
        cancel: Arc::clone(&cancel_flag),
    };
    let dest = dir.path().join("big.bin");
    let err = dl
        .download_file(
            &server.url("/big.bin"),
            &dest,
            payload.len() as u64,
            &hash,
            &prog,
            &|| cancel_flag.load(Ordering::SeqCst),
        )
        .expect_err("must cancel mid-file");
    assert!(matches!(err, FetchError::Cancelled));
    assert!(!dest.exists());
    let part_len = fs::metadata(dir.path().join("big.bin.part")).unwrap().len();
    assert!(part_len > 0 && part_len < payload.len() as u64);

    // Next call resumes from .part and completes.
    dl.download_file(
        &server.url("/big.bin"),
        &dest,
        payload.len() as u64,
        &hash,
        &NoFetchProgress,
        &|| false,
    )
    .expect("resume after cancel must succeed");
    assert_eq!(fs::read(&dest).unwrap(), payload);
    assert!(!dir.path().join("big.bin.part").exists());
    let reqs = server.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(
        reqs[1].range.as_deref(),
        Some(format!("bytes={part_len}-").as_str())
    );
}

// 7. Connection refused (closed port) gives Offline.
#[test]
fn connection_refused_gives_offline() {
    let dir = TestDir::new("offline");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind temp port");
    let closed_port = listener.local_addr().unwrap().port();
    drop(listener);

    let dl = Downloader::new().with_retry_delays(vec![Duration::from_millis(10)]);
    let err = dl
        .download_file(
            &format!("http://127.0.0.1:{closed_port}/model.bin"),
            &dir.path().join("model.bin"),
            10,
            &sha256_hex(b"0123456789"),
            &NoFetchProgress,
            &|| false,
        )
        .expect_err("closed port must fail");
    assert!(matches!(err, FetchError::Offline));
}

// 8. install_components: existing good files skipped, manifest.json written last,
//    not enough space rejected, progress total equals bytes actually fetched.
#[test]
fn install_components_skips_good_writes_manifest_last_checks_space_and_progress() {
    let dir = TestDir::new("install-components");
    let models_dir = dir.path().join("models");

    let existing_bytes = b"already-present-weights".to_vec();
    let new_bytes = b"fresh-weights-to-fetch-1234567890".to_vec();
    let manifest_bytes = br#"{"id":"comp-a","version":1}"#.to_vec();

    // Put manifest.json FIRST in catalog to prove install_components still fetches it LAST.
    let comp = Component {
        id: "comp-a".to_string(),
        sets: vec!["full".to_string(), "minimal".to_string()],
        bytes: (existing_bytes.len() + new_bytes.len() + manifest_bytes.len()) as u64,
        files: vec![
            catalog_file("manifest.json", &manifest_bytes),
            catalog_file("existing.bin", &existing_bytes),
            catalog_file("new.bin", &new_bytes),
        ],
    };

    let mut files = HashMap::new();
    files.insert("/comp-a/existing.bin".to_string(), existing_bytes.clone());
    files.insert("/comp-a/new.bin".to_string(), new_bytes.clone());
    files.insert("/comp-a/manifest.json".to_string(), manifest_bytes.clone());
    let server = LocalHttpServer::start(files);
    let catalog = make_test_catalog(&server.base_url(), vec![comp]);

    // Pre-populate existing.bin with valid bytes.
    fs::create_dir_all(models_dir.join("comp-a")).unwrap();
    fs::write(models_dir.join("comp-a").join("existing.bin"), &existing_bytes).unwrap();

    // Not enough space with injected value -> rejected before any HTTP request.
    let comps = catalog.components_for(ModelSet::Minimal);
    let low_space_opts = InstallOptions::for_catalog(&catalog)
        .with_downloader(Downloader::new().with_retry_delays(vec![]))
        .with_free_space(|_| Ok(10));
    let err = install_components(&models_dir, &comps, &low_space_opts, &NoFetchProgress, &|| false)
        .expect_err("low space must fail");
    assert!(matches!(err, FetchError::NotEnoughSpace { .. }));
    assert!(server.requests().is_empty());

    // Sufficient space -> skips existing.bin, fetches new.bin then manifest.json last.
    let progress = RecordingProgress::new();
    let ok_opts = InstallOptions::for_catalog(&catalog)
        .with_downloader(Downloader::new().with_retry_delays(vec![]))
        .with_free_space(|_| Ok(1_000_000));
    let report = install_components(&models_dir, &comps, &ok_opts, &progress, &|| false)
        .expect("install_components");

    let expected_fetched = (new_bytes.len() + manifest_bytes.len()) as u64;
    assert_eq!(report.skipped_files, 1);
    assert_eq!(report.downloaded_files, 2);
    assert_eq!(report.bytes_downloaded, expected_fetched);
    assert_eq!(progress.total.load(Ordering::SeqCst), expected_fetched);
    assert_eq!(progress.sum.load(Ordering::SeqCst), expected_fetched);

    let req_paths: Vec<String> = server.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(req_paths, vec!["/comp-a/new.bin", "/comp-a/manifest.json"]);
    assert_eq!(
        *progress.files.lock().unwrap(),
        vec![
            "comp-a/new.bin".to_string(),
            "comp-a/manifest.json".to_string(),
        ]
    );
}

// 9. remove_components refuses an id outside the catalog and only deletes catalog folders.
#[test]
fn remove_components_refuses_unknown_id_and_deletes_only_catalog_folders() {
    let dir = TestDir::new("remove");
    let models_dir = dir.path().join("models");
    fs::create_dir_all(models_dir.join("silero-vad")).unwrap();
    fs::write(models_dir.join("silero-vad").join("file.onnx"), b"x").unwrap();
    fs::create_dir_all(models_dir.join("user-custom")).unwrap();
    fs::write(models_dir.join("user-custom").join("keep.txt"), b"keep").unwrap();

    // Unknown id refused; nothing deleted.
    let err = remove_components(&models_dir, &["silero-vad", "user-custom"])
        .expect_err("unknown id must be refused");
    assert!(matches!(err, FetchError::Catalog(_)));
    assert!(models_dir.join("silero-vad").exists());
    assert!(models_dir.join("user-custom").exists());

    // Traversal id refused.
    let err = remove_components(&models_dir, &["../models"])
        .expect_err("traversal id must be refused");
    assert!(matches!(err, FetchError::Catalog(_)));

    // Valid catalog id removed; user-custom untouched.
    remove_components(&models_dir, &["silero-vad"]).expect("remove silero-vad");
    assert!(!models_dir.join("silero-vad").exists());
    assert!(models_dir.join("user-custom").join("keep.txt").exists());

    // Custom catalog variant also enforces catalog membership.
    let m = br#"{"ok":true}"#.to_vec();
    let custom_cat = make_test_catalog(
        "http://127.0.0.1:1/",
        vec![Component {
            id: "alpha".to_string(),
            sets: vec!["full".to_string()],
            bytes: m.len() as u64,
            files: vec![catalog_file("manifest.json", &m)],
        }],
    );
    let err = remove_components_from_catalog(&models_dir, &custom_cat, &["user-custom"])
        .expect_err("id not in custom catalog must fail");
    assert!(matches!(err, FetchError::Catalog(_)));
}

// 10. installed_state: Missing, Partial (files without manifest), Complete.
#[test]
fn installed_state_reports_missing_partial_and_complete() {
    let dir = TestDir::new("state");
    let models_dir = dir.path().join("models");

    let m_a = br#"{"id":"a"}"#.to_vec();
    let w_a = b"weights-a".to_vec();
    let m_b = br#"{"id":"b"}"#.to_vec();
    let m_c = br#"{"id":"c"}"#.to_vec();

    let catalog = make_test_catalog(
        "http://127.0.0.1:1/",
        vec![
            Component {
                id: "comp-missing".to_string(),
                sets: vec!["full".to_string()],
                bytes: m_a.len() as u64,
                files: vec![catalog_file("manifest.json", &m_a)],
            },
            Component {
                id: "comp-partial".to_string(),
                sets: vec!["full".to_string()],
                bytes: (m_b.len() + w_a.len()) as u64,
                files: vec![
                    catalog_file("weights.bin", &w_a),
                    catalog_file("manifest.json", &m_b),
                ],
            },
            Component {
                id: "comp-complete".to_string(),
                sets: vec!["full".to_string()],
                bytes: m_c.len() as u64,
                files: vec![catalog_file("manifest.json", &m_c)],
            },
        ],
    );

    // Partial: has weights.bin (or .part) but no manifest.json.
    fs::create_dir_all(models_dir.join("comp-partial")).unwrap();
    fs::write(models_dir.join("comp-partial").join("weights.bin"), &w_a).unwrap();

    // Complete: has verified manifest.json.
    fs::create_dir_all(models_dir.join("comp-complete")).unwrap();
    fs::write(models_dir.join("comp-complete").join("manifest.json"), &m_c).unwrap();

    let states = installed_state(&models_dir, &catalog);
    assert_eq!(
        states,
        vec![
            ("comp-missing".to_string(), ComponentState::Missing),
            ("comp-partial".to_string(), ComponentState::Partial),
            ("comp-complete".to_string(), ComponentState::Complete),
        ]
    );
}

// 11. get_bytes: bounded size cap enforced, redirect followed, custom headers sent, non-allowed URL refused.
#[test]
fn get_bytes_enforces_cap_follows_redirect_and_refuses_disallowed_urls() {
    let mut files = HashMap::new();
    files.insert("/small.json".to_string(), b"{\"ok\":true}".to_vec());
    files.insert("/exact.bin".to_string(), vec![0x42u8; 16]);
    files.insert("/too-big.bin".to_string(), vec![0x99u8; 17]);
    let server = LocalHttpServer::start(files);
    server
        .config
        .lock()
        .unwrap()
        .redirects
        .insert("/redir".to_string(), "/small.json".to_string());
    server
        .config
        .lock()
        .unwrap()
        .redirects
        .insert("/bad-redir".to_string(), "http://example.com/evil".to_string());

    let dl = Downloader::new().with_retry_delays(vec![]);

    // Exact cap passes and custom User-Agent is forwarded.
    let exact = dl
        .get_bytes(
            &server.url("/exact.bin"),
            16,
            &[("User-Agent", "BetterRecall-Updater/0.3.0-dev")],
        )
        .expect("exact cap succeeds");
    assert_eq!(exact, vec![0x42u8; 16]);
    assert_eq!(
        server.requests().last().and_then(|r| r.user_agent.as_deref()),
        Some("BetterRecall-Updater/0.3.0-dev")
    );

    // Over cap fails with Io error.
    let err = dl
        .get_bytes(&server.url("/too-big.bin"), 16, &[])
        .expect_err("over cap must fail");
    assert!(matches!(err, FetchError::Io { .. }), "got {err:?}");

    // Redirect to allowed loopback URL is followed.
    let redir = dl
        .get_bytes(&server.url("/redir"), 1024, &[])
        .expect("allowed redirect succeeds");
    assert_eq!(redir, b"{\"ok\":true}");

    // Redirect to disallowed plain-HTTP non-loopback host is refused.
    let err = dl
        .get_bytes(&server.url("/bad-redir"), 1024, &[])
        .expect_err("disallowed redirect refused");
    // Same as `download_file`: the refused hop reports the redirect status and its target.
    assert!(
        matches!(&err, FetchError::Http { status: 302, url } if url == "http://example.com/evil"),
        "got {err:?}"
    );

    // Disallowed initial URL is refused without network.
    let err = dl
        .get_bytes("http://example.com/releases", 1024, &[])
        .expect_err("disallowed initial url refused");
    assert!(matches!(err, FetchError::Http { status: 0, .. }), "got {err:?}");
}

// 12. C2: Holding .part open for writing causes another download to fail with InUse, leaving .part untouched.
#[test]
fn hold_part_open_for_writing_returns_in_use_and_preserves_part() {
    let payload = vec![0xABu8; 1024];
    let payload_sha = sha256_hex(&payload);

    let mut files = HashMap::new();
    files.insert("/model.bin".to_string(), payload);
    let server = LocalHttpServer::start(files);

    let dir = TestDir::new("in-use");
    let dest = dir.path().join("model.bin");
    let part = dir.path().join("model.bin.part");

    // Pre-create .part with initial contents
    let initial_part_data = b"untouched part bytes";
    fs::write(&part, initial_part_data).expect("write initial part");

    // Hold .part open for writing
    let _holder = std::fs::OpenOptions::new()
        .write(true)
        .open(&part)
        .expect("open part for writing");

    let dl = Downloader::new().with_retry_delays(vec![]);
    let res = dl.download_file(
        &server.url("/model.bin"),
        &dest,
        1024,
        &payload_sha,
        &NoFetchProgress,
        &|| false,
    );

    match res {
        Err(FetchError::InUse) => {}
        other => panic!("expected FetchError::InUse, got {other:?}"),
    }

    // Assert .part is untouched
    let part_data = fs::read(&part).expect("read part");
    assert_eq!(part_data, initial_part_data);
    assert!(!dest.exists());
}

// 13. C4: Cancel during retry sleep aborts immediately without waiting for the retry delay.
#[test]
fn get_bytes_cancellable_aborts_on_cancel_during_retry_sleep() {
    let mut files = HashMap::new();
    files.insert("/retry.json".to_string(), b"{\"ok\":true}".to_vec());
    let server = LocalHttpServer::start(files);
    server.config.lock().unwrap().fail_503_remaining = 5;

    // Set a 10-second retry delay so a non-cancellable sleep would block for 10 s
    let dl = Downloader::new().with_retry_delays(vec![Duration::from_secs(10)]);

    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_c = Arc::clone(&cancelled);

    // Cancel after 200 ms
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        cancelled_c.store(true, Ordering::SeqCst);
    });

    let cancel_fn = move || cancelled.load(Ordering::SeqCst);

    let start = std::time::Instant::now();
    let res = dl.get_bytes_cancellable(&server.url("/retry.json"), 1024, &[], &cancel_fn);
    let elapsed = start.elapsed();

    assert!(matches!(res, Err(FetchError::Cancelled)), "got {res:?}");
    // Must return well under the 10 s retry delay / 60 s timeout
    assert!(
        elapsed < Duration::from_secs(3),
        "cancellation took too long: {elapsed:?}"
    );
}
