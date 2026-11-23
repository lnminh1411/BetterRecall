// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Ported `BenchmarkTests.cs`: 250k × 768-d search latency and binary-prefilter recall benchmark.
//! Marked `#[ignore]` because inserting 250k rows takes minutes.

mod common;

use std::time::Instant;

use br_contracts::{SearchMode, SearchRequest};
use br_core::{Ids, VectorFilter};
use chrono::{Duration as ChronoDuration, Utc};

use common::{TempStore, vec_info, vec_near, vec_seeded};

#[test]
#[ignore = "benchmark, minutes"]
fn search_latency_at_250k_rows() {
    const TOTAL: usize = 250_000;
    const BATCH: usize = 10_000;
    let mut t = TempStore::new();
    let info = vec_info();
    let start_ts = Utc::now() - ChronoDuration::days(30);
    let apps = ["chrome.exe", "code.exe", "teams.exe", "outlook.exe", "slack.exe"];

    // Plant 20 known query vectors + near matches for recall verification.
    let mut planted_queries = Vec::new();
    for q_idx in 0..20 {
        let qv = vec_seeded(10_000 + q_idx);
        planted_queries.push((q_idx, qv));
    }

    let t_ins = Instant::now();
    for batch_start in (0..TOTAL).step_by(BATCH) {
        let batch_end = (batch_start + BATCH).min(TOTAL);
        let rows: Vec<_> = (batch_start..batch_end)
            .map(|i| {
                let ts = start_ts + ChronoDuration::seconds((i as i64) * 10);
                let app = apps[i % apps.len()].to_string();
                let title = if i % 500 == 0 {
                    format!("Quarterly invoice report #{i}")
                } else {
                    format!("Window title #{i}")
                };
                let v = if i < planted_queries.len() {
                    vec_near(&planted_queries[i].1, (i as i32) + 1)
                } else {
                    vec_seeded((i as i32) + 100_000)
                };
                (ts, app, title, v)
            })
            .collect();
        t.store().insert_synthetic(rows, &info).expect("insert_synthetic");
    }
    let _insert_ms = t_ins.elapsed().as_millis();

    // Reopen from disk to measure cold index load + live count.
    t.reopen();
    assert_eq!(TOTAL, t.store().index().count());

    let median_ms = |mut samples: Vec<f64>| -> f64 {
        samples.sort_by(|a, b| a.total_cmp(b));
        samples[samples.len() / 2]
    };

    // Binary scan median < 100 ms.
    let mut scan_times = Vec::new();
    let mut hits_found = 0usize;
    for (q_idx, qv) in &planted_queries {
        let sw = Instant::now();
        let cands = t.store().index().scan(qv, VectorFilter::ALL, 400);
        scan_times.push(sw.elapsed().as_secs_f64() * 1000.0);
        assert!(!cands.is_empty());
        // Check recall of the planted near-match in top-400 binary candidates.
        let want_rid = (*q_idx as i64) + 1;
        if cands.iter().any(|c| c.rid == want_rid) {
            hits_found += 1;
        }
    }
    assert!(
        median_ms(scan_times) < 100.0,
        "binary scan median must stay under 100 ms"
    );
    let recall = (hits_found as f64) / (planted_queries.len() as f64);
    assert!(recall >= 0.90, "binary prefilter recall {recall:.2} < 0.90");

    // Vector, Hybrid, Text, and filtered search medians < 100 ms.
    for (mode, app_filter) in [
        (SearchMode::Vector, None),
        (SearchMode::Hybrid, None),
        (SearchMode::Text, None),
        (SearchMode::Hybrid, Some("chrome.exe".to_string())),
    ] {
        let mut times = Vec::new();
        for (q_idx, qv) in planted_queries.iter().take(10) {
            let sw = Instant::now();
            let resp = t
                .store()
                .search(
                    &SearchRequest {
                        q: "invoice".to_string(),
                        limit: Some(20),
                        from: None,
                        to: None,
                        app: app_filter.clone(),
                        kind: None,
                        mode,
                        site: None,
                    },
                    qv,
                    20,
                    64 * 1024,
                )
                .expect("search");
            times.push(sw.elapsed().as_secs_f64() * 1000.0);
            assert!(resp.results.len() <= 20);
            if mode == SearchMode::Vector && app_filter.is_none() {
                assert!(!resp.results.is_empty());
                let want_id = Ids::new('s', start_ts + ChronoDuration::seconds((*q_idx as i64) * 10));
                assert_eq!(want_id, resp.results[0].id);
            }
        }
        assert!(
            median_ms(times) < 100.0,
            "mode={mode:?} app={app_filter:?} median exceeded 100 ms"
        );
    }
}
