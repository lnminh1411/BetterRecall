// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{DataPaths, Settings};
use br_core::SettingsStore;
use common::TempHome;
use serde_json::Value;
use windows::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

fn j(json: &str) -> Value {
    serde_json::from_str(json).expect("valid test json")
}

// ---- SettingsTests.cs ----

#[test]
fn missing_file_gives_defaults_and_writes_camel_case() {
    let h = TempHome::new(None);
    let s = SettingsStore::load(h.path());
    assert_eq!(2000, s.current().capture.interval_ms);
    let text = fs::read_to_string(DataPaths::settings_file(h.path())).unwrap();
    assert!(text.contains("\"intervalMs\": 2000"), "text={text}");
    assert!(text.contains("\"keyProtection\": \"auto\""), "text={text}");
    let mut tmp = DataPaths::settings_file(h.path()).into_os_string();
    tmp.push(".tmp");
    assert!(!PathBuf::from(tmp).exists());
}

#[test]
fn try_update_merges_saves_and_raises_changed() {
    let h = TempHome::new(None);
    let s = SettingsStore::load(h.path());
    let seen = Arc::new(Mutex::new(None));
    let seen_c = seen.clone();
    s.on_changed(move |x| *seen_c.lock().unwrap() = Some(x.clone()));
    assert_eq!(
        None,
        s.try_update(&j(r#"{"capture":{"intervalMs":3000},"ipc":{"enabled":true}}"#))
    );
    assert_eq!(3000, s.current().capture.interval_ms);
    assert!(s.current().ipc.enabled);
    assert_eq!(1280, s.current().capture.max_width);
    assert!(seen.lock().unwrap().is_some());
    let reloaded = SettingsStore::load(h.path());
    assert_eq!(3000, reloaded.current().capture.interval_ms);
}

#[test]
fn try_update_rejects_bad_values_and_saves_nothing() {
    let cases = [
        (r#"{"capture":{"intervalMs":499}}"#, "Invalid capture.intervalMs."),
        (r#"{"capture":{"intervalMs":60001}}"#, "Invalid capture.intervalMs."),
        (r#"{"capture":{"intervalMs":1.5}}"#, "Invalid capture.intervalMs."),
        (r#"{"capture":{"intervalMs":"2000"}}"#, "Invalid capture.intervalMs."),
        (r#"{"capture":{"maxWidth":639}}"#, "Invalid capture.maxWidth."),
        (r#"{"capture":{"dhashThreshold":33}}"#, "Invalid capture.dhashThreshold."),
        (r#"{"capture":{"dirtyMinPercent":100.5}}"#, "Invalid capture.dirtyMinPercent."),
        (r#"{"capture":{"monitors":[-1]}}"#, "Invalid capture.monitors."),
        (r#"{"keyframes":{"format":"png"}}"#, "Invalid keyframes.format."),
        (r#"{"keyframes":{"quality":29}}"#, "Invalid keyframes.quality."),
        (r#"{"keyframes":{"thumbCacheMb":1025}}"#, "Invalid keyframes.thumbCacheMb."),
        (r#"{"replay":{"seconds":14}}"#, "Invalid replay.seconds."),
        (r#"{"replay":{"fps":61}}"#, "Invalid replay.fps."),
        (r#"{"replay":{"fps":0}}"#, "Invalid replay.fps."),
        (r#"{"replay":{"bitrateKbps":499}}"#, "Invalid replay.bitrateKbps."),
        (r#"{"replay":{"encoder":"x264"}}"#, "Invalid replay.encoder."),
        (r#"{"replay":{"encoder":"av1_qsv"}}"#, "Invalid replay.encoder."),
        (r#"{"replay":{"encoderAdapter":""}}"#, "Invalid replay.encoderAdapter."),
        (r#"{"replay":{"encoderAdapter":"   "}}"#, "Invalid replay.encoderAdapter."),
        (r#"{"replay":{"layout":"grid"}}"#, "Invalid replay.layout."),
        (r#"{"replay":{"layout":"permonitor"}}"#, "Invalid replay.layout."),
        (r#"{"replay":{"resolution":"4k"}}"#, "Invalid replay.resolution."),
        (r#"{"replay":{"scalePercent":24}}"#, "Invalid replay.scalePercent."),
        (r#"{"replay":{"scalePercent":101}}"#, "Invalid replay.scalePercent."),
        (r#"{"replay":{"exportDir":"relative\\dir"}}"#, "Invalid replay.exportDir."),
        (r#"{"index":{"dim":128}}"#, "Invalid index.dim."),
        (r#"{"retention":{"days":0}}"#, "Invalid retention.days."),
        (r#"{"audio":{"language":"English"}}"#, "Invalid audio.language."),
        (r#"{"audio":{"maxSegmentS":31}}"#, "Invalid audio.maxSegmentS."),
        (r#"{"privacy":{"nsfwThreshold":0}}"#, "Invalid privacy.nsfwThreshold."),
        (r#"{"privacy":{"nsfwThreshold":1.0}}"#, "Invalid privacy.nsfwThreshold."),
        (r#"{"privacy":{"nsfwGrid":4}}"#, "Invalid privacy.nsfwGrid."),
        (r#"{"privacy":{"nsfwGrid":0}}"#, "Invalid privacy.nsfwGrid."),
        (r#"{"privacy":{"nsfwDetectorThreshold":0}}"#, "Invalid privacy.nsfwDetectorThreshold."),
        (r#"{"privacy":{"nsfwDetectorThreshold":1.0}}"#, "Invalid privacy.nsfwDetectorThreshold."),
        (r#"{"privacy":{"nsfwDetector":"yes"}}"#, "Invalid privacy.nsfwDetector."),
        (r#"{"privacy":{"nsfwDevice":"tpu"}}"#, "Invalid privacy.nsfwDevice."),
        (r#"{"privacy":{"nsfwDevice":"NPU"}}"#, "Invalid privacy.nsfwDevice."),
        (r#"{"privacy":{"nsfwGpuAdapter":""}}"#, "Invalid privacy.nsfwGpuAdapter."),
        (r#"{"privacy":{"nsfwGpuAdapter":"   "}}"#, "Invalid privacy.nsfwGpuAdapter."),
        (r#"{"inference":{"gpuAdapter":""}}"#, "Invalid inference.gpuAdapter."),
        (r#"{"inference":{"gpuAdapter":"   "}}"#, "Invalid inference.gpuAdapter."),
        (r#"{"privacy":{"panicPurgeMinutes":1441}}"#, "Invalid privacy.panicPurgeMinutes."),
        (r#"{"security":{"unlockTimeoutMin":0}}"#, "Invalid security.unlockTimeoutMin."),
        (r#"{"capture":{"monitors":[64]}}"#, "Invalid capture.monitors."),
        (r#"{"privacy":{"denyPathPrefixes":["x"]}}"#, "Invalid privacy.denyPathPrefixes."),
        (r#"{"inference":{"device":"tpu"}}"#, "Invalid inference.device."),
        (r#"{"inference":{"imageTokens":100}}"#, "Invalid inference.imageTokens."),
        (r#"{"inference":{"idleExitS":9}}"#, "Invalid inference.idleExitS."),
        (r#"{"governance":{"deferCpuPercent":9}}"#, "Invalid governance.deferCpuPercent."),
        (r#"{"security":{"keyProtection":"none"}}"#, "Invalid security.keyProtection."),
        (r#"{"hotkeys":{"search":"Space"}}"#, "Invalid hotkeys.search."),
        (r#"{"hotkeys":{"panic":"Ctrl+Hyper+X"}}"#, "Invalid hotkeys.panic."),
        (r#"{"hotkeys":{"timeline":"ctrl+alt+p"}}"#, "Invalid hotkeys.pause."),
        (r#"{"hotkeys":{"lock":"Lock"}}"#, "Invalid hotkeys.lock."),
        (r#"{"ipc":{"pipeName":"bad name"}}"#, "Invalid ipc.pipeName."),
        (r#"{"ipc":{"pipeName":"BetterRecall.ui"}}"#, "Invalid ipc.pipeName."),
        (r#"{"ipc":{"pipeName":"BetterRecall.mcp"}}"#, "Invalid ipc.pipeName."),
        (r#"{"ipc":{"maxResults":201}}"#, "Invalid ipc.maxResults."),
        (r#"{"ipc":{"maxResults":5}}"#, "Invalid ipc.maxResults."),
        (r#"{"ipc":{"defaultResults":51}}"#, "Invalid ipc.defaultResults."),
        (r#"{"ipc":{"maxResponseBytes":4095}}"#, "Invalid ipc.maxResponseBytes."),
        (r#"{"ipc":{"maxSearchPerMinute":601}}"#, "Invalid ipc.maxSearchPerMinute."),
        (r#"{"ipc":{"maxPauseS":59}}"#, "Invalid ipc.maxPauseS."),
        (r#"{"ipc":{"idleTimeoutS":3601}}"#, "Invalid ipc.idleTimeoutS."),
        (r#"{"ipc":{"allowClients":["notabs.exe"]}}"#, "Invalid ipc.allowClients."),
        (r#"{"keyframes":{"maxGb":20}}"#, "Invalid keyframes.maxGb."),
        (r#"{"keyframes":{"format":"heic"}}"#, "Invalid keyframes.format."),
        (r#"{"storage":{"minFreeGb":-1}}"#, "Invalid storage.minFreeGb."),
        (r#"{"storage":{"maxGb":0}}"#, "Invalid storage.maxGb."),
        (r#"{"storage":{"overwrite":1}}"#, "Invalid storage.overwrite."),
        (
            r#"{"retention":{"progressiveAfterDays":-1}}"#,
            "Invalid retention.progressiveAfterDays.",
        ),
        (
            r#"{"retention":{"progressiveOverDays":0}}"#,
            "Invalid retention.progressiveOverDays.",
        ),
        (r#"{"log":{"level":"trace"}}"#, "Invalid log.level."),
        (r#"{"capture":{"nope":1}}"#, "Invalid capture.nope."),
        (r#"{"nosection":{}}"#, "Invalid nosection."),
        (r#"{"capture":5}"#, "Invalid capture."),
        (r#"[1]"#, "Invalid JSON."),
    ];
    let h = TempHome::new(None);
    let s = SettingsStore::load(h.path());
    let before = fs::read_to_string(DataPaths::settings_file(h.path())).unwrap();
    let changed = Arc::new(AtomicBool::new(false));
    let changed_c = changed.clone();
    s.on_changed(move |_| changed_c.store(true, Ordering::SeqCst));
    for (json, error) in cases {
        assert_eq!(
            Some(error.to_string()),
            s.try_update(&j(json)),
            "json={json}"
        );
        assert!(!changed.load(Ordering::SeqCst), "json={json}");
        assert_eq!(
            before,
            fs::read_to_string(DataPaths::settings_file(h.path())).unwrap()
        );
    }
}

#[test]
fn hotkey_format() {
    let cases = [
        ("Ctrl+Alt+Space", true),
        ("Ctrl+Alt+Shift+X", true),
        ("Win+F12", true),
        ("Ctrl+=", true),
        (r"Ctrl+Alt+\", true),
        ("Alt+NumPad5", true),
        ("Ctrl+Enter", false),
        ("Ctrl+F25", false),
        ("Ctrl+", false),
        ("Ctrl+Ctrl+X", false),
        ("X", false),
        ("", false),
    ];
    for (hk, ok) in cases {
        assert_eq!(ok, SettingsStore::valid_hotkey(hk), "hk={hk}");
    }
}

#[test]
fn hotkeys_may_be_unassigned_and_new_actions_default_empty() {
    let h = TempHome::new(None);
    let s = SettingsStore::load(h.path());
    assert_eq!("", s.current().hotkeys.timeline);
    assert_eq!("", s.current().hotkeys.home);
    assert_eq!("", s.current().hotkeys.lock);
    assert_eq!(
        None,
        s.try_update(&j(r#"{"hotkeys":{"panic":"","lock":"Ctrl+Alt+L"}}"#))
    );
    assert_eq!("", s.current().hotkeys.panic);
    assert_eq!("Ctrl+Alt+L", s.current().hotkeys.lock);
    assert!(s.current().ui.search_always_on_top);
    assert!(s.current().ui.start_minimized);
}

#[test]
fn replay_keys_accept_spec_values() {
    let h = TempHome::new(None);
    let s = SettingsStore::load(h.path());
    assert_eq!(
        None,
        s.try_update(&j(
            r#"{"replay":{"encoder":"hevc","encoderAdapter":"Intel(R) Graphics","layout":"perMonitor","resolution":"scale","scalePercent":25,"fps":144}}"#
        ))
    );
}

#[test]
fn nsfw_gate_keys_accept_spec_values() {
    let h = TempHome::new(None);
    let s = SettingsStore::load(h.path());
    assert_eq!(1, s.current().privacy.nsfw_grid);
    assert!(s.current().privacy.nsfw_detector);
    assert_eq!(0.4, s.current().privacy.nsfw_detector_threshold);
    assert_eq!("auto", s.current().privacy.nsfw_device);
    assert_eq!("auto", s.current().privacy.nsfw_gpu_adapter);
    for d in ["auto", "npu", "igpu", "gpu", "cpu"] {
        assert_eq!(
            None,
            s.try_update(&j(&format!(r#"{{"privacy":{{"nsfwDevice":"{d}"}}}}"#)))
        );
    }
    assert_eq!(
        None,
        s.try_update(&j(
            r#"{"privacy":{"nsfwDetector":false,"nsfwDetectorThreshold":0.01,"nsfwThreshold":0.01,"nsfwGpuAdapter":"RTX 3050"}}"#
        ))
    );
    assert_eq!(
        None,
        s.try_update(&j(
            r#"{"privacy":{"nsfwDetectorThreshold":0.99,"nsfwThreshold":0.99}}"#
        ))
    );
    assert_eq!("RTX 3050", s.current().privacy.nsfw_gpu_adapter);
}

#[test]
fn adapter_names_accept_up_to_128_chars() {
    let h = TempHome::new(None);
    let s = SettingsStore::load(h.path());
    assert_eq!("auto", s.current().inference.gpu_adapter);
    assert_eq!(
        None,
        s.try_update(&j(
            r#"{"inference":{"device":"gpu","gpuAdapter":"NVIDIA GeForce RTX 3050"}}"#
        ))
    );
    let a128 = "a".repeat(128);
    let a129 = "a".repeat(129);
    for (section, key) in [
        ("replay", "encoderAdapter"),
        ("privacy", "nsfwGpuAdapter"),
        ("inference", "gpuAdapter"),
    ] {
        assert_eq!(
            None,
            s.try_update(&j(&format!(r#"{{"{section}":{{"{key}":"{a128}"}}}}"#))),
            "{section}.{key} len 128"
        );
        assert_eq!(
            Some(format!("Invalid {section}.{key}.")),
            s.try_update(&j(&format!(r#"{{"{section}":{{"{key}":"{a129}"}}}}"#))),
            "{section}.{key} len 129"
        );
    }
    assert_eq!(a128, s.current().inference.gpu_adapter);
    assert_eq!("gpu", s.current().inference.device);
}

#[test]
fn ocr_and_redaction_settings_validate() {
    let h = TempHome::new(None);
    let s = SettingsStore::load(h.path());
    assert!(s.current().ocr.enabled);
    assert_eq!(
        ("onnx", "auto", "auto"),
        (
            s.current().ocr.engine.as_str(),
            s.current().ocr.languages.as_str(),
            s.current().ocr.device.as_str()
        )
    );
    let p = s.current().privacy;
    assert!(p.redact_passwords && p.redact_cards && p.redact_ids && p.redact_birth_dates);
    assert!(!(p.redact_emails || p.redact_phones || p.redact_names || p.redact_addresses));

    assert_eq!(
        None,
        s.try_update(&j(
            r#"{"ocr":{"engine":"windows","languages":"en-US, ja,vi","device":"cpu"}}"#
        ))
    );
    assert_eq!(
        None,
        s.try_update(&j(r#"{"privacy":{"redactEmails":true,"redactNames":true}}"#))
    );
    assert_eq!(
        Some("Invalid ocr.engine.".to_string()),
        s.try_update(&j(r#"{"ocr":{"engine":"tesseract"}}"#))
    );
    assert_eq!(
        Some("Invalid ocr.languages.".to_string()),
        s.try_update(&j(r#"{"ocr":{"languages":"english"}}"#))
    );
    assert_eq!(
        Some("Invalid ocr.languages.".to_string()),
        s.try_update(&j(r#"{"ocr":{"languages":""}}"#))
    );
    assert_eq!(
        Some("Invalid ocr.device.".to_string()),
        s.try_update(&j(r#"{"ocr":{"device":"tpu"}}"#))
    );
    assert_eq!(
        ("windows", "en-US, ja,vi", "cpu"),
        (
            s.current().ocr.engine.as_str(),
            s.current().ocr.languages.as_str(),
            s.current().ocr.device.as_str()
        )
    );
    assert!(s.current().privacy.redact_emails && s.current().privacy.redact_names);
}

#[test]
fn load_resets_bad_keys_and_keeps_good_ones() {
    let h = TempHome::new(None);
    fs::write(
        DataPaths::settings_file(h.path()),
        r#"{"capture":{"intervalMs":5,"maxWidth":1920},"retention":{"days":90}}"#,
    )
    .unwrap();
    let s = SettingsStore::load(h.path());
    assert_eq!(2000, s.current().capture.interval_ms);
    assert_eq!(1920, s.current().capture.max_width);
    assert_eq!(90, s.current().retention.days);
}

// ---- StorageTests.cs (StorageSettingsTests) ----

#[test]
fn defaults_of_batch_2() {
    let s = Settings::default();
    assert_eq!("avif", s.keyframes.format);
    assert_eq!(15.0, s.capture.dirty_min_percent);
    assert_eq!(25, s.storage.min_free_gb);
    assert_eq!(20, s.storage.max_gb);
    assert!(!s.storage.overwrite);
    assert_eq!(7, s.retention.progressive_after_days);
    assert_eq!(30, s.retention.progressive_over_days);
    assert!(!s.ai.enabled);
    assert_eq!("hello", s.security.unlock_method);
    assert!(s.replay.searchable);
    assert_eq!(5, s.replay.sample_seconds);
    assert_eq!(None, SettingsStore::validate(&s));
}

#[test]
fn old_file_moves_keyframes_max_gb_and_old_defaults() {
    let h = TempHome::new(None);
    fs::write(
        DataPaths::settings_file(h.path()),
        r#"{"capture":{"dirtyMinPercent":0.5,"intervalMs":3000},"keyframes":{"format":"webp","maxGb":55,"quality":80}}"#,
    )
    .unwrap();
    let s = SettingsStore::load(h.path()).current();
    assert_eq!(55, s.storage.max_gb);
    assert_eq!("avif", s.keyframes.format);
    assert_eq!(15.0, s.capture.dirty_min_percent);
    assert_eq!(3000, s.capture.interval_ms);
    assert_eq!(80, s.keyframes.quality);
    let text = fs::read_to_string(DataPaths::settings_file(h.path())).unwrap();
    assert!(!text.replace('\r', "").contains("\"maxGb\": 55,\n    \"thumbCacheMb\""));
    assert!(text.contains("\"storage\": {"));
    assert_eq!(55, SettingsStore::load(h.path()).current().storage.max_gb);
}

#[test]
fn old_file_keeps_values_the_user_changed() {
    let h = TempHome::new(None);
    fs::write(
        DataPaths::settings_file(h.path()),
        r#"{"capture":{"dirtyMinPercent":2},"keyframes":{"format":"jpeg","maxGb":20}}"#,
    )
    .unwrap();
    let s = SettingsStore::load(h.path()).current();
    assert_eq!(2.0, s.capture.dirty_min_percent);
    assert_eq!("jpeg", s.keyframes.format);
    assert_eq!(20, s.storage.max_gb);
}

// ---- Batch4Tests.cs (settings cases) ----

#[test]
fn good_settings_accepted() {
    let cases = [
        r#"{"ai":{"endpoint":"https://proxy.example.com/v1"}}"#,
        r#"{"ai":{"endpoint":"http://127.0.0.1:11434/v1"}}"#,
        r#"{"ai":{"endpoint":"http://localhost:1234"}}"#,
        r#"{"ai":{"provider":"openai-compatible","model":"models/gemini-3.8-flash"}}"#,
        r#"{"replay":{"searchable":false,"sampleSeconds":60}}"#,
        r#"{"keyframes":{"format":"jxl"}}"#,
        r#"{"storage":{"minFreeGb":0,"maxGb":500,"overwrite":true}}"#,
        r#"{"retention":{"progressiveAfterDays":0,"progressiveOverDays":3650}}"#,
    ];
    for json in cases {
        let h = TempHome::new(None);
        assert_eq!(
            None,
            SettingsStore::load(h.path()).try_update(&j(json)),
            "json={json}"
        );
    }
}

/// Batch4Tests.Bad_settings_rejected: cleartext remote endpoints, credentials in the endpoint
/// and an API key in the settings file are refused, with the exact error the UI shows.
#[test]
fn bad_settings_rejected() {
    let cases = [
        (r#"{"ai":{"endpoint":"http://example.com"}}"#, "Invalid ai.endpoint."),
        (r#"{"ai":{"endpoint":"https://user:pw@example.com"}}"#, "Invalid ai.endpoint."),
        (r#"{"ai":{"endpoint":"https://example.com/v1?key=1"}}"#, "Invalid ai.endpoint."),
        (r#"{"ai":{"provider":"mistral"}}"#, "Invalid ai.provider."),
        (r#"{"ai":{"model":"bad model"}}"#, "Invalid ai.model."),
        (r#"{"security":{"unlockMethod":"pin"}}"#, "Invalid security.unlockMethod."),
        (r#"{"replay":{"sampleSeconds":1}}"#, "Invalid replay.sampleSeconds."),
        (r#"{"ai":{"apiKey":"x"}}"#, "Invalid ai.apiKey."),
    ];
    for (json, want) in cases {
        let h = TempHome::new(None);
        let store = SettingsStore::load(h.path());
        let before = store.current().ai.endpoint.clone();
        assert_eq!(Some(want.to_string()), store.try_update(&j(json)), "json={json}");
        assert_eq!(before, store.current().ai.endpoint, "json={json}");
    }
}

// ---- Sweep2GTests.cs ----

#[test]
fn audio_language_is_checked_against_whisper_codes() {
    for (lang, ok) in [
        ("auto", true),
        ("vi", true),
        ("haw", true),
        ("xx", false),
        ("eng", false),
        ("zz", false),
    ] {
        let mut s = Settings::default();
        s.audio.language = lang.to_string();
        assert_eq!(
            if ok { None } else { Some("audio.language") },
            SettingsStore::validate(&s),
            "lang={lang}"
        );
    }
}

#[test]
fn settings_file_with_an_unknown_language_keeps_auto() {
    let h = TempHome::new(None);
    fs::write(
        DataPaths::settings_file(h.path()),
        r#"{ "audio": { "language": "xx" }, "index": { "dim": 512 } }"#,
    )
    .unwrap();
    let store = SettingsStore::load(h.path());
    assert_eq!("auto", store.current().audio.language);
    assert_eq!(512, store.current().index.dim);
}

// ---- Sweep3KTests.cs ----

/// Regression B3: unreadable settings refuse saves until reset, preserve `.bad`, and second reset returns false.
#[test]
fn unparsable_file_is_kept_until_the_next_save() {
    let h = TempHome::new(None);
    let path = DataPaths::settings_file(h.path());
    let bad = r#"{"capture":{"consented":true},"inference":{"modelDir":"C:\models"}}"#;
    fs::write(&path, bad).unwrap();
    let s = SettingsStore::load(h.path());
    assert!(!s.current().capture.consented);
    assert!(s.load_error().is_some());
    assert_eq!(bad, fs::read_to_string(&path).unwrap());
    let mut bad_file = path.as_os_str().to_os_string();
    bad_file.push(".bad");
    assert_eq!(bad, fs::read_to_string(PathBuf::from(&bad_file)).unwrap());

    assert_eq!(
        Some(SettingsStore::ERR_UNREADABLE.to_string()),
        s.try_update(&j(r#"{"retention":{"days":60}}"#))
    );
    assert_eq!(bad, fs::read_to_string(&path).unwrap());
    assert!(s.reset_unreadable().unwrap());
    assert!(s.load_error().is_none());
    assert_eq!(None, s.try_update(&j(r#"{"retention":{"days":60}}"#)));
    assert_eq!(60, SettingsStore::read_only(h.path()).retention.days);
    assert!(!s.reset_unreadable().unwrap());
}

#[test]
fn readable_and_missing_files_have_no_load_error() {
    let h = TempHome::new(None);
    assert!(SettingsStore::load(h.path()).load_error().is_none());
    assert!(SettingsStore::load(h.path()).load_error().is_none());
}

// ---- Sweep4UCoreTests.cs ----

/// Regression S6.
#[test]
fn settings_reset_keeps_the_latest_bad_file() {
    let h = TempHome::new(None);
    let path = DataPaths::settings_file(h.path());
    fs::write(&path, r#"{"retention":{"days":7}} x"#).unwrap();
    let s = SettingsStore::load(h.path());
    assert!(s.load_error().is_some());
    fs::write(
        &path,
        r#"{"retention":{"days":9}} edited since the start, still broken"#,
    )
    .unwrap();
    assert!(s.reset_unreadable().unwrap());
    let mut bad_file = path.as_os_str().to_os_string();
    bad_file.push(".bad");
    assert!(
        fs::read_to_string(PathBuf::from(bad_file))
            .unwrap()
            .contains("edited since the start")
    );
}

/// Regressions S4 and C9.
#[test]
fn readonly_during_saves_never_returns_defaults() {
    let h = TempHome::new(Some("dpapi"));
    let s = Arc::new(SettingsStore::load(h.path()));
    assert_eq!(None, s.try_update(&j(r#"{"ui":{"theme":"dark"}}"#)));

    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut readers = Vec::new();
    for _ in 0..4 {
        let stop_c = stop.clone();
        let seen_c = seen.clone();
        let home = h.path().to_path_buf();
        readers.push(thread::spawn(move || {
            while !stop_c.load(Ordering::Relaxed) {
                let theme = SettingsStore::read_only(&home).ui.theme;
                seen_c.lock().unwrap().push(theme);
            }
        }));
    }

    for i in 0..40usize {
        let theme = if i.is_multiple_of(2) { "light" } else { "dark" };
        assert_eq!(
            None,
            s.try_update(&j(&format!(r#"{{"ui":{{"theme":"{theme}"}}}}"#)))
        );
    }
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
    let vals = seen.lock().unwrap();
    assert!(!vals.is_empty());
    assert!(vals.iter().all(|t| t == "dark" || t == "light"));
}

/// Regression C9.
#[test]
fn save_succeeds_while_a_reader_holds_the_file() {
    let h = TempHome::new(Some("dpapi"));
    let s = SettingsStore::load(h.path());
    let path = DataPaths::settings_file(h.path());

    let share_all = (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0;
    {
        let _r = OpenOptions::new()
            .read(true)
            .share_mode(share_all)
            .open(&path)
            .unwrap();
        assert_eq!(None, s.try_update(&j(r#"{"ui":{"theme":"dark"}}"#)));
    }
    assert_eq!("dark", SettingsStore::read_only(h.path()).ui.theme);

    let held = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(&path)
        .unwrap();
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        drop(held);
    });
    assert_eq!(None, s.try_update(&j(r#"{"ui":{"theme":"light"}}"#)));
    release.join().unwrap();
    assert_eq!("light", SettingsStore::read_only(h.path()).ui.theme);
}
