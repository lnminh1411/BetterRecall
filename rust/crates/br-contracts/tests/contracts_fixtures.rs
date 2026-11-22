// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors

use std::fs::{self, OpenOptions};
use std::io::Cursor;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use br_contracts::{
    AgentStatus, BenchDevice, BenchResult, BinaryFrame, DataPaths, Envelope, ErrorReply,
    FrameError, InstallLayout, LineError, LineProtocol, LineReader, McpToolList, ModelsList,
    ModelsProgress, OcrEntity, OcrLine, OcrResult, PipeNames, PurgeResult, SearchResponse,
    Settings, ThumbData, TokenBench, TranscribeResult, TranscriptSegment, WhisperLanguages,
    WorkerHello, WorkerRequest, WorkerResponse, full_path, to_json_string,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use windows::Win32::Storage::FileSystem::{
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, REPLACEFILE_IGNORE_MERGE_ERRORS,
    ReplaceFileW,
};
use windows::core::PCWSTR;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("contracts")
}

fn test_tmp_root() -> PathBuf {
    if let Some(t) = std::env::var_os("BR_TEST_TMP")
        && !t.is_empty()
    {
        return PathBuf::from(t);
    }
    if let Some(t) = std::env::var_os("CARGO_TARGET_DIR")
        && !t.is_empty()
    {
        return PathBuf::from(t).join("test-tmp");
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|d| d.parent()).map(|d| d.join("test-tmp")))
        .expect("unable to determine cargo target dir for test temp folder")
}

struct ScopedDir {
    path: PathBuf,
    junctions: Vec<PathBuf>,
}

impl ScopedDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = test_tmp_root()
            .join("br-contracts-tests")
            .join(format!("{tag}-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).expect("create test dir");
        Self {
            path: dir,
            junctions: Vec::new(),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn add_junction(&mut self, link: PathBuf) {
        self.junctions.push(link);
    }
}

impl Drop for ScopedDir {
    fn drop(&mut self) {
        for j in &self.junctions {
            let _ = fs::remove_dir(j);
        }
        let _ = fs::remove_dir_all(&self.path);
    }
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
    let reserialized = to_json_string(&parsed)
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
    assert_fixture_roundtrip::<Settings>("Settings");
    assert_fixture_roundtrip::<AgentStatus>("AgentStatus");
    assert_fixture_roundtrip::<SearchResponse>("SearchResponse");
    assert_fixture_roundtrip::<ErrorReply>("ErrorReply");
    assert_fixture_roundtrip::<McpToolList>("McpToolList");
    assert_fixture_roundtrip::<ModelsList>("ModelsList");
    assert_fixture_roundtrip::<ModelsProgress>("ModelsProgress");
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
    let sid = PipeNames::user_sid().expect("user_sid");
    assert!(sid.starts_with("S-1-"), "unexpected SID: {sid}");
}

// ---- FramingTests.cs ----

#[test]
fn exactly_4096_bytes_is_accepted_4097_is_rejected() {
    let ok = format!("{}\n", "a".repeat(4096));
    let mut cur = Cursor::new(ok.as_bytes());
    assert_eq!(
        4096,
        LineProtocol::read_line(&mut cur, 4096)
            .unwrap()
            .unwrap()
            .len()
    );

    let bad = format!("{}\n", "a".repeat(4097));
    let mut cur = Cursor::new(bad.as_bytes());
    let err = LineProtocol::read_line(&mut cur, 4096).unwrap_err();
    assert!(matches!(err, LineError::TooLong(4096)));
    assert_eq!(LineProtocol::TOO_LONG_ERROR, err.to_string());

    let mut reader = LineReader::new(Cursor::new(ok.as_bytes()));
    assert_eq!(4096, reader.read_line(4096).unwrap().unwrap().len());
    let mut bad_reader = LineReader::new(Cursor::new(bad.as_bytes()));
    let err = bad_reader.read_line(4096).unwrap_err();
    assert_eq!(LineProtocol::TOO_LONG_ERROR, err.to_string());
}

#[test]
fn bom_and_cr_are_stripped() {
    let mut bytes = vec![0xEFu8, 0xBB, 0xBF];
    bytes.extend_from_slice(b"ping\r\nversion\r\n");
    let mut cur = Cursor::new(bytes.as_slice());
    assert_eq!(
        Some("ping".to_string()),
        LineProtocol::read_line(&mut cur, 4096).unwrap()
    );
    assert_eq!(
        Some("version".to_string()),
        LineProtocol::read_line(&mut cur, 4096).unwrap()
    );
    assert_eq!(None, LineProtocol::read_line(&mut cur, 4096).unwrap());

    let mut reader = LineReader::new(Cursor::new(bytes.as_slice()));
    assert_eq!(Some("ping".to_string()), reader.read_line(4096).unwrap());
    assert_eq!(Some("version".to_string()), reader.read_line(4096).unwrap());
    assert_eq!(None, reader.read_line(4096).unwrap());
}

#[test]
fn last_line_without_newline_and_utf8() {
    let mut cur = Cursor::new("search cà phê 東京".as_bytes());
    assert_eq!(
        Some("search cà phê 東京".to_string()),
        LineProtocol::read_line(&mut cur, 4096).unwrap()
    );
    assert_eq!(None, LineProtocol::read_line(&mut cur, 4096).unwrap());
}

#[test]
fn empty_lines_come_back_empty() {
    let mut cur = Cursor::new(b"\n\r\nping\n".as_slice());
    assert_eq!(
        Some(String::new()),
        LineProtocol::read_line(&mut cur, 4096).unwrap()
    );
    assert_eq!(
        Some(String::new()),
        LineProtocol::read_line(&mut cur, 4096).unwrap()
    );
    assert_eq!(
        Some("ping".to_string()),
        LineProtocol::read_line(&mut cur, 4096).unwrap()
    );
}

#[test]
fn envelope_shapes() {
    assert_eq!(r#"{"ok":true,"data":null}"#, Envelope::ok_raw("null"));
    assert_eq!(
        r#"{"ok":false,"error":"Store is locked."}"#,
        Envelope::error("Store is locked.")
    );
    let (ok, _, err) = Envelope::parse(&Envelope::error("x")).unwrap();
    assert!(!ok);
    assert_eq!(Some("x".to_string()), err);
}

// ---- Sweep4PCoreTests ----

#[test]
fn purge_result_omits_undeleted_when_nothing_is_left() {
    let r = PurgeResult {
        purged: 3,
        undeleted: None,
    };
    assert_eq!(r#"{"purged":3}"#, to_json_string(&r).unwrap());
}

#[test]
fn thumb_data_base64_round_trip() {
    let t = ThumbData {
        id: "s_1".into(),
        mime: "image/webp".into(),
        width: 320,
        height: 180,
        bytes: vec![1, 2, 3, 255],
    };
    let json = to_json_string(&t).unwrap();
    assert!(json.contains(r#""base64":"AQID/w==""#), "json={json}");
    let back: ThumbData = serde_json::from_str(&json).unwrap();
    assert_eq!(t, back);
}

// ---- Sweep2Tests (InstanceScopeTests) ----

fn default_home_path() -> PathBuf {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .expect("LOCALAPPDATA");
    local.join("BetterRecall")
}

#[test]
fn default_home_keeps_plain_names_other_homes_get_the_mutex_suffix() {
    let default = default_home_path();
    let sid = "S-1-5-21-1";
    assert_eq!(format!("BetterRecall.ui.{sid}"), PipeNames::ui(sid, &default));
    assert_eq!(
        format!("BetterRecall.mcp.{sid}"),
        PipeNames::mcp(sid, &default)
    );
    assert_eq!(
        format!("BetterRecall.{sid}"),
        PipeNames::public("BetterRecall", sid, &default)
    );
    assert_eq!(
        PipeNames::SHOW_LAUNCHER_EVENT,
        PipeNames::show_launcher_event_for(&default)
    );
    assert_eq!("BetterRecall.UI", PipeNames::ui_instance_key_for(&default));

    let other = test_tmp_root().join("some-home");
    let mutex_name = PipeNames::agent_mutex_for(&other);
    let suffix = &mutex_name[PipeNames::AGENT_MUTEX.len()..];
    assert_eq!(17, suffix.len());
    assert!(suffix.starts_with('.'));
    assert!(
        suffix[1..]
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'A'..=b'F'))
    );
    assert_eq!(
        format!("BetterRecall.ui.{sid}{suffix}"),
        PipeNames::ui(sid, &other)
    );
    assert_eq!(
        format!("BetterRecall.mcp.{sid}{suffix}"),
        PipeNames::mcp(sid, &other)
    );
    assert_eq!(
        format!("x.{sid}{suffix}"),
        PipeNames::public(r"\\.\pipe\x", sid, &other)
    );
    assert_eq!(
        format!("{}{suffix}", PipeNames::SHOW_LAUNCHER_EVENT),
        PipeNames::show_launcher_event_for(&other)
    );
    assert_eq!(
        format!("BetterRecall.UI{suffix}"),
        PipeNames::ui_instance_key_for(&other)
    );
    let other_slash = PathBuf::from(format!("{}\\", other.display()));
    assert_eq!(
        PipeNames::ui(sid, &other),
        PipeNames::ui(sid, &other_slash)
    );
    assert!(PipeNames::reserved(&PipeNames::ui(sid, &other)));
}

#[test]
fn dev_build_refuses_the_default_home_without_the_flag() {
    let default = default_home_path();
    let dir = ScopedDir::new("dev-guard");
    assert!(!InstallLayout::refuses_home(dir.path(), &default, false));
    fs::write(dir.path().join(InstallLayout::DEV_BUILD_MARKER), "dev").unwrap();
    assert!(InstallLayout::refuses_home(dir.path(), &default, false));
    assert!(!InstallLayout::refuses_home(dir.path(), &default, true));
    let scratch_home = test_tmp_root().join("scratch-home");
    assert!(!InstallLayout::refuses_home(
        dir.path(),
        &scratch_home,
        false
    ));
}

// ---- Sweep4LTests ----

#[test]
fn s2_current_install_never_leaves_the_root() {
    let dir = ScopedDir::new("s4l-s2");
    let root = dir.path();
    let new_folder = "0.2.3-dev-1df40c2e4158";
    let expected = root.join(new_folder);
    fs::create_dir_all(&expected).unwrap();
    let bin = root.join(InstallLayout::BIN_DIR);
    fs::create_dir_all(&bin).unwrap();
    let cur_file = bin.join(InstallLayout::CURRENT_FILE);
    fs::write(&cur_file, expected.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(Some(expected.clone()), InstallLayout::current_install(root, None));

    let other = root.join("other-root");
    fs::create_dir_all(other.join(new_folder)).unwrap();
    fs::write(&cur_file, other.join(new_folder).to_string_lossy().as_bytes()).unwrap();
    assert_eq!(Some(expected.clone()), InstallLayout::current_install(root, None));

    for text in [
        format!(r"\\host\share\{new_folder}"),
        new_folder.to_string(),
        root.join("x")
            .join("..")
            .join("..")
            .join(new_folder)
            .to_string_lossy()
            .into_owned(),
    ] {
        fs::write(&cur_file, text.as_bytes()).unwrap();
        assert_eq!(
            Some(expected.clone()),
            InstallLayout::current_install(root, None),
            "text={text}"
        );
    }

    let real = test_tmp_root().join(format!("real-{}", std::process::id()));
    let real_folder = real.join(new_folder);
    fs::write(&cur_file, real_folder.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(
        Some(full_path(&real_folder).unwrap()),
        InstallLayout::current_install(root, Some(&real))
    );
    assert_eq!(Some(expected), InstallLayout::current_install(root, None));

    fs::write(&cur_file, root.join("tools").to_string_lossy().as_bytes()).unwrap();
    assert_eq!(None, InstallLayout::current_install(root, None));
}

// ---- Sweep5LTests ----

#[test]
fn c2_current_install_shares_delete_so_replace_succeeds_during_concurrent_read() {
    let dir = ScopedDir::new("s5l-c2");
    let root = dir.path();
    let old_folder = "0.2.3-dev-00b55ae338ad";
    let new_folder = "0.2.3-dev-1df40c2e4158";
    let bin = root.join(InstallLayout::BIN_DIR);
    fs::create_dir_all(&bin).unwrap();
    let current_file = bin.join(InstallLayout::CURRENT_FILE);
    let tmp = bin.join("current.tmp");
    fs::write(&current_file, root.join(old_folder).to_string_lossy().as_bytes()).unwrap();

    let share_all = (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0;
    {
        let _reader = OpenOptions::new()
            .read(true)
            .share_mode(share_all)
            .open(&current_file)
            .unwrap();
        assert_eq!(
            Some(root.join(old_folder)),
            InstallLayout::current_install(root, None)
        );
        fs::write(&tmp, root.join(new_folder).to_string_lossy().as_bytes()).unwrap();
        let src_w: Vec<u16> = tmp
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let dst_w: Vec<u16> = current_file
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            ReplaceFileW(
                PCWSTR(dst_w.as_ptr()),
                PCWSTR(src_w.as_ptr()),
                PCWSTR::null(),
                REPLACEFILE_IGNORE_MERGE_ERRORS,
                None,
                None,
            )
            .expect("ReplaceFileW");
        }
    }
    assert_eq!(
        Some(root.join(new_folder)),
        InstallLayout::current_install(root, None)
    );

    let _external = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(&current_file)
        .unwrap();
    assert_eq!(
        Some(root.join(new_folder)),
        InstallLayout::current_install(root, None)
    );
}

#[test]
fn b8_real_path_resolves_junctions() {
    let mut dir = ScopedDir::new("s5l-b8");
    let real_app = dir.path().join("b8-real");
    let ver_real = real_app.join("0.2.4-bbbbbbbbbbbb");
    fs::create_dir_all(&ver_real).unwrap();
    let junc_app = dir.path().join("b8-junc");

    let status = Command::new("cmd.exe")
        .args([
            "/c",
            "mklink",
            "/J",
            &junc_app.to_string_lossy(),
            &real_app.to_string_lossy(),
        ])
        .status();
    let Ok(st) = status else { return };
    if !st.success() || !junc_app.exists() {
        return;
    }
    dir.add_junction(junc_app.clone());

    let expected_real = InstallLayout::real_path(&real_app);
    assert_eq!(expected_real, InstallLayout::real_path(&junc_app));
    assert_eq!(
        expected_real
            .join("0.2.4-bbbbbbbbbbbb")
            .join("nonexistent.exe"),
        InstallLayout::real_path(&junc_app.join("0.2.4-bbbbbbbbbbbb").join("nonexistent.exe"))
    );
}
