// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::io::Read;
use std::path::{Path, PathBuf};

use br_contracts::{DataPaths, Settings, WhisperLanguages, full_path};
use serde_json::{Map, Value};

use crate::hosting::Log;

/// The worker's view of `settings.json` (sweep 4 C5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSettings {
    pub fake: bool,
    pub device: String,
    pub model_dir: PathBuf,
    pub image_tokens: i32,
    pub idle_exit_s: i32,
    pub dim: i32,
    pub audio_language: String,
    pub eco_qos: bool,
    pub memory_isolation: bool,
    pub log_level: String,
    pub nsfw_filter: bool,
    pub nsfw_detector: bool,
    pub nsfw_device: String,
    pub nsfw_gpu_adapter: String,
    pub gpu_adapter: String,
    pub ocr_enabled: bool,
    pub ocr_engine: String,
    pub ocr_languages: String,
    pub ocr_device: String,
}

impl WorkerSettings {
    pub const OCR_DEVICES: [&'static str; 5] = ["auto", "npu", "igpu", "gpu", "cpu"];
    pub const DIMS: [i32; 3] = [768, 512, 256];
    pub const TOKEN_BUDGETS: [i32; 3] = [70, 140, 280];
    pub const DEVICE_VALUES: [&'static str; 4] = ["auto", "npu", "gpu", "cpu"];
    pub const NSFW_DEVICE_VALUES: [&'static str; 5] = ["auto", "npu", "igpu", "gpu", "cpu"];

    /// Cap on the agent's settings input (the agent has the same cap). Typical size is under 1 KB.
    pub const MAX_AGENT_INPUT_BYTES: usize = 64 * 1024;

    /// Loads `settings.json` from `home`, falling back to defaults on missing/unreadable files.
    pub fn load(home: &Path) -> Self {
        let file = DataPaths::settings_file(home);
        let s = if file.exists() {
            match std::fs::read_to_string(&file) {
                Ok(text) => match Self::parse(&text) {
                    Ok(parsed) => parsed,
                    Err(why) => {
                        Log::warn(&format!("settings.json unreadable ({why}); using defaults"));
                        Settings::default()
                    }
                },
                Err(e) => {
                    Log::warn(&format!(
                        "settings.json unreadable ({}); using defaults",
                        e.kind()
                    ));
                    Settings::default()
                }
            }
        } else {
            Settings::default()
        };
        Self::from(&s, home)
    }

    /// Reads the agent's settings from `input` to EOF: UTF-8 text, or `Err(reason)` when missing,
    /// larger than `MAX_AGENT_INPUT_BYTES`, not valid UTF-8, or unreadable.
    pub fn read_agent_input<R: Read>(input: &mut R) -> Result<String, String> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match input.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() + n > Self::MAX_AGENT_INPUT_BYTES {
                        return Err(format!("larger than {} bytes", Self::MAX_AGENT_INPUT_BYTES));
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        if buf.is_empty() {
            return Err("missing".to_string());
        }
        String::from_utf8(buf).map_err(|_| "InvalidUtf8".to_string())
    }

    /// Parses the agent's partial settings JSON object (SF1: fails closed on non-objects or bad JSON;
    /// a key of the wrong type keeps its default as in `parse`).
    pub fn try_from_json(json: &str, home: &Path) -> Result<Self, String> {
        let s = Self::parse(json)?;
        Ok(Self::from(&s, home))
    }

    /// Reads `settings.json` leniently: strips a UTF-8 BOM, `//` and `/* */` comments, and trailing commas;
    /// keys of the wrong type keep only that key's default. Returns `Err` when the text is not a JSON object.
    pub fn parse(json: &str) -> Result<Settings, String> {
        let cleaned = strip_jsonc(json);
        let value: Value =
            serde_json::from_str(&cleaned).map_err(|e| format!("JsonException: {e}"))?;
        let Value::Object(obj) = value else {
            return Err("JsonException: Not an object.".to_string());
        };
        if let Ok(whole) = serde_json::from_value::<Settings>(Value::Object(obj.clone())) {
            return Ok(whole);
        }

        let mut out = Settings::default();
        for (section, sec_val) in obj {
            let Value::Object(sec_map) = sec_val else {
                continue;
            };
            match section.as_str() {
                "index" => merge_section("index", sec_map, &mut out.index),
                "audio" => merge_section("audio", sec_map, &mut out.audio),
                "privacy" => merge_section("privacy", sec_map, &mut out.privacy),
                "inference" => merge_section("inference", sec_map, &mut out.inference),
                "governance" => merge_section("governance", sec_map, &mut out.governance),
                "security" => merge_section("security", sec_map, &mut out.security),
                "log" => merge_section("log", sec_map, &mut out.log),
                "ocr" => merge_section("ocr", sec_map, &mut out.ocr),
                _ => {}
            }
        }
        Ok(out)
    }

    /// Projects `Settings` into validated `WorkerSettings` with fallback defaults.
    pub fn from(s: &Settings, home: &Path) -> Self {
        let inf = &s.inference;
        let device_norm = inf.device.trim().to_ascii_lowercase();
        let device = if Self::DEVICE_VALUES.contains(&device_norm.as_str()) {
            device_norm
        } else {
            "auto".to_string()
        };
        let image_tokens = if Self::TOKEN_BUDGETS.contains(&inf.image_tokens) {
            inf.image_tokens
        } else {
            140
        };
        let dim = if Self::DIMS.contains(&s.index.dim) {
            s.index.dim
        } else {
            768
        };
        let idle_exit_s = inf.idle_exit_s.clamp(10, 3600);
        let raw_model_dir = DataPaths::models_dir(home, s);
        let model_dir = full_path(&raw_model_dir).unwrap_or(raw_model_dir);

        let lang_norm = s.audio.language.trim().to_ascii_lowercase();
        let audio_language = if lang_norm.is_empty() || !WhisperLanguages::is_valid(&lang_norm) {
            "auto".to_string()
        } else {
            lang_norm
        };

        let pv = &s.privacy;
        let nsfw_dev_norm = pv.nsfw_device.trim().to_ascii_lowercase();
        let nsfw_device = if Self::NSFW_DEVICE_VALUES.contains(&nsfw_dev_norm.as_str()) {
            nsfw_dev_norm
        } else {
            "auto".to_string()
        };
        let adapter_trim = pv.nsfw_gpu_adapter.trim();
        let adapter_len = adapter_trim.encode_utf16().count();
        let nsfw_gpu_adapter = if adapter_len == 0 || adapter_len > 128 {
            "auto".to_string()
        } else {
            adapter_trim.to_string()
        };
        let gpu_trim = inf.gpu_adapter.trim();
        let gpu_len = gpu_trim.encode_utf16().count();
        let gpu_adapter = if gpu_len == 0 || gpu_len > 128 {
            "auto".to_string()
        } else {
            gpu_trim.to_string()
        };

        let ocr = &s.ocr;
        let ocr_engine = if ocr.engine == "windows" {
            "windows".to_string()
        } else {
            "onnx".to_string()
        };
        let ocr_dev_norm = ocr.device.trim().to_ascii_lowercase();
        let ocr_device = if Self::OCR_DEVICES.contains(&ocr_dev_norm.as_str()) {
            ocr_dev_norm
        } else {
            "auto".to_string()
        };
        let ocr_lang_trim = ocr.languages.trim();
        let ocr_languages =
            if ocr_lang_trim.is_empty() || ocr.languages.encode_utf16().count() > 128 {
                "auto".to_string()
            } else {
                ocr_lang_trim.to_string()
            };

        Self {
            fake: inf.fake,
            device,
            model_dir,
            image_tokens,
            idle_exit_s,
            dim,
            audio_language,
            eco_qos: s.governance.eco_qos,
            memory_isolation: s.security.memory_isolation,
            log_level: s.log.level.to_ascii_lowercase(),
            nsfw_filter: pv.nsfw_filter,
            nsfw_detector: pv.nsfw_detector,
            nsfw_device,
            nsfw_gpu_adapter,
            gpu_adapter,
            ocr_enabled: ocr.enabled,
            ocr_engine,
            ocr_languages,
            ocr_device,
        }
    }
}

fn merge_section<T>(section_name: &str, incoming: Map<String, Value>, target: &mut T)
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let Ok(Value::Object(mut merged)) = serde_json::to_value(&*target) else {
        return;
    };
    for (key, value) in incoming {
        let mut candidate = merged.clone();
        candidate.insert(key.clone(), value);
        if let Ok(parsed) = serde_json::from_value::<T>(Value::Object(candidate.clone())) {
            *target = parsed;
            merged = candidate;
        } else {
            Log::warn(&format!(
                "settings: {section_name}.{key} unreadable, default kept"
            ));
        }
    }
}

/// Strips UTF-8 BOM, `//` line comments, `/* ... */` block comments, and trailing commas outside strings.
fn strip_jsonc(input: &str) -> String {
    let s = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    let no_comments = strip_comments(s);
    strip_trailing_commas(&no_comments)
}

fn strip_comments(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        if in_string {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if bytes[i] == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i + 1] == b'*' {
                i += 2;
                let mut closed = false;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        closed = true;
                        break;
                    }
                    if bytes[i] == b'\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
                if !closed {
                    // Leave invalid unclosed block comment token so serde_json fails cleanly.
                    out.push_str("/*");
                    break;
                }
                out.push(' ');
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn strip_trailing_commas(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        if in_string {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if bytes[i] == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }
        if bytes[i] == b',' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                i += 1;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}
