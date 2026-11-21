// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

pub mod blob_cipher;
pub mod hello_interop;
pub mod key_vault;
pub mod secure_key;
pub mod unlock_password;

use std::io;

use thiserror::Error;

pub use blob_cipher::{BlobCipher, BlobReader, BlobWriter};
pub use hello_interop::HelloInterop;
pub use key_vault::KeyVault;
pub use secure_key::SecureKey;
pub use unlock_password::{PasswordCheck, UnlockPassword};

/// Cryptographic and key-management errors in `br-core`.
#[derive(Debug, Error)]
pub enum CryptoError {
    /// The key file cannot be unwrapped for good: TPM key gone, DPAPI failed, or file damaged.
    #[error("{0}")]
    KeyLost(String),
    #[error("{0}")]
    Cryptographic(String),
    #[error("{0}")]
    InvalidArgument(String),
    #[error("{0}")]
    InvalidState(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}
