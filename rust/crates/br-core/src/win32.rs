// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Win32, DPAPI, memory-locking, and NCrypt TPM bindings used by stage A1.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{GetLastError, HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    BCRYPT_OAEP_PADDING_INFO, BCRYPT_RSA_ALGORITHM, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    BCryptGenRandom, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
    CRYPTPROTECTMEMORY_SAME_PROCESS, CryptProtectData, CryptProtectMemory, CryptUnprotectData,
    CryptUnprotectMemory, MS_PLATFORM_CRYPTO_PROVIDER, NCRYPT_ALLOW_DECRYPT_FLAG,
    NCRYPT_EXPORT_POLICY_PROPERTY, NCRYPT_FLAGS, NCRYPT_KEY_HANDLE, NCRYPT_KEY_USAGE_PROPERTY,
    NCRYPT_LENGTH_PROPERTY, NCRYPT_PAD_OAEP_FLAG, NCRYPT_PROV_HANDLE, NCryptCreatePersistedKey,
    NCryptDecrypt, NCryptDeleteKey, NCryptEncrypt, NCryptFinalizeKey, NCryptFreeObject,
    NCryptOpenKey, NCryptOpenStorageProvider, NCryptSetProperty,
};
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    REPLACEFILE_IGNORE_MERGE_ERRORS, ReplaceFileW,
};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualLock,
    VirtualUnlock,
};
use windows::core::{PCWSTR, w};
use zeroize::Zeroize;

/// Renames `source` over `target` with `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`.
pub fn move_durable(source: &Path, target: &Path) -> io::Result<()> {
    let src_w: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst_w: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            PCWSTR(src_w.as_ptr()),
            PCWSTR(dst_w.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|_| {
        let code = unsafe { GetLastError().0 };
        io::Error::new(
            io::Error::from_raw_os_error(code as i32).kind(),
            format!("Rename failed (error {code})."),
        )
    })
}

/// Atomically replaces existing `target` with `source` via `ReplaceFileW` (allows concurrent readers sharing DELETE).
pub fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    let src_w: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst_w: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        ReplaceFileW(
            PCWSTR(dst_w.as_ptr()),
            PCWSTR(src_w.as_ptr()),
            PCWSTR::null(),
            REPLACEFILE_IGNORE_MERGE_ERRORS,
            None,
            None,
        )
    }
    .map_err(|e| io::Error::from_raw_os_error(e.code().0))
}

/// Moves `source` over `target` (`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`).
pub fn move_replace(source: &Path, target: &Path) -> io::Result<()> {
    let src_w: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst_w: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            PCWSTR(src_w.as_ptr()),
            PCWSTR(dst_w.as_ptr()),
            MOVEFILE_REPLACE_EXISTING,
        )
    }
    .map_err(|e| io::Error::from_raw_os_error(e.code().0))
}

/// Fills `buf` with cryptographically strong random bytes via `BCryptGenRandom`.
pub fn fill_random(buf: &mut [u8]) {
    if buf.is_empty() {
        return;
    }
    let status = unsafe { BCryptGenRandom(None, buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    assert!(status.is_ok(), "BCryptGenRandom failed");
}

/// Computes SHA-1 (RFC 3174) over `data`, used for WinRT parameterized IIDs.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut out = [0u8; 20];
    sha1_rfc3174(data, &mut out);
    out
}

fn sha1_rfc3174(data: &[u8], out: &mut [u8; 20]) {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = Vec::with_capacity((data.len() + 9 + 63) & !63);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
}

// ---- Page-aligned locked memory & CryptProtectMemory ----

/// Commits fresh read-write pages.
///
/// # Safety
/// The caller owns the returned pages and must release them with `free_page`.
pub unsafe fn alloc_page(size: usize) -> *mut u8 {
    let ptr = unsafe { VirtualAlloc(None, size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
    assert!(!ptr.is_null(), "VirtualAlloc failed");
    ptr.cast::<u8>()
}

/// Releases pages from `alloc_page`.
///
/// # Safety
/// `ptr` must come from `alloc_page`, be released once, and not be used afterwards.
pub unsafe fn free_page(ptr: *mut u8) {
    if !ptr.is_null() {
        let _ = unsafe { VirtualFree(ptr.cast::<c_void>(), 0, MEM_RELEASE) };
    }
}

/// Locks pages in RAM so they are not paged out.
///
/// # Safety
/// `ptr` and `size` must describe live pages from `alloc_page`.
pub unsafe fn virtual_lock(ptr: *const u8, size: usize) -> Result<(), u32> {
    unsafe { VirtualLock(ptr.cast::<c_void>(), size) }.map_err(|_| unsafe { GetLastError().0 })
}

/// Unlocks pages locked with `virtual_lock`.
///
/// # Safety
/// `ptr` and `size` must describe live pages from `alloc_page`.
pub unsafe fn virtual_unlock(ptr: *const u8, size: usize) {
    let _ = unsafe { VirtualUnlock(ptr.cast::<c_void>(), size) };
}

/// Encrypts memory in place for this process only.
///
/// # Safety
/// `ptr` must point to `size` writable bytes; `size` must be a multiple of 16.
pub unsafe fn crypt_protect_memory(ptr: *mut u8, size: u32) -> Result<(), u32> {
    unsafe { CryptProtectMemory(ptr.cast::<c_void>(), size, CRYPTPROTECTMEMORY_SAME_PROCESS) }
        .map_err(|_| unsafe { GetLastError().0 })
}

/// Decrypts memory encrypted with `crypt_protect_memory`.
///
/// # Safety
/// `ptr` must point to `size` writable bytes; `size` must be a multiple of 16.
pub unsafe fn crypt_unprotect_memory(ptr: *mut u8, size: u32) -> Result<(), u32> {
    unsafe { CryptUnprotectMemory(ptr.cast::<c_void>(), size, CRYPTPROTECTMEMORY_SAME_PROCESS) }
        .map_err(|_| unsafe { GetLastError().0 })
}

// ---- DPAPI ----

pub fn dpapi_protect(data: &[u8], description: &str) -> Result<Vec<u8>, u32> {
    let desc_w: Vec<u16> = description.encode_utf16().chain(Some(0)).collect();
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR(desc_w.as_ptr()),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|_| unsafe { GetLastError().0 })?;
    let bytes = unsafe {
        let slice = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let v = slice.to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast::<c_void>())));
        v
    };
    Ok(bytes)
}

pub fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>, u32> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|_| unsafe { GetLastError().0 })?;
    let bytes = unsafe {
        let slice = std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let v = slice.to_vec();
        slice.zeroize();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast::<c_void>())));
        v
    };
    Ok(bytes)
}

// ---- NCrypt TPM (Microsoft Platform Crypto Provider) ----

pub const NTE_BAD_KEYSET: i32 = 0x8009_0016_u32 as i32;
pub const NTE_DEVICE_NOT_READY: i32 = 0x8009_0030_u32 as i32;

/// TPM busy or not ready (TBS and TPM facilities, `NTE_DEVICE_NOT_READY`): may pass, so not a lost key.
pub fn tpm_transient(hr: i32) -> bool {
    let facility = (hr >> 16) & 0x1FFF;
    facility == 0x28 || facility == 0x29 || hr == NTE_DEVICE_NOT_READY
}

struct OwnedProv(NCRYPT_PROV_HANDLE);
impl Drop for OwnedProv {
    fn drop(&mut self) {
        if self.0.0 != 0 {
            unsafe {
                let _ = NCryptFreeObject(self.0.into());
            }
        }
    }
}

struct OwnedKey {
    handle: NCRYPT_KEY_HANDLE,
    freed: bool,
}
impl Drop for OwnedKey {
    fn drop(&mut self) {
        if !self.freed && self.handle.0 != 0 {
            unsafe {
                let _ = NCryptFreeObject(self.handle.into());
            }
        }
    }
}

fn open_tpm_provider() -> Result<OwnedProv, i32> {
    let mut prov = NCRYPT_PROV_HANDLE::default();
    unsafe { NCryptOpenStorageProvider(&mut prov, MS_PLATFORM_CRYPTO_PROVIDER, 0) }
        .map_err(|e| e.code().0)?;
    Ok(OwnedProv(prov))
}

fn open_tpm_key(prov: NCRYPT_PROV_HANDLE, name: &str) -> Result<OwnedKey, i32> {
    let name_w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut key = NCRYPT_KEY_HANDLE::default();
    unsafe {
        NCryptOpenKey(
            prov,
            &mut key,
            PCWSTR(name_w.as_ptr()),
            Default::default(),
            NCRYPT_FLAGS(0),
        )
    }
    .map_err(|e| e.code().0)?;
    Ok(OwnedKey {
        handle: key,
        freed: false,
    })
}

fn create_tpm_rsa2048_key(prov: NCRYPT_PROV_HANDLE, name: &str) -> Result<OwnedKey, i32> {
    let name_w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut key = NCRYPT_KEY_HANDLE::default();
    unsafe {
        NCryptCreatePersistedKey(
            prov,
            &mut key,
            BCRYPT_RSA_ALGORITHM,
            PCWSTR(name_w.as_ptr()),
            Default::default(),
            NCRYPT_FLAGS(0),
        )
    }
    .map_err(|e| e.code().0)?;
    let owned = OwnedKey {
        handle: key,
        freed: false,
    };

    let len_bytes = 2048u32.to_le_bytes();
    unsafe {
        NCryptSetProperty(
            owned.handle.into(),
            NCRYPT_LENGTH_PROPERTY,
            &len_bytes,
            NCRYPT_FLAGS(0),
        )
    }
    .map_err(|e| e.code().0)?;

    let usage_bytes = NCRYPT_ALLOW_DECRYPT_FLAG.to_le_bytes();
    unsafe {
        NCryptSetProperty(
            owned.handle.into(),
            NCRYPT_KEY_USAGE_PROPERTY,
            &usage_bytes,
            NCRYPT_FLAGS(0),
        )
    }
    .map_err(|e| e.code().0)?;

    let export_bytes = 0u32.to_le_bytes();
    unsafe {
        NCryptSetProperty(
            owned.handle.into(),
            NCRYPT_EXPORT_POLICY_PROPERTY,
            &export_bytes,
            NCRYPT_FLAGS(0),
        )
    }
    .map_err(|e| e.code().0)?;

    unsafe { NCryptFinalizeKey(owned.handle, NCRYPT_FLAGS(0)) }.map_err(|e| e.code().0)?;
    Ok(owned)
}

/// Checks if the Microsoft Platform Crypto Provider can be opened (matches `TpmLikelyAvailable`).
pub fn tpm_provider_available() -> bool {
    open_tpm_provider().is_ok()
}

/// Checks whether a persisted TPM key with `name` exists.
pub fn tpm_key_exists(name: &str) -> Result<bool, i32> {
    let prov = open_tpm_provider()?;
    match open_tpm_key(prov.0, name) {
        Ok(_) => Ok(true),
        Err(hr) if hr == NTE_BAD_KEYSET => Ok(false),
        Err(hr) => Err(hr),
    }
}

/// Creates a persisted RSA-2048 TPM key if it does not already exist (for tests and probing).
pub fn tpm_ensure_key(name: &str) -> Result<(), i32> {
    let prov = open_tpm_provider()?;
    if open_tpm_key(prov.0, name).is_ok() {
        return Ok(());
    }
    let _ = create_tpm_rsa2048_key(prov.0, name)?;
    Ok(())
}

/// Encrypts `plain` using RSA-2048 OAEP-SHA1 under the persisted TPM key `name` (creating it if absent).
pub fn tpm_encrypt_oaep_sha1(name: &str, plain: &[u8]) -> Result<Vec<u8>, i32> {
    let prov = open_tpm_provider()?;
    let key = match open_tpm_key(prov.0, name) {
        Ok(k) => k,
        Err(_) => create_tpm_rsa2048_key(prov.0, name)?,
    };
    let padding = BCRYPT_OAEP_PADDING_INFO {
        pszAlgId: w!("SHA1"),
        pbLabel: std::ptr::null_mut(),
        cbLabel: 0,
    };
    let mut out_len = 0u32;
    unsafe {
        NCryptEncrypt(
            key.handle,
            Some(plain),
            Some((&raw const padding).cast::<c_void>()),
            None,
            &mut out_len,
            NCRYPT_PAD_OAEP_FLAG,
        )
    }
    .map_err(|e| e.code().0)?;
    let mut out = vec![0u8; out_len as usize];
    unsafe {
        NCryptEncrypt(
            key.handle,
            Some(plain),
            Some((&raw const padding).cast::<c_void>()),
            Some(&mut out),
            &mut out_len,
            NCRYPT_PAD_OAEP_FLAG,
        )
    }
    .map_err(|e| e.code().0)?;
    out.truncate(out_len as usize);
    Ok(out)
}

/// Decrypts `wrapped` using RSA-2048 OAEP-SHA1 under the persisted TPM key `name`.
pub fn tpm_decrypt_oaep_sha1(name: &str, wrapped: &[u8]) -> Result<Vec<u8>, i32> {
    let prov = open_tpm_provider()?;
    let key = open_tpm_key(prov.0, name)?;
    let padding = BCRYPT_OAEP_PADDING_INFO {
        pszAlgId: w!("SHA1"),
        pbLabel: std::ptr::null_mut(),
        cbLabel: 0,
    };
    let mut out_len = 0u32;
    unsafe {
        NCryptDecrypt(
            key.handle,
            Some(wrapped),
            Some((&raw const padding).cast::<c_void>()),
            None,
            &mut out_len,
            NCRYPT_PAD_OAEP_FLAG,
        )
    }
    .map_err(|e| e.code().0)?;
    let mut out = vec![0u8; out_len as usize];
    unsafe {
        NCryptDecrypt(
            key.handle,
            Some(wrapped),
            Some((&raw const padding).cast::<c_void>()),
            Some(&mut out),
            &mut out_len,
            NCRYPT_PAD_OAEP_FLAG,
        )
    }
    .map_err(|e| e.code().0)?;
    out.truncate(out_len as usize);
    Ok(out)
}

/// Deletes the persisted TPM key `name` if it exists.
pub fn tpm_delete_key(name: &str) -> Result<(), i32> {
    let prov = open_tpm_provider()?;
    let mut key = match open_tpm_key(prov.0, name) {
        Ok(k) => k,
        Err(hr) if hr == NTE_BAD_KEYSET => return Ok(()),
        Err(hr) => return Err(hr),
    };
    unsafe { NCryptDeleteKey(key.handle, 0) }.map_err(|e| e.code().0)?;
    key.freed = true;
    Ok(())
}

// ---- HMAC-SHA256 (pure Rust over sha2::Sha256, RFC 2104) ----

/// Computes HMAC-SHA256(key, data).
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    const BLOCK: usize = 64;
    let mut k_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let hashed = Sha256::digest(key);
        k_block[..32].copy_from_slice(&hashed);
    } else {
        k_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k_block[i];
        opad[i] ^= k_block[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    let out = outer.finalize();

    k_block.zeroize();
    ipad.zeroize();
    opad.zeroize();

    let mut res = [0u8; 32];
    res.copy_from_slice(&out);
    res
}

// ---- Pipes, Security Descriptors, Process & Token Inspection, Mutex, TickCount64 ----

pub const SECURITY_MANDATORY_MEDIUM_RID: i32 = 0x2000;

/// Returns milliseconds elapsed since system boot (`GetTickCount64`).
pub fn tick_count64() -> i64 {
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() as i64 }
}

/// Returns current process ID (`GetCurrentProcessId`).
pub fn current_process_id() -> u32 {
    unsafe { windows::Win32::System::Threading::GetCurrentProcessId() }
}

/// Returns the Terminal Services session ID for `pid`, or `None` on failure.
pub fn process_id_to_session_id(pid: u32) -> Option<u32> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ProcessIdToSessionId(dwProcessId: u32, pSessionId: *mut u32) -> i32;
    }
    let mut session = 0u32;
    let ok = unsafe { ProcessIdToSessionId(pid, &mut session) };
    if ok != 0 { Some(session) } else { None }
}

/// Checks if a named Win32 mutex currently exists (returns `true` even on `ERROR_ACCESS_DENIED`).
pub fn mutex_exists(name: &str) -> bool {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_ACCESS_RIGHTS};

    const SYNCHRONIZE_ACCESS: SYNCHRONIZATION_ACCESS_RIGHTS =
        SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000);
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    match unsafe { OpenMutexW(SYNCHRONIZE_ACCESS, false, PCWSTR(wide.as_ptr())) } {
        Ok(h) => {
            unsafe {
                let _ = CloseHandle(h);
            }
            true
        }
        Err(e) => e.code() == ERROR_ACCESS_DENIED.to_hresult(),
    }
}

/// Converts a `windows::core::Error` into `std::io::Error`, preserving Win32 OS error codes.
pub fn win32_to_io(e: windows::core::Error) -> io::Error {
    let raw = e.code().0 as u32;
    if (raw & 0xFFFF_0000) == 0x8007_0000 {
        io::Error::from_raw_os_error((raw & 0xFFFF) as i32)
    } else {
        io::Error::other(e.to_string())
    }
}

/// Reads the owner, DACL, and mandatory label of a kernel object handle as SDDL.
///
/// # Safety
/// `handle` must be a valid kernel object handle (such as a named pipe handle).
pub unsafe fn read_kernel_sddl(handle: windows::Win32::Foundation::HANDLE) -> Option<String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
        SE_KERNEL_OBJECT,
    };
    use windows::Win32::Security::{
        DACL_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };
    use windows::core::PWSTR;

    let info = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            info,
            None,
            None,
            None,
            None,
            Some(&mut sd),
        )
    };
    if status.0 != 0 || sd.0.is_null() {
        return None;
    }
    let mut out_str = PWSTR::null();
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            info,
            &mut out_str,
            None,
        )
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    if ok.is_err() || out_str.0.is_null() {
        return None;
    }
    let result = unsafe {
        let mut len = 0usize;
        while *out_str.0.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(out_str.0, len);
        let s = OsString::from_wide(slice).to_string_lossy().into_owned();
        let _ = LocalFree(Some(HLOCAL(out_str.0.cast::<c_void>())));
        s
    };
    Some(result)
}
