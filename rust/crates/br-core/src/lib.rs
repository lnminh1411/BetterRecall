// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! BetterRecall Core library (stages A1 + A2: settings, logging, CoreJson, crypto,
//! hand-written SQLite3MC binding, encrypted store, vector index, export journal/exporter, recommender).

pub mod core_json;
pub mod crypto;
pub mod log;
pub mod recommender;
pub mod settings_store;
pub mod sqlite;
pub mod store;
pub mod win32;

pub use core_json::{
    CaptureEvent, CoreJson, ExportEntry, HelpData, PongData, SubscribedCaptures, SubscribedStatus,
};
pub use crypto::{
    BlobCipher, BlobReader, BlobWriter, CryptoError, HelloInterop, KeyVault, PasswordCheck,
    SecureKey, UnlockPassword,
};
pub use log::{Log, LogLevel, LogSink};
pub use recommender::{EncoderProbe, Recommender};
pub use settings_store::SettingsStore;
pub use sqlite::{Connection, SqliteError, Statement, Transaction, Value};
pub use store::{
    BinaryVectorIndex, CaptureExporter, EncryptedStore, ExportJournal, Ids, StoreError,
    VectorCandidate, VectorFilter, VectorIndex,
};
