// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! BetterRecall Core library (stage A1: settings store, logging, CoreJson, and cryptography).

pub mod core_json;
pub mod crypto;
pub mod log;
pub mod settings_store;
pub mod win32;

pub use core_json::{
    CaptureEvent, CoreJson, ExportEntry, HelpData, PongData, SubscribedCaptures, SubscribedStatus,
};
pub use crypto::{
    BlobCipher, BlobReader, BlobWriter, CryptoError, HelloInterop, KeyVault, PasswordCheck,
    SecureKey, UnlockPassword,
};
pub use log::{Log, LogLevel, LogSink};
pub use settings_store::SettingsStore;
