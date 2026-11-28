// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::ptr;

use windows::Win32::Foundation::{GlobalFree, HANDLE};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::core::w;

fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> Result<(), String> {
    let alloc_len = bytes.len().max(1);
    let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, alloc_len) }
        .map_err(|e| format!("GlobalAlloc({format}): {e}"))?;
    let locked = unsafe { GlobalLock(hmem) };
    if locked.is_null() {
        unsafe {
            let _ = GlobalFree(Some(hmem));
        }
        return Err(format!("GlobalLock({format}) returned null"));
    }
    unsafe {
        if !bytes.is_empty() {
            ptr::copy_nonoverlapping(bytes.as_ptr(), locked.cast::<u8>(), bytes.len());
        }
        let _ = GlobalUnlock(hmem);
    }
    let set_res = unsafe { SetClipboardData(format, Some(HANDLE(hmem.0))) };
    if let Err(e) = set_res {
        unsafe {
            let _ = GlobalFree(Some(hmem));
        }
        return Err(format!("SetClipboardData({format}): {e}"));
    }
    Ok(())
}

/// Copy `text` to the Windows clipboard and mark it excluded from clipboard history,
/// cloud sync, and monitor processing.
pub fn copy_text_excluded(text: &str) -> Result<(), String> {
    let mut last_err = None;
    let mut opened = false;
    for attempt in 0..5 {
        match unsafe { OpenClipboard(None) } {
            Ok(()) => {
                opened = true;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < 5 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }
    if !opened {
        let msg = last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".to_string());
        return Err(format!("OpenClipboard: {msg}"));
    }

    if let Err(e) = unsafe { EmptyClipboard() } {
        unsafe {
            let _ = CloseClipboard();
        }
        return Err(format!("EmptyClipboard: {e}"));
    }

    let res = (|| -> Result<(), String> {
        let zero_dword = 0u32.to_le_bytes();

        let fmt_exclude =
            unsafe { RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing")) };
        if fmt_exclude == 0 {
            return Err("RegisterClipboardFormatW(ExcludeClipboardContentFromMonitorProcessing) failed".into());
        }
        set_clipboard_bytes(fmt_exclude, &zero_dword)?;

        let fmt_history = unsafe { RegisterClipboardFormatW(w!("CanIncludeInClipboardHistory")) };
        if fmt_history == 0 {
            return Err("RegisterClipboardFormatW(CanIncludeInClipboardHistory) failed".into());
        }
        set_clipboard_bytes(fmt_history, &zero_dword)?;

        let fmt_cloud = unsafe { RegisterClipboardFormatW(w!("CanUploadToCloudClipboard")) };
        if fmt_cloud == 0 {
            return Err("RegisterClipboardFormatW(CanUploadToCloudClipboard) failed".into());
        }
        set_clipboard_bytes(fmt_cloud, &zero_dword)?;

        let utf16: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        let text_bytes =
            unsafe { std::slice::from_raw_parts(utf16.as_ptr().cast::<u8>(), utf16.len() * 2) };
        set_clipboard_bytes(u32::from(CF_UNICODETEXT.0), text_bytes)?;

        Ok(())
    })();

    if res.is_err() {
        unsafe {
            let _ = EmptyClipboard();
        }
    }
    unsafe {
        let _ = CloseClipboard();
    }
    res
}
