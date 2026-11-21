// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use br_contracts::{
    DataPaths, HotkeySettings, Settings, hotkey_ok, parse_jsonc_object, valid_ai_endpoint,
    valid_hotkey, valid_ocr_languages, validate_settings,
};
use serde_json::{Map, Value};
use windows::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};

use crate::core_json::CoreJson;
use crate::log::Log;
use crate::win32;

type ChangedHandler = Arc<dyn Fn(&Settings) + Send + Sync>;

/// `settings.json` with validation, lenient load, unreadable-file guard (D3/D4/D5), and atomic save. Thread-safe.
pub struct SettingsStore {
    path: PathBuf,
    gate: Mutex<State>,
    /// Serialises saves (merge, write, swap). Readers of `current()` never wait on it.
    write_gate: Mutex<()>,
    changed: Mutex<Vec<ChangedHandler>>,
}

struct State {
    current: Settings,
    load_error: Option<String>,
}

impl SettingsStore {
    /// `try_update` answer while `load_error` is set (sweep 4 B3, D4).
    pub const ERR_UNREADABLE: &'static str =
        "settings.json could not be read. Reset the settings or fix the file first.";

    /// `try_update` answer when the file could not be replaced.
    pub const ERR_WRITE: &'static str = "settings.json could not be written.";

    pub const READ_RETRIES: usize = 3;
    pub const READ_RETRY_DELAY_MS: u64 = 25;
    pub const MOVE_RETRIES: usize = 8;
    pub const MOVE_RETRY_DELAY_MS: u64 = 25;

    /// Loads `settings.json` from `home`. Missing file: defaults, written out. Bad keys are reset
    /// to defaults and logged. An unparsable file is kept untouched (copy saved as `settings.json.bad`)
    /// and sets `load_error` until `reset_unreadable()` or a full `save()`/`replace()`.
    pub fn load(home: &Path) -> Self {
        let path = DataPaths::settings_file(home);
        let mut settings = Settings::default();
        let mut write = false;
        let mut load_error: Option<String> = None;

        if path.exists() {
            match read_shared(&path) {
                Ok(json) => match parse_jsonc_object(&json) {
                    Ok(mut obj) => {
                        let (loaded, changed) = load_lenient(&mut obj);
                        settings = loaded;
                        write = changed;
                    }
                    Err(why) => {
                        Log::warn_err(
                            "settings.json unreadable, defaults used until the next save; file left as it is, copy kept as settings.json.bad",
                            &why,
                        );
                        let _ = fs::copy(&path, bad_path(&path));
                        settings = Settings::default();
                        load_error = Some("settings.json is not valid JSON.".to_string());
                    }
                },
                Err(e) => {
                    Log::warn_err(
                        "settings.json unreadable, defaults used until the next save; file left as it is, copy kept as settings.json.bad",
                        &format!("{:?}", e.kind()),
                    );
                    let _ = fs::copy(&path, bad_path(&path));
                    settings = Settings::default();
                    load_error = Some("settings.json could not be read.".to_string());
                }
            }
        } else {
            write = true;
        }

        let store = Self {
            path,
            gate: Mutex::new(State {
                current: settings,
                load_error,
            }),
            write_gate: Mutex::new(()),
            changed: Mutex::new(Vec::new()),
        };
        if write
            && let Err(e) = store.save()
        {
            Log::warn_err("settings.json could not be written", &format!("{:?}", e.kind()));
        }
        store
    }

    /// Reads `settings.json` without creating or fixing it (CLI, MCP). Retries briefly across concurrent saves.
    pub fn read_only(home: &Path) -> Settings {
        let path = DataPaths::settings_file(home);
        for attempt in 0..=Self::READ_RETRIES {
            match read_shared(&path) {
                Ok(json) => {
                    return match parse_jsonc_object(&json) {
                        Ok(mut obj) => load_lenient(&mut obj).0,
                        Err(_) => Settings::default(),
                    };
                }
                Err(_) if attempt < Self::READ_RETRIES => {
                    thread::sleep(Duration::from_millis(Self::READ_RETRY_DELAY_MS));
                }
                Err(_) => return Settings::default(),
            }
        }
        Settings::default()
    }

    /// Current settings snapshot.
    pub fn current(&self) -> Settings {
        self.gate.lock().expect("settings gate").current.clone()
    }

    pub fn file_path(&self) -> &Path {
        &self.path
    }

    /// Non-`None` when `settings.json` could not be read/parsed at load (status warning `settings-unreadable`,
    /// refuses partial saves, skips retention/thinning).
    pub fn load_error(&self) -> Option<String> {
        self.gate.lock().expect("settings gate").load_error.clone()
    }

    /// Registers a listener invoked whenever settings are updated, replaced, or reset.
    pub fn on_changed(&self, handler: impl Fn(&Settings) + Send + Sync + 'static) {
        self.changed
            .lock()
            .expect("changed lock")
            .push(Arc::new(handler));
    }

    /// Merges a partial settings JSON value. Returns `None` on success, else `"Invalid section.key."` / error message.
    pub fn try_update(&self, partial: &Value) -> Option<String> {
        let Value::Object(partial_obj) = partial else {
            return Some("Invalid JSON.".to_string());
        };
        let next = {
            let _write_guard = self.write_gate.lock().expect("settings write gate");
            let current = {
                let st = self.gate.lock().expect("settings gate");
                if st.load_error.is_some() {
                    return Some(Self::ERR_UNREADABLE.to_string());
                }
                st.current.clone()
            };
            let Value::Object(schema) = serde_json::to_value(Settings::default()).ok()? else {
                return Some("Invalid JSON.".to_string());
            };
            let Value::Object(mut merged) = serde_json::to_value(&current).ok()? else {
                return Some("Invalid JSON.".to_string());
            };
            let mut touched = Vec::new();

            for (sec_name, sec_val) in partial_obj {
                let Some(Value::Object(schema_sec)) = schema.get(sec_name) else {
                    return Some(format!("Invalid {sec_name}."));
                };
                let Value::Object(incoming_sec) = sec_val else {
                    return Some(format!("Invalid {sec_name}."));
                };
                for (key_name, key_val) in incoming_sec {
                    let Some(schema_val) = schema_sec.get(key_name) else {
                        return Some(format!("Invalid {sec_name}.{key_name}."));
                    };
                    if !same_kind(schema_val, key_val) {
                        return Some(format!("Invalid {sec_name}.{key_name}."));
                    }
                    let mut candidate = merged.clone();
                    if let Some(Value::Object(cand_sec)) = candidate.get_mut(sec_name) {
                        cand_sec.insert(key_name.clone(), key_val.clone());
                    }
                    if try_deserialize_obj(&candidate).is_none() {
                        return Some(format!("Invalid {sec_name}.{key_name}."));
                    }
                    merged = candidate;
                    touched.push(format!("{sec_name}.{key_name}"));
                }
            }

            let Some(parsed) = try_deserialize_obj(&merged) else {
                return Some("Invalid JSON.".to_string());
            };
            if let Some(mut bad) = Self::validate(&parsed) {
                if bad == "ipc.defaultResults"
                    && !touched.iter().any(|t| t == bad)
                    && touched.iter().any(|t| t == "ipc.maxResults")
                {
                    bad = "ipc.maxResults";
                }
                return Some(format!("Invalid {bad}."));
            }

            if let Err(e) = self.write_file(&parsed) {
                Log::warn_err("settings.json could not be written", &format!("{:?}", e.kind()));
                return Some(Self::ERR_WRITE.to_string());
            }
            {
                let mut st = self.gate.lock().expect("settings gate");
                st.current = parsed.clone();
                st.load_error = None;
            }
            parsed
        };
        self.raise_changed(&next);
        None
    }

    /// `"settings reset"` while `settings.json` is unreadable: copies the file as it is now to
    /// `settings.json.bad`, writes the defaults in use, clears `load_error`, and allows saves again.
    /// Returns `Ok(false)` when the file had loaded cleanly.
    pub fn reset_unreadable(&self) -> io::Result<bool> {
        let snapshot = {
            let _write_guard = self.write_gate.lock().expect("settings write gate");
            let snapshot = {
                let st = self.gate.lock().expect("settings gate");
                if st.load_error.is_none() {
                    return Ok(false);
                }
                st.current.clone()
            };
            if self.path.exists()
                && let Err(e) = fs::copy(&self.path, bad_path(&self.path))
            {
                Log::warn_err(
                    "settings reset: copy to settings.json.bad failed, nothing reset",
                    &format!("{:?}", e.kind()),
                );
                return Err(io::Error::new(
                    e.kind(),
                    "settings.json could not be kept as settings.json.bad, so nothing was reset.",
                ));
            }
            self.write_file(&snapshot)
                .map_err(|e| io::Error::new(e.kind(), Self::ERR_WRITE))?;
            {
                let mut st = self.gate.lock().expect("settings gate");
                st.load_error = None;
            }
            snapshot
        };
        Log::info("settings.json reset to defaults (the unreadable file is kept as settings.json.bad)");
        self.raise_changed(&snapshot);
        Ok(true)
    }

    pub fn save(&self) -> io::Result<()> {
        let _write_guard = self.write_gate.lock().expect("settings write gate");
        let snapshot = self.current();
        self.write_file(&snapshot)?;
        let mut st = self.gate.lock().expect("settings gate");
        st.load_error = None;
        Ok(())
    }

    /// Replaces the settings in code (agent use, e.g. `capture.consented`). Validates first.
    pub fn replace(&self, next: Settings) -> Result<Option<String>, io::Error> {
        if let Some(bad) = Self::validate(&next) {
            return Ok(Some(format!("Invalid {bad}.")));
        }
        {
            let _write_guard = self.write_gate.lock().expect("settings write gate");
            self.write_file(&next)?;
            let mut st = self.gate.lock().expect("settings gate");
            st.current = next.clone();
            st.load_error = None;
        }
        self.raise_changed(&next);
        Ok(None)
    }

    pub fn validate(s: &Settings) -> Option<&'static str> {
        validate_settings(s)
    }

    pub fn valid_hotkey(s: &str) -> bool {
        valid_hotkey(s)
    }

    pub fn hotkey_ok(h: &HotkeySettings, s: &str) -> bool {
        hotkey_ok(h, s)
    }

    pub fn valid_ocr_languages(v: &str) -> bool {
        valid_ocr_languages(v)
    }

    pub fn valid_ai_endpoint(url: &str) -> bool {
        valid_ai_endpoint(url)
    }

    /// Moves legacy `keyframes.maxGb` to `storage.maxGb` and upgrades old default `webp` / `dirtyMinPercent: 0.5`.
    pub fn migrate_legacy(obj: &mut Map<String, Value>) -> bool {
        let Some(Value::Object(kf)) = obj.get_mut("keyframes") else {
            return false;
        };
        let Some(old) = kf.remove("maxGb") else {
            return false;
        };
        if !matches!(obj.get("storage"), Some(Value::Object(_))) {
            obj.insert("storage".to_string(), Value::Object(Map::new()));
        }
        if let Some(Value::Object(st)) = obj.get_mut("storage")
            && !st.contains_key("maxGb")
            && !old.is_null()
        {
            st.insert("maxGb".to_string(), old);
        }
        if let Some(Value::Object(kf)) = obj.get_mut("keyframes")
            && kf.get("format").and_then(Value::as_str) == Some("webp")
        {
            kf.insert("format".to_string(), Value::String("avif".to_string()));
        }
        if let Some(Value::Object(cap)) = obj.get_mut("capture")
            && cap.get("dirtyMinPercent").and_then(Value::as_f64) == Some(0.5)
        {
            cap.insert(
                "dirtyMinPercent".to_string(),
                serde_json::json!(15.0),
            );
        }
        Log::info("settings: keyframes.maxGb moved to storage.maxGb");
        true
    }

    fn raise_changed(&self, s: &Settings) {
        let handlers: Vec<ChangedHandler> = self.changed.lock().expect("changed lock").clone();
        for h in handlers {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(s))).is_err() {
                Log::error("settings Changed handler failed");
            }
        }
    }

    fn write_file(&self, s: &Settings) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = CoreJson::to_pretty_string(s)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = tmp_path(&self.path);
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .share_mode(0)
                .open(&tmp)?;
            f.write_all(json.as_bytes())?;
            f.sync_all()?;
        }

        struct Cleanup<'a>(&'a Path);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                if self.0.exists() {
                    let _ = fs::remove_file(self.0);
                }
            }
        }
        let _cleanup = Cleanup(&tmp);

        for attempt in 0..=Self::MOVE_RETRIES {
            let res = if self.path.exists() {
                win32::replace_file(&tmp, &self.path)
            } else {
                win32::move_replace(&tmp, &self.path)
            };
            match res {
                Ok(()) => return Ok(()),
                Err(_) if attempt < Self::MOVE_RETRIES => {
                    thread::sleep(Duration::from_millis(
                        Self::MOVE_RETRY_DELAY_MS * (attempt as u64 + 1),
                    ));
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }
}

fn bad_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".bad");
    PathBuf::from(os)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    PathBuf::from(os)
}

fn read_shared(path: &Path) -> io::Result<String> {
    let share = (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0;
    let mut f = OpenOptions::new().read(true).share_mode(share).open(path)?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    let s = String::from_utf8(bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(s.strip_prefix('\u{FEFF}').unwrap_or(&s).to_string())
}

fn same_kind(schema_val: &Value, incoming: &Value) -> bool {
    matches!(
        (schema_val, incoming),
        (Value::Null, _)
            | (Value::Bool(_), Value::Bool(_))
            | (Value::Number(_), Value::Number(_))
            | (Value::String(_), Value::String(_))
            | (Value::Array(_), Value::Array(_))
            | (Value::Object(_), Value::Object(_))
    )
}

fn try_deserialize_obj(map: &Map<String, Value>) -> Option<Settings> {
    serde_json::from_value::<Settings>(Value::Object(map.clone())).ok()
}

/// Applies each known key of `obj` over the defaults, skipping keys that do not parse or validate.
fn load_lenient(obj: &mut Map<String, Value>) -> (Settings, bool) {
    let mut changed = SettingsStore::migrate_legacy(obj);
    let Value::Object(schema) =
        serde_json::to_value(Settings::default()).expect("default settings serialize")
    else {
        return (Settings::default(), true);
    };
    let mut merged = schema.clone();

    for (section, section_val) in obj.iter() {
        let Some(Value::Object(schema_sec)) = schema.get(section) else {
            changed = true;
            continue;
        };
        let Value::Object(sec) = section_val else {
            changed = true;
            continue;
        };
        for (key, value) in sec {
            if !schema_sec.contains_key(key) {
                changed = true;
                continue;
            }
            if value.is_null() {
                Log::warn(&format!("settings: {section}.{key} invalid, default kept"));
                changed = true;
                continue;
            }
            let mut candidate = merged.clone();
            if let Some(Value::Object(cand_sec)) = candidate.get_mut(section) {
                cand_sec.insert(key.clone(), value.clone());
            }
            let parsed = try_deserialize_obj(&candidate);
            let bad_self = parsed
                .as_ref()
                .and_then(SettingsStore::validate)
                .is_some_and(|bad| bad == format!("{section}.{key}"));
            if parsed.is_none() || bad_self {
                Log::warn(&format!("settings: {section}.{key} invalid, default kept"));
                changed = true;
                continue;
            }
            merged = candidate;
        }
    }

    let mut result = try_deserialize_obj(&merged).unwrap_or_default();
    for _ in 0..64 {
        let Some(bad) = SettingsStore::validate(&result) else {
            break;
        };
        Log::warn(&format!("settings: {bad} invalid, reset to default"));
        if let Some((sec, key)) = bad.split_once('.')
            && let Some(default_val) = schema.get(sec).and_then(|s| s.get(key))
            && let Some(Value::Object(merged_sec)) = merged.get_mut(sec)
        {
            merged_sec.insert(key.to_string(), default_val.clone());
        }
        result = try_deserialize_obj(&merged).unwrap_or_default();
        changed = true;
    }
    (result, changed)
}
