// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use br_contracts::{
    AuditEntry, CaptureKind, DataPaths, EmbeddingInfo, Envelope, NewReplayFrame, PurgeScope,
    SearchMode, SearchRequest, Settings,
};
use br_core::{Connection, CoreJson, EncryptedStore, KeyVault, SettingsStore, StoreError};
use chrono::{Duration as ChronoDuration, Utc};

use common::{
    FAKE_WEBP, TempHome, TempStore, add_ready, audio, find, hex_upper, raw_bytes, screen, vec_info,
    vec_near, vec_seeded,
};

#[test]
fn files_are_encrypted_not_plaintext_sqlite() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    add_ready(t.store(), t0, "Acme invoice 4471 - Gmail", &vec_seeded(1), "chrome.exe", true);
    t.store().checkpoint().expect("checkpoint");

    let db = raw_bytes(&DataPaths::store_db(t.home.path()));
    assert!(!db.starts_with(b"SQLite format 3"));
    assert!(!String::from_utf8_lossy(&db).contains("Acme invoice"));

    let kf_files: Vec<_> = fs::read_dir(DataPaths::keyframes_dir(t.home.path()))
        .expect("read kf dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("db")))
        .collect();
    assert_eq!(kf_files.len(), 1);
    let kf_bytes = raw_bytes(&kf_files[0]);
    assert!(!kf_bytes.starts_with(b"SQLite format 3"));
    assert!(!String::from_utf8_lossy(&kf_bytes).contains("MARKER-PIXELS"));
}

#[test]
fn wrong_key_cannot_open() {
    let mut t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    add_ready(t.store(), t0, "x", &vec_seeded(1), "chrome.exe", true);
    t.close();

    let err = EncryptedStore::open(t.home.path(), || vec![0u8; 32], None)
        .err()
        .expect("expected error");
    assert!(err.is_not_a_db(), "expected SQLITE_NOTADB (26), got {err:?}");
    t.reopen();
}

#[test]
fn spike_fts5_and_vector_blobs_under_sqlite3mc_survive_reopen() {
    let mut t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let v = vec_seeded(42);
    let id = add_ready(t.store(), t0, "Quarterly report draft", &v, "chrome.exe", true);
    t.reopen();

    let text = find(t.store(), "quarterly", &[], SearchMode::Text, 50, 1 << 20);
    assert_eq!(text.results.len(), 1);
    assert_eq!(text.results[0].id, id);

    let vec = find(t.store(), "anything", &v, SearchMode::Vector, 50, 1 << 20);
    assert_eq!(vec.results[0].id, id);
    assert_eq!(t.store().index().count(), 1);
    assert_eq!(t.store().stats().expect("stats").vectors, 1);
}

#[test]
fn pending_rows_are_invisible_until_embedded() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let id = t
        .store()
        .add_screen_capture(&screen(t0, "chrome.exe", "Secret pending title", true))
        .expect("add");
    assert!(find(t.store(), "secret", &[], SearchMode::Hybrid, 50, 1 << 20).results.is_empty());
    assert!(t.store().get(&id).expect("get").is_none());
    assert!(t.store().get_keyframe(&id).expect("kf").is_none());
    assert!(
        t.store()
            .timeline(t0 - ChronoDuration::days(1), t0 + ChronoDuration::days(1), 10)
            .expect("timeline")
            .is_empty()
    );
    assert_eq!(t.store().stats().expect("stats").pending_embeddings, 1);

    t.store()
        .set_embedding(&id, &vec_seeded(3), &vec_info())
        .expect("embed");
    assert_eq!(find(t.store(), "secret", &[], SearchMode::Hybrid, 50, 1 << 20).results.len(), 1);
    assert!(t.store().get(&id).expect("get").is_some());
    let kf = t.store().get_keyframe(&id).expect("kf").expect("some kf");
    assert_eq!(kf.bytes, FAKE_WEBP);
    assert_eq!(kf.mime, "image/webp");
    assert_eq!(
        t.store()
            .timeline(t0 - ChronoDuration::days(1), t0 + ChronoDuration::days(1), 10)
            .expect("timeline")
            .len(),
        1
    );
}

#[test]
fn take_does_not_hand_out_the_same_job_twice() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    for i in 0..5 {
        t.store()
            .add_screen_capture(&screen(t0 + ChronoDuration::seconds(i), "chrome.exe", "Some window", true))
            .expect("add");
    }
    let a = t.store().take_pending_embeddings(3).expect("take a");
    let b = t.store().take_pending_embeddings(3).expect("take b");
    assert_eq!(a.len(), 3);
    assert_eq!(b.len(), 2);
    for ja in &a {
        assert!(!b.iter().any(|jb| jb.id == ja.id));
    }
}

#[test]
fn nsfw_drop_leaves_no_trace() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let mut cap = screen(t0, "chrome.exe", "Dropped frame title", true);
    cap.image = b"DROPPED-IMAGE-BYTES-xxxxxxxx".to_vec();
    let id = t.store().add_screen_capture(&cap).expect("add");
    t.store().take_pending_embeddings(5).expect("take");
    t.store().drop_capture(&id, "nsfw").expect("drop");
    t.store().checkpoint().expect("cp");

    assert!(t.store().get(&id).expect("get").is_none());
    assert!(t.store().get_keyframe(&id).expect("kf").is_none());
    assert!(find(t.store(), "dropped", &[], SearchMode::Hybrid, 50, 1 << 20).results.is_empty());
    assert!(t.store().take_pending_embeddings(5).expect("take").is_empty());
    let st = t.store().stats().expect("stats");
    assert_eq!(st.rows, 0);
    assert_eq!(st.pending_embeddings, 0);
    assert_eq!(st.vectors, 0);

    // A late SetEmbedding for the dropped id must not resurrect it.
    t.store()
        .set_embedding(&id, &vec_seeded(1), &vec_info())
        .expect("late embed");
    assert!(t.store().get(&id).expect("get").is_none());
    assert_eq!(t.store().index().count(), 0);
    assert_eq!(
        t.store()
            .count_purge(&PurgeScope {
                kind: "all".to_string(),
                from: None,
                to: None,
                app: None,
                site: None,
                id: None,
            })
            .expect("count"),
        0
    );
}

#[test]
fn failed_embedding_stays_invisible() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let id = t
        .store()
        .add_screen_capture(&screen(t0, "chrome.exe", "Failure case", true))
        .expect("add");
    t.store().fail_embedding(&id, "decode").expect("fail");
    assert!(t.store().get(&id).expect("get").is_none());
    assert!(t.store().take_pending_embeddings(5).expect("take").is_empty());
}

#[test]
fn keep_keyframe_false_deletes_image_after_embedding() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let id = add_ready(t.store(), t0, "No keyframe kept", &vec_seeded(9), "chrome.exe", false);
    assert!(t.store().get_keyframe(&id).expect("kf").is_none());
    assert!(!t.store().get(&id).expect("get").expect("some").has_thumb);
}

#[test]
fn audio_is_text_searchable_at_once_and_gets_a_job() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let text = format!(
        "{}the Acme budget meeting moved to Friday {}",
        "filler words here ".repeat(40),
        "more filler ".repeat(40)
    );
    let id = t
        .store()
        .add_audio_capture(&audio(t0, &text, "teams.exe"))
        .expect("add audio");
    let resp = find(t.store(), "budget", &[], SearchMode::Text, 50, 1 << 20);
    assert_eq!(resp.results.len(), 1);
    let r = &resp.results[0];
    assert_eq!(r.id, id);
    assert_eq!(r.kind, CaptureKind::Audio);
    assert_eq!(r.duration_s, Some(4));
    assert!(r.snippet.chars().count() <= 300);
    assert!(r.snippet.contains("budget"));

    let jobs = t.store().take_pending_embeddings(5).expect("take");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, CaptureKind::Audio);
    assert_eq!(jobs[0].text.as_deref(), Some(text.as_str()));
    assert_eq!(t.store().get(&id).expect("get").expect("some").text, text);
}

#[test]
fn screen_ocr_text_is_searchable_snippeted_and_returned() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let ocr = format!(
        "{}Quarterly zebra forecast for Northwind {}",
        "lorem ipsum ".repeat(40),
        "dolor sit ".repeat(40)
    );
    let mut cap = screen(t0, "chrome.exe", "Report - Excel", true);
    cap.text = Some(ocr.clone());
    let id = t.store().add_screen_capture(&cap).expect("add");
    let mut plain_cap = screen(t0 + ChronoDuration::seconds(5), "chrome.exe", "Zebra notes", true);
    plain_cap.text = Some("  ".to_string());
    let plain = t.store().add_screen_capture(&plain_cap).expect("add plain");

    assert!(find(t.store(), "northwind", &[], SearchMode::Text, 50, 1 << 20).results.is_empty());
    t.store().set_embedding(&id, &vec_seeded(4), &vec_info()).expect("embed");
    t.store().set_embedding(&plain, &vec_seeded(5), &vec_info()).expect("embed");

    let hits = find(t.store(), "northwind", &[], SearchMode::Text, 50, 1 << 20).results;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, id);
    assert!(hits[0].snippet.contains("Northwind"));
    assert!(hits[0].snippet.chars().count() <= 300);

    let both = find(t.store(), "zebra", &[], SearchMode::Text, 50, 1 << 20).results;
    assert_eq!(both.len(), 2);
    assert_eq!(
        both.iter().find(|x| x.id == plain).expect("plain").snippet,
        "Zebra notes"
    );
    assert_eq!(t.store().get(&id).expect("get").expect("some").text, ocr);
    assert_eq!(
        t.store().get(&plain).expect("get").expect("some").text,
        "Zebra notes"
    );
}

#[test]
fn hybrid_ranking_fuses_text_and_vector() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let target = vec_seeded(100);
    let a = add_ready(t.store(), t0, "Acme invoice 4471", &target, "chrome.exe", true);
    let b = add_ready(
        t.store(),
        t0 + ChronoDuration::minutes(1),
        "Acme newsletter",
        &vec_seeded(200),
        "chrome.exe",
        true,
    );
    let c = add_ready(
        t.store(),
        t0 + ChronoDuration::minutes(2),
        "Holiday photos",
        &vec_near(&target, 5),
        "chrome.exe",
        true,
    );
    for i in 0..30 {
        add_ready(
            t.store(),
            t0 + ChronoDuration::minutes(3 + i),
            &format!("Unrelated {i}"),
            &vec_seeded(1000 + i as i32),
            "chrome.exe",
            true,
        );
    }
    let r = find(t.store(), "acme invoice", &target, SearchMode::Hybrid, 50, 1 << 20);
    assert_eq!(r.results[0].id, a);
    assert!((r.results[0].score - 1.0).abs() < 0.001);
    assert!(r.results.iter().any(|x| x.id == b));
    assert!(r.results.iter().any(|x| x.id == c));
    for x in &r.results {
        assert!((0.0..=1.0).contains(&x.score));
    }
    for w in r.results.windows(2) {
        assert!(w[0].score >= w[1].score);
    }
    assert!(
        !find(t.store(), "acme invoice", &target, SearchMode::Text, 50, 1 << 20)
            .results
            .iter()
            .any(|x| x.id == c)
    );
    assert!(
        find(t.store(), "zzz", &target, SearchMode::Vector, 50, 1 << 20)
            .results
            .iter()
            .take(2)
            .any(|x| x.id == c)
    );
}

#[test]
fn search_filters_time_app_and_kind() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let v = vec_seeded(7);
    let early = add_ready(t.store(), t0, "report alpha", &v, "word.exe", true);
    let late = add_ready(
        t.store(),
        t0 + ChronoDuration::hours(1),
        "report beta",
        &v,
        "excel.exe",
        true,
    );
    let aud = t
        .store()
        .add_audio_capture(&audio(
            t0 + ChronoDuration::minutes(30),
            "the report is late",
            "teams.exe",
        ))
        .expect("audio");
    let run = |req: SearchRequest| -> Vec<String> {
        t.store()
            .search(&req, &v, 50, 1 << 20)
            .expect("search")
            .results
            .into_iter()
            .map(|h| h.id)
            .collect()
    };
    assert_eq!(
        run(SearchRequest {
            q: "report".to_string(),
            limit: None,
            from: Some(t0 + ChronoDuration::minutes(45)),
            to: None,
            app: None,
            kind: None,
            mode: SearchMode::Hybrid,
            site: None,
        }),
        vec![late]
    );
    assert_eq!(
        run(SearchRequest {
            q: "report".to_string(),
            limit: None,
            from: None,
            to: None,
            app: Some("WORD.EXE".to_string()),
            kind: None,
            mode: SearchMode::Hybrid,
            site: None,
        }),
        vec![early]
    );
    assert_eq!(
        run(SearchRequest {
            q: "report".to_string(),
            limit: None,
            from: None,
            to: None,
            app: None,
            kind: Some(vec![CaptureKind::Audio]),
            mode: SearchMode::Hybrid,
            site: None,
        }),
        vec![aud]
    );
    assert!(
        run(SearchRequest {
            q: "report".to_string(),
            limit: None,
            from: None,
            to: None,
            app: Some("nothere.exe".to_string()),
            kind: None,
            mode: SearchMode::Hybrid,
            site: None,
        })
        .is_empty()
    );
}

#[test]
fn max_response_bytes_trims_from_the_end_and_sets_truncated() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    for i in 0..40 {
        add_ready(
            t.store(),
            t0 + ChronoDuration::seconds(i),
            &format!("Common title number {i}{}", "x".repeat(200)),
            &vec_seeded(i as i32),
            "chrome.exe",
            true,
        );
    }
    let req = SearchRequest {
        q: "common".to_string(),
        limit: Some(40),
        from: None,
        to: None,
        app: None,
        kind: None,
        mode: SearchMode::Hybrid,
        site: None,
    };
    let full = t.store().search(&req, &[], 40, 1 << 20).expect("full");
    assert_eq!(full.count, 40);
    assert!(!full.truncated);

    let cut = t.store().search(&req, &[], 40, 4096).expect("cut");
    assert!(cut.truncated);
    assert!(cut.count < 40);
    let full_ids: Vec<&str> = full.results.iter().take(cut.count as usize).map(|x| x.id.as_str()).collect();
    let cut_ids: Vec<&str> = cut.results.iter().map(|x| x.id.as_str()).collect();
    assert_eq!(full_ids, cut_ids);
    let json = Envelope::ok_raw(&CoreJson::to_string(&cut).expect("json"));
    assert!(json.len() <= 4096);
}

#[test]
fn purge_scopes_and_wal_truncate() {
    let t = TempStore::new();
    let now = Utc::now();
    let old = add_ready(t.store(), now - ChronoDuration::hours(3), "old one", &vec_seeded(1), "a.exe", true);
    let mid = add_ready(t.store(), now - ChronoDuration::hours(2), "mid one", &vec_seeded(2), "b.exe", true);
    let recent = add_ready(t.store(), now - ChronoDuration::minutes(5), "recent one", &vec_seeded(3), "a.exe", true);
    let s1 = add_site(t.store(), now - ChronoDuration::hours(1), "site one", &vec_seeded(4), "github.com");
    let s2 = add_site(t.store(), now - ChronoDuration::minutes(59), "site two", &vec_seeded(5), "github.com");
    let s3 = add_site(t.store(), now - ChronoDuration::minutes(58), "site three", &vec_seeded(6), "gitlab.com");

    // last + WAL truncate
    assert_eq!(t.store().count_purge(&PurgeScope::last(ChronoDuration::minutes(10))).expect("c"), 1);
    assert_eq!(t.store().purge(&PurgeScope::last(ChronoDuration::minutes(10))).expect("p"), 1);
    assert!(t.store().get(&recent).expect("g").is_none());
    let mut wal = DataPaths::store_db(t.home.path()).into_os_string();
    wal.push("-wal");
    assert_eq!(fs::metadata(&wal).map(|m| m.len()).unwrap_or(0), 0);

    // id + site + no-match
    assert_eq!(t.store().count_purge(&PurgeScope::for_id(&s1)).expect("c"), 1);
    assert_eq!(t.store().purge(&PurgeScope::for_id(&s1)).expect("p"), 1);
    assert!(t.store().get(&s1).expect("g").is_none());
    assert_eq!(t.store().purge(&PurgeScope::for_site("WWW.github.com")).expect("p"), 1);
    assert!(t.store().get(&s2).expect("g").is_none());
    assert!(t.store().get(&s3).expect("g").is_some());
    assert_eq!(t.store().purge(&PurgeScope::for_id("s_nope")).expect("p"), 0);
    assert_eq!(t.store().purge(&PurgeScope::for_id(&s3)).expect("p"), 1);

    // app + range
    assert_eq!(t.store().purge(&PurgeScope::for_app("B.EXE")).expect("p"), 1);
    assert!(t.store().get(&mid).expect("g").is_none());
    assert_eq!(
        t.store()
            .purge(&PurgeScope::range(
                now - ChronoDuration::hours(4),
                now - ChronoDuration::minutes(150),
            ))
            .expect("p"),
        1
    );
    assert!(t.store().get(&old).expect("g").is_none());
    assert_eq!(t.store().stats().expect("s").rows, 0);
    assert_eq!(t.store().index().count(), 0);
    assert_eq!(t.store().purge(&PurgeScope::last(ChronoDuration::zero())).expect("p"), 0);
}

#[test]
fn retention_deletes_old_rows_and_old_month_files() {
    let t = TempStore::new();
    let now = Utc::now();
    let ancient = add_ready(t.store(), now - ChronoDuration::days(100), "ancient", &vec_seeded(1), "chrome.exe", true);
    let fresh = add_ready(t.store(), now - ChronoDuration::days(1), "fresh", &vec_seeded(2), "chrome.exe", true);
    let old_month = (now - ChronoDuration::days(100)).format("%Y-%m").to_string();
    let old_file = DataPaths::keyframes_dir(t.home.path()).join(format!("{old_month}.db"));
    assert!(old_file.exists());
    t.store()
        .append_audit(&AuditEntry {
            ts: now - ChronoDuration::days(100),
            pipe: "ipc".to_string(),
            client_pid: 1,
            client_image: "x".to_string(),
            verb: "search".to_string(),
            result_count: 0,
            query_sha256: String::new(),
        })
        .expect("audit");
    let mut s = Settings::default();
    s.retention.days = 30;
    t.store().run_maintenance(&s, true).expect("maint");
    assert!(t.store().get(&ancient).expect("g").is_none());
    assert!(t.store().get(&fresh).expect("g").is_some());
    assert!(!old_file.exists());
    assert!(t.store().read_audit(10).expect("audit").is_empty());
}

#[test]
fn embedding_space_dim_truncation_and_model_change() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let v = vec_seeded(11);
    let id = add_ready(t.store(), t0, "space test", &v, "chrome.exe", true);
    t.store().ensure_embedding_space(&vec_info()).expect("space");
    assert_eq!(t.store().stats().expect("s").pending_embeddings, 0);

    let mut lower = vec_info();
    lower.dim = 256;
    t.store().ensure_embedding_space(&lower).expect("lower");
    assert_eq!(t.store().stats().expect("s").pending_embeddings, 0);
    let r = find(t.store(), "q", &v[..256], SearchMode::Vector, 10, 1 << 20);
    assert_eq!(r.results.len(), 1);
    assert_eq!(r.results[0].id, id);
    let r2 = find(t.store(), "q", &v, SearchMode::Vector, 10, 1 << 20);
    assert_eq!(r2.results.len(), 1);
    assert_eq!(r2.results[0].id, id);

    let no_kf = add_ready(
        t.store(),
        t0 + ChronoDuration::minutes(1),
        "no keyframe",
        &vec_seeded(12),
        "chrome.exe",
        false,
    );
    t.store()
        .ensure_embedding_space(&EmbeddingInfo {
            model_id: "siglip2".to_string(),
            revision: "r1".to_string(),
            dim: 768,
        })
        .expect("new model");
    assert_eq!(t.store().stats().expect("s").pending_embeddings, 1);
    let jobs = t.store().take_pending_embeddings(10).expect("jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, id);
    assert!(find(t.store(), "q", &v, SearchMode::Vector, 10, 1 << 20).results.is_empty());
    assert!(t.store().get(&no_kf).expect("g").is_some());
    assert_eq!(find(t.store(), "keyframe", &[], SearchMode::Text, 10, 1 << 20).results.len(), 1);
}

#[test]
fn audit_round_trip_newest_first() {
    let t = TempStore::new();
    t.store()
        .append_audit(&AuditEntry {
            ts: Utc::now(),
            pipe: "ipc".to_string(),
            client_pid: 10,
            client_image: r"C:\a.exe".to_string(),
            verb: "connect".to_string(),
            result_count: 0,
            query_sha256: String::new(),
        })
        .expect("a1");
    t.store()
        .append_audit(&AuditEntry {
            ts: Utc::now(),
            pipe: "mcp".to_string(),
            client_pid: 11,
            client_image: r"C:\b.exe".to_string(),
            verb: "search".to_string(),
            result_count: 3,
            query_sha256: "abc".to_string(),
        })
        .expect("a2");
    let a = t.store().read_audit(10).expect("read");
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].verb, "search");
    assert_eq!(a[0].query_sha256, "abc");
}

#[test]
fn concurrent_writes_and_searches_are_safe() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let store = t.store();
    thread::scope(|scope| {
        for w in 0..4 {
            scope.spawn(move || {
                for i in 0..30 {
                    let id = store
                        .add_screen_capture(&screen(
                            t0 + ChronoDuration::seconds(w * 100 + i),
                            "chrome.exe",
                            &format!("worker {w} item {i}"),
                            true,
                        ))
                        .expect("add");
                    store
                        .set_embedding(&id, &vec_seeded((w * 1000 + i) as i32), &vec_info())
                        .expect("embed");
                    let _ = find(store, "worker", &vec_seeded(i as i32), SearchMode::Hybrid, 10, 1 << 20);
                }
            });
        }
    });
    assert_eq!(store.stats().expect("s").rows, 120);
    assert_eq!(store.index().count(), 120);
}

#[test]
fn fts_query_escapes_operators() {
    assert_eq!(
        EncryptedStore::fts_query("a AND bc").as_deref(),
        Some("\"a\" OR \"AND\" OR \"bc\"*")
    );
    assert!(EncryptedStore::fts_query("\"*() :").is_none());
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    add_ready(t.store(), t0, "C# NEAR(x) title", &vec_seeded(1), "chrome.exe", true);
    assert_eq!(find(t.store(), "NEAR( \" OR", &[], SearchMode::Text, 50, 1 << 20).results.len(), 1);
}

fn add_site(s: &EncryptedStore, ts: chrono::DateTime<Utc>, title: &str, v: &[f32], site: &str) -> String {
    let mut cap = screen(ts, "chrome.exe", title, true);
    cap.site = site.to_string();
    let id = s.add_screen_capture(&cap).expect("add");
    s.take_pending_embeddings(10).expect("take");
    s.set_embedding(&id, v, &vec_info()).expect("embed");
    id
}

#[test]
fn site_is_stored_normalised_and_filters_text_and_vector_search() {
    let mut t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let gh = add_site(t.store(), t0, "Pull request 12 - review", &vec_seeded(1), "WWW.GitHub.com");
    let gl = add_site(
        t.store(),
        t0 + ChronoDuration::minutes(1),
        "Merge request 12 - review",
        &vec_seeded(2),
        "gitlab.com",
    );
    add_ready(
        t.store(),
        t0 + ChronoDuration::minutes(2),
        "review notes",
        &vec_seeded(3),
        "Code.exe",
        true,
    );
    assert_eq!(t.store().get(&gh).expect("g").expect("s").site, "github.com");
    assert_eq!(
        t.store()
            .timeline(t0 - ChronoDuration::minutes(1), t0 + ChronoDuration::minutes(5), 10)
            .expect("tl")
            .into_iter()
            .find(|m| m.id == gh)
            .expect("gh")
            .site,
        "github.com"
    );

    let text = t
        .store()
        .search(
            &SearchRequest {
                q: "review".to_string(),
                limit: Some(50),
                from: None,
                to: None,
                app: None,
                kind: None,
                mode: SearchMode::Text,
                site: Some("github.com".to_string()),
            },
            &[],
            50,
            1 << 20,
        )
        .expect("s");
    assert_eq!(text.results.len(), 1);
    assert_eq!(text.results[0].id, gh);
    assert_eq!(text.results[0].site, "github.com");

    let vec = t
        .store()
        .search(
            &SearchRequest {
                q: "x".to_string(),
                limit: Some(50),
                from: None,
                to: None,
                app: None,
                kind: None,
                mode: SearchMode::Vector,
                site: Some("gitlab.com".to_string()),
            },
            &vec_seeded(1),
            50,
            1 << 20,
        )
        .expect("s");
    assert_eq!(vec.results.len(), 1);
    assert_eq!(vec.results[0].id, gl);

    assert!(
        t.store()
            .search(
                &SearchRequest {
                    q: "review".to_string(),
                    limit: Some(50),
                    from: None,
                    to: None,
                    app: None,
                    kind: None,
                    mode: SearchMode::Hybrid,
                    site: Some("nowhere.org".to_string()),
                },
                &vec_seeded(1),
                50,
                1 << 20,
            )
            .expect("s")
            .results
            .is_empty()
    );

    t.reopen();
    let vec2 = t
        .store()
        .search(
            &SearchRequest {
                q: "x".to_string(),
                limit: Some(50),
                from: None,
                to: None,
                app: None,
                kind: None,
                mode: SearchMode::Vector,
                site: Some("github.com".to_string()),
            },
            &vec_seeded(2),
            50,
            1 << 20,
        )
        .expect("s");
    assert_eq!(vec2.results.len(), 1);
    assert_eq!(vec2.results[0].id, gh);
}

#[test]
fn facets_count_apps_and_sites_in_the_window() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    add_site(t.store(), t0, "a", &vec_seeded(1), "github.com");
    add_site(t.store(), t0 + ChronoDuration::minutes(1), "b", &vec_seeded(2), "github.com");
    add_site(t.store(), t0 + ChronoDuration::minutes(2), "c", &vec_seeded(3), "gitlab.com");
    add_ready(t.store(), t0 + ChronoDuration::minutes(3), "d", &vec_seeded(4), "Code.exe", true);
    t.store()
        .add_audio_capture(&audio(t0 - ChronoDuration::hours(30), "old standup", "teams.exe"))
        .expect("audio");
    let f = t.store().facets(None, None, 10).expect("facets");
    let apps: Vec<(&str, i32)> = f.apps.iter().map(|a| (a.name.as_str(), a.count)).collect();
    let sites: Vec<(&str, i32)> = f.sites.iter().map(|a| (a.name.as_str(), a.count)).collect();
    assert_eq!(apps, vec![("chrome.exe", 3), ("Code.exe", 1), ("teams.exe", 1)]);
    assert_eq!(sites, vec![("github.com", 2), ("gitlab.com", 1)]);

    let f2 = t
        .store()
        .facets(Some(t0 + ChronoDuration::minutes(1)), Some(t0 + ChronoDuration::hours(1)), 10)
        .expect("f2");
    let apps2: Vec<(&str, i32)> = f2.apps.iter().map(|a| (a.name.as_str(), a.count)).collect();
    assert_eq!(apps2, vec![("chrome.exe", 2), ("Code.exe", 1)]);
    assert_eq!(t.store().facets(None, None, 1).expect("f1").apps.len(), 1);
}

#[test]
fn search_history_is_newest_first_deduplicated_capped_and_encrypted() {
    let mut t = TempStore::new();
    let now = Utc::now();
    t.store().add_history("acme invoice", now - ChronoDuration::minutes(3), 3).expect("h");
    t.store().add_history("tokyo flight", now - ChronoDuration::minutes(2), 3).expect("h");
    t.store().add_history("  acme invoice ", now - ChronoDuration::minutes(1), 3).expect("h");
    let h: Vec<String> = t.store().history(10).expect("h").into_iter().map(|x| x.q).collect();
    assert_eq!(h, vec!["acme invoice", "tokyo flight"]);

    t.store().add_history("standup", now, 3).expect("h");
    t.store().add_history("budget", now + ChronoDuration::seconds(1), 3).expect("h");
    let h: Vec<String> = t.store().history(10).expect("h").into_iter().map(|x| x.q).collect();
    assert_eq!(h, vec!["budget", "standup", "acme invoice"]);

    t.store().checkpoint().expect("cp");
    let raw = String::from_utf8_lossy(&raw_bytes(&DataPaths::store_db(t.home.path()))).into_owned();
    assert!(!raw.contains("tokyo"));
    assert!(!raw.contains("budget"));

    assert_eq!(t.store().delete_history(Some("standup")).expect("d"), 1);
    let h: Vec<String> = t.store().history(10).expect("h").into_iter().map(|x| x.q).collect();
    assert_eq!(h, vec!["budget", "acme invoice"]);

    t.store()
        .purge(&PurgeScope::range(
            now - ChronoDuration::seconds(30),
            now + ChronoDuration::minutes(1),
        ))
        .expect("p");
    let h: Vec<String> = t.store().history(10).expect("h").into_iter().map(|x| x.q).collect();
    assert_eq!(h, vec!["acme invoice"]);

    t.reopen();
    assert_eq!(t.store().history(10).expect("h").len(), 1);
    assert_eq!(t.store().delete_history(None).expect("d"), 1);
    assert!(t.store().history(10).expect("h").is_empty());
}

#[test]
fn schema_1_store_gains_site_and_history_on_open() {
    let mut t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    let id = add_ready(t.store(), t0, "Acme invoice", &vec_seeded(1), "chrome.exe", true);
    t.close();

    {
        let c = Connection::open(&DataPaths::store_db(t.home.path())).expect("raw open");
        c.exec_batch("PRAGMA cipher='sqlcipher'").expect("c");
        c.exec_batch("PRAGMA legacy=4").expect("l");
        c.exec_batch(&format!("PRAGMA key=\"x'{}'\"", hex_upper(&t.key))).expect("k");
        c.exec_batch("DROP INDEX ix_captures_site").expect("drop idx");
        c.exec_batch("ALTER TABLE captures DROP COLUMN site").expect("drop col");
        c.exec_batch("DROP TABLE search_history").expect("drop hist");
        c.exec_batch("UPDATE meta SET v='1' WHERE k='schema'").expect("v1");
    }

    t.reopen();
    assert_eq!(t.store().get(&id).expect("g").expect("s").site, "");
    assert_eq!(find(t.store(), "acme", &vec_seeded(1), SearchMode::Hybrid, 50, 1 << 20).results.len(), 1);
    t.store().add_history("acme", Utc::now(), 10).expect("h");
    assert_eq!(t.store().history(10).expect("h").len(), 1);
}

#[test]
fn transaction_left_open_by_a_failed_begin_does_not_wedge_writes() {
    let mut t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(2);
    t.store().begin_raw_for_test().expect("raw begin");
    let id = add_ready(t.store(), t0, "after the wedge", &vec_seeded(3), "chrome.exe", true);
    assert_eq!(
        find(t.store(), "wedge", &[], SearchMode::Text, 50, 1 << 20).results[0].id,
        id
    );
    t.store()
        .add_audio_capture(&audio(t0 + ChronoDuration::minutes(1), "spoken words", "teams.exe"))
        .expect("audio");
    t.reopen();
    assert_eq!(
        find(t.store(), "wedge", &[], SearchMode::Text, 50, 1 << 20).results[0].id,
        id
    );
    assert_eq!(find(t.store(), "spoken", &[], SearchMode::Text, 50, 1 << 20).results.len(), 1);
}

#[test]
fn native_callback_warm_up_runs_without_a_store() {
    EncryptedStore::warm_native_callbacks().expect("warm 1");
    EncryptedStore::warm_native_callbacks().expect("warm 2");
}

// ---- Batch4Tests.ReplayFrameStoreTests ----

fn replay_frame(t0: chrono::DateTime<Utc>, replay: &str, offset_ms: i32, title: &str) -> NewReplayFrame {
    NewReplayFrame {
        ts: t0 + ChronoDuration::milliseconds(i64::from(offset_ms)),
        replay_id: replay.to_string(),
        offset_ms,
        app: "game.exe".to_string(),
        title: title.to_string(),
        monitor: 0,
        width: 1920,
        height: 1080,
        image: FAKE_WEBP.to_vec(),
        image_mime: "image/webp".to_string(),
        site: String::new(),
        captured_at: None,
    }
}

fn add_ready_replay(t: &TempStore, f: &NewReplayFrame, v: &[f32]) -> String {
    let id = t.store().add_replay_frame(f).expect("add replay");
    let jobs = t.store().take_pending_embeddings(10).expect("take");
    let job = jobs.iter().find(|j| j.id == id).expect("job");
    assert_eq!(job.kind, CaptureKind::Replay);
    assert!(job.image.is_some());
    t.store().set_embedding(&id, v, &vec_info()).expect("embed");
    id
}

#[test]
fn replay_frames_are_searchable_by_kind_and_carry_offset() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(1);
    let v = vec_seeded(11);
    let screen_id = add_ready(t.store(), t0, "boss fight", &v, "game.exe", true);
    let frame = add_ready_replay(&t, &replay_frame(t0, "20261112T100000Z", 15000, "boss fight"), &v);

    let all = find(t.store(), "boss", &v, SearchMode::Hybrid, 50, 1 << 20).results;
    assert!(all.iter().any(|h| h.id == screen_id));
    let hit = all.iter().find(|h| h.id == frame).expect("frame hit");
    assert_eq!(hit.kind, CaptureKind::Replay);
    assert_eq!(hit.replay_id.as_deref(), Some("20261112T100000Z"));
    assert_eq!(hit.replay_offset_ms, Some(15000));
    assert!(all.iter().find(|h| h.id == screen_id).expect("s").replay_id.is_none());

    let replay_only = t
        .store()
        .search(
            &SearchRequest {
                q: "boss".to_string(),
                limit: None,
                from: None,
                to: None,
                app: None,
                kind: Some(vec![CaptureKind::Replay]),
                mode: SearchMode::Hybrid,
                site: None,
            },
            &v,
            50,
            1 << 20,
        )
        .expect("s");
    assert_eq!(replay_only.results.len(), 1);
    assert_eq!(replay_only.results[0].id, frame);

    let replay_vec = t
        .store()
        .search(
            &SearchRequest {
                q: "boss".to_string(),
                limit: None,
                from: None,
                to: None,
                app: None,
                kind: Some(vec![CaptureKind::Replay]),
                mode: SearchMode::Vector,
                site: None,
            },
            &v,
            50,
            1 << 20,
        )
        .expect("s");
    assert_eq!(replay_vec.results.len(), 1);
    assert_eq!(replay_vec.results[0].id, frame);

    let screen_only = t
        .store()
        .search(
            &SearchRequest {
                q: "boss".to_string(),
                limit: None,
                from: None,
                to: None,
                app: None,
                kind: Some(vec![CaptureKind::Screen]),
                mode: SearchMode::Hybrid,
                site: None,
            },
            &v,
            50,
            1 << 20,
        )
        .expect("s");
    assert!(!screen_only.results.iter().any(|h| h.id == frame));

    let d = t.store().get(&frame).expect("g").expect("some");
    assert_eq!(d.kind, CaptureKind::Replay);
    assert_eq!(d.replay_offset_ms, Some(15000));
    assert!(t.store().get_keyframe(&frame).expect("kf").is_some());
}

#[test]
fn timeline_skips_replay_frames_and_delete_removes_them() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(1);
    let v = vec_seeded(12);
    let scr = add_ready(t.store(), t0, "lobby", &v, "game.exe", true);
    let a = add_ready_replay(&t, &replay_frame(t0, "rA", 0, "lobby"), &v);
    add_ready_replay(&t, &replay_frame(t0, "rA", 5000, "lobby"), &v);
    let b = add_ready_replay(&t, &replay_frame(t0, "rB", 0, "lobby"), &v);

    let tl: Vec<String> = t
        .store()
        .timeline(t0 - ChronoDuration::hours(1), t0 + ChronoDuration::hours(1), 50)
        .expect("tl")
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(tl, vec![scr]);
    assert_eq!(t.store().delete_replay_frames("rA").expect("del"), 2);
    assert!(t.store().get(&a).expect("g").is_none());
    assert!(t.store().get(&b).expect("g").is_some());
    assert_eq!(t.store().delete_replay_frames("rA").expect("del"), 0);
    assert!(
        !find(t.store(), "lobby", &v, SearchMode::Hybrid, 50, 1 << 20)
            .results
            .iter()
            .any(|h| h.id == a)
    );
}

#[test]
fn migration_is_idempotent_and_keeps_frames() {
    let mut t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::hours(1);
    let f = add_ready_replay(&t, &replay_frame(t0, "rC", 2500, "map"), &vec_seeded(13));
    t.reopen();
    t.reopen();
    assert_eq!(t.store().get(&f).expect("g").expect("s").replay_offset_ms, Some(2500));
}

// ---- Sweep4WCoreTests ----

#[test]
fn embedding_leases_hold_jobs_back_until_they_expire() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::minutes(30);
    assert_eq!(t.store().embedding_lease_timeout(), Duration::from_secs(120));
    for i in 0..3 {
        t.store()
            .add_screen_capture(&screen(
                t0 + ChronoDuration::seconds(i),
                "chrome.exe",
                &format!("row {i}"),
                true,
            ))
            .expect("add");
    }
    assert_eq!(t.store().take_pending_embeddings(32).expect("take").len(), 3);
    assert!(t.store().take_pending_embeddings(32).expect("take").is_empty());

    t.store().set_embedding_lease_timeout(Duration::ZERO);
    let again = t.store().take_pending_embeddings(32).expect("take again");
    assert_eq!(again.len(), 3);

    t.store().set_embedding_lease_timeout(Duration::from_secs(120));
    assert!(t.store().take_pending_embeddings(32).expect("leased").is_empty());
    for j in &again {
        t.store()
            .set_embedding(&j.id, &vec_seeded(j.id.len() as i32), &vec_info())
            .expect("embed");
    }
    assert_eq!(t.store().stats().expect("s").pending_embeddings, 0);
}

#[test]
fn going_back_up_in_dim_queues_the_truncated_rows() {
    let t = TempStore::new();
    let t0 = Utc::now() - ChronoDuration::minutes(30);
    t.store().ensure_embedding_space(&vec_info()).expect("space");
    let kept = t
        .store()
        .add_screen_capture(&screen(t0, "chrome.exe", "kept", true))
        .expect("add");
    let dropped = t
        .store()
        .add_screen_capture(&screen(
            t0 + ChronoDuration::seconds(1),
            "chrome.exe",
            "no keyframe kept",
            false,
        ))
        .expect("add");
    t.store().take_pending_embeddings(8).expect("take");
    t.store().set_embedding(&kept, &vec_seeded(1), &vec_info()).expect("e1");
    t.store().set_embedding(&dropped, &vec_seeded(2), &vec_info()).expect("e2");

    let mut dim512 = vec_info();
    dim512.dim = 512;
    t.store().ensure_embedding_space(&dim512).expect("512");
    assert_eq!(t.store().stats().expect("s").pending_embeddings, 0);

    t.store().ensure_embedding_space(&vec_info()).expect("768");
    assert_eq!(t.store().stats().expect("s").pending_embeddings, 1);
    let jobs = t.store().take_pending_embeddings(8).expect("take");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, kept);
    t.store().set_embedding(&kept, &vec_seeded(1), &vec_info()).expect("re-embed");
    assert_eq!(t.store().stats().expect("s").pending_embeddings, 0);
}

// ---- Sweep2Tests.KeyStateTests.Wrong_key_for_the_store_is_reported_as_lost ----

#[test]
fn wrong_key_for_the_store_is_reported_as_lost() {
    let other = TempHome::new(Some("dpapi"));
    let home = TempHome::new(Some("dpapi"));

    {
        let st = SettingsStore::load(home.path());
        let vault = Arc::new(KeyVault::new(home.path(), move || st.current(), None, false));
        assert!(vault.try_load_silent().expect("load"));
        let _store = EncryptedStore::open_with_vault(home.path(), vault, None).expect("open");
    }

    // Another folder's valid DPAPI key file swapped in: try_load_silent succeeds, open_with_vault reports KeyLost.
    {
        let st = SettingsStore::load(other.path());
        let v = KeyVault::new(other.path(), move || st.current(), None, false);
        assert!(v.try_load_silent().expect("load other"));
    }
    fs::copy(DataPaths::key_file(other.path()), DataPaths::key_file(home.path())).expect("copy key");

    {
        let st = SettingsStore::load(home.path());
        let vault = Arc::new(KeyVault::new(home.path(), move || st.current(), None, false));
        assert!(vault.try_load_silent().expect("load swapped"));
        let err = EncryptedStore::open_with_vault(home.path(), vault, None)
            .err()
            .expect("must fail");
        assert!(matches!(err, StoreError::KeyLost(_)), "expected KeyLost, got {err:?}");
    }

    fs::remove_file(DataPaths::key_file(home.path())).expect("delete key");
    {
        let st = SettingsStore::load(home.path());
        let vault = KeyVault::new(home.path(), move || st.current(), None, false);
        let err = vault.try_load_silent().expect_err("missing key");
        assert!(matches!(err, br_core::CryptoError::KeyLost(_)));
        assert!(!DataPaths::key_file(home.path()).exists());
    }
}

/// Regression C7.
#[test]
fn purge_last_deletes_rows_stamped_in_the_future() {
    let t = TempStore::new();
    let future = t
        .store()
        .add_screen_capture(&screen(Utc::now() + ChronoDuration::hours(2), "chrome.exe", "f", true))
        .expect("f");
    t.store().set_embedding(&future, &vec_seeded(9), &vec_info()).expect("e1");
    let old = t
        .store()
        .add_screen_capture(&screen(Utc::now() - ChronoDuration::hours(1), "chrome.exe", "o", true))
        .expect("o");
    t.store().set_embedding(&old, &vec_seeded(10), &vec_info()).expect("e2");

    assert_eq!(t.store().count_purge(&PurgeScope::last(ChronoDuration::minutes(5))).expect("c"), 1);
    assert_eq!(t.store().purge(&PurgeScope::last(ChronoDuration::minutes(5))).expect("p"), 1);
    assert!(t.store().get(&future).expect("g").is_none());
    assert!(t.store().get(&old).expect("g").is_some());
    assert!(
        t.store()
            .add_screen_capture(&screen(Utc::now() + ChronoDuration::seconds(1), "chrome.exe", "n", true))
            .is_ok()
    );
}

/// Regression C7.
#[test]
fn panic_with_zero_minutes_still_deletes_nothing() {
    let t = TempStore::new();
    let future = t
        .store()
        .add_screen_capture(&screen(Utc::now() + ChronoDuration::hours(2), "chrome.exe", "f", true))
        .expect("f");
    t.store().set_embedding(&future, &vec_seeded(11), &vec_info()).expect("e");
    assert_eq!(t.store().purge(&PurgeScope::last(ChronoDuration::zero())).expect("p"), 0);
    assert!(t.store().get(&future).expect("g").is_some());
}

/// Regression B20.
#[test]
fn history_needles() {
    assert_eq!(
        EncryptedStore::history_needles(&PurgeScope::for_site("www.pornhub.com")),
        vec!["pornhub.com", "pornhub"]
    );
    assert_eq!(
        EncryptedStore::history_needles(&PurgeScope::for_site("news.bbc.co.uk")),
        vec!["news.bbc.co.uk", "bbc"]
    );
    assert_eq!(
        EncryptedStore::history_needles(&PurgeScope::for_app("chrome.exe")),
        vec!["chrome.exe", "chrome"]
    );
    // N1: multi-byte app names must not panic (the `.exe` check cut inside a character).
    assert!(EncryptedStore::history_needles(&PurgeScope::for_app("日日")).is_empty());
    assert_eq!(
        EncryptedStore::history_needles(&PurgeScope::for_app("日本語.exe")),
        vec!["日本語.exe", "日本語"]
    );
}

/// Regression B20.
#[test]
fn app_and_site_purges_forget_matching_saved_searches() {
    let t = TempStore::new();
    let now = Utc::now();
    for q in ["pornhub", "PornHub videos", "chrome tips", "weather", "acme invoice"] {
        t.store().add_history(q, now, 100).expect("add");
    }
    t.store().purge(&PurgeScope::for_site("www.pornhub.com")).expect("p");
    let mut h: Vec<String> = t.store().history(100).expect("h").into_iter().map(|x| x.q).collect();
    h.sort();
    assert_eq!(h, vec!["acme invoice", "chrome tips", "weather"]);

    t.store().purge(&PurgeScope::for_app("chrome.exe")).expect("p");
    let mut h2: Vec<String> = t.store().history(100).expect("h").into_iter().map(|x| x.q).collect();
    h2.sort();
    assert_eq!(h2, vec!["acme invoice", "weather"]);
}

#[test]
fn hybrid_search_without_text_hits_reports_the_cosine() {
    let t = TempStore::new();
    let id = t
        .store()
        .add_screen_capture(&screen(
            Utc::now() - ChronoDuration::minutes(5),
            "chrome.exe",
            "Acme invoice 4471 - Gmail",
            true,
        ))
        .expect("add");
    t.store().set_embedding(&id, &vec_seeded(1), &vec_info()).expect("embed");

    let q = vec_seeded(2);
    let resp = t
        .store()
        .search(
            &SearchRequest {
                q: "zzqqxx".to_string(),
                limit: None,
                from: None,
                to: None,
                app: None,
                kind: None,
                mode: SearchMode::Hybrid,
                site: None,
            },
            &q,
            10,
            0,
        )
        .expect("search");
    assert_eq!(resp.results.len(), 1);
    let hit = &resp.results[0];
    assert!(!hit.text_match);
    let expected = ((f64::from(hit.cosine.expect("cos").max(0.0)) * 10_000.0).round() / 10_000.0) as f32;
    assert!((hit.score - expected).abs() < 1e-4);
    assert_ne!(hit.score, 0.5f32);

    let fused = t
        .store()
        .search(
            &SearchRequest {
                q: "acme".to_string(),
                limit: None,
                from: None,
                to: None,
                app: None,
                kind: None,
                mode: SearchMode::Hybrid,
                site: None,
            },
            &q,
            10,
            0,
        )
        .expect("fused");
    assert_eq!(fused.results.len(), 1);
    assert!(fused.results[0].text_match);
}
