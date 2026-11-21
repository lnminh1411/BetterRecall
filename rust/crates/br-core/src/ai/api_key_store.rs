// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! DPAPI-protected cloud API key store bound to provider and endpoint (`ApiKeyStore.cs`).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use br_contracts::{AiDefaults, AiSettings, DataPaths};
use zeroize::Zeroize;

use crate::log::Log;
use crate::win32;

/// Cloud API key in `keys\ai-key.bin`: `"BRA2"` + DPAPI (CurrentUser) blob of UTF-8
/// `"provider|endpoint\nkey"`. Also supports legacy `"BRA1"` files (0.2, no binding) on default
/// endpoints when the key prefix identifies the provider.
pub struct ApiKeyStore {
    file: PathBuf,
    gate: Mutex<()>,
}

impl ApiKeyStore {
    pub const MAX_LENGTH: usize = 4096;
    const MAGIC: [u8; 4] = *b"BRA2";
    const LEGACY_MAGIC: [u8; 4] = *b"BRA1";

    pub fn new(home: &Path) -> Self {
        Self {
            file: DataPaths::ai_key_file(home),
            gate: Mutex::new(()),
        }
    }

    /// What a key is bound to: provider and effective endpoint (lower case).
    pub fn binding(a: &AiSettings) -> String {
        format!("{}|{}", a.provider, AiDefaults::effective_endpoint(a)).to_lowercase()
    }

    pub fn is_set(&self) -> bool {
        let _g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.file.exists()
    }

    pub fn valid(key: Option<&str>) -> bool {
        let Some(k) = key else {
            return false;
        };
        let len = k.encode_utf16().count();
        (1..=Self::MAX_LENGTH).contains(&len)
            && k.trim().len() == k.len()
            && k.chars().all(|c| c > ' ' && (c as u32) < 127)
    }

    pub fn set(&self, key: &str, bound_to: &AiSettings) -> Result<(), String> {
        if !Self::valid(Some(key)) {
            return Err("Invalid API key.".to_string());
        }
        let mut plain = format!("{}\n{key}", Self::binding(bound_to)).into_bytes();
        let res = Self::protect(&plain);
        plain.zeroize();
        let blob = res.map_err(|code| format!("DPAPI protect failed ({code})."))?;
        let mut file_bytes = Vec::with_capacity(4 + blob.len());
        file_bytes.extend_from_slice(&Self::MAGIC);
        file_bytes.extend_from_slice(&blob);
        {
            let _g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(parent) = self.file.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let tmp = PathBuf::from(format!("{}.tmp", self.file.display()));
            fs::write(&tmp, &file_bytes).map_err(|e| e.to_string())?;
            win32::move_replace(&tmp, &self.file).map_err(|e| e.to_string())?;
        }
        Log::info("cloud describe API key stored");
        Ok(())
    }

    pub fn clear(&self) {
        let _g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if !self.file.exists() {
            return;
        }
        let len = fs::metadata(&self.file).map(|m| m.len() as usize).unwrap_or(16);
        let mut noise = vec![0u8; len.max(16)];
        win32::fill_random(&mut noise);
        if let Ok(mut f) = OpenOptions::new().write(true).open(&self.file) {
            let _ = f.write_all(&noise);
        }
        let _ = fs::remove_file(&self.file);
        Log::info("cloud describe API key removed");
    }

    /// The key for these AI settings, or `None` when none is stored, it was entered for another
    /// provider or endpoint, or the file cannot be decrypted.
    pub fn read(&self, a: &AiSettings) -> Option<String> {
        let file = {
            let _g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            if !self.file.exists() {
                return None;
            }
            fs::read(&self.file).ok()?
        };
        let legacy = file.len() >= 5 && file[..4] == Self::LEGACY_MAGIC;
        if file.len() < 5 || (!legacy && file[..4] != Self::MAGIC) {
            Log::warn("API key file damaged");
            return None;
        }
        if legacy && !a.endpoint.trim().is_empty() {
            Log::info("API key predates endpoint binding: enter it again for a custom endpoint");
            return None;
        }
        let mut plain = match win32::dpapi_unprotect(&file[4..]) {
            Ok(p) => p,
            Err(_) => {
                Log::warn("API key file cannot be decrypted");
                return None;
            }
        };
        let result = match std::str::from_utf8(&plain) {
            Ok(text) => {
                if legacy {
                    if Self::legacy_provider(text) == Some(a.provider.as_str()) {
                        Some(text.to_string())
                    } else {
                        Log::info("API key predates provider binding and does not look like a key for this provider: enter it again");
                        None
                    }
                } else if let Some((bound, key)) = text.split_once('\n') {
                    if bound == Self::binding(a) {
                        Some(key.to_string())
                    } else {
                        Log::info("API key was entered for another provider or endpoint");
                        None
                    }
                } else {
                    Log::info("API key was entered for another provider or endpoint");
                    None
                }
            }
            Err(_) => None,
        };
        plain.zeroize();
        result
    }

    /// The provider a 0.2 key was most likely entered for, from its published prefix; `None` when unknown.
    pub fn legacy_provider(key: &str) -> Option<&'static str> {
        if key.starts_with("sk-ant-") {
            Some(AiDefaults::CLAUDE)
        } else if key.starts_with("AIza") {
            Some(AiDefaults::GEMINI)
        } else if key.starts_with("sk-") {
            Some(AiDefaults::OPEN_AI)
        } else {
            None
        }
    }

    /// DPAPI-protects raw bytes with the cloud describe key description (also used by legacy tests).
    pub fn protect(data: &[u8]) -> Result<Vec<u8>, u32> {
        win32::dpapi_protect(data, "BetterRecall cloud describe key")
    }
}
