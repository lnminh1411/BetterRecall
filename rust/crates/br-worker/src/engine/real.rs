// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use br_contracts::{
    BenchDevice, BenchResult, DataPaths, OcrEntity, OcrLine, OcrResult, TokenBench,
    TranscribeResult, WorkerHello,
};
use chrono::{SecondsFormat, Utc};

use crate::audio::{SileroVad, WhisperGenAi};
use crate::engine::{EmbedderIdentity, IWorkerEngine, ImageEmbedding, ImageInput, TextKind};
use crate::hosting::{Log, WorkerSettings};
use crate::inference::device_selection::{
    BenchOutcome, DeviceCache, DeviceCacheFile, DeviceCacheKey, DeviceChoice, DevicePolicy,
};
use crate::inference::dxgi::{DxgiAdapter, DxgiAdapters};
use crate::inference::gpu_selection::GpuSelection;
use crate::inference::nsfw_plan::NsfwDevicePlan;
use crate::inference::onnx::OnnxBackend;
use crate::inference::openvino::OpenVinoBackend;
use crate::inference::{DeviceInfo, DeviceKind, IInferenceBackend};
use crate::models::catalog::ModelCatalog;
use crate::models::embedder::Embedder;
use crate::models::image_pipeline::{ImagePipeline, RgbImage};
use crate::models::manifest::ModelManifest;
use crate::models::ner::{NerGate, PiiNer};
use crate::models::nsfw_gate::{NsfwCandidate, NsfwGate};
use crate::models::ocr::windows_ocr::user_profile_languages;
use crate::models::ocr::{OcrMath, PaddleOcr, WindowsOcr};
use crate::protocol::{WorkerError, WorkerErrors};
use crate::vectors::VectorMath;

pub const RUN_FAILURES_BEFORE_REPLAN: usize = 2;
pub const BENCH_TEXT: &str =
    "BetterRecall benchmark: quarterly sales spreadsheet, Tabelle mit Umsatzzahlen, bảng doanh thu.";

type OutcomeKey = (String, String, Option<String>);

fn outcome_key(o: &BenchOutcome) -> OutcomeKey {
    (
        o.model.clone(),
        o.device.id.clone(),
        o.text_device.as_ref().map(|d| d.id.clone()),
    )
}

struct TokenRow {
    model: String,
    row: TokenBench,
}

struct RealState {
    s: WorkerSettings,
    home: PathBuf,
    main: Option<Arc<dyn IInferenceBackend>>,
    cpu_ort: Option<Arc<dyn IInferenceBackend>>,
    gpu_ort: Option<Arc<dyn IInferenceBackend>>,
    adapters: Option<Vec<DxgiAdapter>>,
    dml: Option<bool>,
    gpus: Option<Vec<DeviceInfo>>,
    embedder: Option<Embedder>,
    choice: Option<DeviceChoice>,
    nsfw: Option<NsfwGate>,
    vad: Option<SileroVad>,
    whisper: Option<WhisperGenAi>,
    ocr: Option<PaddleOcr>,
    win_ocr: Option<WindowsOcr>,
    ner: Option<PiiNer>,
    /// False in the test seam: NSFW/OCR/NER candidates see only injected backends (no DirectML adapter).
    probe_dml: bool,
    failed_devices: HashSet<String>,
    run_failures: HashMap<String, usize>,
    /// Model unloaded by a run-time failure (sweep 4 A-M3).
    replan_model: Option<String>,
}

/// Real models. Everything loads lazily on first use; the serial worker loop guarantees one NPU
/// job at a time.
pub struct RealEngine {
    state: Mutex<RealState>,
    ner_gate: Mutex<NerGate>,
}

impl RealEngine {
    pub fn new(settings: WorkerSettings, home: PathBuf) -> Self {
        Self {
            state: Mutex::new(RealState {
                s: settings,
                home,
                main: None,
                cpu_ort: None,
                gpu_ort: None,
                adapters: None,
                dml: None,
                gpus: None,
                embedder: None,
                choice: None,
                nsfw: None,
                vad: None,
                whisper: None,
                ocr: None,
                win_ocr: None,
                ner: None,
                probe_dml: true,
                failed_devices: HashSet::new(),
                run_failures: HashMap::new(),
                replan_model: None,
            }),
            ner_gate: Mutex::new(NerGate::new()),
        }
    }

    /// Test seam: inject backends (`main` and `cpu`), disabling live DXGI/DirectML probing.
    pub fn with_backends(
        settings: WorkerSettings,
        home: PathBuf,
        main: Arc<dyn IInferenceBackend>,
        cpu: Arc<dyn IInferenceBackend>,
    ) -> Self {
        Self {
            state: Mutex::new(RealState {
                s: settings,
                home,
                main: Some(main),
                cpu_ort: Some(cpu),
                gpu_ort: None,
                adapters: None,
                dml: None,
                gpus: None,
                embedder: None,
                choice: None,
                nsfw: None,
                vad: None,
                whisper: None,
                ocr: None,
                win_ocr: None,
                ner: None,
                probe_dml: false,
                failed_devices: HashSet::new(),
                run_failures: HashMap::new(),
                replan_model: None,
            }),
            ner_gate: Mutex::new(NerGate::new()),
        }
    }

    /// Snapshot of accelerators marked failed in this process (for tests).
    pub fn failed_devices(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("real engine lock")
            .failed_devices
            .iter()
            .cloned()
            .collect()
    }

    /// Snapshot of compiled embedder session keys (for C13 budget drop test).
    pub fn embedder_session_keys(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("real engine lock")
            .embedder
            .as_ref()
            .map(|e| e.session_keys().into_iter().collect())
            .unwrap_or_default()
    }

    /// Deterministic 1280x720 test frame: gradients, a grid and blocks, like a busy desktop.
    pub fn bench_image() -> RgbImage {
        const W: i32 = 1280;
        const H: i32 = 720;
        let mut rgb = vec![0u8; (W * H * 3) as usize];
        for y in 0..H {
            for x in 0..W {
                let o = ((y * W + x) * 3) as usize;
                let grid = x % 64 == 0 || y % 48 == 0;
                let block = (x / 160 + y / 120) % 3 == 0;
                rgb[o] = if grid {
                    30
                } else if block {
                    220
                } else {
                    ((x * 255) / W) as u8
                };
                rgb[o + 1] = if grid {
                    30
                } else if block {
                    230
                } else {
                    ((y * 255) / H) as u8
                };
                rgb[o + 2] = if grid {
                    30
                } else if block {
                    240
                } else {
                    ((x + y) % 256) as u8
                };
            }
        }
        RgbImage {
            width: W,
            height: H,
            rgb,
        }
    }

    pub fn to_bench_device(
        o: &BenchOutcome,
        gpus: &[DeviceInfo],
        auto_gpu: Option<&DeviceInfo>,
    ) -> BenchDevice {
        let named = o.device.kind == DeviceKind::Gpu && gpus.len() > 1;
        BenchDevice {
            device: o.device.label().to_string(),
            ep: o.device.ep.clone(),
            ok: o.ok,
            image_ms: round_dp(o.image_ms, 1),
            text_ms: round_dp(o.text_ms, 1),
            cosine: if o.cosine.is_nan() {
                0.0
            } else {
                round_dp(o.cosine, 6)
            },
            error: o.error.clone(),
            model: Some(o.model.clone()),
            text_device: if o.split() {
                o.text_device.as_ref().map(|d| d.label().to_string())
            } else {
                None
            },
            compile_ms: if o.compile_cached || o.compile_ms <= 0.0 {
                None
            } else {
                Some(o.compile_ms.round_ties_even())
            },
            adapter: if named {
                Some(o.device.name.clone())
            } else {
                None
            },
            adapter_auto: named && auto_gpu.is_some_and(|a| a.id == o.device.id),
        }
    }
}

impl RealState {
    fn main_backend(&mut self) -> Result<Arc<dyn IInferenceBackend>, WorkerError> {
        if let Some(ref b) = self.main {
            return Ok(Arc::clone(b));
        }
        if let Some(ov) = OpenVinoBackend::try_create()
            && ov.devices().iter().any(|d| {
                d.kind != DeviceKind::Cpu || d.name.to_ascii_lowercase().contains("intel")
            })
        {
            let arc: Arc<dyn IInferenceBackend> = Arc::new(ov);
            self.main = Some(Arc::clone(&arc));
            return Ok(arc);
        }
        let ort: Arc<dyn IInferenceBackend> = Arc::new(OnnxBackend::new(false)?);
        self.main = Some(Arc::clone(&ort));
        Ok(ort)
    }

    fn cpu_ort_backend(&mut self) -> Result<Arc<dyn IInferenceBackend>, WorkerError> {
        if let Some(ref b) = self.cpu_ort {
            return Ok(Arc::clone(b));
        }
        let ort: Arc<dyn IInferenceBackend> = Arc::new(OnnxBackend::new(true)?);
        self.cpu_ort = Some(Arc::clone(&ort));
        Ok(ort)
    }

    fn backend_for(&mut self, d: &DeviceInfo) -> Result<Arc<dyn IInferenceBackend>, WorkerError> {
        let main = self.main_backend()?;
        if d.ep == "DirectML" && !main.is_onnx_backend() {
            if let Some(ref b) = self.gpu_ort {
                return Ok(Arc::clone(b));
            }
            let ort: Arc<dyn IInferenceBackend> = Arc::new(OnnxBackend::new(false)?);
            self.gpu_ort = Some(Arc::clone(&ort));
            return Ok(ort);
        }
        Ok(main)
    }

    fn adapters(&mut self) -> &[DxgiAdapter] {
        if self.adapters.is_none() {
            self.adapters = Some(if self.probe_dml {
                DxgiAdapters::enumerate()
            } else {
                Vec::new()
            });
        }
        self.adapters.as_deref().unwrap_or(&[])
    }

    fn dml_available(&mut self) -> bool {
        if let Some(d) = self.dml {
            return d;
        }
        let v = if self.probe_dml {
            OnnxBackend::dml_available()
        } else {
            false
        };
        self.dml = Some(v);
        v
    }

    fn gpus(&mut self) -> Vec<DeviceInfo> {
        if let Some(ref g) = self.gpus {
            return g.clone();
        }
        let main_devs = self
            .main_backend()
            .map(|b| b.devices().to_vec())
            .unwrap_or_default();
        let dml = self.dml_available();
        let adapters = self.adapters().to_vec();
        let gpus = GpuSelection::all(&main_devs, &adapters, dml);
        let desc: Vec<String> = gpus.iter().map(|g| format!("{} {}", g.id, g.name)).collect();
        Log::info(&format!("GPUs: [{}]", desc.join("; ")));
        self.gpus = Some(gpus.clone());
        gpus
    }

    fn resolve_gpu(&mut self) -> Option<DeviceInfo> {
        let main_devs = self
            .main_backend()
            .map(|b| b.devices().to_vec())
            .unwrap_or_default();
        if GpuSelection::is_auto(Some(&self.s.gpu_adapter)) {
            return GpuSelection::auto(&main_devs);
        }
        let dml = self.dml_available();
        let adapters = self.adapters().to_vec();
        let setting = self.s.gpu_adapter.clone();
        let (gpu, warning) = GpuSelection::resolve(Some(&setting), &main_devs, &adapters, dml);
        if let Some(w) = warning {
            Log::warn(&w);
        } else {
            let ep = gpu.as_ref().map(|g| g.ep.as_str()).unwrap_or("");
            let id = gpu.as_ref().map(|g| g.id.as_str()).unwrap_or("");
            let name = gpu.as_ref().map(|g| g.name.as_str()).unwrap_or("");
            Log::info(&format!(
                "inference.gpuAdapter '{}' -> {ep} {id} {name}",
                setting
            ));
        }
        gpu
    }

    fn find_device(&mut self, id: &str) -> Option<DeviceInfo> {
        if let Ok(main) = self.main_backend()
            && let Some(d) = main.devices().iter().find(|d| d.id == id)
        {
            return Some(d.clone());
        }
        if id.starts_with("DML:") {
            return self.gpus().into_iter().find(|d| d.id == id);
        }
        None
    }

    fn missing(&self) -> Vec<String> {
        let mut m = Vec::new();
        if ModelCatalog::load(&self.s.model_dir, ModelCatalog::EMBEDDING_GEMMA).is_none() {
            m.push(ModelCatalog::EMBEDDING_GEMMA.to_string());
        }
        if ModelCatalog::load(&self.s.model_dir, ModelCatalog::SIGLIP).is_none() {
            m.push(ModelCatalog::SIGLIP.to_string());
        }
        if !ModelCatalog::whisper_present(&self.s.model_dir) {
            m.push(ModelCatalog::WHISPER.to_string());
        }
        if ModelCatalog::vad_path(&self.s.model_dir).is_none() {
            m.push(ModelCatalog::VAD.to_string());
        }
        if ModelCatalog::load(&self.s.model_dir, ModelCatalog::NSFW).is_none() {
            m.push(ModelCatalog::NSFW.to_string());
        }
        if ModelCatalog::load(&self.s.model_dir, ModelCatalog::NSFW_DETECTOR).is_none() {
            m.push(ModelCatalog::NSFW_DETECTOR.to_string());
        }
        if ModelCatalog::load(&self.s.model_dir, ModelCatalog::OCR).is_none() {
            m.push(ModelCatalog::OCR.to_string());
        }
        if ModelCatalog::load(&self.s.model_dir, ModelCatalog::NER).is_none() {
            m.push(ModelCatalog::NER.to_string());
        }
        m
    }

    fn models(
        &mut self,
    ) -> Result<(Option<ModelManifest>, Option<ModelManifest>, DeviceCacheKey), WorkerError> {
        let primary = ModelCatalog::load(&self.s.model_dir, ModelCatalog::EMBEDDING_GEMMA);
        let fallback = ModelCatalog::load(&self.s.model_dir, ModelCatalog::SIGLIP);
        if primary.is_none() && fallback.is_none() {
            return Err(WorkerError::ModelNotInstalled(
                ModelCatalog::EMBEDDING_GEMMA.to_string(),
            ));
        }
        let b = self.main_backend()?;
        let model_hash = format!(
            "{}+{}",
            primary.as_ref().map(|m| m.hash.as_str()).unwrap_or("none"),
            fallback.as_ref().map(|m| m.hash.as_str()).unwrap_or("none")
        );
        let key = DeviceCacheKey {
            runtime: b.name().to_string(),
            runtime_version: b.version().to_string(),
            npu_driver: b.npu_driver_version().to_string(),
            model_hash,
            setting: DeviceCacheKey::setting_of(&self.s.device, &self.s.gpu_adapter),
            image_tokens: self.s.image_tokens,
        };
        Ok((primary, fallback, key))
    }

    fn compiled_dir(&self, m: &ModelManifest, key: &DeviceCacheKey) -> PathBuf {
        let sub_key = DeviceCacheKey {
            runtime: key.runtime.clone(),
            runtime_version: key.runtime_version.clone(),
            npu_driver: key.npu_driver.clone(),
            model_hash: m.hash.clone(),
            setting: String::new(),
            image_tokens: 0,
        };
        DeviceCache::compiled_dir(
            &DataPaths::compiled_cache(&self.home),
            &m.name,
            &sub_key.compiled_id(),
        )
    }

    fn ensure_embedder(&mut self) -> Result<(), WorkerError> {
        if self.embedder.is_some() {
            return Ok(());
        }
        self.select_device(false).map(|_| ())
    }

    fn without_failed(&self, devices: &[DeviceInfo]) -> Vec<DeviceInfo> {
        devices
            .iter()
            .filter(|d| !self.failed_devices.contains(&d.id))
            .cloned()
            .collect()
    }

    fn select_device(&mut self, force: bool) -> Result<BenchResult, WorkerError> {
        let (primary, fallback, key) = self.models()?;
        let cache_path = DataPaths::device_cache(&self.home);
        // inference.gpuAdapter is resolved against today's adapters (a DXGI index can move when cards change).
        let gpu = if self.s.device == "gpu" {
            self.resolve_gpu()
        } else {
            None
        };

        if !force
            && let Some(cached) = DeviceCache::read(&cache_path, &key)
            && let Some(cc) = cached.chosen
        {
            let mut dev = self.find_device(&cc.device_id);
            let text_dev = match cc.text_device_id.as_deref() {
                Some(tid) => self.find_device(tid),
                None => dev.clone(),
            };
            let m = if cc.model == ModelCatalog::SIGLIP {
                fallback.as_ref()
            } else {
                primary.as_ref()
            };
            if let Some(ref d) = dev
                && d.kind == DeviceKind::Gpu
                && let Some(ref g) = gpu
                && d.id != g.id
            {
                dev = None;
            }
            if dev
                .as_ref()
                .is_some_and(|d| self.failed_devices.contains(&d.id))
                || text_dev
                    .as_ref()
                    .is_some_and(|td| self.failed_devices.contains(&td.id))
            {
                dev = None;
            }
            if let (Some(d), Some(td), Some(manifest)) = (dev, text_dev, m) {
                let started = Instant::now();
                let compiled = self.compiled_dir(manifest, &key);
                let backend = self.backend_for(&d)?;
                let mut e = Embedder::new(
                    backend,
                    d,
                    manifest.clone(),
                    self.s.image_tokens,
                    Some(compiled),
                    Some(td),
                )?;
                match e.load_all(true, true) {
                    Ok(()) => {
                        let ms = started.elapsed().as_millis();
                        Log::info(&format!(
                            "device from cache: {} {} {}, load {ms} ms",
                            cc.device, cc.ep, cc.model
                        ));
                        self.embedder = Some(e);
                        self.choice = Some(cc.clone());
                        return Ok(cached.bench.unwrap_or(BenchResult {
                            chosen: cc.device,
                            devices: Vec::new(),
                            tokens: None,
                            chosen_adapter: None,
                        }));
                    }
                    Err(ex) => {
                        drop(e);
                        Log::warn(&format!(
                            "cached device failed to load ({}); benchmarking again",
                            ex.wire_message()
                        ));
                    }
                }
            }
        }

        let main = self.main_backend()?;
        let main_devices = main.devices().to_vec();
        let all_gpus = self.gpus();
        let mut outcomes: Vec<BenchOutcome> = Vec::new();
        let mut keep: HashMap<OutcomeKey, Embedder> = HashMap::new();
        let mut tokens: Option<Vec<TokenRow>> = if force { Some(Vec::new()) } else { None };
        let base_allowed = DevicePolicy::allowed(&self.s.device, &main_devices, gpu.as_ref());
        let mut allowed = if force {
            base_allowed
        } else {
            self.without_failed(&base_allowed)
        };
        if allowed.iter().any(|d| d.ep == "DirectML") {
            allowed = allowed
                .into_iter()
                .map(|d| {
                    all_gpus
                        .iter()
                        .find(|g| g.id == d.id)
                        .cloned()
                        .unwrap_or(d)
                })
                .collect();
        }
        let allowed_ids: HashSet<String> = allowed.iter().map(|d| d.id.clone()).collect();
        let measured = if force {
            DevicePolicy::bench_devices(&allowed, &all_gpus)
        } else {
            allowed.clone()
        };

        if let Some(ref p) = primary {
            self.bench_model(
                p,
                &measured,
                &key,
                &mut outcomes,
                &mut keep,
                &allowed_ids,
            )?;
        }
        let npu_primary_failed = outcomes.iter().any(|o| {
            o.model == ModelCatalog::EMBEDDING_GEMMA && o.device.kind == DeviceKind::Npu && !o.ok
        });
        let primary_all_failed = primary.is_none()
            || !outcomes.iter().any(|o| {
                o.model == ModelCatalog::EMBEDDING_GEMMA && o.ok && allowed_ids.contains(&o.device.id)
            });
        let keep_fallback =
            !force && self.replan_model.as_deref() == Some(ModelCatalog::SIGLIP);
        if let Some(ref fb) = fallback
            && (npu_primary_failed || primary_all_failed || keep_fallback)
        {
            let fb_devices: Vec<DeviceInfo> = if npu_primary_failed {
                allowed
                    .iter()
                    .filter(|d| matches!(d.kind, DeviceKind::Npu | DeviceKind::Cpu))
                    .cloned()
                    .collect()
            } else {
                allowed.clone()
            };
            self.bench_model(
                fb,
                &fb_devices,
                &key,
                &mut outcomes,
                &mut keep,
                &allowed_ids,
            )?;
        }

        let mut candidates: Vec<BenchOutcome> = outcomes
            .iter()
            .filter(|o| allowed_ids.contains(&o.device.id))
            .cloned()
            .collect();
        // A replan after a run-time failure keeps the model it unloaded when that model passes somewhere (A-M3).
        if !force
            && let Some(ref km) = self.replan_model
            && candidates.iter().any(|o| o.ok && o.model == *km)
        {
            candidates.retain(|o| o.model == *km);
        }

        let chosen = DevicePolicy::choose(
            &candidates,
            ModelCatalog::EMBEDDING_GEMMA,
            ModelCatalog::SIGLIP,
        )
        .cloned();

        let mut chosen_embedder = chosen.as_ref().and_then(|c| keep.remove(&outcome_key(c)));
        // Dispose non-chosen compiled embedders before optional budget sweep so NPU memory stays free.
        drop(keep);

        if let (Some(tok_rows), Some(c), Some(emb)) =
            (tokens.as_mut(), chosen.as_ref(), chosen_embedder.as_mut())
        {
            bench_budgets(emb, c, tok_rows);
        }

        let token_rows: Option<Vec<TokenBench>> = tokens.map(|rows| {
            rows.into_iter()
                .filter(|t| chosen.as_ref().is_some_and(|c| c.model == t.model))
                .map(|t| t.row)
                .collect()
        });
        let gpus_for_report: Vec<DeviceInfo> = if outcomes.iter().any(|o| o.device.kind == DeviceKind::Gpu) {
            all_gpus
        } else {
            Vec::new()
        };
        let auto_gpu = GpuSelection::auto(&main_devices);
        let devices_out: Vec<BenchDevice> = outcomes
            .iter()
            .map(|o| RealEngine::to_bench_device(o, &gpus_for_report, auto_gpu.as_ref()))
            .collect();
        let result = BenchResult {
            chosen: chosen
                .as_ref()
                .map(|c| c.device.label().to_string())
                .unwrap_or_else(|| "none".to_string()),
            devices: devices_out,
            tokens: token_rows.filter(|v| !v.is_empty()),
            chosen_adapter: chosen.as_ref().and_then(|c| {
                if c.device.kind == DeviceKind::Gpu && gpus_for_report.len() > 1 {
                    Some(c.device.name.clone())
                } else {
                    None
                }
            }),
        };

        let Some(chosen) = chosen else {
            let why = outcomes
                .iter()
                .find_map(|o| o.error.clone())
                .unwrap_or_else(|| "no device passed".to_string());
            return Err(WorkerError::DeviceFailed(why));
        };
        let chosen_embedder = chosen_embedder.expect("chosen outcome had embedder");
        let new_choice = DeviceChoice {
            device: chosen.device.label().to_string(),
            device_id: chosen.device.id.clone(),
            ep: chosen.device.ep.clone(),
            model: chosen.model.clone(),
            text_device_id: if chosen.split() {
                chosen.text_device.as_ref().map(|d| d.id.clone())
            } else {
                None
            },
        };
        if force {
            self.failed_devices.remove(&new_choice.device_id);
            if let Some(ref tid) = new_choice.text_device_id {
                self.failed_devices.remove(tid);
            }
        }
        // SF6: persist when forced, no failed devices remain, or the replan switched embedding model.
        let model_switched = !force
            && self
                .replan_model
                .as_ref()
                .is_some_and(|was| was != &new_choice.model);
        if model_switched {
            Log::warn(&format!(
                "replan switched the embedding model {} -> {}; device.json keeps it",
                self.replan_model.as_deref().unwrap_or(""),
                new_choice.model
            ));
        }
        if force || self.failed_devices.is_empty() || model_switched {
            let file = DeviceCacheFile {
                key: Some(key),
                chosen: Some(new_choice.clone()),
                bench: Some(result.clone()),
                written_utc: Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
            };
            if let Err(e) = DeviceCache::write(&cache_path, &file) {
                Log::warn(&format!("device cache write failed: {e}"));
            }
        }
        if model_switched {
            self.replan_model = Some(new_choice.model.clone());
        }
        let text_note = new_choice
            .text_device_id
            .as_deref()
            .map(|t| format!(" text on {t}"))
            .unwrap_or_default();
        Log::info(&format!(
            "device chosen: {} ({} {}) {}{}",
            new_choice.device,
            new_choice.device_id,
            chosen.device.name,
            new_choice.model,
            text_note
        ));
        self.embedder = Some(chosen_embedder);
        self.choice = Some(new_choice);
        Ok(result)
    }

    fn bench_model(
        &mut self,
        m: &ModelManifest,
        devices: &[DeviceInfo],
        key: &DeviceCacheKey,
        outcomes: &mut Vec<BenchOutcome>,
        keep: &mut HashMap<OutcomeKey, Embedder>,
        keep_ids: &HashSet<String>,
    ) -> Result<(), WorkerError> {
        let image = RealEngine::bench_image();
        let mut ref_img: Option<Vec<f32>> = None;
        let mut ref_txt: Option<Vec<f32>> = None;
        let mut order: Vec<DeviceInfo> = devices
            .iter()
            .filter(|d| d.kind == DeviceKind::Cpu)
            .cloned()
            .collect();
        order.extend(
            devices
                .iter()
                .filter(|d| d.kind != DeviceKind::Cpu)
                .cloned(),
        );
        let cpu = devices.iter().find(|d| d.kind == DeviceKind::Cpu).cloned();
        for dev in order {
            let should_keep = keep_ids.contains(&dev.id);
            let o = self.bench_one(
                m,
                &dev,
                &dev,
                key,
                &image,
                &mut ref_img,
                &mut ref_txt,
                if should_keep { Some(keep) } else { None },
            );
            let try_split = !o.ok
                && dev.kind != DeviceKind::Cpu
                && ref_img.is_some()
                && m.vision
                    .iter()
                    .any(|v| v.fusion.as_ref().is_some_and(|f| f.inputs_embeds.is_some()));
            outcomes.push(o);
            if try_split && let Some(ref c) = cpu {
                let same_backend = match (self.backend_for(&dev), self.backend_for(c)) {
                    (Ok(b1), Ok(b2)) => Arc::ptr_eq(&b1, &b2),
                    _ => false,
                };
                if same_backend {
                    let split_o = self.bench_one(
                        m,
                        &dev,
                        c,
                        key,
                        &image,
                        &mut ref_img,
                        &mut ref_txt,
                        if should_keep { Some(keep) } else { None },
                    );
                    outcomes.push(split_o);
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn bench_one(
        &mut self,
        m: &ModelManifest,
        dev: &DeviceInfo,
        text_dev: &DeviceInfo,
        key: &DeviceCacheKey,
        image: &RgbImage,
        ref_img: &mut Option<Vec<f32>>,
        ref_txt: &mut Option<Vec<f32>>,
        keep: Option<&mut HashMap<OutcomeKey, Embedder>>,
    ) -> BenchOutcome {
        let where_str = if dev.id == text_dev.id {
            dev.id.clone()
        } else {
            format!("{}+text:{}", dev.id, text_dev.id)
        };
        let dir = self.compiled_dir(m, key);
        let blobs_before = count_files(&dir);
        let attempt = (|| -> Result<(Embedder, BenchOutcome), WorkerError> {
            let started = Instant::now();
            let backend = self.backend_for(dev)?;
            let is_ov = backend.is_openvino_backend();
            let mut e = Embedder::new(
                backend,
                dev.clone(),
                m.clone(),
                self.s.image_tokens,
                Some(dir.clone()),
                Some(text_dev.clone()),
            )?;
            e.load_all(true, true)?;
            let compile_ms = started.elapsed().as_secs_f64() * 1000.0;
            let cached = is_ov && count_files(&dir) == blobs_before;
            let runs = if dev.kind == DeviceKind::Cpu { 1 } else { 3 };
            let (img, img_ms) = time_fn(|| e.embed_image(image, None), runs)?;
            let text_runs = if dev.kind == text_dev.kind { runs } else { 1 };
            let (txt, txt_ms) = time_fn(|| e.embed_text(BENCH_TEXT, TextKind::Query), text_runs)?;
            let nan = VectorMath::has_non_finite(&img) || VectorMath::has_non_finite(&txt);
            let cos = if dev.kind == DeviceKind::Cpu && text_dev.kind == DeviceKind::Cpu {
                if nan {
                    return Err(WorkerError::DeviceFailed(
                        "CPU reference output contains NaN".to_string(),
                    ));
                }
                *ref_img = Some(img);
                *ref_txt = Some(txt);
                1.0
            } else {
                let (Some(ri), Some(rt)) = (ref_img.as_deref(), ref_txt.as_deref()) else {
                    return Err(WorkerError::DeviceFailed("no CPU reference".to_string()));
                };
                let c1 = VectorMath::cosine(&img, ri).unwrap_or(f64::NAN);
                let c2 = VectorMath::cosine(&txt, rt).unwrap_or(f64::NAN);
                c1.min(c2)
            };
            let ok = DevicePolicy::accept(cos, nan);
            let err = if ok {
                None
            } else if nan {
                Some("output contains NaN".to_string())
            } else {
                Some(format!("cosine {cos:.4} below {}", DevicePolicy::MIN_COSINE))
            };
            let o = BenchOutcome {
                device: dev.clone(),
                model: m.name.clone(),
                ok,
                image_ms: img_ms,
                text_ms: txt_ms,
                cosine: cos,
                error: err,
                compile_ms,
                text_device: Some(text_dev.clone()),
                compile_cached: cached,
            };
            let dropped = if e.dropped_lengths.is_empty() {
                String::new()
            } else {
                let items: Vec<String> = e.dropped_lengths.iter().map(i32::to_string).collect();
                format!(" dropped lengths [{}]", items.join(","))
            };
            Log::info(&format!(
                "bench {} {where_str}: ok={ok} compile={compile_ms:.0}ms image={img_ms:.1}ms text={txt_ms:.1}ms cos={cos:.6}{dropped}",
                m.name
            ));
            Ok((e, o))
        })();

        match attempt {
            Ok((e, o)) => {
                if o.ok
                    && let Some(map) = keep
                {
                    map.insert(outcome_key(&o), e);
                }
                o
            }
            Err(ex) => {
                let msg = match &ex {
                    WorkerError::DeviceFailed(detail) => WorkerErrors::clean(detail),
                    other => other.wire_message(),
                };
                Log::warn(&format!("bench {} {where_str} failed: {msg}", m.name));
                BenchOutcome {
                    device: dev.clone(),
                    model: m.name.clone(),
                    ok: false,
                    image_ms: 0.0,
                    text_ms: 0.0,
                    cosine: f64::NAN,
                    error: Some(msg),
                    compile_ms: 0.0,
                    text_device: Some(text_dev.clone()),
                    compile_cached: false,
                }
            }
        }
    }

    fn ensure_nsfw(&mut self, detector: bool) -> Result<(), WorkerError> {
        if let Some(ref gate) = self.nsfw
            && (!detector || gate.detector.is_some())
        {
            return Ok(());
        }
        let cls = ModelCatalog::load(&self.s.model_dir, ModelCatalog::NSFW)
            .ok_or_else(|| WorkerError::ModelNotInstalled(ModelCatalog::NSFW.to_string()))?;
        let det = if detector {
            Some(
                ModelCatalog::load(&self.s.model_dir, ModelCatalog::NSFW_DETECTOR).ok_or_else(
                    || WorkerError::ModelNotInstalled(ModelCatalog::NSFW_DETECTOR.to_string()),
                )?,
            )
        } else {
            None
        };
        self.nsfw = None;

        let det_for_key = det
            .as_ref()
            .cloned()
            .or_else(|| ModelCatalog::load(&self.s.model_dir, ModelCatalog::NSFW_DETECTOR));
        let combined_hash = format!(
            "{}+{}",
            cls.hash,
            det_for_key.as_ref().map(|d| d.hash.as_str()).unwrap_or("none")
        );
        let setting = self.s.nsfw_device.clone();
        let gpu_adapter = self.s.nsfw_gpu_adapter.clone();
        let cands = self.candidates(&setting, &gpu_adapter, "nsfw", &combined_hash, true, true)?;
        let nsfw_cands: Vec<NsfwCandidate<'_>> = cands
            .iter()
            .map(|c| NsfwCandidate {
                device: c.device.clone(),
                backend: c.backend.as_ref(),
                cache_dir: c.cache_dir.clone(),
            })
            .collect();
        let gate = NsfwGate::load(&nsfw_cands, &cls, det.as_ref())?;
        self.nsfw = Some(gate);
        Ok(())
    }

    fn candidates(
        &mut self,
        setting: &str,
        gpu_adapter: &str,
        tag: &str,
        model_hash: &str,
        allow_npu: bool,
        auto_discrete: bool,
    ) -> Result<Vec<BackendCandidate>, WorkerError> {
        let mut available: Vec<DeviceInfo> = Vec::new();
        let mut backend_map: HashMap<String, Arc<dyn IInferenceBackend>> = HashMap::new();
        if matches!(setting, "auto" | "npu" | "igpu")
            && let Ok(main) = self.main_backend()
            && main.is_openvino_backend()
        {
            for d in main
                .devices()
                .iter()
                .filter(|d| d.kind != DeviceKind::Cpu && (allow_npu || d.kind != DeviceKind::Npu))
            {
                backend_map.insert(d.id.clone(), Arc::clone(&main));
                available.push(d.clone());
            }
        }
        let cpu_ort = self.cpu_ort_backend()?;
        if self.probe_dml
            && (setting == "gpu" || (setting == "auto" && auto_discrete))
            && let Some(dml) = dml_device(gpu_adapter, tag)
        {
            backend_map.insert(dml.id.clone(), Arc::clone(&cpu_ort));
            available.push(dml);
        }
        if let Some(cpu) = cpu_ort
            .devices()
            .iter()
            .find(|d| d.kind == DeviceKind::Cpu)
            .cloned()
        {
            backend_map.insert(cpu.id.clone(), Arc::clone(&cpu_ort));
            available.push(cpu);
        }

        let order = self.without_failed(&NsfwDevicePlan::order(setting, &available));
        let ov_cache = if order.iter().any(|d| d.ep == "OpenVINO") {
            let main = self.main_backend()?;
            let cache_id = DeviceCacheKey {
                runtime: main.name().to_string(),
                runtime_version: main.version().to_string(),
                npu_driver: main.npu_driver_version().to_string(),
                model_hash: model_hash.to_string(),
                setting: String::new(),
                image_tokens: 0,
            }
            .compiled_id();
            Some(DeviceCache::compiled_dir(
                &DataPaths::compiled_cache(&self.home),
                tag,
                &cache_id,
            ))
        } else {
            None
        };

        let order_log: Vec<String> = order
            .iter()
            .map(|d| format!("{} {} {}", NsfwDevicePlan::label(d), d.ep, d.id))
            .collect();
        Log::info(&format!(
            "{tag} device order ({setting}): {}",
            order_log.join(", ")
        ));

        Ok(order
            .into_iter()
            .map(|d| {
                let cache_dir = if d.ep == "OpenVINO" {
                    ov_cache.clone()
                } else {
                    None
                };
                let backend = Arc::clone(&backend_map[&d.id]);
                BackendCandidate {
                    device: d,
                    backend,
                    cache_dir,
                }
            })
            .collect())
    }

    fn ocr_language_codes(&self) -> Vec<String> {
        if !self.s.ocr_languages.eq_ignore_ascii_case("auto") && !self.s.ocr_languages.trim().is_empty() {
            return self
                .s
                .ocr_languages
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect();
        }
        let mut list = match user_profile_languages() {
            Ok(v) => v,
            Err(e) => {
                Log::warn(&format!("user profile languages: {e}"));
                Vec::new()
            }
        };
        if !list
            .iter()
            .any(|l| l.len() >= 2 && l[..2].eq_ignore_ascii_case("en"))
        {
            list.push("en-US".to_string());
        }
        list
    }

    fn ensure_windows_ocr(&mut self) -> Result<(), WorkerError> {
        if self.win_ocr.is_some() {
            return Ok(());
        }
        let codes = self.ocr_language_codes();
        let ocr = WindowsOcr::try_create(&codes)
            .ok_or_else(|| WorkerError::ModelNotInstalled(ModelCatalog::WINDOWS_OCR.to_string()))?;
        Log::info(&format!("windows ocr ready ({})", ocr.language));
        self.win_ocr = Some(ocr);
        Ok(())
    }

    fn ensure_ocr(&mut self) -> Result<(), WorkerError> {
        if self.ocr.is_some() {
            return Ok(());
        }
        let m = ModelCatalog::load(&self.s.model_dir, ModelCatalog::OCR)
            .ok_or_else(|| WorkerError::ModelNotInstalled(ModelCatalog::OCR.to_string()))?;
        let spec = m
            .ocr
            .as_ref()
            .ok_or_else(|| WorkerError::Internal("ocr manifest lacks ocr".into()))?;
        let codes = self.ocr_language_codes();
        let recs = OcrMath::pick_recognisers(&spec.rec, &codes);
        let rec_names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
        Log::info(&format!(
            "ocr languages [{}] -> recognisers [{}]",
            codes.join(", "),
            rec_names.join(", ")
        ));
        let setting = self.s.ocr_device.clone();
        let cands = self.candidates(&setting, "auto", "ocr", &m.hash, true, false)?;
        let mut last: Option<String> = None;
        for c in cands {
            let started = Instant::now();
            match PaddleOcr::new(
                Arc::clone(&c.backend),
                c.device.clone(),
                &m,
                &recs,
                c.cache_dir.as_deref(),
                None,
            )
            .and_then(|mut o| {
                o.warm_up()?;
                Ok(o)
            }) {
                Ok(o) => {
                    let label = NsfwDevicePlan::label(&c.device);
                    let loaded_recs = o.recognisers().join(", ");
                    Log::info(&format!(
                        "ocr on {label} ({} {}): [{loaded_recs}], load {} ms",
                        c.device.ep,
                        c.device.id,
                        started.elapsed().as_millis()
                    ));
                    self.ocr = Some(o);
                    return Ok(());
                }
                Err(WorkerError::DeviceFailed(detail)) => {
                    let msg = WorkerErrors::clean(&detail);
                    Log::warn(&format!(
                        "ocr on {} ({} {}) failed: {msg}",
                        NsfwDevicePlan::label(&c.device),
                        c.device.ep,
                        c.device.id
                    ));
                    last = Some(msg);
                }
                Err(e) => {
                    // C14: non-device/config failures abort immediately so hello stays isolated.
                    return Err(e);
                }
            }
        }
        Err(WorkerError::DeviceFailed(match last {
            None => "ocr has no device".to_string(),
            Some(l) => format!("ocr failed on every device ({l})"),
        }))
    }

    fn preload_ocr(&mut self, missing: &[String]) -> Option<String> {
        if !self.s.ocr_enabled {
            return None;
        }
        let attempt = (|| -> Result<Option<String>, WorkerError> {
            if self.s.ocr_engine == "windows" {
                self.ensure_windows_ocr()?;
                return Ok(self
                    .win_ocr
                    .as_ref()
                    .map(|w| format!("windows {}", w.language)));
            }
            if missing.iter().any(|m| m == ModelCatalog::OCR) {
                return Ok(None);
            }
            self.ensure_ocr()?;
            Ok(self
                .ocr
                .as_ref()
                .map(|o| format!("onnx {}", NsfwDevicePlan::label(&o.device))))
        })();
        match attempt {
            Ok(v) => v,
            Err(e) => {
                Log::warn(&format!("hello: ocr not loaded: {}", e.wire_message()));
                None
            }
        }
    }

    fn ocr_onnx(&mut self, rgb: &RgbImage) -> Result<Vec<OcrLine>, WorkerError> {
        self.ensure_ocr()?;
        let dev = self.ocr.as_ref().expect("ocr").device.clone();
        let res = self.ocr.as_mut().expect("ocr").read(rgb);
        match res {
            Ok(lines) => {
                self.run_failures.remove("ocr");
                Ok(lines)
            }
            Err(e) => Err(self.record_failure("ocr", e, &[dev])),
        }
    }

    fn ensure_ner(&mut self) -> Result<(), WorkerError> {
        if self.ner.is_some() {
            return Ok(());
        }
        let m = ModelCatalog::load(&self.s.model_dir, ModelCatalog::NER)
            .ok_or_else(|| WorkerError::ModelNotInstalled(ModelCatalog::NER.to_string()))?;
        let cands = self.candidates("auto", "auto", "ner", &m.hash, false, false)?;
        let mut last: Option<String> = None;
        for c in cands {
            let started = Instant::now();
            match PiiNer::new(c.backend.as_ref(), &c.device, &m, c.cache_dir.as_deref()) {
                Ok(n) => {
                    let label = NsfwDevicePlan::label(&c.device);
                    Log::info(&format!(
                        "ner on {label} ({} {}), load {} ms",
                        c.device.ep,
                        c.device.id,
                        started.elapsed().as_millis()
                    ));
                    self.ner = Some(n);
                    return Ok(());
                }
                Err(e) => {
                    let msg = match &e {
                        WorkerError::DeviceFailed(detail) => WorkerErrors::clean(detail),
                        other => other.wire_message(),
                    };
                    Log::warn(&format!(
                        "ner on {} ({} {}) failed: {msg}",
                        NsfwDevicePlan::label(&c.device),
                        c.device.ep,
                        c.device.id
                    ));
                    last = Some(msg);
                }
            }
        }
        Err(WorkerError::DeviceFailed(match last {
            None => "ner has no device".to_string(),
            Some(l) => format!("ner failed on every device ({l})"),
        }))
    }

    fn run_ner(&mut self, texts: &[String]) -> Result<Vec<Option<Vec<OcrEntity>>>, WorkerError> {
        self.ensure_ner()?;
        let dev = self.ner.as_ref().expect("ner").device().clone();
        let res = self.ner.as_ref().expect("ner").find(texts);
        match res {
            Ok(ents) => {
                self.run_failures.remove("ner");
                Ok(ents)
            }
            Err(e) => Err(self.record_failure("ner", e, &[dev])),
        }
    }

    fn load_whisper(&mut self) -> Result<(), WorkerError> {
        if self.whisper.is_some() {
            return Ok(());
        }
        if !ModelCatalog::whisper_present(&self.s.model_dir) {
            return Err(WorkerError::ModelNotInstalled(
                ModelCatalog::WHISPER.to_string(),
            ));
        }
        if !WhisperGenAi::runtime_present() {
            return Err(WorkerError::DeviceFailed(
                "OpenVINO GenAI runtime not found".to_string(),
            ));
        }
        let main = self.main_backend()?;
        let ov_devs = if main.is_openvino_backend() {
            main.devices().to_vec()
        } else {
            vec![DeviceInfo::new(
                DeviceKind::Cpu,
                "CPU",
                "CPU",
                "OpenVINO",
                None,
            )]
        };
        let devices = self.without_failed(&DevicePolicy::whisper_allowed(&self.s.device, &ov_devs));
        let model_dir = ModelCatalog::dir(&self.s.model_dir, ModelCatalog::WHISPER);
        let key = DeviceCacheKey {
            runtime: main.name().to_string(),
            runtime_version: main.version().to_string(),
            npu_driver: main.npu_driver_version().to_string(),
            model_hash: folder_hash(&model_dir),
            setting: String::new(),
            image_tokens: 0,
        }
        .compiled_id();
        let cache_root = DataPaths::compiled_cache(&self.home);
        let mut last: Option<WorkerError> = None;
        for dev in devices {
            let cache_dir = DeviceCache::compiled_dir(&cache_root, ModelCatalog::WHISPER, &key);
            match WhisperGenAi::new(&model_dir, &dev.id, Some(&cache_dir)) {
                Ok(w) => {
                    Log::info(&format!("whisper on {} ({})", dev.label(), dev.id));
                    self.whisper = Some(w);
                    return Ok(());
                }
                Err(e) => {
                    Log::warn(&format!(
                        "whisper on {} ({}) failed: {}",
                        dev.label(),
                        dev.id,
                        e.wire_message()
                    ));
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            WorkerError::DeviceFailed("Whisper failed on every device".to_string())
        }))
    }

    fn record_failure(&mut self, component: &str, err: WorkerError, devices: &[DeviceInfo]) -> WorkerError {
        if let WorkerError::DeviceFailed(ref detail) = err {
            let n = self.run_failures.get(component).copied().unwrap_or(0) + 1;
            if n < RUN_FAILURES_BEFORE_REPLAN {
                self.run_failures.insert(component.to_string(), n);
                return err;
            }
            self.run_failures.remove(component);
            let mut ids = Vec::new();
            for d in devices {
                if d.kind != DeviceKind::Cpu {
                    self.failed_devices.insert(d.id.clone());
                }
                if !ids.contains(&d.id) {
                    ids.push(d.id.clone());
                }
            }
            Log::warn(&format!(
                "{component} failed {n} times in a row on {} ({}); unloaded, the next request picks another device",
                ids.join("+"),
                WorkerErrors::clean(detail)
            ));
            match component {
                "embedder" => self.drop_embedder(),
                "nsfw" => self.nsfw = None,
                "ocr" => self.ocr = None,
                "ner" => self.ner = None,
                _ => {}
            }
        }
        err
    }

    fn drop_embedder(&mut self) {
        if let Some(ref c) = self.choice {
            self.replan_model = Some(c.model.clone());
        }
        self.embedder = None;
        self.choice = None;
    }
}

impl IWorkerEngine for RealEngine {
    fn hello(&self, dim: i32) -> Result<WorkerHello, WorkerError> {
        let mut st = self.state.lock().expect("real engine lock");
        let missing = st.missing();
        let mut device = "CPU".to_string();
        let mut ep = match st.main_backend() {
            Ok(b) if b.name() == "OpenVINO" => "OpenVINO".to_string(),
            Ok(_) => "CPU".to_string(),
            Err(_) => "CPU".to_string(),
        };
        let mut model = "google/embeddinggemma-2".to_string();
        let mut rev = String::new();
        match st.ensure_embedder() {
            Ok(()) => {
                if let (Some(c), Some(e)) = (&st.choice, &st.embedder) {
                    device = c.device.clone();
                    ep = c.ep.clone();
                    model = e.manifest.id.clone();
                    rev = e.manifest.revision.clone();
                }
            }
            Err(WorkerError::ModelNotInstalled(_)) => {}
            Err(e) => {
                Log::error(&format!("hello: no usable device: {}", e.wire_message()));
            }
        }
        // Preload NSFW gate when filter enabled so hello reports `nsfwDevice`.
        let nsfw_device = if st.s.nsfw_filter && !missing.iter().any(|m| m == ModelCatalog::NSFW) {
            let need_det =
                st.s.nsfw_detector && !missing.iter().any(|m| m == ModelCatalog::NSFW_DETECTOR);
            match st.ensure_nsfw(need_det) {
                Ok(()) => st.nsfw.as_ref().map(|g| g.device_label.clone()),
                Err(e) => {
                    Log::warn(&format!("hello: nsfw gate not loaded: {}", e.wire_message()));
                    None
                }
            }
        } else {
            None
        };
        let vad = ModelCatalog::vad_path(&st.s.model_dir).is_some();
        let asr = ModelCatalog::whisper_present(&st.s.model_dir) && WhisperGenAi::runtime_present();
        let ocr_device = st.preload_ocr(&missing);
        Ok(WorkerHello {
            protocol: 1,
            device,
            ep,
            model,
            revision: rev,
            dim,
            asr,
            vad,
            missing,
            nsfw_device,
            ocr_device,
        })
    }

    fn embed_image(
        &self,
        image: &ImageInput<'_>,
        nsfw_threshold: Option<f64>,
        nsfw_grid: i32,
        nsfw_detector: Option<f64>,
    ) -> Result<ImageEmbedding, WorkerError> {
        let mut st = self.state.lock().expect("real engine lock");
        let rgb = ImagePipeline::decode(image)?;
        if nsfw_threshold.is_some() || nsfw_detector.is_some() {
            st.ensure_nsfw(nsfw_detector.is_some())?;
            let gate_dev = st.nsfw.as_ref().expect("nsfw gate").device.clone();
            if let Some(t) = nsfw_threshold {
                let res = st
                    .nsfw
                    .as_ref()
                    .expect("nsfw gate")
                    .classifier
                    .score(&rgb, t, nsfw_grid);
                match res {
                    Ok(score) => {
                        st.run_failures.remove("nsfw");
                        if score >= t {
                            return Ok(ImageEmbedding {
                                vector: None,
                                dropped: Some("nsfw".to_string()),
                            });
                        }
                    }
                    Err(e) => return Err(st.record_failure("nsfw", e, &[gate_dev])),
                }
            }
            if let Some(dt) = nsfw_detector {
                let res = st
                    .nsfw
                    .as_ref()
                    .expect("nsfw gate")
                    .detector
                    .as_ref()
                    .expect("nsfw detector")
                    .score(&rgb);
                match res {
                    Ok(score) => {
                        st.run_failures.remove("nsfw");
                        if score >= dt {
                            return Ok(ImageEmbedding {
                                vector: None,
                                dropped: Some("nsfw".to_string()),
                            });
                        }
                    }
                    Err(e) => return Err(st.record_failure("nsfw", e, &[gate_dev])),
                }
            }
        }
        st.ensure_embedder()?;
        let devs = {
            let emb = st.embedder.as_ref().expect("embedder");
            [emb.device.clone(), emb.text_device.clone()]
        };
        let res = st.embedder.as_mut().expect("embedder").embed_image(&rgb, None);
        match res {
            Ok(v) => {
                st.run_failures.remove("embedder");
                Ok(ImageEmbedding {
                    vector: Some(v),
                    dropped: None,
                })
            }
            Err(e) => Err(st.record_failure("embedder", e, &devs)),
        }
    }

    fn embed_text(&self, text: &str, kind: TextKind) -> Result<Vec<f32>, WorkerError> {
        let mut st = self.state.lock().expect("real engine lock");
        st.ensure_embedder()?;
        let devs = {
            let emb = st.embedder.as_ref().expect("embedder");
            [emb.device.clone(), emb.text_device.clone()]
        };
        let res = st
            .embedder
            .as_mut()
            .expect("embedder")
            .embed_text(text, kind);
        match res {
            Ok(v) => {
                st.run_failures.remove("embedder");
                Ok(v)
            }
            Err(e) => Err(st.record_failure("embedder", e, &devs)),
        }
    }

    fn transcribe(
        &self,
        pcm_16k_mono: &[f32],
        _source: &str,
        language: &str,
    ) -> Result<TranscribeResult, WorkerError> {
        let started = Instant::now();
        let mut st = self.state.lock().expect("real engine lock");
        if st.vad.is_none() {
            let vad_path = ModelCatalog::vad_path(&st.s.model_dir)
                .ok_or_else(|| WorkerError::ModelNotInstalled(ModelCatalog::VAD.to_string()))?;
            let cpu = st.cpu_ort_backend()?;
            st.vad = Some(SileroVad::new(cpu.as_ref(), &vad_path)?);
        }
        let segs = st.vad.as_ref().expect("vad").segments(pcm_16k_mono)?;
        if segs.is_empty() {
            return Ok(TranscribeResult {
                speech: false,
                text: String::new(),
                language: if language == "auto" {
                    "und".to_string()
                } else {
                    language.to_string()
                },
                segments: Vec::new(),
                ms: started.elapsed().as_millis() as i64,
            });
        }
        st.load_whisper()?;
        st.whisper
            .as_ref()
            .expect("whisper")
            .transcribe(pcm_16k_mono, language, started)
    }

    fn bench(&self) -> Result<BenchResult, WorkerError> {
        let mut st = self.state.lock().expect("real engine lock");
        st.embedder = None;
        st.choice = None;
        // User's bench chooses afresh; its reply tells the agent the model it chose.
        st.replan_model = None;
        st.select_device(true)
    }

    fn ocr(&self, image: &ImageInput<'_>, entities: bool) -> Result<OcrResult, WorkerError> {
        let started = Instant::now();
        let (lines, engine) = {
            let mut st = self.state.lock().expect("real engine lock");
            // E5/E2: the decoded pixels are zeroized on every path, early `?` returns included.
            let guard = crate::ZeroOnDrop::new(ImagePipeline::decode(image)?);
            let rgb = &*guard;
            let res = if st.s.ocr_engine == "windows" {
                st.ensure_windows_ocr()?;
                st.win_ocr
                    .as_ref()
                    .expect("win_ocr")
                    .read(rgb)
                    .map(|l| (l, "windows"))
            } else {
                st.ocr_onnx(rgb).map(|l| (l, "onnx"))
            };
            res?
        };
        let (lines, entities_missing) = if entities && !lines.is_empty() {
            let mut gate = self.ner_gate.lock().expect("ner gate lock");
            gate.apply(lines, |texts| {
                let mut st = self.state.lock().expect("real engine lock");
                st.run_ner(texts)
            })
        } else {
            (lines, false)
        };
        Ok(OcrResult {
            lines,
            engine: engine.to_string(),
            ms: round_dp(started.elapsed().as_secs_f64() * 1000.0, 2),
            entities_missing,
        })
    }

    fn embedder(&self) -> Option<EmbedderIdentity> {
        let st = self.state.lock().expect("real engine lock");
        let (e, c) = (st.embedder.as_ref()?, st.choice.as_ref()?);
        Some(EmbedderIdentity {
            model: e.manifest.id.clone(),
            revision: e.manifest.revision.clone(),
            device: c.device.clone(),
        })
    }
}

fn dml_device(adapter_setting: &str, what: &str) -> Option<DeviceInfo> {
    if !OnnxBackend::dml_available() {
        return None;
    }
    let adapters = DxgiAdapters::enumerate();
    let pick = DxgiAdapters::pick(Some(adapter_setting), &adapters);
    let desc: Vec<String> = adapters
        .iter()
        .map(|a| {
            format!(
                "{} {} {} MB",
                a.index,
                a.description,
                a.dedicated_video_memory >> 20
            )
        })
        .collect();
    let picked = pick
        .as_ref()
        .map(|p| format!("{} {}", p.index, p.description))
        .unwrap_or_else(|| "none".to_string());
    Log::info(&format!(
        "{what}: DXGI adapters [{}], setting '{adapter_setting}' -> {picked}",
        desc.join("; ")
    ));
    pick.map(|p| {
        DeviceInfo::new(
            DeviceKind::Gpu,
            format!("DML:{}", p.index),
            p.description,
            "DirectML",
            None,
        )
    })
}

fn bench_budgets(e: &mut Embedder, o: &BenchOutcome, tokens: &mut Vec<TokenRow>) {
    let image = RealEngine::bench_image();
    let runs = if o.device.kind == DeviceKind::Cpu { 1 } else { 3 };
    let configured = e.session_keys();
    for b in e.budgets() {
        if b == e.image_tokens() {
            tokens.push(TokenRow {
                model: o.model.clone(),
                row: TokenBench {
                    device: o.device.label().to_string(),
                    image_tokens: b,
                    image_ms: round_dp(o.image_ms, 1),
                },
            });
            continue;
        }
        let started = Instant::now();
        match time_fn(|| e.embed_image(&image, Some(b)), runs) {
            Ok((_, ms)) => {
                tokens.push(TokenRow {
                    model: o.model.clone(),
                    row: TokenBench {
                        device: o.device.label().to_string(),
                        image_tokens: b,
                        image_ms: round_dp(ms, 1),
                    },
                });
                Log::info(&format!(
                    "bench {} {} tokens={b}: image={ms:.1}ms (with compile {} ms)",
                    o.model,
                    o.device.id,
                    started.elapsed().as_millis()
                ));
            }
            Err(ex) => {
                Log::warn(&format!(
                    "bench {} {} tokens={b} failed: {}",
                    o.model,
                    o.device.id,
                    ex.wire_message()
                ));
            }
        }
    }
    e.drop_sessions_except(&configured);
}

fn count_files(dir: &Path) -> usize {
    fn walk(p: &Path, acc: &mut usize) {
        let Ok(entries) = std::fs::read_dir(p) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, acc);
            } else {
                *acc += 1;
            }
        }
    }
    let mut n = 0;
    walk(dir, &mut n);
    n
}

fn time_fn<F>(mut f: F, runs: usize) -> Result<(Vec<f32>, f64), WorkerError>
where
    F: FnMut() -> Result<Vec<f32>, WorkerError>,
{
    let mut v = f()?;
    let n = runs.max(1);
    let mut times = Vec::with_capacity(n);
    for _ in 0..n {
        let sw = Instant::now();
        v = f()?;
        times.push(sw.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    Ok((v, times[times.len() / 2]))
}

struct BackendCandidate {
    device: DeviceInfo,
    backend: Arc<dyn IInferenceBackend>,
    cache_dir: Option<PathBuf>,
}

fn folder_hash(dir: &Path) -> String {
    let mut entries: Vec<(String, u64)> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                if !meta.is_file() {
                    return None;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                Some((name, meta.len()))
            })
            .collect(),
        Err(_) => return String::new(),
    };
    entries.sort_by(|a, b| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()));
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for (name, len) in entries {
        h.update(format!("{name}:{len}|").as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn round_dp(v: f64, digits: i32) -> f64 {
    let factor = 10.0f64.powi(digits);
    (v * factor).round_ties_even() / factor
}
