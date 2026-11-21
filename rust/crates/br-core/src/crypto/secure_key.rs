// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::sync::Mutex;

use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::crypto::CryptoError;
use crate::log::Log;
use crate::win32;

/// 32-byte key in native page-aligned memory: `VirtualLock`'ed, and `CryptProtectMemory`-wrapped
/// while idle when memory isolation is enabled.
pub struct SecureKey {
    gate: Mutex<Inner>,
    isolate: bool,
}

struct Inner {
    ptr: *mut u8,
    protected: bool,
}

// SAFETY: `Inner::ptr` points to a private page-aligned allocation only accessed under `gate`.
unsafe impl Send for Inner {}

impl SecureKey {
    pub const KEY_BYTES: usize = 32;
    const PAGE_BYTES: usize = 4096;

    pub fn new(key: &[u8], memory_isolation: bool) -> Result<Self, CryptoError> {
        if key.len() != Self::KEY_BYTES {
            return Err(CryptoError::InvalidArgument(
                "Key must be 32 bytes.".to_string(),
            ));
        }
        let ptr = unsafe { win32::alloc_page(Self::PAGE_BYTES) };
        if let Err(code) = unsafe { win32::virtual_lock(ptr, Self::PAGE_BYTES) } {
            Log::debug(&format!("VirtualLock on key page failed, error {code}"));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(key.as_ptr(), ptr, Self::KEY_BYTES);
        }
        let mut inner = Inner {
            ptr,
            protected: false,
        };
        if memory_isolation {
            protect_inner(&mut inner, memory_isolation);
        }
        Ok(Self {
            gate: Mutex::new(inner),
            isolate: memory_isolation,
        })
    }

    pub fn random(memory_isolation: bool) -> Result<Self, CryptoError> {
        let mut k = Zeroizing::new([0u8; Self::KEY_BYTES]);
        win32::fill_random(&mut *k);
        Self::new(&*k, memory_isolation)
    }

    /// Runs `f` with the plaintext 32-byte key, then re-wraps the key with `CryptProtectMemory`.
    pub fn use_key<T>(&self, f: impl FnOnce(&[u8]) -> T) -> Result<T, CryptoError> {
        let mut inner = self.gate.lock().expect("secure key gate");
        if inner.ptr.is_null() {
            return Err(CryptoError::InvalidState(
                "SecureKey has been disposed.".to_string(),
            ));
        }
        unprotect_inner(&mut inner)?;
        struct Reprotect<'a> {
            inner: &'a mut Inner,
            isolate: bool,
        }
        impl Drop for Reprotect<'_> {
            fn drop(&mut self) {
                protect_inner(self.inner, self.isolate);
            }
        }
        let guard = Reprotect {
            inner: &mut inner,
            isolate: self.isolate,
        };
        let slice = unsafe { std::slice::from_raw_parts(guard.inner.ptr, Self::KEY_BYTES) };
        Ok(f(slice))
    }

    /// HKDF-SHA256 subkey for `label` (empty salt, matching .NET `HKDF.DeriveKey(SHA256, k, out, [], info)`).
    pub fn derive(&self, label: &str, length: usize) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        self.use_key(|k| Zeroizing::new(hkdf_sha256(k, &[], label.as_bytes(), length)))
    }
}

impl Drop for SecureKey {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.gate.lock()
            && !inner.ptr.is_null()
        {
            unsafe {
                let slice = std::slice::from_raw_parts_mut(inner.ptr, Self::PAGE_BYTES);
                slice.zeroize();
                win32::virtual_unlock(inner.ptr, Self::PAGE_BYTES);
                win32::free_page(inner.ptr);
            }
            inner.ptr = std::ptr::null_mut();
        }
    }
}

fn protect_inner(inner: &mut Inner, isolate: bool) {
    if !isolate || inner.protected || inner.ptr.is_null() {
        return;
    }
    match unsafe { win32::crypt_protect_memory(inner.ptr, SecureKey::KEY_BYTES as u32) } {
        Ok(()) => inner.protected = true,
        Err(code) => Log::warn(&format!("CryptProtectMemory failed, error {code}")),
    }
}

fn unprotect_inner(inner: &mut Inner) -> Result<(), CryptoError> {
    if !inner.protected {
        return Ok(());
    }
    unsafe { win32::crypt_unprotect_memory(inner.ptr, SecureKey::KEY_BYTES as u32) }
        .map_err(|_| CryptoError::Cryptographic("CryptUnprotectMemory failed.".to_string()))?;
    inner.protected = false;
    Ok(())
}

/// RFC 5869 HKDF-SHA256.
pub(crate) fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8], length: usize) -> Vec<u8> {
    let salt_bytes = if salt.is_empty() {
        &[0u8; 32][..]
    } else {
        salt
    };
    let mut prk = Zeroizing::new(hmac_sha256(salt_bytes, &[ikm]));
    let mut out = Vec::with_capacity(length);
    let mut prev: [u8; 32] = [0u8; 32];
    let mut prev_len = 0usize;
    let mut counter: u8 = 1;
    while out.len() < length {
        let ctr = [counter];
        let block = hmac_sha256(&*prk, &[&prev[..prev_len], info, &ctr]);
        let need = (length - out.len()).min(32);
        out.extend_from_slice(&block[..need]);
        prev = block;
        prev_len = 32;
        counter = counter.wrapping_add(1);
    }
    prev.zeroize();
    prk.zeroize();
    out
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut k_block = [0u8; 64];
    if key.len() > 64 {
        let digest = Sha256::digest(key);
        k_block[..32].copy_from_slice(&digest);
    } else {
        k_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for ((ip, op), &kb) in ipad.iter_mut().zip(&mut opad).zip(&k_block) {
        *ip ^= kb;
        *op ^= kb;
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    for p in parts {
        inner.update(p);
    }
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    let res = outer.finalize();
    k_block.zeroize();
    ipad.zeroize();
    opad.zeroize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&res);
    out
}
