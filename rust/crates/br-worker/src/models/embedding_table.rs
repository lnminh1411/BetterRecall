// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READONLY,
    UnmapViewOfFile,
};
use windows::core::PCWSTR;

use crate::models::manifest::EmbeddingTableSpec;

/// Token embedding table read through a memory map: only the rows a request touches become resident.
/// bf16 rows are widened exactly to float32, then multiplied by `Scale`.
pub struct EmbeddingTable {
    file: HANDLE,
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
    bf16: bool,
    pub rows: i32,
    pub dim: i32,
    pub scale: f32,
}

// Read-only memory mapping never mutates; sharing across threads under `IWorkerEngine: Sync` is sound.
unsafe impl Send for EmbeddingTable {}
unsafe impl Sync for EmbeddingTable {}

impl EmbeddingTable {
    pub fn open(path: &Path, spec: &EmbeddingTableSpec) -> Result<Self, String> {
        let bf16 = match spec.dtype.as_str() {
            "bf16" => true,
            "f32" => false,
            other => return Err(format!("embedding dtype {other}")),
        };
        let rows = spec.rows;
        let dim = spec.dim;
        if rows < 0 || dim < 0 {
            return Err("embedding table dimensions negative".to_string());
        }
        let elem_bytes: i64 = if bf16 { 2 } else { 4 };
        let expect = (rows as i64) * (dim as i64) * elem_bytes;
        let actual = fs::metadata(path).map_err(|e| e.to_string())?.len();
        if actual != expect as u64 {
            return Err("embedding table size does not match the manifest".to_string());
        }
        if expect == 0 {
            return Ok(Self {
                file: HANDLE::default(),
                mapping: HANDLE::default(),
                view: MEMORY_MAPPED_VIEW_ADDRESS::default(),
                len: 0,
                bf16,
                rows,
                dim,
                scale: spec.scale,
            });
        }

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let file = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| format!("open embedding table: {e}"))?;

        let mapping = match unsafe { CreateFileMappingW(file, None, PAGE_READONLY, 0, 0, None) } {
            Ok(h) => h,
            Err(e) => {
                unsafe {
                    let _ = CloseHandle(file);
                }
                return Err(format!("CreateFileMappingW: {e}"));
            }
        };

        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0) };
        if view.Value.is_null() {
            unsafe {
                let _ = CloseHandle(mapping);
                let _ = CloseHandle(file);
            }
            return Err(format!(
                "MapViewOfFile failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        Ok(Self {
            file,
            mapping,
            view,
            len: expect as usize,
            bf16,
            rows,
            dim,
            scale: spec.scale,
        })
    }

    /// Writes the rows of `ids` into `dst` (`ids.len() * self.dim` floats).
    pub fn gather(&self, ids: &[i64], dst: &mut [f32]) -> Result<(), String> {
        let dim = self.dim as usize;
        if dst.len() < ids.len() * dim {
            return Err("embedding gather destination buffer too short".to_string());
        }
        if self.len == 0 {
            return Err("token id outside the embedding table".to_string());
        }
        // SAFETY: `self.view.Value` is a valid read-only mapping of `self.len` bytes alive for `&self`.
        let bytes = unsafe { std::slice::from_raw_parts(self.view.Value as *const u8, self.len) };
        let scale = self.scale;
        let elem = if self.bf16 { 2 } else { 4 };
        for (t, &id) in ids.iter().enumerate() {
            if id < 0 || id >= self.rows as i64 {
                return Err("token id outside the embedding table".to_string());
            }
            let row_off = (id as usize) * dim * elem;
            let row_bytes = &bytes[row_off..row_off + dim * elem];
            let out = &mut dst[t * dim..(t + 1) * dim];
            if self.bf16 {
                for (i, chunk) in row_bytes.as_chunks::<2>().0.iter().enumerate() {
                    let bits = u16::from_le_bytes([chunk[0], chunk[1]]) as u32;
                    out[i] = f32::from_bits(bits << 16) * scale;
                }
            } else {
                for (i, chunk) in row_bytes.as_chunks::<4>().0.iter().enumerate() {
                    let v = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    out[i] = v * scale;
                }
            }
        }
        Ok(())
    }
}

impl Drop for EmbeddingTable {
    fn drop(&mut self) {
        if !self.view.Value.is_null() {
            unsafe {
                let _ = UnmapViewOfFile(self.view);
            }
        }
        if !self.mapping.is_invalid() && !self.mapping.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.mapping);
            }
        }
        if !self.file.is_invalid() && !self.file.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.file);
            }
        }
    }
}
