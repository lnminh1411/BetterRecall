// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::fs;

use br_contracts::{
    CaptureKind, ChatMessage, ChatRecord, ChatSource, DataPaths, PurgeScope, SearchMode, Settings,
};
use br_core::{Connection, EncryptedStore};
use chrono::{DateTime, Duration as ChronoDuration, Utc};

use common::{TempStore, add_ready, audio, find, hex_upper, raw_bytes, vec_seeded};

fn rec(id: &str, messages: i32, start: DateTime<Utc>, transcript: &str) -> ChatRecord {
    let msgs = (0..messages)
        .map(|i| ChatMessage {
            seq: i,
            ts: start + ChronoDuration::minutes(i64::from(i)),
            role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
            text: format!("msg {i}"),
            sources: if i % 2 == 1 {
                Some(vec![ChatSource {
                    id: format!("cap{i}"),
                    ts: start,
                    kind: CaptureKind::Screen,
                    app: "chrome.exe".to_string(),
                    title: "t".to_string(),
                    site: "example.com".to_string(),
                    replay_id: None,
                    replay_offset_ms: None,
                }])
            } else {
                None
            },
        })
        .collect();
    ChatRecord {
        id: id.to_string(),
        title: format!("Title {id}"),
        created: start,
        updated: start + ChronoDuration::minutes(i64::from(messages)),
        provider: "gemini".to_string(),
        model: "m1".to_string(),
        transcript: transcript.to_string(),
        messages: msgs,
        endpoint: String::new(),
    }
}

#[test]
fn save_appends_and_reads_back() {
    let t = TempStore::new();
    let start = Utc::now() - ChronoDuration::hours(2);
    t.store().save_chat(&rec("c1", 2, start, "[]")).expect("save 1");
    t.store().save_chat(&rec("c1", 4, start, "[1,2]")).expect("save 2");

    let c = t.store().get_chat("c1").expect("get").expect("some");
    assert_eq!(c.messages.len(), 4);
    assert_eq!(c.transcript, "[1,2]");
    assert_eq!(c.messages[1].sources.as_ref().expect("src")[0].id, "cap1");
    assert_eq!(c.messages[1].sources.as_ref().expect("src")[0].site, "example.com");
    assert!(c.messages[0].sources.as_deref().unwrap_or(&[]).is_empty());
    assert!(t.store().get_chat("nope").expect("get").is_none());
}

#[test]
fn list_rename_delete() {
    let t = TempStore::new();
    let start = Utc::now() - ChronoDuration::hours(2);
    t.store().save_chat(&rec("old", 2, start, "[]")).expect("save");
    t.store()
        .save_chat(&rec("new", 2, start + ChronoDuration::minutes(30), "[]"))
        .expect("save");

    let list = t.store().chats(10).expect("chats");
    let ids: Vec<&str> = list.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["new", "old"]);
    assert_eq!(list[0].messages, 2);

    assert!(
        t.store()
            .rename_chat("old", &format!("  {} ", "x".repeat(200)))
            .expect("rename")
    );
    assert_eq!(
        t.store().get_chat("old").expect("g").expect("s").title.chars().count(),
        EncryptedStore::CHAT_TITLE_MAX
    );
    assert!(!t.store().rename_chat("nope", "x").expect("rename nope"));
    assert_eq!(t.store().delete_chats(Some("old")).expect("del"), 1);
    assert_eq!(t.store().delete_chats(Some("old")).expect("del 2"), 0);
    assert_eq!(t.store().delete_chats(None).expect("del all"), 1);
    assert!(t.store().chats(10).expect("chats").is_empty());
}

#[test]
fn purging_a_capture_removes_chats_that_cite_it() {
    for by_range in [false, true] {
        let t = TempStore::new();
        let old = Utc::now() - ChronoDuration::days(3);
        let cap = if by_range {
            add_ready(t.store(), old, "PIN letter", &vec_seeded(1), "chrome.exe", true)
        } else {
            t.store()
                .add_audio_capture(&audio(old, "acme invoice total", "teams.exe"))
                .expect("audio")
        };
        let other = add_ready(
            t.store(),
            Utc::now() - ChronoDuration::minutes(1),
            "Other",
            &vec_seeded(2),
            "chrome.exe",
            true,
        );
        // Chat timestamp is now (outside the purged range when by_range == true).
        t.store().save_chat(&citing_chat("c_cite", &cap, Utc::now())).expect("save");
        t.store().save_chat(&citing_chat("c_other", &other, Utc::now())).expect("save");
        let scope = if by_range {
            PurgeScope::range(old - ChronoDuration::hours(1), old + ChronoDuration::hours(1))
        } else {
            PurgeScope::for_id(&cap)
        };
        assert_eq!(t.store().purge(&scope).expect("purge"), 1);
        assert!(t.store().get_chat("c_cite").expect("g").is_none());
        assert!(t.store().get_chat("c_other").expect("g").is_some());
    }
}

#[test]
fn retention_removes_old_chats_and_searches() {
    let t = TempStore::new();
    let now = Utc::now();
    t.store()
        .save_chat(&rec("old", 2, now - ChronoDuration::days(40), "[]"))
        .expect("save");
    t.store()
        .save_chat(&rec("recent", 2, now - ChronoDuration::days(1), "[]"))
        .expect("save");
    t.store().add_history("old search", now - ChronoDuration::days(40), 10).expect("h");
    t.store().add_history("new search", now - ChronoDuration::days(1), 10).expect("h");

    let mut s = Settings::default();
    s.retention.days = 30;
    t.store().run_maintenance(&s, true).expect("maint");

    let chats: Vec<String> = t.store().chats(10).expect("c").into_iter().map(|c| c.id).collect();
    let hist: Vec<String> = t.store().history(10).expect("h").into_iter().map(|h| h.q).collect();
    assert_eq!(chats, vec!["recent"]);
    assert_eq!(hist, vec!["new search"]);
}

#[test]
fn time_purge_removes_chats_active_in_the_window() {
    let t = TempStore::new();
    let start = Utc::now() - ChronoDuration::hours(2);
    t.store().save_chat(&rec("inside", 2, start, "[]")).expect("save");
    t.store()
        .save_chat(&rec("outside", 2, start - ChronoDuration::days(3), "[]"))
        .expect("save");
    t.store()
        .purge(&PurgeScope::range(
            start - ChronoDuration::minutes(1),
            start + ChronoDuration::minutes(5),
        ))
        .expect("purge");
    assert!(t.store().get_chat("inside").expect("g").is_none());
    assert!(t.store().get_chat("outside").expect("g").is_some());

    t.store()
        .purge(&PurgeScope::last(ChronoDuration::days(4)))
        .expect("purge last");
    assert!(t.store().chats(10).expect("c").is_empty());
}

#[test]
fn usage_accumulates_per_day_and_survives_reopen() {
    let mut t = TempStore::new();
    let start = Utc::now() - ChronoDuration::hours(2);
    assert_eq!(t.store().ai_usage("2026-11-12").expect("u"), (0, 0));
    t.store().add_ai_usage("2026-11-12", 1, 300).expect("a");
    t.store().add_ai_usage("2026-11-12", 2, 50).expect("a");
    t.store().add_ai_usage("2026-11-11", 1, 1).expect("a");
    t.store().save_chat(&rec("c1", 2, start, "[]")).expect("save");
    t.reopen();
    assert_eq!(t.store().ai_usage("2026-11-12").expect("u"), (3, 350));
    assert_eq!(t.store().ai_usage("2026-11-11").expect("u"), (1, 1));
    assert!(t.store().get_chat("c1").expect("g").is_some());
}

// ---- Sweep2Tests.StorePurgeReachTests ----

const TOKEN: &str = "zqxjvbtoken";

fn raw_conn(t: &TempStore) -> Connection {
    let c = Connection::open(&DataPaths::store_db(t.home.path())).expect("open");
    c.exec_batch("PRAGMA cipher='sqlcipher'").expect("c");
    c.exec_batch("PRAGMA legacy=4").expect("l");
    c.exec_batch(&format!("PRAGMA key=\"x'{}'\"", hex_upper(&t.key))).expect("k");
    c.exec_batch("PRAGMA secure_delete=ON").expect("sd");
    c
}

fn plain_bytes(t: &TempStore) -> String {
    let copy = t.home.path().join("plain-copy.db");
    fs::copy(DataPaths::store_db(t.home.path()), &copy).expect("copy");
    {
        let c = Connection::open(&copy).expect("open copy");
        c.exec_batch("PRAGMA cipher='sqlcipher'").expect("c");
        c.exec_batch("PRAGMA legacy=4").expect("l");
        c.exec_batch(&format!("PRAGMA key=\"x'{}'\"", hex_upper(&t.key))).expect("k");
        c.exec_batch("PRAGMA journal_mode=DELETE").expect("jm");
        c.exec_batch("PRAGMA rekey=''").expect("rekey");
    }
    let bytes = raw_bytes(&copy);
    let _ = fs::remove_file(&copy);
    let text: String = bytes.into_iter().map(char::from).collect();
    assert!(text.starts_with("SQLite format 3"));
    text
}

fn citing_chat(chat_id: &str, capture_id: &str, at: DateTime<Utc>) -> ChatRecord {
    ChatRecord {
        id: chat_id.to_string(),
        title: "t".to_string(),
        created: at,
        updated: at,
        provider: "gemini".to_string(),
        model: "m".to_string(),
        transcript: "[]".to_string(),
        messages: vec![
            ChatMessage {
                seq: 0,
                ts: at,
                role: "user".to_string(),
                text: "q".to_string(),
                sources: None,
            },
            ChatMessage {
                seq: 1,
                ts: at,
                role: "assistant".to_string(),
                text: "a".to_string(),
                sources: Some(vec![ChatSource {
                    id: capture_id.to_string(),
                    ts: at,
                    kind: CaptureKind::Screen,
                    app: "chrome.exe".to_string(),
                    title: "t".to_string(),
                    site: String::new(),
                    replay_id: None,
                    replay_offset_ms: None,
                }]),
            },
        ],
        endpoint: String::new(),
    }
}

#[test]
fn purged_words_leave_the_fts_index_and_the_file() {
    let mut t = TempStore::new();
    let id = add_ready(
        t.store(),
        Utc::now() - ChronoDuration::minutes(5),
        &format!("Bank {TOKEN} balance"),
        &vec_seeded(1),
        "chrome.exe",
        true,
    );
    add_ready(
        t.store(),
        Utc::now() - ChronoDuration::minutes(4),
        "Something else",
        &vec_seeded(2),
        "chrome.exe",
        true,
    );
    t.close();
    assert!(plain_bytes(&t).contains(TOKEN));

    t.reopen();
    assert_eq!(t.store().purge(&PurgeScope::for_id(&id)).expect("p"), 1);
    t.close();
    assert!(!plain_bytes(&t).contains(TOKEN));
    t.reopen();
}

#[test]
fn existing_store_gets_secure_delete_and_loses_words_of_earlier_deletes() {
    let mut t = TempStore::new();
    add_ready(
        t.store(),
        Utc::now() - ChronoDuration::minutes(5),
        &format!("Bank {TOKEN} balance"),
        &vec_seeded(1),
        "chrome.exe",
        true,
    );
    add_ready(
        t.store(),
        Utc::now() - ChronoDuration::minutes(4),
        "Something else",
        &vec_seeded(2),
        "chrome.exe",
        true,
    );
    t.close();

    {
        let c = raw_conn(&t);
        c.exec_batch("INSERT INTO fts(fts, rank) VALUES('secure-delete', 0)").expect("off");
        c.exec_batch("DELETE FROM meta WHERE k='fts_secure_delete'").expect("meta");
        c.exec_batch(&format!(
            "DELETE FROM fts WHERE rowid = (SELECT rid FROM captures WHERE title LIKE '%{TOKEN}%')"
        ))
        .expect("fts");
        c.exec_batch(&format!(
            "DELETE FROM vectors WHERE rid = (SELECT rid FROM captures WHERE title LIKE '%{TOKEN}%')"
        ))
        .expect("vec");
        c.exec_batch(&format!(
            "DELETE FROM captures WHERE title LIKE '%{TOKEN}%'"
        ))
        .expect("cap");
    }
    assert!(plain_bytes(&t).contains(TOKEN));

    t.reopen();
    t.close();
    assert!(!plain_bytes(&t).contains(TOKEN));

    t.reopen();
    assert_eq!(
        find(t.store(), "something", &[], SearchMode::Text, 10, 1 << 20).results.len(),
        1
    );
}

#[test]
fn retention_drops_recent_chats_citing_expired_captures() {
    let t = TempStore::new();
    let cap = add_ready(
        t.store(),
        Utc::now() - ChronoDuration::days(40),
        "Old doctor letter",
        &vec_seeded(1),
        "chrome.exe",
        true,
    );
    let kept = add_ready(
        t.store(),
        Utc::now() - ChronoDuration::days(1),
        "Recent",
        &vec_seeded(2),
        "chrome.exe",
        true,
    );
    t.store().save_chat(&citing_chat("c_old", &cap, Utc::now())).expect("s");
    t.store().save_chat(&citing_chat("c_new", &kept, Utc::now())).expect("s");
    let mut s = Settings::default();
    s.retention.days = 30;
    t.store().run_maintenance(&s, true).expect("maint");
    assert!(t.store().get(&cap).expect("g").is_none());
    assert!(t.store().get_chat("c_old").expect("g").is_none());
    assert!(t.store().get_chat("c_new").expect("g").is_some());
}

#[test]
fn open_once_drops_saved_chats_citing_captures_deleted_before_the_fix() {
    let mut t = TempStore::new();
    let cap = add_ready(
        t.store(),
        Utc::now() - ChronoDuration::minutes(5),
        "x",
        &vec_seeded(1),
        "chrome.exe",
        true,
    );
    t.store().save_chat(&citing_chat("c_live", &cap, Utc::now())).expect("s");
    t.store().save_chat(&citing_chat("c_dead", "s_gone", Utc::now())).expect("s");
    t.close();
    {
        let c = raw_conn(&t);
        c.exec_batch("DELETE FROM meta WHERE k='chat_cite_sweep'").expect("del");
    }
    t.reopen();
    assert!(t.store().get_chat("c_live").expect("g").is_some());
    assert!(t.store().get_chat("c_dead").expect("g").is_none());
}
