// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors

use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::LazyLock;

use serde::{Deserialize, Deserializer, Serialize};

use crate::json::dotnet_float;
use crate::layout::PipeNames;
use crate::whisper_languages::WhisperLanguages;

fn deser_string_default_on_null<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

fn deser_or_default<'de, T, D>(d: D) -> Result<T, D::Error>
where
    T: Deserialize<'de> + Default,
    D: Deserializer<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Every key and default from `src/BetterRecall.Contracts/Settings.cs` (`docs/v1-design.md` section 8).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    #[serde(deserialize_with = "deser_or_default")]
    pub capture: CaptureSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub keyframes: KeyframeSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub replay: ReplaySettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub index: IndexSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub retention: RetentionSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub storage: StorageSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub audio: AudioSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub privacy: PrivacySettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub inference: InferenceSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub governance: GovernanceSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub security: SecuritySettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub hotkeys: HotkeySettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub ipc: IpcSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub mcp: McpSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub log: LogSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub ui: UiSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub ocr: OcrSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub ai: AiSettings,
    #[serde(deserialize_with = "deser_or_default")]
    pub update: UpdateSettings,
}

/// Auto-update from GitHub Releases (R8). Off by default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateSettings {
    pub auto: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OcrSettings {
    pub enabled: bool,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub engine: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub languages: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub device: String,
}

impl Default for OcrSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            engine: "onnx".to_string(),
            languages: "auto".to_string(),
            device: "auto".to_string(),
        }
    }
}

/// Cloud describe & chat settings. Off by default; API key lives DPAPI-encrypted in `keys\ai-key.bin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AiSettings {
    pub enabled: bool,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub provider: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub endpoint: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub model: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub share: String,
    pub chat_max_hits: i32,
    pub chat_max_images: i32,
    pub daily_requests: i32,
    pub daily_tokens: i64,
    pub chat_history: bool,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "gemini".to_string(),
            endpoint: String::new(),
            model: String::new(),
            share: "textImages".to_string(),
            chat_max_hits: 8,
            chat_max_images: 3,
            daily_requests: 200,
            daily_tokens: 500_000,
            chat_history: true,
        }
    }
}

/// `ai.share`: what a chat may send about a capture. Each level includes the one below.
pub struct AiShare;

impl AiShare {
    pub const METADATA: &'static str = "metadata";
    pub const TEXT: &'static str = "text";
    pub const TEXT_IMAGES: &'static str = "textImages";
    pub const ALL: [&'static str; 3] = [Self::TEXT_IMAGES, Self::TEXT, Self::METADATA];

    pub fn rank(share: Option<&str>) -> i32 {
        match share {
            Some(Self::METADATA) => 0,
            Some(Self::TEXT) => 1,
            Some(Self::TEXT_IMAGES) => 2,
            _ => -1,
        }
    }
}

/// Provider defaults for cloud describe.
pub struct AiDefaults;

impl AiDefaults {
    pub const GEMINI: &'static str = "gemini";
    pub const OPEN_AI: &'static str = "openai-compatible";
    pub const CLAUDE: &'static str = "claude";
    pub const PROVIDERS: [&'static str; 3] = [Self::GEMINI, Self::OPEN_AI, Self::CLAUDE];

    pub fn endpoint(provider: &str) -> &'static str {
        match provider {
            Self::OPEN_AI => "https://api.openai.com/v1",
            Self::CLAUDE => "https://api.anthropic.com",
            _ => "https://generativelanguage.googleapis.com",
        }
    }

    pub fn model(provider: &str) -> &'static str {
        match provider {
            Self::OPEN_AI => "gpt-6-luna",
            Self::CLAUDE => "claude-opus-5-5",
            _ => "gemini-3.8-flash",
        }
    }

    pub fn effective_endpoint(a: &AiSettings) -> String {
        let trimmed = a.endpoint.trim();
        if trimmed.is_empty() {
            Self::endpoint(&a.provider).to_string()
        } else {
            trimmed.trim_end_matches('/').to_string()
        }
    }

    pub fn effective_model(a: &AiSettings) -> String {
        let trimmed = a.model.trim();
        if trimmed.is_empty() {
            Self::model(&a.provider).to_string()
        } else {
            trimmed.to_string()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UiSettings {
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub theme: String,
    pub search_always_on_top: bool,
    pub start_minimized: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            theme: "system".to_string(),
            search_always_on_top: true,
            start_minimized: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CaptureSettings {
    pub consented: bool,
    pub enabled: bool,
    pub interval_ms: i32,
    #[serde(deserialize_with = "deser_or_default")]
    pub monitors: Vec<i32>,
    pub max_width: i32,
    pub dhash_threshold: i32,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub dirty_min_percent: f64,
    pub event_triggers: bool,
    pub pause_on_fullscreen: bool,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            consented: false,
            enabled: true,
            interval_ms: 2000,
            monitors: Vec::new(),
            max_width: 1280,
            dhash_threshold: 6,
            dirty_min_percent: 15.0,
            event_triggers: true,
            pause_on_fullscreen: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KeyframeSettings {
    pub enabled: bool,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub format: String,
    pub quality: i32,
    pub thumb_cache_mb: i32,
}

impl Default for KeyframeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            format: "avif".to_string(),
            quality: 75,
            thumb_cache_mb: 64,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReplaySettings {
    pub enabled: bool,
    pub seconds: i32,
    pub fps: i32,
    pub bitrate_kbps: i32,
    pub audio: bool,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub encoder: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub encoder_adapter: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub layout: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub resolution: String,
    pub scale_percent: i32,
    pub during_fullscreen: bool,
    pub encrypt_exports: bool,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub export_dir: String,
    pub notify: bool,
    pub searchable: bool,
    pub sample_seconds: i32,
}

impl ReplaySettings {
    pub const FPS_CHOICES: [i32; 16] = [
        10, 15, 24, 25, 30, 48, 50, 60, 72, 75, 90, 100, 120, 144, 165, 240,
    ];
}

impl Default for ReplaySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            seconds: 120,
            fps: 30,
            bitrate_kbps: 6000,
            audio: true,
            encoder: "auto".to_string(),
            encoder_adapter: "auto".to_string(),
            layout: "composited".to_string(),
            resolution: "native".to_string(),
            scale_percent: 100,
            during_fullscreen: true,
            encrypt_exports: true,
            export_dir: String::new(),
            notify: true,
            searchable: true,
            sample_seconds: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IndexSettings {
    pub dim: i32,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub weak_match_below: f64,
}

impl Default for IndexSettings {
    fn default() -> Self {
        Self {
            dim: 768,
            weak_match_below: 0.65,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetentionSettings {
    pub days: i32,
    pub progressive_after_days: i32,
    pub progressive_over_days: i32,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            days: 30,
            progressive_after_days: 7,
            progressive_over_days: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageSettings {
    pub min_free_gb: i32,
    pub max_gb: i32,
    pub overwrite: bool,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            min_free_gb: 25,
            max_gb: 20,
            overwrite: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AudioSettings {
    pub system: bool,
    pub mic: bool,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub language: String,
    pub max_segment_s: i32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            system: false,
            mic: false,
            language: "auto".to_string(),
            max_segment_s: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrivacySettings {
    #[serde(deserialize_with = "deser_or_default")]
    pub deny_processes: Vec<String>,
    #[serde(deserialize_with = "deser_or_default")]
    pub deny_path_prefixes: Vec<String>,
    #[serde(deserialize_with = "deser_or_default")]
    pub deny_title_patterns: Vec<String>,
    pub honor_display_affinity: bool,
    pub pause_on_lock: bool,
    pub browser_badge_detection: bool,
    pub nsfw_filter: bool,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub nsfw_threshold: f64,
    pub nsfw_grid: i32,
    pub nsfw_detector: bool,
    #[serde(serialize_with = "dotnet_float::ser_f64")]
    pub nsfw_detector_threshold: f64,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub nsfw_device: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub nsfw_gpu_adapter: String,
    pub panic_purge_minutes: i32,
    pub record_site: bool,
    pub search_history: bool,
    pub search_history_max: i32,
    pub redact_passwords: bool,
    pub redact_cards: bool,
    pub redact_ids: bool,
    pub redact_birth_dates: bool,
    pub redact_emails: bool,
    pub redact_phones: bool,
    pub redact_names: bool,
    pub redact_addresses: bool,
}

impl Default for PrivacySettings {
    fn default() -> Self {
        Self {
            deny_processes: [
                "1Password.exe",
                "Bitwarden.exe",
                "KeePass.exe",
                "KeePassXC.exe",
                "Dashlane.exe",
                "LastPass.exe",
                "ProtonPass.exe",
                "Enpass.exe",
                "RoboForm.exe",
                "mstsc.exe",
                "VMConnect.exe",
                "msrdc.exe",
                "Windows365.exe",
                "CredentialUIBroker.exe",
                "consent.exe",
                "LogonUI.exe",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            deny_path_prefixes: Vec::new(),
            deny_title_patterns: [
                "InPrivate",
                "Incognito",
                "Private Browsing",
                "Private browsing",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            honor_display_affinity: true,
            pause_on_lock: true,
            browser_badge_detection: true,
            nsfw_filter: true,
            nsfw_threshold: 0.8,
            nsfw_grid: 1,
            nsfw_detector: true,
            nsfw_detector_threshold: 0.4,
            nsfw_device: "auto".to_string(),
            nsfw_gpu_adapter: "auto".to_string(),
            panic_purge_minutes: 0,
            record_site: true,
            search_history: true,
            search_history_max: 100,
            redact_passwords: true,
            redact_cards: true,
            redact_ids: true,
            redact_birth_dates: true,
            redact_emails: false,
            redact_phones: false,
            redact_names: false,
            redact_addresses: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InferenceSettings {
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub device: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub gpu_adapter: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub model_dir: String,
    pub image_tokens: i32,
    pub idle_exit_s: i32,
    pub fake: bool,
}

impl Default for InferenceSettings {
    fn default() -> Self {
        Self {
            device: "auto".to_string(),
            gpu_adapter: "auto".to_string(),
            model_dir: String::new(),
            image_tokens: 140,
            idle_exit_s: 120,
            fake: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GovernanceSettings {
    pub eco_qos: bool,
    pub defer_cpu_percent: i32,
    pub defer_gpu_percent: i32,
    pub pause_on_battery: bool,
}

impl Default for GovernanceSettings {
    fn default() -> Self {
        Self {
            eco_qos: true,
            defer_cpu_percent: 70,
            defer_gpu_percent: 60,
            pause_on_battery: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SecuritySettings {
    pub exclude_own_windows: bool,
    pub require_unlock: bool,
    pub unlock_timeout_min: i32,
    pub memory_isolation: bool,
    pub lock_on_session_lock: bool,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub key_protection: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub unlock_method: String,
}

impl Default for SecuritySettings {
    fn default() -> Self {
        Self {
            exclude_own_windows: true,
            require_unlock: true,
            unlock_timeout_min: 15,
            memory_isolation: true,
            lock_on_session_lock: true,
            key_protection: "auto".to_string(),
            unlock_method: "hello".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HotkeySettings {
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub search: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub pause: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub replay: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub panic: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub timeline: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub home: String,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub lock: String,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            search: "Ctrl+Alt+Space".to_string(),
            pause: "Ctrl+Alt+P".to_string(),
            replay: "Ctrl+Alt+R".to_string(),
            panic: "Ctrl+Alt+Shift+X".to_string(),
            timeline: String::new(),
            home: String::new(),
            lock: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IpcSettings {
    pub enabled: bool,
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub pipe_name: String,
    pub allow_control: bool,
    pub allow_purge: bool,
    pub allow_thumbnails: bool,
    #[serde(deserialize_with = "deser_or_default")]
    pub allow_clients: Vec<String>,
    pub require_unlocked: bool,
    pub max_results: i32,
    pub default_results: i32,
    pub max_response_bytes: i32,
    pub max_search_per_minute: i32,
    pub max_pause_s: i32,
    pub idle_timeout_s: i32,
}

impl Default for IpcSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            pipe_name: "BetterRecall".to_string(),
            allow_control: false,
            allow_purge: false,
            allow_thumbnails: false,
            allow_clients: Vec::new(),
            require_unlocked: true,
            max_results: 50,
            default_results: 10,
            max_response_bytes: 262_144,
            max_search_per_minute: 60,
            max_pause_s: 86_400,
            idle_timeout_s: 300,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpSettings {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogSettings {
    #[serde(deserialize_with = "deser_string_default_on_null")]
    pub level: String,
}

impl Default for LogSettings {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

// ---- Settings validation rules (mirrored from BetterRecall.Core.SettingsStore) ----

const MODIFIERS: [&str; 4] = ["Ctrl", "Alt", "Shift", "Win"];

static NAMED_KEYS: LazyLock<HashSet<String>> = LazyLock::new(|| {
    [
        "Space",
        "PageUp",
        "PageDown",
        "Home",
        "End",
        "Left",
        "Up",
        "Right",
        "Down",
        "Insert",
        "Delete",
        "Pause",
        "PrintScreen",
        "NumPad0",
        "NumPad1",
        "NumPad2",
        "NumPad3",
        "NumPad4",
        "NumPad5",
        "NumPad6",
        "NumPad7",
        "NumPad8",
        "NumPad9",
        ";",
        "=",
        ",",
        "-",
        ".",
        "/",
        "`",
        "[",
        "\\",
        "]",
        "'",
    ]
    .into_iter()
    .map(|s| s.to_ascii_lowercase())
    .collect()
});

pub fn all_hotkeys(h: &HotkeySettings) -> [&str; 7] {
    [
        &h.search,
        &h.pause,
        &h.replay,
        &h.panic,
        &h.timeline,
        &h.home,
        &h.lock,
    ]
}

/// Empty (not assigned), or a valid combination no other action uses.
pub fn hotkey_ok(h: &HotkeySettings, s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    if !valid_hotkey(s) {
        return false;
    }
    let target = s.trim();
    all_hotkeys(h)
        .into_iter()
        .filter(|o| o.trim().eq_ignore_ascii_case(target))
        .count()
        == 1
}

/// Hotkey grammar of `v1-design.md` section 5.
pub fn valid_hotkey(s: &str) -> bool {
    if s.trim().is_empty() {
        return false;
    }
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    if parts.len() < 2 {
        return false;
    }
    let mods = &parts[..parts.len() - 1];
    let mut seen = Vec::with_capacity(mods.len());
    for &m in mods {
        if !MODIFIERS.iter().any(|allowed| allowed.eq_ignore_ascii_case(m)) {
            return false;
        }
        if seen.iter().any(|prev: &&str| prev.eq_ignore_ascii_case(m)) {
            return false;
        }
        seen.push(m);
    }
    let key = parts[parts.len() - 1];
    if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() {
        return true;
    }
    if (2..=3).contains(&key.len())
        && (key.as_bytes()[0] == b'F' || key.as_bytes()[0] == b'f')
        && let Ok(f) = key[1..].parse::<i32>()
        && (1..=24).contains(&f)
    {
        return true;
    }
    NAMED_KEYS.contains(&key.to_ascii_lowercase())
}

/// `"auto"`, or 1 to 16 comma-separated language codes (`"en"`, `"vi"`, `"zh-Hant"`).
pub fn valid_ocr_languages(v: &str) -> bool {
    if v == "auto" {
        return true;
    }
    if v.trim().is_empty() || v.encode_utf16().count() > 128 {
        return false;
    }
    let parts: Vec<&str> = v.split(',').map(str::trim).collect();
    parts.len() <= 16 && parts.iter().all(|p| is_ocr_language_code(p))
}

fn is_ocr_language_code(p: &str) -> bool {
    let (lang, rest) = match p.split_once('-') {
        Some((l, r)) => (l, Some(r)),
        None => (p, None),
    };
    if !(2..=3).contains(&lang.len()) || !lang.bytes().all(|b| b.is_ascii_lowercase()) {
        return false;
    }
    match rest {
        None => true,
        Some(sub) => (2..=4).contains(&sub.len()) && sub.bytes().all(|b| b.is_ascii_alphabetic()),
    }
}

fn is_valid_pipe_base(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=64).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

fn is_valid_ai_model(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=128).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b[1..].iter().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
}

/// Cloud describe endpoint: absolute `https` URL without user info, query or fragment; plain `http` only on a loopback host.
pub fn valid_ai_endpoint(url: &str) -> bool {
    if url.trim().is_empty() || url.encode_utf16().count() > 512 {
        return false;
    }
    let t = url.trim();
    if t.contains('?') || t.contains('#') {
        return false;
    }
    let Some((scheme_raw, after_scheme)) = t.split_once("://") else {
        return false;
    };
    let is_https = scheme_raw.eq_ignore_ascii_case("https");
    let is_http = scheme_raw.eq_ignore_ascii_case("http");
    if !is_https && !is_http {
        return false;
    }
    let authority = after_scheme.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let (host, port_opt) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((ipv6, after_bracket)) = rest.split_once(']') else {
            return false;
        };
        if ipv6.parse::<Ipv6Addr>().is_err() {
            return false;
        }
        let port = if after_bracket.is_empty() {
            None
        } else if let Some(p) = after_bracket.strip_prefix(':') {
            Some(p)
        } else {
            return false;
        };
        (ipv6, port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return false;
    }
    if let Some(p) = port_opt
        && (p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u16>().is_err())
    {
        return false;
    }
    if host.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control()) {
        return false;
    }
    if is_https {
        return true;
    }
    // Plain HTTP only on loopback (matching .NET Uri.IsLoopback).
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip4) = host.parse::<Ipv4Addr>() {
        return ip4.is_loopback();
    }
    if let Ok(ip6) = host.parse::<Ipv6Addr>() {
        return ip6.is_loopback();
    }
    false
}

pub fn is_abs_path(p: &str) -> bool {
    if p.trim().is_empty() {
        return false;
    }
    if p.chars().any(|c| c == '|' || (c as u32) < 32) {
        return false;
    }
    crate::layout::is_path_fully_qualified(p)
}

fn abs_or_empty(p: &str) -> bool {
    p.is_empty() || is_abs_path(p)
}

fn non_empty_trimmed_len_128(s: &str) -> bool {
    let n = s.encode_utf16().count();
    (1..=128).contains(&n) && !s.trim().is_empty()
}

fn range_i32(v: i32, lo: i32, hi: i32) -> bool {
    (lo..=hi).contains(&v)
}

fn range_f64(v: f64, lo: f64, hi: f64) -> bool {
    !v.is_nan() && (lo..=hi).contains(&v)
}

fn strings_ok(a: &[String], each: impl Fn(&str) -> bool) -> bool {
    a.len() <= 1024 && a.iter().all(|x| each(x))
}

/// First invalid key as `"section.key"`, or `None` when every key is valid.
pub fn validate_settings(s: &Settings) -> Option<&'static str> {
    let rules: [(&'static str, bool); 73] = [
        ("capture.intervalMs", range_i32(s.capture.interval_ms, 500, 60_000)),
        (
            "capture.monitors",
            s.capture.monitors.len() <= 64
                && s.capture.monitors.iter().all(|&m| (0..64).contains(&m)),
        ),
        ("capture.maxWidth", range_i32(s.capture.max_width, 640, 3840)),
        ("capture.dhashThreshold", range_i32(s.capture.dhash_threshold, 0, 32)),
        ("capture.dirtyMinPercent", range_f64(s.capture.dirty_min_percent, 0.0, 100.0)),
        (
            "keyframes.format",
            ["avif", "webp", "jxl", "jpeg"].contains(&s.keyframes.format.as_str()),
        ),
        ("keyframes.quality", range_i32(s.keyframes.quality, 30, 95)),
        ("keyframes.thumbCacheMb", range_i32(s.keyframes.thumb_cache_mb, 8, 1024)),
        ("replay.seconds", range_i32(s.replay.seconds, 15, 600)),
        ("replay.fps", ReplaySettings::FPS_CHOICES.contains(&s.replay.fps)),
        ("replay.bitrateKbps", range_i32(s.replay.bitrate_kbps, 500, 50_000)),
        (
            "replay.encoder",
            ["auto", "av1", "hevc", "h264"].contains(&s.replay.encoder.as_str()),
        ),
        ("replay.encoderAdapter", non_empty_trimmed_len_128(&s.replay.encoder_adapter)),
        (
            "replay.layout",
            ["composited", "perMonitor"].contains(&s.replay.layout.as_str()),
        ),
        (
            "replay.resolution",
            ["native", "2160p", "1440p", "1080p", "900p", "720p", "540p", "scale"]
                .contains(&s.replay.resolution.as_str()),
        ),
        ("replay.scalePercent", range_i32(s.replay.scale_percent, 25, 100)),
        ("replay.exportDir", abs_or_empty(&s.replay.export_dir)),
        ("index.dim", matches!(s.index.dim, 768 | 512 | 256)),
        ("index.weakMatchBelow", range_f64(s.index.weak_match_below, 0.0, 1.0)),
        ("retention.days", range_i32(s.retention.days, 1, 3650)),
        ("audio.language", WhisperLanguages::is_valid(&s.audio.language)),
        ("audio.maxSegmentS", range_i32(s.audio.max_segment_s, 5, 30)),
        (
            "privacy.denyProcesses",
            strings_ok(&s.privacy.deny_processes, |x| {
                let n = x.trim().encode_utf16().count();
                (1..=260).contains(&n)
            }),
        ),
        (
            "privacy.denyPathPrefixes",
            strings_ok(&s.privacy.deny_path_prefixes, is_abs_path),
        ),
        (
            "privacy.denyTitlePatterns",
            strings_ok(&s.privacy.deny_title_patterns, |x| {
                let n = x.encode_utf16().count();
                (1..=256).contains(&n)
            }),
        ),
        ("privacy.nsfwThreshold", range_f64(s.privacy.nsfw_threshold, 0.01, 0.99)),
        ("privacy.nsfwGrid", range_i32(s.privacy.nsfw_grid, 1, 3)),
        (
            "privacy.nsfwDetectorThreshold",
            range_f64(s.privacy.nsfw_detector_threshold, 0.01, 0.99),
        ),
        (
            "privacy.nsfwDevice",
            ["auto", "npu", "igpu", "gpu", "cpu"].contains(&s.privacy.nsfw_device.as_str()),
        ),
        ("privacy.nsfwGpuAdapter", non_empty_trimmed_len_128(&s.privacy.nsfw_gpu_adapter)),
        ("privacy.panicPurgeMinutes", range_i32(s.privacy.panic_purge_minutes, 0, 1440)),
        ("privacy.searchHistoryMax", range_i32(s.privacy.search_history_max, 10, 1000)),
        (
            "inference.device",
            ["auto", "npu", "gpu", "cpu"].contains(&s.inference.device.as_str()),
        ),
        ("inference.gpuAdapter", non_empty_trimmed_len_128(&s.inference.gpu_adapter)),
        ("inference.modelDir", abs_or_empty(&s.inference.model_dir)),
        ("inference.imageTokens", matches!(s.inference.image_tokens, 70 | 140 | 280)),
        ("inference.idleExitS", range_i32(s.inference.idle_exit_s, 10, 3600)),
        ("governance.deferCpuPercent", range_i32(s.governance.defer_cpu_percent, 10, 100)),
        ("governance.deferGpuPercent", range_i32(s.governance.defer_gpu_percent, 10, 100)),
        ("security.unlockTimeoutMin", range_i32(s.security.unlock_timeout_min, 1, 1440)),
        (
            "security.keyProtection",
            ["auto", "tpm", "passphrase", "dpapi"].contains(&s.security.key_protection.as_str()),
        ),
        ("hotkeys.search", hotkey_ok(&s.hotkeys, &s.hotkeys.search)),
        ("hotkeys.pause", hotkey_ok(&s.hotkeys, &s.hotkeys.pause)),
        ("hotkeys.replay", hotkey_ok(&s.hotkeys, &s.hotkeys.replay)),
        ("hotkeys.panic", hotkey_ok(&s.hotkeys, &s.hotkeys.panic)),
        ("hotkeys.timeline", hotkey_ok(&s.hotkeys, &s.hotkeys.timeline)),
        ("hotkeys.home", hotkey_ok(&s.hotkeys, &s.hotkeys.home)),
        ("hotkeys.lock", hotkey_ok(&s.hotkeys, &s.hotkeys.lock)),
        (
            "ipc.pipeName",
            is_valid_pipe_base(PipeNames::base(&s.ipc.pipe_name))
                && !PipeNames::reserved(&s.ipc.pipe_name),
        ),
        ("ipc.allowClients", strings_ok(&s.ipc.allow_clients, is_abs_path)),
        ("ipc.maxResults", range_i32(s.ipc.max_results, 1, 200)),
        ("ipc.defaultResults", range_i32(s.ipc.default_results, 1, s.ipc.max_results)),
        ("ipc.maxResponseBytes", range_i32(s.ipc.max_response_bytes, 4096, 1_048_576)),
        ("ipc.maxSearchPerMinute", range_i32(s.ipc.max_search_per_minute, 1, 600)),
        ("ipc.maxPauseS", range_i32(s.ipc.max_pause_s, 60, 604_800)),
        ("ipc.idleTimeoutS", range_i32(s.ipc.idle_timeout_s, 10, 3600)),
        (
            "log.level",
            ["debug", "info", "warn", "error"].contains(&s.log.level.as_str()),
        ),
        ("ui.theme", ["system", "light", "dark"].contains(&s.ui.theme.as_str())),
        ("ocr.engine", ["onnx", "windows"].contains(&s.ocr.engine.as_str())),
        ("ocr.languages", valid_ocr_languages(&s.ocr.languages)),
        (
            "ocr.device",
            ["auto", "npu", "igpu", "gpu", "cpu"].contains(&s.ocr.device.as_str()),
        ),
        ("replay.sampleSeconds", range_i32(s.replay.sample_seconds, 2, 60)),
        (
            "security.unlockMethod",
            ["hello", "password"].contains(&s.security.unlock_method.as_str()),
        ),
        ("ai.provider", AiDefaults::PROVIDERS.contains(&s.ai.provider.as_str())),
        (
            "ai.endpoint",
            s.ai.endpoint.is_empty() || valid_ai_endpoint(&s.ai.endpoint),
        ),
        (
            "ai.model",
            s.ai.model.is_empty() || is_valid_ai_model(&s.ai.model),
        ),
        (
            "retention.progressiveAfterDays",
            range_i32(s.retention.progressive_after_days, 0, 3650),
        ),
        (
            "retention.progressiveOverDays",
            range_i32(s.retention.progressive_over_days, 1, 3650),
        ),
        ("storage.minFreeGb", range_i32(s.storage.min_free_gb, 0, 10_000)),
        ("storage.maxGb", range_i32(s.storage.max_gb, 1, 20_000)),
        ("ai.share", AiShare::ALL.contains(&s.ai.share.as_str())),
        ("ai.chatMaxHits", range_i32(s.ai.chat_max_hits, 1, 20)),
        ("ai.chatMaxImages", range_i32(s.ai.chat_max_images, 0, 8)),
    ];
    for (key, ok) in rules {
        if !ok {
            return Some(key);
        }
    }
    if !range_i32(s.ai.daily_requests, 0, 100_000) {
        return Some("ai.dailyRequests");
    }
    if !(0..=1_000_000_000).contains(&s.ai.daily_tokens) {
        return Some("ai.dailyTokens");
    }
    None
}
