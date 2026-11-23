// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

mod common;

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use br_contracts::{DataPaths, PipeNames, Settings, full_path};
use br_worker::args::WorkerArgs;
use br_worker::hosting::{Log, LogSink, WorkerSettings};
use br_worker::protocol::{AgentPipe, RequestHandler};
use br_worker::vectors::VectorMath;
use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, Utc};
use common::{ScopedDir, test_settings};

// ---- UnitTests (W1 subset) ----

#[test]
fn matryoshka_truncates_and_renormalises() {
    let mut full: Vec<f32> = (1..=768).map(|i| (i as f64).sin() as f32).collect();
    VectorMath::l2_normalize_in_place(&mut full);
    for dim in [768usize, 512, 256] {
        let v = VectorMath::matryoshka(&full, dim).expect("matryoshka");
        assert_eq!(dim, v.len());
        let norm = v.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert!(VectorMath::cosine(&v, &full[..dim]).unwrap() > 0.99999);
    }
    assert!(VectorMath::matryoshka(&full, 1024).is_err());
}

#[test]
fn cosine_and_nan() {
    assert!((VectorMath::cosine(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]).unwrap() - 1.0).abs() < 1e-10);
    assert!((VectorMath::cosine(&[1.0, 0.0], &[-1.0, 0.0]).unwrap() + 1.0).abs() < 1e-10);
    assert!(VectorMath::cosine(&[1.0, f32::NAN], &[1.0, 1.0]).unwrap().is_nan());
    assert!(VectorMath::cosine(&[0.0, 0.0], &[1.0, 1.0]).unwrap().is_nan());
    assert!(VectorMath::has_non_finite(&[1.0, f32::INFINITY]));
    assert!(!VectorMath::has_non_finite(&[1.0, 2.0]));
}

#[test]
fn worker_settings_fall_back_on_bad_and_out_of_range_values() {
    let home = Path::new(r"Z:\no-such-dir");

    // Defaults
    let w = WorkerSettings::from(&Settings::default(), home);
    assert_eq!("auto", w.nsfw_device);
    assert_eq!("auto", w.nsfw_gpu_adapter);
    assert!(w.nsfw_filter && w.nsfw_detector);
    assert_eq!("auto", w.gpu_adapter);

    // Valid overrides (trimmed / lowercased)
    let mut s = Settings::default();
    s.privacy.nsfw_device = " IGPU ".into();
    s.privacy.nsfw_gpu_adapter = " RTX 3050 ".into();
    s.privacy.nsfw_detector = false;
    s.inference.gpu_adapter = " RTX 3050 ".into();
    s.audio.language = "DE".into();
    let w = WorkerSettings::from(&s, home);
    assert_eq!("igpu", w.nsfw_device);
    assert_eq!("RTX 3050", w.nsfw_gpu_adapter);
    assert!(!w.nsfw_detector);
    assert_eq!("RTX 3050", w.gpu_adapter);
    assert_eq!("de", w.audio_language);

    // Bad / out-of-range values fall back to defaults.
    let mut s = Settings::default();
    s.privacy.nsfw_device = "tpu".into();
    s.privacy.nsfw_gpu_adapter = "   ".into();
    s.inference.gpu_adapter = "   ".into();
    s.index.dim = 128;
    s.inference.image_tokens = 1120;
    s.inference.device = "TPU".into();
    s.audio.language = "xx".into();
    let w = WorkerSettings::from(&s, Path::new(r"C:\home"));
    assert_eq!("auto", w.nsfw_device);
    assert_eq!("auto", w.nsfw_gpu_adapter);
    assert_eq!("auto", w.gpu_adapter);
    assert_eq!(768, w.dim);
    assert_eq!(140, w.image_tokens);
    assert_eq!("auto", w.device);
    assert_eq!(PathBuf::from(r"C:\home\models"), w.model_dir);
    assert_eq!("auto", w.audio_language);

    // Overlong gpu_adapter falls back to auto.
    let mut s = Settings::default();
    s.inference.gpu_adapter = "a".repeat(129);
    assert_eq!("auto", WorkerSettings::from(&s, home).gpu_adapter);
}

#[test]
fn args_and_pipe_names() {
    let name = PipeNames::new_worker();
    assert_eq!(
        Some(name.clone()),
        AgentPipe::normalize_name(Some(&format!(r"\\.\pipe\{name}")))
    );
    assert_eq!(None, AgentPipe::normalize_name(Some("BetterRecall.ui.S-1-5")));
    assert_eq!(
        None,
        AgentPipe::normalize_name(Some("BetterRecall.worker.XYZ"))
    );
    assert!(WorkerArgs::parse(&["--pipe", &name, "--home", r"C:\x"]).is_some());
    assert!(WorkerArgs::parse(&["--pipe", &name]).is_none());
    assert!(WorkerArgs::parse(&["--pipe", "evil", "--home", r"C:\x"]).is_none());
    assert!(WorkerArgs::parse(&["--pipe", &name, "--home", r"C:\x", "--extra"]).is_none());
    assert_eq!(
        PathBuf::from(r"C:\Prog\BR\BetterRecall.Agent.exe"),
        AgentPipe::expected_agent_image(Path::new(r"C:\Prog\BR\worker\"))
    );
}

#[test]
fn final_path_resolves_a_junction() {
    let dir = ScopedDir::new("junction");
    let real = dir.path().join("real");
    let link = dir.path().join("link");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("a.exe"), [1u8, 2, 3]).unwrap();

    let status = Command::new("cmd.exe")
        .args([
            "/c",
            "mklink",
            "/J",
            &link.to_string_lossy(),
            &real.to_string_lossy(),
        ])
        .status()
        .expect("run cmd.exe /c mklink /J");
    assert!(
        status.success() && link.exists(),
        "mklink /J failed with {status:?} (link exists: {})",
        link.exists()
    );

    let real_final = AgentPipe::final_path(&real.join("a.exe"));
    let link_final = AgentPipe::final_path(&link.join("a.exe"));
    assert!(
        real_final
            .to_string_lossy()
            .eq_ignore_ascii_case(&link_final.to_string_lossy()),
        "{} != {}",
        real_final.display(),
        link_final.display()
    );
    assert!(!real_final.to_string_lossy().starts_with(r"\\?\"));
    assert_eq!(
        full_path(&dir.path().join("missing.exe")).unwrap(),
        AgentPipe::final_path(&dir.path().join("missing.exe"))
    );
}


#[test]
fn language_must_be_a_whisper_code() {
    let cases = [
        ("auto", true),
        ("en", true),
        ("vi", true),
        ("haw", true),
        ("yue", true),
        ("xx", false),
        ("zz", false),
        ("eng", false),
        ("EN", false),
    ];
    for (lang, ok) in cases {
        assert_eq!(ok, RequestHandler::is_language(lang), "lang={lang}");
    }
}

#[test]
fn settings_with_bom_comments_and_trailing_commas_are_read() {
    let dir = ScopedDir::new("lenient");
    let json = concat!(
        "\u{FEFF}{\n",
        "  // set by hand\n",
        "  \"inference\": { \"device\": \"cpu\", \"idleExitS\": 77, },\n",
        "  \"log\": { \"level\": \"debug\" },\n",
        "}\n"
    );
    fs::write(DataPaths::settings_file(dir.path()), json.as_bytes()).unwrap();
    let s = WorkerSettings::load(dir.path());
    assert_eq!("cpu", s.device);
    assert_eq!(77, s.idle_exit_s);
    assert_eq!("debug", s.log_level);
}

#[test]
fn a_key_of_the_wrong_type_keeps_only_that_default() {
    let parsed = WorkerSettings::parse(
        r#"{ "inference": { "device": "cpu", "idleExitS": "soon" }, "index": { "dim": 512 } }"#,
    )
    .expect("parse");
    assert_eq!("cpu", parsed.inference.device);
    assert_eq!(
        Settings::default().inference.idle_exit_s,
        parsed.inference.idle_exit_s
    );
    assert_eq!(512, parsed.index.dim);
}


#[test]
fn agent_settings_input_wins_over_settings_json_and_fails_closed() {
    let dir = ScopedDir::new("s4w-args");
    let file = test_settings(dir.path(), |x| {
        x.index.dim = 768;
        x.inference.device = "cpu".into();
        x.ocr.languages = "de".into();
    });
    assert_eq!(768, file.dim);
    let s = WorkerSettings::try_from_json(
        r#"{"index":{"dim":512},"ocr":{"languages":"en"}}"#,
        dir.path(),
    )
    .expect("try_from_json");
    assert_eq!(512, s.dim);
    assert_eq!("en", s.ocr_languages);
    assert_eq!("auto", s.device);
    assert_eq!(140, s.image_tokens);
    assert!(WorkerSettings::try_from_json("not json", dir.path()).is_err());
    assert!(WorkerSettings::try_from_json("[1,2]", dir.path()).is_err());

    let read = |b: &[u8]| -> Result<String, String> {
        let mut cur = Cursor::new(b);
        WorkerSettings::read_agent_input(&mut cur)
    };
    assert_eq!(Ok("{}".to_string()), read(b"{}"));
    assert_eq!(Err("missing".to_string()), read(&[]));
    let oversized = vec![b' '; WorkerSettings::MAX_AGENT_INPUT_BYTES + 1];
    assert!(read(&oversized).unwrap_err().starts_with("larger than"));
    assert!(read(&[0xC3, 0x28]).is_err());

    let name = PipeNames::new_worker();
    let home_str = dir.path().to_string_lossy().into_owned();
    assert!(
        WorkerArgs::parse(&["--pipe", &name, "--home", &home_str, "--settings-stdin"])
            .unwrap()
            .settings_from_stdin
    );
    assert!(
        !WorkerArgs::parse(&["--pipe", &name, "--home", &home_str])
            .unwrap()
            .settings_from_stdin
    );
    assert!(WorkerArgs::parse(&["--pipe", &name, "--home", &home_str, "--settings", "{}"])
        .is_none());
    assert!(WorkerArgs::parse(&["--pipe", &name, "--home"]).is_none());
}

// ---- Sweep5WWorkerTests (W1 log rotation/retention) ----

fn to_system_time(dt: DateTime<FixedOffset>) -> SystemTime {
    let secs = dt.timestamp().max(0) as u64;
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

#[test]
fn c8_worker_log_rotates_at_10mib_and_prunes_beyond_14_files() {
    let dir = ScopedDir::new("s5w-log");
    let utc = FixedOffset::east_opt(0).unwrap();
    let start_day = Utc::now().date_naive().and_hms_opt(10, 0, 0).unwrap();
    let now_base = DateTime::<FixedOffset>::from_naive_utc_and_offset(start_day, utc);
    let clock_val = Arc::new(Mutex::new(now_base));

    let mut old_names = Vec::new();
    for d in 1..=18 {
        let old_date = now_base - ChronoDuration::days(30 - d);
        let name = format!("worker-{}.log", old_date.format("%Y%m%d"));
        old_names.push(name.clone());
        let old_path = dir.path().join(&name);
        fs::write(&old_path, "old\n").unwrap();
        let f = OpenOptions::new().write(true).open(&old_path).unwrap();
        f.set_modified(to_system_time(old_date)).unwrap();
    }

    let today_file = dir
        .path()
        .join(format!("worker-{}.log", now_base.format("%Y%m%d")));
    {
        let f = File::create(&today_file).unwrap();
        f.set_len(Log::MAX_FILE_BYTES).unwrap();
        f.set_modified(to_system_time(now_base)).unwrap();
    }

    let c = clock_val.clone();
    let mut sink = LogSink::with_clock(move || *c.lock().unwrap());
    sink.init(Some(dir.path().to_path_buf()), "debug");

    let list_names = || -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("worker-") && n.ends_with(".log"))
            .collect();
        v.sort();
        v
    };

    let after_init: HashSet<String> = list_names().into_iter().collect();
    assert_eq!(Log::KEEP_FILES, after_init.len());
    assert!(!after_init.contains(&old_names[4]));
    assert!(after_init.contains(&old_names[5]));

    sink.write(0, "DBG", "first-rotated");
    let first_rot = dir
        .path()
        .join(format!("worker-{}-100000.log", now_base.format("%Y%m%d")));
    assert!(first_rot.exists());
    assert!(fs::read_to_string(&first_rot).unwrap().contains("first-rotated"));
    assert_eq!(Log::MAX_FILE_BYTES, fs::metadata(&today_file).unwrap().len());
    let after_first: HashSet<String> = list_names().into_iter().collect();
    assert_eq!(Log::KEEP_FILES, after_first.len());
    assert!(!after_first.contains(&old_names[5]));

    {
        let f = OpenOptions::new().write(true).open(&first_rot).unwrap();
        f.set_len(Log::MAX_FILE_BYTES).unwrap();
    }
    *clock_val.lock().unwrap() = now_base + ChronoDuration::seconds(1);
    sink.write(1, "INF", "second-rotated");
    let second_rot = dir
        .path()
        .join(format!("worker-{}-100001.log", now_base.format("%Y%m%d")));
    assert!(second_rot.exists());
    assert!(
        fs::read_to_string(&second_rot)
            .unwrap()
            .contains("second-rotated")
    );
    assert_eq!(Log::KEEP_FILES, list_names().len());
}

#[test]
fn c8_static_log_facade_writes_and_prunes_beyond_14_files() {
    let dir = ScopedDir::new("s5w-log-static");
    let utc = FixedOffset::east_opt(0).unwrap();
    let today = Utc::now().date_naive().and_hms_opt(0, 0, 0).unwrap();
    let today_dt = DateTime::<FixedOffset>::from_naive_utc_and_offset(today, utc);
    let mut oldest_path: Option<PathBuf> = None;
    for d in 1..=16 {
        let old_date = today_dt - ChronoDuration::days(30 - d);
        let old = dir
            .path()
            .join(format!("worker-{}.log", old_date.format("%Y%m%d")));
        if oldest_path.is_none() {
            oldest_path = Some(old.clone());
        }
        fs::write(&old, "old\n").unwrap();
        let f = OpenOptions::new().write(true).open(&old).unwrap();
        f.set_modified(to_system_time(old_date)).unwrap();
    }

    Log::init(Some(dir.path().to_path_buf()), "info");
    Log::info("static-probe");
    Log::init(None, "info");

    let log_files: Vec<PathBuf> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy();
            n.starts_with("worker-") && n.ends_with(".log")
        })
        .collect();
    assert_eq!(Log::KEEP_FILES, log_files.len());
    assert!(!oldest_path.unwrap().exists());
    let wrote_probe = log_files
        .iter()
        .any(|p| fs::read_to_string(p).unwrap_or_default().contains("static-probe"));
    assert!(wrote_probe, "today's worker-*.log must contain 'static-probe'");
}
