// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Integration tests for `AgentHost` `models_list` and `models_download` (stage I4)
//! against a local loopback HTTP server. No test ever contacts the internet.

mod common;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use br_contracts::{CoreHostOptions, DataPaths, ModelsProgress};
use br_core::{AgentControl, AgentError, CancelSource, CancelToken, CommandProcessor, CoreHost};
use br_fetch::{Catalog, CatalogFile, Component};
use sha2::{Digest, Sha256};

use br_agent::host::{AgentHost, AgentOptions};
use common::TempDir;

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

fn catalog_file(path: &str, data: &[u8]) -> CatalogFile {
    CatalogFile {
        path: path.to_string(),
        size: data.len() as u64,
        sha256: sha256_hex(data),
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

#[derive(Default)]
struct ServerConfig {
    files: HashMap<String, Vec<u8>>,
    requests: Vec<String>,
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
            requests: Vec::new(),
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

    fn requests(&self) -> Vec<String> {
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

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            break;
        }
    }

    let file_data = {
        let mut cfg = config.lock().unwrap();
        cfg.requests.push(path.clone());
        cfg.files.get(&path).cloned()
    };

    let Some(data) = file_data else {
        let _ = write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    };

    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        data.len()
    );
    let _ = stream.write_all(&data);
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

#[test]
fn models_list_reports_missing_partial_and_complete_on_test_home() {
    let dir = TempDir::new("models-list");
    let host = make_test_host(&dir);

    let m_a = br#"{"id":"ocr"}"#.to_vec();
    let w_b = b"partial-weights".to_vec();
    let m_b = br#"{"id":"ner"}"#.to_vec();
    let m_c = br#"{"id":"silero-vad"}"#.to_vec();

    let catalog = make_test_catalog(
        "http://127.0.0.1:1/",
        vec![
            Component {
                id: "ocr".to_string(),
                sets: vec!["full".to_string(), "minimal".to_string()],
                bytes: m_a.len() as u64,
                files: vec![catalog_file("manifest.json", &m_a)],
            },
            Component {
                id: "ner".to_string(),
                sets: vec!["full".to_string(), "minimal".to_string()],
                bytes: (w_b.len() + m_b.len()) as u64,
                files: vec![
                    catalog_file("weights.bin", &w_b),
                    catalog_file("manifest.json", &m_b),
                ],
            },
            Component {
                id: "silero-vad".to_string(),
                sets: vec!["full".to_string()],
                bytes: m_c.len() as u64,
                files: vec![catalog_file("manifest.json", &m_c)],
            },
        ],
    );
    host.set_catalog_for_test(Some(catalog));

    let models_dir = DataPaths::models_dir(&dir.path().join("home"), &Default::default());
    // Partial: weights without manifest.json.
    fs::create_dir_all(models_dir.join("ner")).unwrap();
    fs::write(models_dir.join("ner").join("weights.bin"), &w_b).unwrap();
    // Complete: valid manifest.json.
    fs::create_dir_all(models_dir.join("silero-vad")).unwrap();
    fs::write(models_dir.join("silero-vad").join("manifest.json"), &m_c).unwrap();

    let list = host.models_list().expect("models_list ok");
    assert_eq!(list.repo, "test/models");
    assert_eq!(list.revision, "0123456789abcdef0123456789abcdef01234567");
    assert!(!list.downloading);
    assert!(list.free_bytes > 0);
    assert_eq!(list.components.len(), 3);

    assert_eq!(list.components[0].id, "ocr");
    assert_eq!(list.components[0].title, "OCR");
    assert_eq!(list.components[0].state, "missing");

    assert_eq!(list.components[1].id, "ner");
    assert_eq!(list.components[1].title, "NER");
    assert_eq!(list.components[1].state, "partial");

    assert_eq!(list.components[2].id, "silero-vad");
    assert_eq!(list.components[2].title, "Silero VAD");
    assert_eq!(list.components[2].state, "complete");

    host.shutdown();
}

#[test]
fn models_download_writes_manifest_last_and_restarts_worker_only_when_fetched() {
    let dir = TempDir::new("models-dl");
    let host = make_test_host(&dir);

    let weights = vec![0xABu8; 4096];
    let manifest = br#"{"id":"silero-vad","version":1}"#.to_vec();

    let mut files = HashMap::new();
    files.insert("/silero-vad/silero_vad.onnx".to_string(), weights.clone());
    files.insert("/silero-vad/manifest.json".to_string(), manifest.clone());
    let server = LocalHttpServer::start(files);

    let catalog = make_test_catalog(
        &server.base_url(),
        vec![Component {
            id: "silero-vad".to_string(),
            sets: vec!["full".to_string()],
            bytes: (weights.len() + manifest.len()) as u64,
            files: vec![
                catalog_file("manifest.json", &manifest),
                catalog_file("silero_vad.onnx", &weights),
            ],
        }],
    );
    host.set_catalog_for_test(Some(catalog));

    let restarts = Arc::new(AtomicUsize::new(0));
    let restarts_cb = restarts.clone();
    host.set_on_worker_restart_for_test(Some(Arc::new(move |why| {
        assert_eq!(why, "models");
        restarts_cb.fetch_add(1, Ordering::AcqRel);
    })));

    // First download: fetches both files, manifest.json last, restarts worker once.
    let mut events: Vec<ModelsProgress> = Vec::new();
    host.models_download("silero-vad", &mut |p| events.push(p), &CancelToken::none())
        .expect("first download ok");

    assert_eq!(events.first().map(|e| e.phase.as_str()), Some("checking"));
    assert_eq!(events.last().map(|e| e.phase.as_str()), Some("done"));
    assert_eq!(restarts.load(Ordering::Acquire), 1);

    let reqs = server.requests();
    assert_eq!(
        reqs,
        vec![
            "/silero-vad/silero_vad.onnx".to_string(),
            "/silero-vad/manifest.json".to_string(),
        ]
    );

    let models_dir = DataPaths::models_dir(&dir.path().join("home"), &Default::default());
    assert_eq!(
        fs::read(models_dir.join("silero-vad").join("silero_vad.onnx")).unwrap(),
        weights
    );
    assert_eq!(
        fs::read(models_dir.join("silero-vad").join("manifest.json")).unwrap(),
        manifest
    );

    // Second download: everything already verified on disk -> 0 files fetched -> NO worker restart.
    let mut events2: Vec<ModelsProgress> = Vec::new();
    host.models_download("silero-vad", &mut |p| events2.push(p), &CancelToken::none())
        .expect("second download ok");
    assert_eq!(events2.last().map(|e| e.phase.as_str()), Some("done"));
    assert_eq!(restarts.load(Ordering::Acquire), 1);

    // Unknown target returns usage error.
    let err = host
        .models_download("nonexistent", &mut |_| {}, &CancelToken::none())
        .unwrap_err();
    match err {
        AgentError::InvalidOperation(msg) => assert_eq!(msg, CommandProcessor::USAGE_MODELS),
        other => panic!("expected InvalidOperation usage error, got {other:?}"),
    }

    // Pre-cancelled token emits cancelled phase and does not restart worker.
    let cts = CancelSource::new();
    cts.cancel();
    let mut events3: Vec<ModelsProgress> = Vec::new();
    host.models_download("silero-vad", &mut |p| events3.push(p), &cts.token())
        .expect("cancelled download returns ok after emitting cancelled");
    assert_eq!(events3.last().map(|e| e.phase.as_str()), Some("cancelled"));
    assert_eq!(restarts.load(Ordering::Acquire), 1);

    host.shutdown();
}
