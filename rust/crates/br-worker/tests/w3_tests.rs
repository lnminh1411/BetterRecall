#![allow(clippy::type_complexity, clippy::nonminimal_bool)] // test fixtures and readable assertions
#![allow(
    clippy::too_many_arguments,
    clippy::identity_op,
    clippy::cloned_ref_to_slice_refs,
    clippy::needless_range_loop
)]
// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use br_contracts::{DataPaths, OcrEntity, OcrLine, Settings, WorkerRequest, WorkerResponse};
use br_worker::audio::SileroVad;
use br_worker::engine::{IWorkerEngine, ImageInput, RealEngine};
use br_worker::hosting::WorkerSettings;
use br_worker::inference::device_selection::DevicePolicy;
use br_worker::inference::onnx::OnnxBackend;
use br_worker::inference::{
    DeviceInfo, DeviceKind, HostTensor, IInferenceBackend, IInferenceSession, TensorSpec,
    TensorType,
};
use br_worker::models::catalog::ModelCatalog;
use br_worker::models::image_pipeline::RgbImage;
use br_worker::models::manifest::{
    FusionGraph, ManifestFile, ModelManifest, NerSpec, OcrDetSpec, OcrRecSpec, OcrSpec,
    PromptSpec, TextGraph, TokenizerSpec, VisionGraph,
};
use br_worker::models::ner::{BpeTokenizer, NerGate, PiiNer};
use br_worker::models::ocr::{CtcResult, OcrMath, PaddleOcr, WindowsOcr};
use br_worker::protocol::{RequestHandler, WorkerError};

// ---- Test helpers (local to W3 suite) ----

struct ScopedDir {
    path: PathBuf,
}

impl ScopedDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let base = std::env::var_os("BR_TEST_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base
            .join("betterrecall-worker-rust")
            .join(format!("w3-{tag}-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).expect("create test temp dir");
        Self { path: dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScopedDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn data_path(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../tests/BetterRecall.Worker.Tests/TestData")
        .join(file)
}

fn test_settings(home: &Path, edit: impl FnOnce(&mut Settings)) -> WorkerSettings {
    let mut s = Settings::default();
    s.inference.model_dir = home.join("models").to_string_lossy().into_owned();
    edit(&mut s);
    fs::create_dir_all(home).expect("create home dir");
    let json = serde_json::to_vec(&s).expect("serialize settings");
    fs::write(DataPaths::settings_file(home), json).expect("write settings.json");
    WorkerSettings::from(&s, home)
}

fn copy_model_file(dir: &Path, src: &str, dst: &str) -> ManifestFile {
    fs::create_dir_all(dir).expect("create model dir");
    let to = dir.join(dst);
    fs::copy(data_path(src), &to).expect("copy test data file");
    let size = fs::metadata(&to).expect("stat copied file").len() as i64;
    ManifestFile {
        file: dst.to_string(),
        size,
        sha256: String::new(),
        source: None,
    }
}

fn write_stub_file(dir: &Path, name: &str, bytes: &[u8]) -> ManifestFile {
    fs::create_dir_all(dir).expect("create model dir");
    let p = dir.join(name);
    fs::write(&p, bytes).expect("write stub file");
    ManifestFile {
        file: name.to_string(),
        size: bytes.len() as i64,
        sha256: String::new(),
        source: None,
    }
}

fn write_manifest(dir: &Path, m: &ModelManifest) {
    let json = serde_json::to_vec_pretty(m).expect("serialize manifest");
    fs::write(dir.join("manifest.json"), json).expect("write manifest.json");
}

fn bgra_pixel_bytes(w: usize, h: usize) -> Vec<u8> {
    vec![255u8; w * h * 4]
}

fn send_req(h: &RequestHandler<'_>, r: &WorkerRequest, payload: &[u8]) -> WorkerResponse {
    let json = serde_json::to_vec(r).expect("serialize request");
    h.handle(&json, payload).header
}

fn dev_cpu() -> DeviceInfo {
    DeviceInfo::new(DeviceKind::Cpu, "CPU", "CPU", "CPU", None)
}

fn dev_igpu() -> DeviceInfo {
    DeviceInfo::new(DeviceKind::Gpu, "GPU.0", "Intel Arc", "OpenVINO", Some(true))
}

fn dev_npu() -> DeviceInfo {
    DeviceInfo::new(DeviceKind::Npu, "NPU", "Intel AI Boost", "OpenVINO", None)
}

fn dev_dml() -> DeviceInfo {
    DeviceInfo::new(DeviceKind::Gpu, "DML:0", "Discrete GPU", "DirectML", None)
}

fn spec(name: &str, ty: TensorType, shape: &[i64]) -> TensorSpec {
    TensorSpec {
        name: name.to_string(),
        ty,
        shape: shape.to_vec(),
    }
}

struct FnSession {
    device: DeviceInfo,
    inputs: Vec<TensorSpec>,
    outputs: Vec<TensorSpec>,
    handler: Box<dyn Fn(&[HostTensor]) -> Result<HashMap<String, HostTensor>, WorkerError> + Send + Sync>,
}

impl IInferenceSession for FnSession {
    fn device(&self) -> &DeviceInfo {
        &self.device
    }
    fn inputs(&self) -> &[TensorSpec] {
        &self.inputs
    }
    fn outputs(&self) -> &[TensorSpec] {
        &self.outputs
    }
    fn run(&self, inputs: &[HostTensor]) -> Result<HashMap<String, HostTensor>, WorkerError> {
        (self.handler)(inputs)
    }
}

struct MockBackend {
    name: &'static str,
    is_ov: bool,
    devices: Vec<DeviceInfo>,
    loader: Box<
        dyn Fn(&Path, &DeviceInfo, Option<&HashMap<String, Vec<i64>>>) -> Result<Box<dyn IInferenceSession>, WorkerError>
            + Send
            + Sync,
    >,
}

impl IInferenceBackend for MockBackend {
    fn name(&self) -> &str {
        self.name
    }
    fn version(&self) -> &str {
        "1.0"
    }
    fn npu_driver_version(&self) -> &str {
        ""
    }
    fn devices(&self) -> &[DeviceInfo] {
        &self.devices
    }
    fn is_openvino_backend(&self) -> bool {
        self.is_ov
    }
    fn is_onnx_backend(&self) -> bool {
        !self.is_ov
    }
    fn load(
        &self,
        model_path: &Path,
        device: &DeviceInfo,
        _cache_dir: Option<&Path>,
        static_shapes: Option<&HashMap<String, Vec<i64>>>,
    ) -> Result<Box<dyn IInferenceSession>, WorkerError> {
        (self.loader)(model_path, device, static_shapes)
    }
}

fn install_mock_ocr(model_dir: &Path, mean: Vec<f32>, std: Vec<f32>) {
    let dir = ModelCatalog::dir(model_dir, ModelCatalog::OCR);
    let dict = "a\nb\nc\nd\ne\nf\nC\nJ\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\nu\nS\nA";
    let files = vec![
        write_stub_file(&dir, "det.onnx", b"stub-det"),
        write_stub_file(&dir, "rec.onnx", b"stub-rec"),
        write_stub_file(&dir, "dict-main.txt", dict.as_bytes()),
    ];
    let m = ModelManifest {
        name: ModelCatalog::OCR.to_string(),
        id: "synthetic/ocr".to_string(),
        revision: "syn1".to_string(),
        ocr: Some(OcrSpec {
            det: OcrDetSpec {
                file: "det.onnx".to_string(),
                input: "x".to_string(),
                thresh: 0.2,
                box_thresh: 0.45,
                unclip: 1.0,
                mean,
                std,
                max_side: 320,
            },
            rec: vec![OcrRecSpec {
                name: "main".to_string(),
                file: "rec.onnx".to_string(),
                dict: "dict-main.txt".to_string(),
                input: "x".to_string(),
                height: 48,
                languages: vec!["en".to_string()],
                classes: 26,
            }],
        }),
        files,
        ..ModelManifest::default()
    };
    write_manifest(&dir, &m);
}

fn sample_bpe_tokenizer_json() -> String {
    let pairs: &[(&str, i32)] = &[
        ("<pad>", 0),
        ("<eos>", 1),
        ("<bos>", 2),
        ("<unk>", 3),
        ("\u{2581}", 4),
        ("a", 5),
        ("b", 6),
        ("ab", 7),
        ("\u{2581}ab", 8),
        ("<0xF0>", 12),
        ("<0x9F>", 13),
        ("<0x98>", 14),
        ("<0x80>", 15),
        ("c", 20),
        ("l", 21),
        ("\u{2581}c", 22),
        ("\u{2581}ca", 23),
        ("\u{2581}cal", 24),
        ("\u{2581}call", 25),
        ("j", 26),
        ("o", 27),
        ("h", 28),
        ("n", 29),
        ("jo", 30),
        ("joh", 31),
        ("john", 32),
    ];
    let vocab: serde_json::Map<String, serde_json::Value> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), serde_json::Value::from(*v)))
        .collect();
    serde_json::json!({
        "model": {
            "type": "BPE",
            "vocab": vocab,
            "merges": [
                "a b",
                "\u{2581} ab",
                "\u{2581} c",
                "\u{2581}c a",
                "\u{2581}ca l",
                "\u{2581}cal l",
                "j o",
                "jo h",
                "joh n"
            ]
        },
        "added_tokens": [
            { "id": 100, "content": "<mask>" }
        ]
    })
    .to_string()
}

fn install_mock_ner(model_dir: &Path, tokenizer_json: Option<&str>) {
    let dir = ModelCatalog::dir(model_dir, ModelCatalog::NER);
    let tok_text = tokenizer_json
        .map(ToString::to_string)
        .unwrap_or_else(sample_bpe_tokenizer_json);
    let files = vec![
        write_stub_file(&dir, "model.onnx", b"stub-ner"),
        write_stub_file(&dir, "tokenizer.json", tok_text.as_bytes()),
    ];
    let mut entities = HashMap::new();
    entities.insert("GIVEN_NAME".to_string(), "person".to_string());
    entities.insert("SURNAME".to_string(), "person".to_string());
    entities.insert("STREET_ADDRESS".to_string(), "address".to_string());
    let m = ModelManifest {
        name: ModelCatalog::NER.to_string(),
        id: "synthetic/ner".to_string(),
        revision: "syn1".to_string(),
        ner: Some(NerSpec {
            file: "model.onnx".to_string(),
            tokenizer: "tokenizer.json".to_string(),
            input_ids: "input_ids".to_string(),
            attention_mask: "attention_mask".to_string(),
            output: "logits".to_string(),
            max_tokens: 64,
            labels: vec![
                "O".to_string(),
                "B-GIVEN_NAME".to_string(),
                "I-GIVEN_NAME".to_string(),
                "B-SURNAME".to_string(),
                "B-STREET_ADDRESS".to_string(),
            ],
            entities,
        }),
        files,
        ..ModelManifest::default()
    };
    write_manifest(&dir, &m);
}

fn paint_box(prob: &mut [f32], pw: i32, x0: i32, y0: i32, x1: i32, y1: i32, v: f32) {
    for y in y0..y1 {
        for x in x0..x1 {
            prob[(y * pw + x) as usize] = v;
        }
    }
}

fn rec_logits_for_indices(indices: &[usize], classes: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; indices.len() * classes];
    for (s, &cls) in indices.iter().enumerate() {
        out[s * classes + cls] = 0.9;
    }
    out
}

/// Renders dark text on a white canvas using Win32 GDI so `WindowsOcrTests` has zero external font/image assets.
fn render_text_rgb(text: &str, width: i32, height: i32, x: i32, y: i32, size: i32) -> RgbImage {
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Gdi::{
        ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS,
        CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DEFAULT_PITCH,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, FF_SWISS, FW_NORMAL, GdiFlush, HGDIOBJ,
        OUT_DEFAULT_PRECIS, PatBlt, SelectObject, SetBkMode, SetTextColor, TRANSPARENT, TextOutW,
        WHITENESS,
    };
    use windows::core::w;

    struct GdiCleanup {
        hdc: windows::Win32::Graphics::Gdi::HDC,
        old_bmp: HGDIOBJ,
        hbmp: windows::Win32::Graphics::Gdi::HBITMAP,
        old_font: HGDIOBJ,
        hfont: windows::Win32::Graphics::Gdi::HFONT,
    }

    impl Drop for GdiCleanup {
        fn drop(&mut self) {
            unsafe {
                if !self.old_font.is_invalid() {
                    SelectObject(self.hdc, self.old_font);
                }
                if !self.hfont.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(self.hfont.0));
                }
                if !self.old_bmp.is_invalid() {
                    SelectObject(self.hdc, self.old_bmp);
                }
                if !self.hbmp.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(self.hbmp.0));
                }
                if !self.hdc.is_invalid() {
                    let _ = DeleteDC(self.hdc);
                }
            }
        }
    }

    unsafe {
        let hdc = CreateCompatibleDC(None);
        assert!(!hdc.is_invalid(), "CreateCompatibleDC failed");
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down DIB
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .expect("CreateDIBSection failed");
        let old_bmp = SelectObject(hdc, HGDIOBJ(hbmp.0));
        let hfont = CreateFontW(
            -size,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
            w!("Segoe UI"),
        );
        let old_font = SelectObject(hdc, HGDIOBJ(hfont.0));
        let _guard = GdiCleanup {
            hdc,
            old_bmp,
            hbmp,
            old_font,
            hfont,
        };

        let _ = PatBlt(hdc, 0, 0, width, height, WHITENESS);
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(0x0014_1414));
        let wide: Vec<u16> = text.encode_utf16().collect();
        let _ = TextOutW(hdc, x, y, &wide);
        let _ = GdiFlush();

        let n = (width * height) as usize;
        let bgra = std::slice::from_raw_parts(bits as *const u8, n * 4);
        let mut rgb = vec![0u8; n * 3];
        for i in 0..n {
            rgb[i * 3] = bgra[i * 4 + 2];
            rgb[i * 3 + 1] = bgra[i * 4 + 1];
            rgb[i * 3 + 2] = bgra[i * 4];
        }
        RgbImage { width, height, rgb }
    }
}

// ---- OcrTests: OcrMathTests ----

#[test]
fn det_size_rounds_to_32_and_caps_the_longer_side() {
    assert_eq!((1920, 1088), OcrMath::det_size(1920, 1080, 1920));
    assert_eq!((1920, 1088), OcrMath::det_size(3840, 2160, 1920));
    assert_eq!((128, 96), OcrMath::det_size(120, 80, 1920));
    assert_eq!((32, 32), OcrMath::det_size(20, 20, 1920));
}

#[test]
fn bgr_tensor_normalizes_in_bgr_order_and_rejects_short_params() {
    let img = RgbImage {
        width: 1,
        height: 1,
        rgb: vec![255, 128, 0], // R=255, G=128, B=0
    };
    let mean = [0.0, 0.5, 1.0];
    let std = [1.0, 0.5, 0.5];
    let t = OcrMath::bgr_tensor(&img, &mean, &std).unwrap();
    assert!((t[0] - 0.0).abs() < 1e-5); // B=0 -> (0-0)/1 = 0
    assert!((t[1] - ((128.0 / 255.0 - 0.5) / 0.5)).abs() < 1e-5); // G
    assert!((t[2] - 0.0).abs() < 1e-5); // R=255 -> (1-1)/0.5 = 0

    let err = OcrMath::bgr_tensor(&img, &[], &std).unwrap_err();
    assert!(matches!(err, WorkerError::Internal(_)));
}

#[test]
fn ctc_decode_collapses_repeats_and_splits_on_blanks() {
    let table = OcrMath::table(&["a".into(), "b".into()]);
    assert_eq!(vec!["", "a", "b", " "], table);
    let probs = rec_logits_for_indices(&[1, 1, 0, 1, 2, 3, 2], table.len());
    let ctc = OcrMath::ctc(&probs, 7, table.len(), &table);
    assert_eq!("aab b", ctc.text);
    assert!((ctc.confidence - 0.9).abs() < 1e-5);
    assert_eq!(vec![0, 3, 4, 5, 6], ctc.step_start);
    assert_eq!(vec![1, 3, 4, 5, 6], ctc.step_end);
}

#[test]
fn ctc_and_cuts_handle_surrogate_pairs_monotonically() {
    let table = OcrMath::table(&["😀".into(), "x".into()]);
    let probs = rec_logits_for_indices(&[1, 1, 0, 2, 0, 0], table.len());
    let ctc = OcrMath::ctc(&probs, 6, table.len(), &table);
    assert_eq!("😀x", ctc.text);
    assert_eq!(3, ctc.text.encode_utf16().count());
    assert_eq!(vec![0, 0, 3], ctc.step_start);
    assert_eq!(vec![1, 1, 3], ctc.step_end);
    let cuts = OcrMath::cuts(&ctc, 6, 256, 160, 100, 80);
    assert_eq!(4, cuts.len());
    assert_eq!(100, cuts[0]);
    assert_eq!(180, cuts[3]);
    for w in cuts.windows(2) {
        assert!(w[1] >= w[0]);
    }

    // Malformed / short / zero inputs must never panic.
    let truncated = OcrMath::ctc(&probs[..12], 6, table.len(), &table);
    assert_eq!("😀", truncated.text);
    assert!(OcrMath::ctc(&probs, 6, 0, &table).text.is_empty());
    assert_eq!(vec![100, 126, 153, 180], OcrMath::cuts(&ctc, 0, 256, 160, 100, 80));
}

#[test]
fn cuts_cover_text_monotonically_inside_the_crop() {
    let ctc = CtcResult {
        text: "abc".to_string(),
        confidence: 0.95,
        step_start: vec![1, 4, 8],
        step_end: vec![2, 5, 9],
    };
    let cuts = OcrMath::cuts(&ctc, 20, 256, 160, 100, 80);
    assert_eq!(4, cuts.len());
    assert_eq!(100, cuts[0]);
    assert_eq!(180, cuts[3]);
    assert!(cuts[1] > cuts[0] && cuts[2] >= cuts[1] && cuts[3] >= cuts[2]);
}

#[test]
fn pick_recognisers_always_keeps_main_and_picks_requested_scripts() {
    let rec = |name: &str, langs: &[&str]| OcrRecSpec {
        name: name.to_string(),
        languages: langs.iter().map(|s| (*s).to_string()).collect(),
        ..OcrRecSpec::default()
    };
    let all = vec![
        rec("main", &["en", "de"]),
        rec("latin", &["fr", "vi"]),
        rec("cyrillic", &["ru", "uk"]),
        rec("korean", &["ko"]),
    ];
    let p1 = OcrMath::pick_recognisers(&all, &["en-US".into(), "ru-RU".into()]);
    let n1: Vec<&str> = p1.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(vec!["main", "cyrillic"], n1);

    let p2 = OcrMath::pick_recognisers(&all, &["ja-JP".into()]);
    let n2: Vec<&str> = p2.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(vec!["main"], n2);
}

#[test]
fn boxes_extracts_separated_regions_above_thresh() {
    let pw = 64;
    let ph = 64;
    let mut prob = vec![0.0f32; (pw * ph) as usize];
    paint_box(&mut prob, pw, 8, 10, 24, 16, 0.9);
    paint_box(&mut prob, pw, 12, 34, 48, 42, 0.85);
    paint_box(&mut prob, pw, 4, 50, 12, 54, 0.35); // below box_thresh 0.45

    let exact = OcrMath::boxes(&prob, pw, ph, 128, 128, 0.2, 0.45, 0.0, 3000).unwrap();
    assert_eq!(2, exact.len());
    assert!(exact[0].y < exact[1].y);
    assert_eq!(16, exact[0].x);
    assert_eq!(20, exact[0].y);
    assert_eq!(32, exact[0].w);
    assert_eq!(12, exact[0].h);

    let unclipped = OcrMath::boxes(&prob, pw, ph, 128, 128, 0.2, 0.45, 1.0, 3000).unwrap();
    assert_eq!(2, unclipped.len());
    assert!(unclipped[0].x < 16 && unclipped[0].y < 20);
}

#[test]
fn rec_bucket_picks_smallest_fitting_width_up_to_3200() {
    assert_eq!(256, OcrMath::rec_bucket(10));
    assert_eq!(256, OcrMath::rec_bucket(256));
    assert_eq!(512, OcrMath::rec_bucket(257));
    assert_eq!(1536, OcrMath::rec_bucket(1500));
    assert_eq!(3200, OcrMath::rec_bucket(4000));
}

// ---- OcrTests: BpeTokenizerTests ----

#[test]
fn bpe_tokenizer_encodes_metaspaces_added_tokens_and_byte_fallback_with_utf16_spans() {
    let dir = ScopedDir::new("bpe");
    let path = dir.path().join("tokenizer.json");
    fs::write(&path, sample_bpe_tokenizer_json()).unwrap();
    let tok = BpeTokenizer::load(&path).unwrap();

    let text = "ab 😀<mask>";
    let toks = tok.encode(text);
    // "ab" -> 8 ("\u{2581}ab", [0,2)), " " -> 4 ("\u{2581}", [2,3)), "😀" -> 4 byte tokens ([3,5)), "<mask>" -> 100 ([5,11))
    assert_eq!(7, toks.len());
    assert_eq!(8, toks[0].id);
    assert_eq!((0, 2), (toks[0].start, toks[0].end));
    assert_eq!(4, toks[1].id);
    assert_eq!((2, 3), (toks[1].start, toks[1].end));

    let emoji_ids: Vec<i32> = toks[2..6].iter().map(|t| t.id).collect();
    assert_eq!(vec![12, 13, 14, 15], emoji_ids);
    for t in &toks[2..6] {
        assert_eq!((3, 5), (t.start, t.end));
    }
    let last = *toks.last().unwrap();
    assert_eq!(100, last.id);
    assert_eq!((5, 11), (last.start, last.end));
}

#[test]
fn bpe_tokenizer_handles_empty_and_whitespace() {
    let dir = ScopedDir::new("bpe-empty");
    let path = dir.path().join("tokenizer.json");
    fs::write(&path, sample_bpe_tokenizer_json()).unwrap();
    let tok = BpeTokenizer::load(&path).unwrap();
    assert!(tok.encode("").is_empty());
    let spaces = tok.encode("   ");
    assert_eq!(vec![4, 4, 4], spaces.iter().map(|t| t.id).collect::<Vec<_>>());
}

// ---- OcrTests: PiiNerLogicTests ----

#[test]
fn pii_ner_tags_maps_bio_prefixes_and_groups() {
    let mut entities = HashMap::new();
    entities.insert("GIVEN_NAME".into(), "person".into());
    entities.insert("CITY".into(), "address".into());
    let spec = NerSpec {
        labels: vec![
            "O".into(),
            "B-GIVEN_NAME".into(),
            "I-GIVEN_NAME".into(),
            "E-GIVEN_NAME".into(),
            "S-CITY".into(),
            "CITY".into(),
            "EMAIL".into(),
        ],
        entities,
        ..NerSpec::default()
    };
    let tags = PiiNer::tags(&spec);
    assert!(tags[0].is_none());
    let b = tags[1].as_ref().unwrap();
    assert_eq!(("person", "GIVEN_NAME", true), (b.entity.as_str(), b.base.as_str(), b.begin));
    let i = tags[2].as_ref().unwrap();
    assert!(!i.begin);
    let e = tags[3].as_ref().unwrap();
    assert!(!e.begin);
    let s = tags[4].as_ref().unwrap();
    assert_eq!(("address", true), (s.entity.as_str(), s.begin));
    assert!(tags[5].as_ref().unwrap().begin);
    assert!(tags[6].is_none());
}

#[test]
fn pii_ner_pick_picks_tag_at_or_above_threshold() {
    let mut entities = HashMap::new();
    entities.insert("GIVEN_NAME".into(), "person".into());
    let spec = NerSpec {
        labels: vec!["O".into(), "B-GIVEN_NAME".into()],
        entities,
        ..NerSpec::default()
    };
    let map = PiiNer::tags(&spec);
    assert_eq!((-1, 0.0), PiiNer::pick(&[2.0, 0.0], &map).unwrap());
    let (cls, p) = PiiNer::pick(&[0.0, 0.0], &map).unwrap();
    assert_eq!(1, cls);
    assert!((p - 0.5).abs() < 1e-5);
}

#[test]
fn pii_ner_spans_merges_i_tags_and_splits_on_b_tags() {
    use br_worker::models::ner::BpeToken;
    let mut entities = HashMap::new();
    entities.insert("GIVEN_NAME".into(), "person".into());
    entities.insert("SURNAME".into(), "person".into());
    let spec = NerSpec {
        labels: vec![
            "O".into(),
            "B-GIVEN_NAME".into(),
            "I-GIVEN_NAME".into(),
            "B-SURNAME".into(),
        ],
        entities,
        ..NerSpec::default()
    };
    let map = PiiNer::tags(&spec);
    let tokens = vec![
        BpeToken { id: 1, start: 0, end: 4 },
        BpeToken { id: 2, start: 4, end: 6 },
        BpeToken { id: 3, start: 7, end: 13 },
    ];
    let tags = vec![(1, 0.9), (2, 0.7), (3, 0.8)];
    let spans = PiiNer::spans(&tokens, 0, &tags, &map);
    assert_eq!(2, spans.len());
    assert_eq!((0, 6, "person".to_string()), (spans[0].0, spans[0].1, spans[0].2.clone()));
    assert!((spans[0].3 - 0.8).abs() < 1e-5);
    assert_eq!((7, 13, "person".to_string()), (spans[1].0, spans[1].1, spans[1].2.clone()));
}

#[test]
fn ner_gate_keeps_ocr_lines_on_model_missing_and_backs_off_on_failure() {
    let clock = Arc::new(AtomicI64::new(0));
    let clock_clone = Arc::clone(&clock);
    let mut gate = NerGate::with_clock(move || clock_clone.load(Ordering::Relaxed));

    let sample_lines = || {
        vec![OcrLine {
            text: "John Carter".into(),
            x: 10,
            y: 20,
            w: 80,
            h: 16,
            confidence: 0.9,
            cuts: (0..=11).map(|i| 10 + i * 7).collect(),
            entities: None,
        }]
    };

    // Empty lines never invoke NER.
    let mut calls = 0usize;
    let (empty_out, empty_miss) = gate.apply(Vec::new(), |_| {
        calls += 1;
        Ok(Vec::new())
    });
    assert!(empty_out.is_empty());
    assert!(!empty_miss);
    assert_eq!(0, calls);

    // ModelNotInstalled: keeps lines, sets entities_missing = true, does NOT back off.
    let (miss_out, miss_flag) = gate.apply(sample_lines(), |_| {
        calls += 1;
        Err(WorkerError::ModelNotInstalled("ner".into()))
    });
    assert_eq!(1, calls);
    assert!(miss_flag);
    assert_eq!(1, miss_out.len());
    assert!(miss_out[0].entities.is_none());

    // Failure: keeps lines, backs off for BACKOFF_MS.
    let (err_out, err_flag) = gate.apply(sample_lines(), |_| {
        calls += 1;
        Err(WorkerError::DeviceFailed("broken ner".into()))
    });
    assert_eq!(2, calls);
    assert!(err_flag);
    assert_eq!(1, err_out.len());
    assert_eq!(Some("DeviceFailedException"), gate.last_error.as_deref());

    // Inside backoff window: skipped without invoking closure.
    clock.store(1000, Ordering::Relaxed);
    let (_, still_miss) = gate.apply(sample_lines(), |_| {
        calls += 1;
        Ok(vec![None])
    });
    assert_eq!(2, calls);
    assert!(still_miss);

    // Past backoff window: retries and succeeds.
    clock.store(NerGate::BACKOFF_MS + 1, Ordering::Relaxed);
    let (ok_out, ok_miss) = gate.apply(sample_lines(), |_| {
        calls += 1;
        Ok(vec![Some(vec![OcrEntity {
            start: 0,
            length: 11,
            label: "person".into(),
            score: 0.95,
        }])])
    });
    assert_eq!(3, calls);
    assert!(!ok_miss);
    assert!(gate.last_error.is_none());
    assert_eq!(1, ok_out[0].entities.as_ref().unwrap().len());
}

// ---- OcrTests: WindowsOcrTests ----

#[test]
fn windows_ocr_reads_a_rendered_line() {
    let Some(win) = WindowsOcr::try_create(&["en-US".to_string()]) else {
        // Optional Windows OCR language pack not installed on this machine.
        return;
    };
    let img = render_text_rgb("Meeting notes for Thursday", 900, 120, 30, 40, 28);
    let lines = win.read(&img).expect("windows ocr read");
    let joined: String = lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join(" ");
    let squashed: String = joined.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        squashed.contains("Meetingnotes"),
        "unexpected Windows OCR text: {joined:?}"
    );
    for l in &lines {
        assert_eq!(l.text.encode_utf16().count() + 1, l.cuts.len());
    }
}

// ---- OcrTests: RealOcrTests / OcrDeviceTests (env-gated) ----

#[test]
fn real_ocr_and_ner_when_models_installed() {
    let Some(model_dir) = std::env::var_os("BR_OCR_MODELS").map(PathBuf::from) else {
        return;
    };
    if !model_dir.is_dir() {
        return;
    }
    let dir = ScopedDir::new("real-ocr");
    let mut s = Settings::default();
    s.inference.model_dir = model_dir.to_string_lossy().into_owned();
    s.ocr.enabled = true;
    s.ocr.engine = "onnx".into();
    let ws = WorkerSettings::from(&s, dir.path());
    let engine = RealEngine::new(ws, dir.path().to_path_buf());
    let img = render_text_rgb("Please contact Anna Schmidt at 12 Main Street", 960, 160, 30, 40, 28);
    let mut bgra = vec![0u8; (img.width * img.height * 4) as usize];
    for i in 0..(img.width * img.height) as usize {
        bgra[i * 4] = img.rgb[i * 3 + 2];
        bgra[i * 4 + 1] = img.rgb[i * 3 + 1];
        bgra[i * 4 + 2] = img.rgb[i * 3];
        bgra[i * 4 + 3] = 255;
    }
    let input = ImageInput {
        format: "bgra8",
        bytes: &bgra,
        width: img.width,
        height: img.height,
        stride: img.width * 4,
    };
    let res = engine.ocr(&input, true).expect("real ocr");
    assert_eq!("onnx", res.engine);
    assert!(!res.lines.is_empty());
}

#[test]
fn ocr_on_requested_devices_when_available() {
    let (Some(model_dir), Some(devices_raw)) = (
        std::env::var_os("BR_OCR_MODELS").map(PathBuf::from),
        std::env::var("BR_OCR_DEVICES").ok(),
    ) else {
        return;
    };
    let Some(manifest) = ModelCatalog::load(&model_dir, ModelCatalog::OCR) else {
        return;
    };
    let spec = manifest.ocr.as_ref().unwrap();
    let recs = OcrMath::pick_recognisers(&spec.rec, &["en-US".to_string()]);
    for dev_setting in devices_raw.split(',').map(str::trim).filter(|d| !d.is_empty()) {
        let dir = ScopedDir::new("ocr-dev");
        let mut s = Settings::default();
        s.inference.model_dir = model_dir.to_string_lossy().into_owned();
        s.ocr.device = dev_setting.to_ascii_lowercase();
        let ws = WorkerSettings::from(&s, dir.path());
        let engine = RealEngine::new(ws, dir.path().to_path_buf());
        let img = render_text_rgb("Hello BetterRecall", 480, 120, 30, 40, 28);
        let mut bgra = vec![0u8; (img.width * img.height * 4) as usize];
        for i in 0..(img.width * img.height) as usize {
            bgra[i * 4] = img.rgb[i * 3 + 2];
            bgra[i * 4 + 1] = img.rgb[i * 3 + 1];
            bgra[i * 4 + 2] = img.rgb[i * 3];
            bgra[i * 4 + 3] = 255;
        }
        let input = ImageInput {
            format: "bgra8",
            bytes: &bgra,
            width: img.width,
            height: img.height,
            stride: img.width * 4,
        };
        let _ = recs.len();
        let res = engine.ocr(&input, false).expect("device ocr");
        assert_eq!("onnx", res.engine);
    }
}

// ---- BatchHOcrTests ----

#[test]
fn c12_non_finite_value_anywhere_in_a_detector_map_is_found() {
    assert!(PaddleOcr::all_finite(&[0.0, 1.0, -3.5]));
    let mut big = vec![0.0f32; 1_000_000];
    big[999_999] = f32::NAN;
    assert!(!PaddleOcr::all_finite(&big));
    big[999_999] = f32::INFINITY;
    assert!(!PaddleOcr::all_finite(&big));
    big[999_999] = f32::NEG_INFINITY;
    assert!(!PaddleOcr::all_finite(&big));

    let dir = ScopedDir::new("c12-det");
    install_mock_ocr(dir.path(), vec![0.5, 0.5, 0.5], vec![0.5, 0.5, 0.5]);
    let manifest = ModelCatalog::load(dir.path(), ModelCatalog::OCR).unwrap();
    let rec_spec = manifest.ocr.as_ref().unwrap().rec.clone();

    let cpu = dev_cpu();
    let backend: Arc<dyn IInferenceBackend> = Arc::new(MockBackend {
        name: "Mock",
        is_ov: false,
        devices: vec![cpu.clone()],
        loader: Box::new(|model_path, device, _| {
            let file_name = model_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if file_name == "det.onnx" {
                let d = device.clone();
                Ok(Box::new(FnSession {
                    device: d,
                    inputs: vec![spec("x", TensorType::Float32, &[1, 3, -1, -1])],
                    outputs: vec![spec("out", TensorType::Float32, &[1, 1, -1, -1])],
                    handler: Box::new(|inputs| {
                        let sh = &inputs[0].shape;
                        let h = sh[2] as usize;
                        let w = sh[3] as usize;
                        let mut prob = vec![0.0f32; h * w];
                        prob[h * w - 1] = f32::NAN;
                        let mut map = HashMap::new();
                        map.insert(
                            "out".into(),
                            HostTensor::f("out", prob, &[1, 1, h as i64, w as i64]),
                        );
                        Ok(map)
                    }),
                }))
            } else {
                let d = device.clone();
                Ok(Box::new(FnSession {
                    device: d,
                    inputs: vec![spec("x", TensorType::Float32, &[-1, 3, 48, -1])],
                    outputs: vec![spec("out", TensorType::Float32, &[-1, 4, 26])],
                    handler: Box::new(|inputs| {
                        let n = inputs[0].shape[0] as usize;
                        let mut map = HashMap::new();
                        map.insert(
                            "out".into(),
                            HostTensor::f("out", vec![0.0f32; n * 4 * 26], &[n as i64, 4, 26]),
                        );
                        Ok(map)
                    }),
                }))
            }
        }),
    });

    let mut ocr = PaddleOcr::new(backend, cpu, &manifest, &rec_spec, None, Some(false)).unwrap();
    let err = ocr
        .read(&RgbImage {
            width: 32,
            height: 32,
            rgb: vec![255; 32 * 32 * 3],
        })
        .unwrap_err();
    assert!(matches!(err, WorkerError::DeviceFailed(_)));
    assert!(err.wire_message().contains("not finite"));
}

#[test]
fn c12_nan_recogniser_row_is_not_read_as_blank() {
    let dir = ScopedDir::new("c12-rec");
    install_mock_ocr(dir.path(), vec![0.5, 0.5, 0.5], vec![0.5, 0.5, 0.5]);
    let manifest = ModelCatalog::load(dir.path(), ModelCatalog::OCR).unwrap();
    let rec_spec = manifest.ocr.as_ref().unwrap().rec.clone();

    let cpu = dev_cpu();
    let backend: Arc<dyn IInferenceBackend> = Arc::new(MockBackend {
        name: "Mock",
        is_ov: false,
        devices: vec![cpu.clone()],
        loader: Box::new(|model_path, device, _| {
            let file_name = model_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if file_name == "det.onnx" {
                let d = device.clone();
                Ok(Box::new(FnSession {
                    device: d,
                    inputs: vec![spec("x", TensorType::Float32, &[1, 3, -1, -1])],
                    outputs: vec![spec("out", TensorType::Float32, &[1, 1, -1, -1])],
                    handler: Box::new(|inputs| {
                        let h = inputs[0].shape[2] as i32;
                        let w = inputs[0].shape[3] as i32;
                        let mut prob = vec![0.0f32; (w * h) as usize];
                        paint_box(&mut prob, w, 4, 4, 28, 28, 0.9);
                        let mut map = HashMap::new();
                        map.insert(
                            "out".into(),
                            HostTensor::f("out", prob, &[1, 1, h as i64, w as i64]),
                        );
                        Ok(map)
                    }),
                }))
            } else {
                let d = device.clone();
                Ok(Box::new(FnSession {
                    device: d,
                    inputs: vec![spec("x", TensorType::Float32, &[-1, 3, 48, -1])],
                    outputs: vec![spec("out", TensorType::Float32, &[-1, 2, 26])],
                    handler: Box::new(|inputs| {
                        let n = inputs[0].shape[0] as usize;
                        let mut probs = vec![0.0f32; n * 2 * 26];
                        probs[1] = 0.9; // step 0: 'a'
                        probs[26] = f32::NAN; // step 1: NaN would otherwise pick blank (index 0)
                        let mut map = HashMap::new();
                        map.insert(
                            "out".into(),
                            HostTensor::f("out", probs, &[n as i64, 2, 26]),
                        );
                        Ok(map)
                    }),
                }))
            }
        }),
    });

    let mut ocr = PaddleOcr::new(backend, cpu, &manifest, &rec_spec, None, Some(false)).unwrap();
    let err = ocr
        .read(&RgbImage {
            width: 32,
            height: 32,
            rgb: vec![255; 32 * 32 * 3],
        })
        .unwrap_err();
    assert!(matches!(err, WorkerError::DeviceFailed(_)));
    assert!(err.wire_message().contains("not finite"));
}

// ---- Sweep2FTests ----

#[test]
fn outer_cuts_of_single_char_span_the_whole_box() {
    let ctc = CtcResult {
        text: "A".into(),
        confidence: 0.9,
        step_start: vec![3],
        step_end: vec![4],
    };
    let cuts = OcrMath::cuts(&ctc, 10, 256, 48, 20, 40);
    assert_eq!(vec![20, 60], cuts);
}

#[test]
fn outer_cuts_pin_to_crop_edges_even_when_steps_drift() {
    let ctc = CtcResult {
        text: "abc".into(),
        confidence: 0.9,
        step_start: vec![0, 5, 19],
        step_end: vec![2, 8, 19],
    };
    let cuts = OcrMath::cuts(&ctc, 20, 256, 64, 50, 120);
    assert_eq!(50, cuts[0]);
    assert_eq!(170, cuts[3]);
    assert!(cuts[1] <= cuts[2] && cuts[2] <= cuts[3]);
}

fn ocr_ner_mock_backend(
    emit_det_box: bool,
    ner_fails: Arc<AtomicBool>,
    ner_loads: Arc<AtomicUsize>,
    warmup_only_empty_boxes: bool,
) -> Arc<dyn IInferenceBackend> {
    let cpu = dev_cpu();
    Arc::new(MockBackend {
        name: "Mock",
        is_ov: false,
        devices: vec![cpu],
        loader: Box::new(move |model_path, device, _| {
            let folder = model_path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let file = model_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if folder == "ner" {
                ner_loads.fetch_add(1, Ordering::Relaxed);
                if ner_fails.load(Ordering::Relaxed) {
                    return Err(WorkerError::DeviceFailed("broken ner model".into()));
                }
                let d = device.clone();
                return Ok(Box::new(FnSession {
                    device: d,
                    inputs: vec![
                        spec("input_ids", TensorType::Int64, &[1, -1]),
                        spec("attention_mask", TensorType::Int64, &[1, -1]),
                    ],
                    outputs: vec![spec("logits", TensorType::Float32, &[1, -1, 5])],
                    handler: Box::new(|inputs| {
                        let seq = inputs[0].shape[1] as usize;
                        let mut logits = vec![0.0f32; seq * 5];
                        for t in 0..seq {
                            logits[t * 5] = 5.0; // O
                        }
                        if seq >= 5 {
                            // [<bos>, "\u{2581}call"(1), "\u{2581}"(2), "john"(3), <eos>(4)] -> tag token index 3 ("john")
                            logits[3 * 5 + 1] = 10.0; // B-GIVEN_NAME
                            logits[3 * 5] = 0.0;
                        }
                        let mut map = HashMap::new();
                        map.insert(
                            "logits".into(),
                            HostTensor::f("logits", logits, &[1, seq as i64, 5]),
                        );
                        Ok(map)
                    }),
                }));
            }
            if file == "det.onnx" {
                let d = device.clone();
                return Ok(Box::new(FnSession {
                    device: d,
                    inputs: vec![spec("x", TensorType::Float32, &[1, 3, -1, -1])],
                    outputs: vec![spec("out", TensorType::Float32, &[1, 1, -1, -1])],
                    handler: Box::new(move |inputs| {
                        let h = inputs[0].shape[2] as i32;
                        let w = inputs[0].shape[3] as i32;
                        let mut prob = vec![0.0f32; (w * h) as usize];
                        // warm_up uses 320x96; real tests use smaller 64x64 frame unless warmup_only_empty_boxes
                        let is_warmup = w == 320 && h == 96;
                        if emit_det_box && !(warmup_only_empty_boxes && !is_warmup) {
                            paint_box(&mut prob, w, 4, 4, (w - 4).max(8), (h - 4).max(8), 0.9);
                        }
                        let mut map = HashMap::new();
                        map.insert(
                            "out".into(),
                            HostTensor::f("out", prob, &[1, 1, h as i64, w as i64]),
                        );
                        Ok(map)
                    }),
                }));
            }
            // rec.onnx: spell "call john" (1-based dict indices: c=3 a=1 l=13 blank=0 l=13 space=25 j=11 o=16 h=9 n=15)
            let d = device.clone();
            Ok(Box::new(FnSession {
                device: d,
                inputs: vec![spec("x", TensorType::Float32, &[-1, 3, 48, -1])],
                outputs: vec![spec("out", TensorType::Float32, &[-1, 10, 26])],
                handler: Box::new(|inputs| {
                    let n = inputs[0].shape[0] as usize;
                    let seq = rec_logits_for_indices(&[3, 1, 13, 0, 13, 25, 11, 16, 9, 15], 26);
                    let mut all = Vec::with_capacity(n * seq.len());
                    for _ in 0..n {
                        all.extend_from_slice(&seq);
                    }
                    let mut map = HashMap::new();
                    map.insert(
                        "out".into(),
                        HostTensor::f("out", all, &[n as i64, 10, 26]),
                    );
                    Ok(map)
                }),
            }))
        }),
    })
}

fn ocr_request(w: i32, h: i32, entities: bool) -> WorkerRequest {
    WorkerRequest {
        id: 1,
        op: "ocr".into(),
        format: Some("bgra8".into()),
        width: Some(w),
        height: Some(h),
        stride: Some(w * 4),
        entities: Some(entities),
        ..Default::default()
    }
}

#[test]
fn ner_success_attaches_entities() {
    let dir = ScopedDir::new("s2f-ok");
    let models = dir.path().join("models");
    install_mock_ocr(&models, vec![0.5, 0.5, 0.5], vec![0.5, 0.5, 0.5]);
    install_mock_ner(&models, None);
    let s = test_settings(dir.path(), |x| x.inference.device = "cpu".into());
    let b = ocr_ner_mock_backend(
        true,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicUsize::new(0)),
        false,
    );
    let engine = RealEngine::with_backends(s.clone(), dir.path().to_path_buf(), Arc::clone(&b), b);
    let h = RequestHandler::new(&engine, || s.clone());

    let resp = send_req(&h, &ocr_request(64, 64, true), &bgra_pixel_bytes(64, 64));
    assert!(resp.ok, "{:?}", resp.error);
    let ocr = resp.ocr.unwrap();
    assert!(!ocr.entities_missing);
    assert_eq!(1, ocr.lines.len());
    assert_eq!("call john", ocr.lines[0].text);
    let ents = ocr.lines[0].entities.as_ref().unwrap();
    assert_eq!(1, ents.len());
    assert_eq!("person", ents[0].label);
    assert_eq!(5, ents[0].start);
    assert_eq!(4, ents[0].length);
}

#[test]
fn missing_ner_model_returns_ocr_lines_with_entities_missing() {
    let dir = ScopedDir::new("s2f-miss");
    let models = dir.path().join("models");
    install_mock_ocr(&models, vec![0.5, 0.5, 0.5], vec![0.5, 0.5, 0.5]);
    let s = test_settings(dir.path(), |x| x.inference.device = "cpu".into());
    let b = ocr_ner_mock_backend(
        true,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicUsize::new(0)),
        false,
    );
    let engine = RealEngine::with_backends(s.clone(), dir.path().to_path_buf(), Arc::clone(&b), b);
    let h = RequestHandler::new(&engine, || s.clone());

    let resp = send_req(&h, &ocr_request(64, 64, true), &bgra_pixel_bytes(64, 64));
    assert!(resp.ok, "{:?}", resp.error);
    let ocr = resp.ocr.unwrap();
    assert!(ocr.entities_missing);
    assert_eq!(1, ocr.lines.len());
    assert!(ocr.lines[0].entities.is_none());
}

#[test]
fn broken_ner_model_backs_off_and_still_returns_ocr_lines() {
    let dir = ScopedDir::new("s2f-broken");
    let models = dir.path().join("models");
    install_mock_ocr(&models, vec![0.5, 0.5, 0.5], vec![0.5, 0.5, 0.5]);
    install_mock_ner(&models, None);
    let s = test_settings(dir.path(), |x| x.inference.device = "cpu".into());
    let ner_loads = Arc::new(AtomicUsize::new(0));
    let b = ocr_ner_mock_backend(
        true,
        Arc::new(AtomicBool::new(true)),
        Arc::clone(&ner_loads),
        false,
    );
    let engine = RealEngine::with_backends(s.clone(), dir.path().to_path_buf(), Arc::clone(&b), b);
    let h = RequestHandler::new(&engine, || s.clone());

    let r1 = send_req(&h, &ocr_request(64, 64, true), &bgra_pixel_bytes(64, 64));
    assert!(r1.ok);
    let o1 = r1.ocr.unwrap();
    assert!(o1.entities_missing);
    assert_eq!(1, o1.lines.len());
    let loads_after_first = ner_loads.load(Ordering::Relaxed);
    assert!(loads_after_first >= 1);

    let r2 = send_req(&h, &ocr_request(64, 64, true), &bgra_pixel_bytes(64, 64));
    assert!(r2.ok);
    let o2 = r2.ocr.unwrap();
    assert!(o2.entities_missing);
    assert_eq!(1, o2.lines.len());
    assert_eq!(loads_after_first, ner_loads.load(Ordering::Relaxed));
}

#[test]
fn no_lines_skip_the_ner() {
    let dir = ScopedDir::new("s2f-nolines");
    let models = dir.path().join("models");
    install_mock_ocr(&models, vec![0.5, 0.5, 0.5], vec![0.5, 0.5, 0.5]);
    install_mock_ner(&models, None);
    let s = test_settings(dir.path(), |x| x.inference.device = "cpu".into());
    let ner_loads = Arc::new(AtomicUsize::new(0));
    let b = ocr_ner_mock_backend(
        true,
        Arc::new(AtomicBool::new(true)),
        Arc::clone(&ner_loads),
        true,
    );
    let engine = RealEngine::with_backends(s.clone(), dir.path().to_path_buf(), Arc::clone(&b), b);
    let h = RequestHandler::new(&engine, || s.clone());

    let resp = send_req(&h, &ocr_request(64, 64, true), &bgra_pixel_bytes(64, 64));
    assert!(resp.ok, "{:?}", resp.error);
    let ocr = resp.ocr.unwrap();
    assert!(ocr.lines.is_empty());
    assert!(!ocr.entities_missing);
    assert_eq!(0, ner_loads.load(Ordering::Relaxed));
}

#[test]
fn redaction_box_of_a_line_starting_span_covers_the_first_glyph() {
    let ctc = CtcResult {
        text: "Anna Schmidt".into(),
        confidence: 0.95,
        step_start: vec![2, 5, 7, 9, 11, 13, 15, 17, 19, 21, 23, 25],
        step_end: vec![3, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26],
    };
    let cuts = OcrMath::cuts(&ctc, 30, 256, 180, 100, 120);
    // Entity starting at index 0 with length 4 ("Anna") starts right at line.x (100) so the first glyph is covered.
    let x0 = cuts[0];
    let x1 = cuts[4];
    assert_eq!(100, x0);
    assert!(x1 > x0 && x1 <= 220);
}

// ---- Sweep3ITests (C17 box cap) ----

fn c17_blobs(pw: i32, ph: i32, count: usize) -> Vec<f32> {
    // Separate 4x4 blobs on a grid with 2-pixel gaps (mirrors Sweep3ITests.Blobs).
    let mut prob = vec![0.0f32; (pw * ph) as usize];
    let per_row = (pw / 6) as usize;
    for n in 0..count {
        let x0 = ((n % per_row) * 6) as i32;
        let y0 = ((n / per_row) * 6) as i32;
        paint_box(&mut prob, pw, x0, y0, x0 + 4, y0 + 4, 0.9);
    }
    prob
}

#[test]
fn c17_box_cap_fails_the_read_instead_of_stopping_part_way() {
    let pw = 60;
    let ph = 60;
    assert_eq!(
        5,
        OcrMath::boxes(&c17_blobs(pw, ph, 5), pw, ph, pw, ph, 0.2, 0.45, 1.4, 5)
            .unwrap()
            .len()
    );
    let err = OcrMath::boxes(&c17_blobs(pw, ph, 6), pw, ph, pw, ph, 0.2, 0.45, 1.4, 5)
        .unwrap_err();
    assert!(matches!(err, WorkerError::OcrOverflow(_)));
    assert!(err.wire_message().starts_with("Device failed: "));

    // Default 3000 cap (360x360 fits 60x60 = 3600 4x4 blobs with 2-px gaps).
    let err_default = OcrMath::boxes(
        &c17_blobs(360, 360, 3001),
        360,
        360,
        360,
        360,
        0.2,
        0.45,
        1.4,
        OcrMath::DEFAULT_MAX_BOXES,
    )
    .unwrap_err();
    assert!(matches!(err_default, WorkerError::OcrOverflow(_)));
}

// ---- Sweep3JWorkerTests ----

fn build_c6_ner_session(inject_non_finite: Option<f32>) -> PiiNer {
    let dir = ScopedDir::new("s3j-c6");
    let models = dir.path().join("models");
    install_mock_ner(&models, None);
    let manifest = ModelCatalog::load(&models, ModelCatalog::NER).unwrap();
    let cpu = dev_cpu();
    let backend = MockBackend {
        name: "Mock",
        is_ov: false,
        devices: vec![cpu.clone()],
        loader: Box::new(move |_, device, _| {
            let d = device.clone();
            Ok(Box::new(FnSession {
                device: d,
                inputs: vec![
                    spec("input_ids", TensorType::Int64, &[1, -1]),
                    spec("attention_mask", TensorType::Int64, &[1, -1]),
                ],
                outputs: vec![spec("logits", TensorType::Float32, &[1, -1, 5])],
                handler: Box::new(move |inputs| {
                    let seq = inputs[0].shape[1] as usize;
                    let mut logits = vec![0.0f32; seq * 5];
                    for t in 0..seq {
                        logits[t * 5] = 5.0;
                    }
                    if seq >= 3 {
                        logits[1 * 5 + 1] = 10.0; // B-GIVEN_NAME
                        logits[1 * 5] = 0.0;
                    }
                    if let Some(bad) = inject_non_finite {
                        logits[seq * 5 - 1] = bad;
                    }
                    let mut map = HashMap::new();
                    map.insert(
                        "logits".into(),
                        HostTensor::f("logits", logits, &[1, seq as i64, 5]),
                    );
                    Ok(map)
                }),
            }))
        }),
    };
    PiiNer::new(&backend, &cpu, &manifest, None).unwrap()
}

#[test]
fn c6_non_finite_logit_anywhere_fails_the_device() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let ner = build_c6_ner_session(Some(bad));
        let err = ner.find(&["call john".into()]).unwrap_err();
        assert!(matches!(err, WorkerError::DeviceFailed(_)));
        assert!(err.wire_message().contains("not finite"));

        let pick_err = PiiNer::pick(&[0.0, bad], &[]).unwrap_err();
        assert!(matches!(pick_err, WorkerError::DeviceFailed(_)));
    }
}

#[test]
fn c6_finite_logits_still_tag() {
    let ner = build_c6_ner_session(None);
    let found = ner.find(&["call john".into()]).unwrap();
    assert_eq!(1, found.len());
    let ents = found[0].as_ref().unwrap();
    assert_eq!(1, ents.len());
    assert_eq!("person", ents[0].label);
}

// ---- Sweep3KTests ----

fn reference_rescan_bpe(
    mut symbols: Vec<(i32, usize, usize)>,
    merges: &HashMap<(i32, i32), (i32, i32)>,
) -> Vec<(i32, usize, usize)> {
    loop {
        let mut best_idx = None;
        let mut best_rank = i32::MAX;
        let mut best_id = 0;
        for i in 0..symbols.len().saturating_sub(1) {
            if let Some(&(rank, id)) = merges.get(&(symbols[i].0, symbols[i + 1].0))
                && rank < best_rank
            {
                best_rank = rank;
                best_id = id;
                best_idx = Some(i);
            }
        }
        let Some(i) = best_idx else {
            break;
        };
        let merged = (best_id, symbols[i].1, symbols[i + 1].2);
        symbols[i] = merged;
        symbols.remove(i + 1);
    }
    symbols
}

#[test]
fn bpe_merge_matches_the_rescanning_algorithm() {
    let dir = ScopedDir::new("s3k-bpe-cmp");
    let vocab_obj: serde_json::Map<String, serde_json::Value> = [
        ("<pad>", 0),
        ("<eos>", 1),
        ("<bos>", 2),
        ("<unk>", 3),
        ("\u{2581}", 4),
        ("a", 5),
        ("b", 6),
        ("c", 7),
        ("d", 8),
        ("aa", 9),
        ("ab", 10),
        ("aaab", 11),
        ("ba", 12),
        ("cd", 13),
        ("\u{2581}aaab", 14),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), serde_json::Value::from(v)))
    .collect();
    let tok_json = serde_json::json!({
        "model": {
            "type": "BPE",
            "vocab": vocab_obj,
            "merges": [
                "a a",
                "a b",
                "aa ab",
                "\u{2581} aaab",
                "b a",
                "c d"
            ]
        }
    });
    let tok_path = dir.path().join("tokenizer.json");
    fs::write(&tok_path, serde_json::to_vec(&tok_json).unwrap()).unwrap();
    let tok = BpeTokenizer::load(&tok_path).unwrap();

    let mut merges_map = HashMap::new();
    merges_map.insert((5, 5), (0, 9));
    merges_map.insert((5, 6), (1, 10));
    merges_map.insert((9, 10), (2, 11));
    merges_map.insert((4, 11), (3, 14));
    merges_map.insert((6, 5), (4, 12));
    merges_map.insert((7, 8), (5, 13));

    for text in ["aaab", "aaabaaab", "aaabcdbaaa", "abaaab", "baaabacd"] {
        let actual = tok.encode(text);
        let mut syms = vec![(4, 0, 0)];
        let mut u16_pos = 0usize;
        for ch in text.chars() {
            let id = vocab_obj[&ch.to_string()].as_i64().unwrap() as i32;
            let len = ch.len_utf16();
            syms.push((id, u16_pos, u16_pos + len));
            u16_pos += len;
        }
        let expected = reference_rescan_bpe(syms, &merges_map);
        let actual_tuples: Vec<(i32, usize, usize)> =
            actual.iter().map(|t| (t.id, t.start, t.end)).collect();
        assert_eq!(expected, actual_tuples, "text={text}");
    }
}

#[test]
fn bpe_long_word_is_not_quadratic() {
    let dir = ScopedDir::new("s3k-bpe-long");
    let tok_json = serde_json::json!({
        "model": {
            "type": "BPE",
            "vocab": {
                "<pad>": 0, "<eos>": 1, "<bos>": 2, "<unk>": 3, "\u{2581}": 4,
                "a": 5, "aa": 6, "aaaa": 7, "aaaaaaaa": 8
            },
            "merges": ["a a", "aa aa", "aaaa aaaa"]
        }
    });
    let tok_path = dir.path().join("tokenizer.json");
    fs::write(&tok_path, serde_json::to_vec(&tok_json).unwrap()).unwrap();
    let tok = BpeTokenizer::load(&tok_path).unwrap();

    let word = "a".repeat(32_768);
    let started = Instant::now();
    let toks = tok.encode(&word);
    let elapsed = started.elapsed();
    assert!(
        elapsed.as_secs_f64() < 5.0,
        "BPE took {:.2}s on 32K word",
        elapsed.as_secs_f64()
    );
    // 1 meta token + 4096 "aaaaaaaa" tokens
    assert_eq!(4097, toks.len());
}

#[test]
fn whisper_auto_skips_the_gpu() {
    let npu = dev_npu();
    let gpu = dev_igpu();
    let cpu = dev_cpu();

    let a1 = DevicePolicy::whisper_allowed("auto", &[npu.clone(), gpu.clone(), cpu.clone()]);
    assert_eq!(vec!["NPU", "CPU"], a1.iter().map(|d| d.id.as_str()).collect::<Vec<_>>());

    let a2 = DevicePolicy::whisper_allowed("auto", &[gpu.clone(), cpu.clone()]);
    assert_eq!(vec!["CPU"], a2.iter().map(|d| d.id.as_str()).collect::<Vec<_>>());

    let g = DevicePolicy::whisper_allowed("gpu", &[npu.clone(), gpu.clone(), cpu.clone()]);
    assert_eq!(vec!["GPU.0", "CPU"], g.iter().map(|d| d.id.as_str()).collect::<Vec<_>>());

    let c = DevicePolicy::whisper_allowed("cpu", &[npu, gpu, cpu]);
    assert_eq!(vec!["CPU"], c.iter().map(|d| d.id.as_str()).collect::<Vec<_>>());
}

#[test]
fn bench_drops_the_graphs_of_other_budgets() {
    let dir = ScopedDir::new("s3k-c13");
    let model_dir = dir.path().join("models");
    let gdir = ModelCatalog::dir(&model_dir, ModelCatalog::EMBEDDING_GEMMA);
    let files = vec![
        copy_model_file(&gdir, "sp.model", "tokenizer.model"),
        copy_model_file(&gdir, "text_16.onnxt", "text_16.onnx"),
        copy_model_file(&gdir, "text_32.onnxt", "text_32.onnx"),
        copy_model_file(&gdir, "vision_28.onnxt", "vision_70.onnx"),
        copy_model_file(&gdir, "fusion_28.onnxt", "fusion_70.onnx"),
        copy_model_file(&gdir, "vision_28.onnxt", "vision_140.onnx"),
        copy_model_file(&gdir, "fusion_28.onnxt", "fusion_140.onnx"),
        copy_model_file(&gdir, "vision_28.onnxt", "vision_280.onnx"),
        copy_model_file(&gdir, "fusion_28.onnxt", "fusion_280.onnx"),
    ];
    let mut ids = vec![300i64; 32];
    ids[0] = 2;
    ids[31] = 1;
    let vision = |tokens: i32, suffix: &str| VisionGraph {
        tokens,
        file: format!("vision_{suffix}.onnx"),
        height: 64,
        width: 112,
        output: "image_embeds".into(),
        fusion: Some(FusionGraph {
            file: format!("fusion_{suffix}.onnx"),
            input_ids_input: Some("input_ids".into()),
            attention_mask_input: Some("attention_mask".into()),
            input_ids: ids.clone(),
            ..FusionGraph::default()
        }),
        ..VisionGraph::default()
    };
    let manifest = ModelManifest {
        name: ModelCatalog::EMBEDDING_GEMMA.into(),
        id: "synthetic/embeddinggemma-2".into(),
        revision: "syn1".into(),
        dim: 768,
        tokenizer: Some(TokenizerSpec {
            file: "tokenizer.model".into(),
            add_bos: true,
            add_eos: true,
            bos_id: 2,
            eos_id: 1,
            pad_id: 0,
            ..TokenizerSpec::default()
        }),
        prompts: Some(PromptSpec {
            query: "task: search result | query: {text}".into(),
            document: "title: none | text: {text}".into(),
        }),
        text: vec![
            TextGraph {
                file: "text_16.onnx".into(),
                seq: 16,
                ..TextGraph::default()
            },
            TextGraph {
                file: "text_32.onnx".into(),
                seq: 32,
                ..TextGraph::default()
            },
        ],
        vision: vec![vision(70, "70"), vision(140, "140"), vision(280, "280")],
        files,
        ..ModelManifest::default()
    };
    write_manifest(&gdir, &manifest);

    let s = test_settings(dir.path(), |x| {
        x.inference.device = "cpu".into();
        x.inference.image_tokens = 140;
        x.privacy.nsfw_filter = false;
        x.ocr.enabled = false;
    });
    let ort = Arc::new(OnnxBackend::new(true).unwrap());
    let engine = RealEngine::with_backends(
        s,
        dir.path().to_path_buf(),
        Arc::clone(&ort) as Arc<dyn IInferenceBackend>,
        ort,
    );
    let res = engine.bench().expect("bench");
    assert_eq!(3, res.tokens.as_ref().unwrap().len());

    let keys = engine.embedder_session_keys();
    assert!(keys.iter().any(|k| k.contains("vision_140.onnx")));
    assert!(keys.iter().any(|k| k.contains("fusion_140.onnx")));
    assert!(!keys.iter().any(|k| k.contains("70.onnx")));
    assert!(!keys.iter().any(|k| k.contains("280.onnx")));
}

#[test]
fn hello_survives_an_ocr_preload_that_throws_anything() {
    let dir = ScopedDir::new("s3k-c14");
    let models = dir.path().join("models");
    // Empty det.mean causes OcrMath::bgr_tensor inside warm_up to return Internal error.
    install_mock_ocr(&models, Vec::new(), Vec::new());
    let s = test_settings(dir.path(), |x| {
        x.inference.device = "cpu".into();
        x.ocr.enabled = true;
        x.ocr.engine = "onnx".into();
        x.privacy.nsfw_filter = false;
    });
    let b = ocr_ner_mock_backend(
        true,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicUsize::new(0)),
        false,
    );
    let engine = RealEngine::with_backends(s, dir.path().to_path_buf(), Arc::clone(&b), b);
    let hello = engine.hello(768).expect("hello must survive OCR preload throw");
    assert!(hello.ocr_device.is_none());
}

// ---- UnitTests: SmallPartsTests.Vad_segmenter_rules ----

#[test]
fn vad_segmenter_rules() {
    // C# UnitTests.Vad_segmenter_rules on synthetic vad.onnxt: 1 s silence, 1 s tone, 1 s silence.
    let ort = OnnxBackend::new(true).unwrap();
    let vad = SileroVad::new(&ort, &data_path("vad.onnxt")).unwrap();
    let mut pcm = vec![0.0f32; 48_000];
    for (i, sample) in pcm.iter_mut().enumerate().take(32_000).skip(16_000) {
        *sample = 0.2 * ((i as f32) * 0.05).sin();
    }
    let segs = vad.segments(&pcm).unwrap();
    assert_eq!(1, segs.len());
    assert!((900..=1000).contains(&segs[0].0), "start={}", segs[0].0);
    assert!((2000..=2100).contains(&segs[0].1), "end={}", segs[0].1);

    assert!(vad.segments(&[0.0f32; 16_000]).unwrap().is_empty());

    // Too short to count (100 ms).
    let mut blip = vec![0.0f32; 16_000];
    blip[8000..9600].fill(0.2);
    assert!(vad.segments(&blip).unwrap().is_empty());

    // Pure segmenter edge rules: silence, short blip, hysteresis + pad (end = temp_end + pad = 640 + 30 = 670 ms), and multi-segment split.
    assert!(SileroVad::segment_probs(&[0.0; 100], 100 * 512, 0.5, 250, 100, 30).is_empty());

    let mut short = vec![0.0f32; 40];
    short[5..10].fill(0.9);
    assert!(SileroVad::segment_probs(&short, 40 * 512, 0.5, 250, 100, 30).is_empty());

    let mut one = vec![0.0f32; 50];
    one[10..20].fill(0.9);
    one[15] = 0.4; // >= threshold - 0.15 (0.35): stays triggered
    let rule_segs = SileroVad::segment_probs(&one, 50 * 512, 0.5, 250, 100, 30);
    assert_eq!(vec![(290, 670)], rule_segs);

    let mut two = vec![0.0f32; 80];
    two[5..16].fill(0.9);
    two[25..36].fill(0.9);
    let segs2 = SileroVad::segment_probs(&two, 80 * 512, 0.5, 250, 100, 30);
    assert_eq!(2, segs2.len());
    assert!(segs2[0].1 <= segs2[1].0);
}

// Keep unused test helper from drifting into dead-code warning.
#[test]
fn device_list_sanity() {
    assert_eq!(DeviceKind::Gpu, dev_dml().kind);
}
