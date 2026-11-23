// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! RAM binary vector index tests (`BinaryVectorIndex`).

mod common;

use std::sync::Arc;

use br_contracts::PurgeScope;
use br_core::{BinaryVectorIndex, EncryptedStore, VectorFilter, VectorIndex, win32};
use chrono::{Duration as ChronoDuration, Utc};

use common::{TempHome, screen, vec_info, vec_seeded};

/// Regression B19.
#[test]
fn purged_rows_leave_no_bits_or_names_in_the_ram_index() {
    let index = Arc::new(BinaryVectorIndex::new());
    let home = TempHome::new(None);
    let mut key = vec![0u8; 32];
    win32::fill_random(&mut key);
    let k2 = key.clone();
    let store = EncryptedStore::open(home.path(), move || k2.clone(), Some(index.clone()))
        .expect("open");

    let keep = store
        .add_screen_capture(&screen(
            Utc::now() - ChronoDuration::minutes(3),
            "notepad.exe",
            "kept",
            true,
        ))
        .expect("add");
    store.set_embedding(&keep, &vec_seeded(41), &vec_info()).expect("e1");

    let mut sec = screen(
        Utc::now() - ChronoDuration::minutes(2),
        "tor.exe",
        "secret site",
        true,
    );
    sec.site = "secret-site.example".to_string();
    let id = store.add_screen_capture(&sec).expect("add");
    store.set_embedding(&id, &vec_seeded(42), &vec_info()).expect("e2");

    let before = index.name_counts();
    store
        .purge(&PurgeScope::for_site("secret-site.example"))
        .expect("purge");
    assert_eq!(index.count(), 1);
    assert_eq!(index.name_counts(), (before.0 - 1, before.1 - 1));
    assert_eq!(index.site_id("secret-site.example"), -2);
    assert_eq!(index.app_id("tor.exe"), -2);
    assert_ne!(index.app_id("notepad.exe"), -2);
    assert!(!index.has_stale_bits(vec_seeded(42).len()));
}

/// Regression B19.
#[test]
fn compaction_clears_the_tail() {
    let index = BinaryVectorIndex::new();
    let v = vec![1.0f32; 64];
    for r in 1..=6000i64 {
        index.upsert(r, &v, r, 0, &format!("app{}", r % 7), "");
    }
    for r in 1..=5000i64 {
        index.remove(r);
    }
    assert_eq!(index.count(), 1000);
    assert!(!index.has_stale_bits(64));
    assert_eq!(index.name_counts().0, 7);
    assert_eq!(index.scan(&v, VectorFilter::ALL, 1).len(), 1);
}
