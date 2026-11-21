// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use br_contracts::{
    BenchDevice, BenchResult, BinaryFrame, DataPaths, FrameError, InstallLayout, OcrEntity,
    OcrLine, OcrResult, PipeNames, TokenBench, TranscribeResult, TranscriptSegment,
    WhisperLanguages, WorkerHello, WorkerRequest, WorkerResponse, full_path,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("contracts")
}

fn assert_fixture_roundtrip<T>(name: &str)
where
    T: DeserializeOwned + Serialize,
{
    let path = fixtures_dir().join(format!("{name}.json"));
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let parsed: T = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("failed to deserialize {name} from {raw}: {e}"));
    let reserialized = serde_json::to_string(&parsed)
        .unwrap_or_else(|e| panic!("failed to serialize {name}: {e}"));
    assert_eq!(reserialized, raw, "fixture mismatch for {name}");
}

#[test]
fn contract_fixtures_round_trip_identically() {
    assert_fixture_roundtrip::<WorkerRequest>("WorkerRequest");
    assert_fixture_roundtrip::<WorkerResponse>("WorkerResponse");
    assert_fixture_roundtrip::<WorkerHello>("WorkerHello");
    assert_fixture_roundtrip::<TranscriptSegment>("TranscriptSegment");
    assert_fixture_roundtrip::<TranscribeResult>("TranscribeResult");
    assert_fixture_roundtrip::<BenchDevice>("BenchDevice");
    assert_fixture_roundtrip::<TokenBench>("TokenBench");
    assert_fixture_roundtrip::<BenchResult>("BenchResult");
    assert_fixture_roundtrip::<OcrEntity>("OcrEntity");
    assert_fixture_roundtrip::<OcrLine>("OcrLine");
    assert_fixture_roundtrip::<OcrResult>("OcrResult");
}

#[test]
fn frame_write_and_read_round_trip_with_start_token() {
    let mut buf = Vec::new();
    BinaryFrame::write(&mut buf, br#"{"id":1}"#, &[1, 2, 3]).expect("write frame");
    assert_eq!(4 + 8 + 4 + 3, buf.len());

    let mut cur = Cursor::new(buf);
    let (h, p) = BinaryFrame::read(&mut cur)
        .expect("read frame")
        .expect("non-eof");
    assert_eq!(br#"{"id":1}"#, h.as_slice());
    assert_eq!(&[1u8, 2, 3], p.as_slice());
    assert!(BinaryFrame::read(&mut cur).expect("eof").is_none());
}

#[test]
fn frame_rejects_oversized_header_and_mid_frame_eof() {
    let mut too_big = Vec::new();
    too_big.extend_from_slice(&((BinaryFrame::MAX_HEADER_BYTES as u32) + 1).to_le_bytes());
    let mut cur = Cursor::new(too_big);
    assert!(matches!(
        BinaryFrame::read(&mut cur),
        Err(FrameError::HeaderTooLarge)
    ));

    let mut partial = Cursor::new(vec![4u8, 0u8]);
    assert!(matches!(
        BinaryFrame::read(&mut partial),
        Err(FrameError::UnexpectedEof)
    ));
}

#[test]
fn floats_and_bytes_round_trip() {
    let input = [1.5f32, -0.25, 0.0, 42.125];
    let bytes = BinaryFrame::floats_to_bytes(&input);
    assert_eq!(16, bytes.len());
    assert_eq!(input.to_vec(), BinaryFrame::bytes_to_floats(&bytes));
}

#[test]
fn whisper_languages_has_100_codes_and_is_case_sensitive() {
    assert_eq!(100, WhisperLanguages::ALL_CODES.len());
    assert!(WhisperLanguages::is_valid("auto"));
    assert!(WhisperLanguages::is_valid("en"));
    assert!(WhisperLanguages::is_valid("yue"));
    assert!(!WhisperLanguages::is_valid("EN"));
    assert!(!WhisperLanguages::is_valid("xx"));
}

#[test]
fn pipe_names_and_layout_paths() {
    let w = PipeNames::new_worker();
    assert!(w.starts_with("BetterRecall.worker."));
    assert_eq!("BetterRecall.worker.".len() + 32, w.len());
    assert_eq!(w, PipeNames::base(&format!(r"\\.\pipe\{w}")));
    assert_eq!(w, PipeNames::base(&format!(r"\pipe\{w}")));
    assert_eq!(
        PathBuf::from(r"C:\Prog\BR\BetterRecall.Agent.exe"),
        InstallLayout::full(Path::new(r"C:\Prog\BR"), InstallLayout::AGENT_EXE)
    );
    assert_eq!(
        PathBuf::from(r"C:\Prog\BR\BetterRecall.Agent.exe"),
        full_path(Path::new(r"C:\Prog\BR\worker\..\BetterRecall.Agent.exe")).unwrap()
    );
    assert_eq!(
        PathBuf::from(r"C:\home\logs"),
        DataPaths::logs_dir(Path::new(r"C:\home"))
    );
}
