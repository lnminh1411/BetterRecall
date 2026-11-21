// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use br_contracts::{TranscribeResult, TranscriptSegment};
use libloading::Library;

use crate::inference::{find_native_dll, load_native_library};
use crate::protocol::WorkerError;

#[derive(Clone, Copy)]
enum GenAiLoadError {
    NotFound,
    TooOld,
}

impl GenAiLoadError {
    fn into_worker_error(self) -> WorkerError {
        match self {
            Self::NotFound => WorkerError::DeviceFailed("OpenVINO GenAI runtime not found".into()),
            Self::TooOld => WorkerError::DeviceFailed("OpenVINO GenAI runtime too old".into()),
        }
    }
}

struct GenAiApi {
    create: unsafe extern "C" fn(*const c_char, *const c_char, usize, *mut *mut c_void, ...) -> c_int,
    pipeline_free: unsafe extern "C" fn(*mut c_void),
    get_generation_config: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int,
    generation_config_free: unsafe extern "C" fn(*mut c_void),
    set_language: unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int,
    set_task: unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int,
    set_return_timestamps: unsafe extern "C" fn(*mut c_void, u8) -> c_int,
    generate: unsafe extern "C" fn(*mut c_void, *const f32, usize, *mut c_void, *mut *mut c_void) -> c_int,
    decoded_results_free: unsafe extern "C" fn(*mut c_void),
    decoded_results_get_string: unsafe extern "C" fn(*mut c_void, *mut u8, *mut usize) -> c_int,
    decoded_results_has_chunks: unsafe extern "C" fn(*mut c_void, *mut u8) -> c_int,
    decoded_results_get_chunks_count: unsafe extern "C" fn(*mut c_void, *mut usize) -> c_int,
    decoded_results_get_chunk_at: unsafe extern "C" fn(*mut c_void, usize, *mut *mut c_void) -> c_int,
    chunk_free: unsafe extern "C" fn(*mut c_void),
    chunk_get_start_ts: unsafe extern "C" fn(*mut c_void, *mut f32) -> c_int,
    chunk_get_end_ts: unsafe extern "C" fn(*mut c_void, *mut f32) -> c_int,
    chunk_get_text: unsafe extern "C" fn(*mut c_void, *mut u8, *mut usize) -> c_int,
    _lib: Library,
    _ov_preload: Option<Library>,
}

unsafe impl Send for GenAiApi {}
unsafe impl Sync for GenAiApi {}

macro_rules! load_sym {
    ($lib:expr, $name:literal) => {
        *unsafe { $lib.get($name) }.map_err(|_| GenAiLoadError::TooOld)?
    };
}

impl GenAiApi {
    fn load() -> Result<Self, GenAiLoadError> {
        let Some(dll) = find_native_dll("openvino_genai_c.dll") else {
            return Err(GenAiLoadError::NotFound);
        };
        // In cargo test (debug_assertions), ensure core OpenVINO DLLs from NuGet/repo are mapped before
        // openvino_genai_c.dll resolves its imports if OpenVinoBackend wasn't loaded first.
        #[cfg(debug_assertions)]
        let ov_preload = find_native_dll("openvino_c.dll")
            .and_then(|p| load_native_library(&p).ok());
        #[cfg(not(debug_assertions))]
        let ov_preload: Option<Library> = None;

        let lib = load_native_library(&dll).map_err(|_| GenAiLoadError::NotFound)?;
        Ok(Self {
            create: load_sym!(lib, b"ov_genai_whisper_pipeline_create\0"),
            pipeline_free: load_sym!(lib, b"ov_genai_whisper_pipeline_free\0"),
            get_generation_config: load_sym!(lib, b"ov_genai_whisper_pipeline_get_generation_config\0"),
            generation_config_free: load_sym!(lib, b"ov_genai_whisper_generation_config_free\0"),
            set_language: load_sym!(lib, b"ov_genai_whisper_generation_config_set_language\0"),
            set_task: load_sym!(lib, b"ov_genai_whisper_generation_config_set_task\0"),
            set_return_timestamps: load_sym!(lib, b"ov_genai_whisper_generation_config_set_return_timestamps\0"),
            generate: load_sym!(lib, b"ov_genai_whisper_pipeline_generate\0"),
            decoded_results_free: load_sym!(lib, b"ov_genai_whisper_decoded_results_free\0"),
            decoded_results_get_string: load_sym!(lib, b"ov_genai_whisper_decoded_results_get_string\0"),
            decoded_results_has_chunks: load_sym!(lib, b"ov_genai_whisper_decoded_results_has_chunks\0"),
            decoded_results_get_chunks_count: load_sym!(lib, b"ov_genai_whisper_decoded_results_get_chunks_count\0"),
            decoded_results_get_chunk_at: load_sym!(lib, b"ov_genai_whisper_decoded_results_get_chunk_at\0"),
            chunk_free: load_sym!(lib, b"ov_genai_whisper_decoded_result_chunk_free\0"),
            chunk_get_start_ts: load_sym!(lib, b"ov_genai_whisper_decoded_result_chunk_get_start_ts\0"),
            chunk_get_end_ts: load_sym!(lib, b"ov_genai_whisper_decoded_result_chunk_get_end_ts\0"),
            chunk_get_text: load_sym!(lib, b"ov_genai_whisper_decoded_result_chunk_get_text\0"),
            _lib: lib,
            _ov_preload: ov_preload,
        })
    }
}

fn global_genai() -> Result<&'static GenAiApi, WorkerError> {
    static GENAI: OnceLock<Result<GenAiApi, GenAiLoadError>> = OnceLock::new();
    GENAI
        .get_or_init(GenAiApi::load)
        .as_ref()
        .map_err(|e| e.into_worker_error())
}

struct ConfigGuard<'a> {
    api: &'a GenAiApi,
    ptr: *mut c_void,
}

impl Drop for ConfigGuard<'_> {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.api.generation_config_free)(self.ptr) };
        }
    }
}

struct ResultsGuard<'a> {
    api: &'a GenAiApi,
    ptr: *mut c_void,
}

impl Drop for ResultsGuard<'_> {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.api.decoded_results_free)(self.ptr) };
        }
    }
}

struct ChunkGuard<'a> {
    api: &'a GenAiApi,
    ptr: *mut c_void,
}

impl Drop for ChunkGuard<'_> {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.api.chunk_free)(self.ptr) };
        }
    }
}

/// Whisper through OpenVINO GenAI's `WhisperPipeline` C API (`openvino_genai_c.dll`).
pub struct WhisperGenAi {
    api: &'static GenAiApi,
    pipe: *mut c_void,
    pub device_id: String,
}

unsafe impl Send for WhisperGenAi {}
unsafe impl Sync for WhisperGenAi {}

impl WhisperGenAi {
    pub fn runtime_present() -> bool {
        global_genai().is_ok()
    }

    pub fn new(
        model_dir: &Path,
        device_id: &str,
        cache_dir: Option<&Path>,
    ) -> Result<Self, WorkerError> {
        let api = global_genai()?;
        let c_model = CString::new(model_dir.to_string_lossy().as_bytes())
            .map_err(|_| WorkerError::Internal("whisper model path contains NUL".into()))?;
        let c_dev = CString::new(device_id)
            .map_err(|_| WorkerError::Internal("whisper device id contains NUL".into()))?;
        let mut pipe: *mut c_void = std::ptr::null_mut();

        let st = match cache_dir {
            None => unsafe { (api.create)(c_model.as_ptr(), c_dev.as_ptr(), 0, &mut pipe) },
            Some(dir) => {
                let c_k1 = CString::new("CACHE_DIR").expect("static");
                let c_v1 = CString::new(dir.to_string_lossy().as_bytes())
                    .map_err(|_| WorkerError::Internal("whisper cache path contains NUL".into()))?;
                if device_id.starts_with("NPU") {
                    let c_k2 = CString::new("NPU_TURBO").expect("static");
                    let c_v2 = CString::new("NO").expect("static");
                    unsafe {
                        (api.create)(
                            c_model.as_ptr(),
                            c_dev.as_ptr(),
                            4,
                            &mut pipe,
                            c_k1.as_ptr(),
                            c_v1.as_ptr(),
                            c_k2.as_ptr(),
                            c_v2.as_ptr(),
                        )
                    }
                } else {
                    unsafe {
                        (api.create)(
                            c_model.as_ptr(),
                            c_dev.as_ptr(),
                            2,
                            &mut pipe,
                            c_k1.as_ptr(),
                            c_v1.as_ptr(),
                        )
                    }
                }
            }
        };
        if st != 0 || pipe.is_null() {
            return Err(WorkerError::DeviceFailed(format!(
                "Whisper on {device_id} failed to load (status {st})"
            )));
        }
        Ok(Self {
            api,
            pipe,
            device_id: device_id.to_string(),
        })
    }

    pub fn transcribe(
        &self,
        pcm: &[f32],
        language: &str,
        started: Instant,
    ) -> Result<TranscribeResult, WorkerError> {
        let mut cfg_ptr: *mut c_void = std::ptr::null_mut();
        check(
            unsafe { (self.api.get_generation_config)(self.pipe, &mut cfg_ptr) },
            "config",
        )?;
        let cfg = ConfigGuard {
            api: self.api,
            ptr: cfg_ptr,
        };

        let c_task = CString::new("transcribe").expect("static");
        check(
            unsafe { (self.api.set_task)(cfg.ptr, c_task.as_ptr()) },
            "task",
        )?;
        let c_lang = if language == "auto" {
            None
        } else {
            Some(
                CString::new(format!("<|{language}|>"))
                    .map_err(|_| WorkerError::Internal("language tag contains NUL".into()))?,
            )
        };
        let lang_ptr = c_lang.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
        check(
            unsafe { (self.api.set_language)(cfg.ptr, lang_ptr) },
            "language",
        )?;
        check(
            unsafe { (self.api.set_return_timestamps)(cfg.ptr, 1) },
            "timestamps",
        )?;

        let mut res_ptr: *mut c_void = std::ptr::null_mut();
        check(
            unsafe { (self.api.generate)(self.pipe, pcm.as_ptr(), pcm.len(), cfg.ptr, &mut res_ptr) },
            "generate",
        )?;
        let res = ResultsGuard {
            api: self.api,
            ptr: res_ptr,
        };

        let text = read_genai_string(|buf, sz| unsafe {
            (self.api.decoded_results_get_string)(res.ptr, buf, sz)
        })
        .trim()
        .to_string();

        let mut segs = Vec::new();
        let mut has = 0u8;
        let mut count = 0usize;
        if unsafe { (self.api.decoded_results_has_chunks)(res.ptr, &mut has) } == 0
            && has != 0
            && unsafe { (self.api.decoded_results_get_chunks_count)(res.ptr, &mut count) } == 0
        {
            for i in 0..count {
                let mut ch_ptr: *mut c_void = std::ptr::null_mut();
                if unsafe { (self.api.decoded_results_get_chunk_at)(res.ptr, i, &mut ch_ptr) } != 0
                    || ch_ptr.is_null()
                {
                    continue;
                }
                let ch = ChunkGuard {
                    api: self.api,
                    ptr: ch_ptr,
                };
                let mut s0 = 0.0f32;
                let mut s1 = 0.0f32;
                unsafe {
                    (self.api.chunk_get_start_ts)(ch.ptr, &mut s0);
                    (self.api.chunk_get_end_ts)(ch.ptr, &mut s1);
                }
                let t = read_genai_string(|buf, sz| unsafe {
                    (self.api.chunk_get_text)(ch.ptr, buf, sz)
                })
                .trim()
                .to_string();
                let end_ms = if s1 >= s0 {
                    (s1 * 1000.0) as i32
                } else {
                    (pcm.len() * 1000 / 16_000) as i32
                };
                if !t.is_empty() {
                    segs.push(TranscriptSegment {
                        start_ms: (s0 * 1000.0) as i32,
                        end_ms,
                        text: t,
                    });
                }
            }
        }
        if segs.is_empty() && !text.is_empty() {
            segs.push(TranscriptSegment {
                start_ms: 0,
                end_ms: (pcm.len() * 1000 / 16_000) as i32,
                text: text.clone(),
            });
        }
        let ms = started.elapsed().as_millis() as i64;
        Ok(TranscribeResult {
            speech: !text.is_empty(),
            text,
            language: if language == "auto" {
                "und".to_string()
            } else {
                language.to_string()
            },
            segments: segs,
            ms,
        })
    }
}

impl Drop for WhisperGenAi {
    fn drop(&mut self) {
        if !self.pipe.is_null() {
            unsafe { (self.api.pipeline_free)(self.pipe) };
            self.pipe = std::ptr::null_mut();
        }
    }
}

fn read_genai_string<F>(mut get: F) -> String
where
    F: FnMut(*mut u8, *mut usize) -> c_int,
{
    let mut size = 0usize;
    if get(std::ptr::null_mut(), &mut size) != 0 || size == 0 {
        return String::new();
    }
    let mut buf = vec![0u8; size];
    if get(buf.as_mut_ptr(), &mut size) != 0 {
        return String::new();
    }
    if let Ok(cstr) = CStr::from_bytes_until_nul(&buf) {
        cstr.to_string_lossy().into_owned()
    } else {
        String::from_utf8_lossy(&buf).into_owned()
    }
}

fn check(st: c_int, what: &str) -> Result<(), WorkerError> {
    if st != 0 {
        return Err(WorkerError::DeviceFailed(format!(
            "Whisper {what} failed (status {st})"
        )));
    }
    Ok(())
}
