// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use br_contracts::Settings;

use br_agent::worker::WorkerSettingsDiff;

#[test]
fn worker_side_settings_are_detected() {
    let a = Settings::default();
    assert_eq!(None, WorkerSettingsDiff::changed(&a, &Settings::default()));
    let mut b = Settings::default();
    b.ocr.languages = "en,vi".to_string();
    assert_eq!(Some("ocr.languages"), WorkerSettingsDiff::changed(&a, &b));
    let mut c = Settings::default();
    c.privacy.nsfw_device = "cpu".to_string();
    assert_eq!(
        Some("privacy.nsfwDevice"),
        WorkerSettingsDiff::changed(&a, &c)
    );
    let mut d = Settings::default();
    d.index.dim = 512;
    assert_eq!(Some("index.dim"), WorkerSettingsDiff::changed(&a, &d));
    let mut e = Settings::default();
    e.capture.interval_ms = 5000;
    e.privacy.nsfw_threshold = 0.5;
    assert_eq!(None, WorkerSettingsDiff::changed(&a, &e));
}

#[test]
fn worker_settings_argument_carries_the_agents_worker_keys_only() {
    let mut s = Settings::default();
    s.index.dim = 512;
    s.inference.device = "cpu".to_string();
    s.ocr.languages = "en,vi".to_string();
    let json = WorkerSettingsDiff::json(&s);
    let root: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(512, root["index"]["dim"].as_i64().unwrap());
    assert_eq!("cpu", root["inference"]["device"].as_str().unwrap());
    assert_eq!("en,vi", root["ocr"]["languages"].as_str().unwrap());
    assert!(root["governance"]["ecoQos"].is_boolean());
    assert!(root.get("ai").is_none());
    assert!(root.get("capture").is_none());
    assert!(root["privacy"].get("denyProcesses").is_none());

    let back: Settings = serde_json::from_str(&json).unwrap();
    assert_eq!(None, WorkerSettingsDiff::changed(&s, &back));
}

#[test]
fn worker_settings_keys_are_pinned() {
    assert_eq!(
        &[
            "inference.device",
            "inference.gpuAdapter",
            "inference.modelDir",
            "inference.imageTokens",
            "inference.idleExitS",
            "inference.fake",
            "index.dim",
            "audio.language",
            "governance.ecoQos",
            "security.memoryIsolation",
            "log.level",
            "privacy.nsfwFilter",
            "privacy.nsfwDetector",
            "privacy.nsfwDevice",
            "privacy.nsfwGpuAdapter",
            "ocr.enabled",
            "ocr.engine",
            "ocr.languages",
            "ocr.device",
        ],
        WorkerSettingsDiff::KEY_NAMES
    );
}
