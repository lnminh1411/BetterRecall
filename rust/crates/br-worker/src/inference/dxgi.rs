// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

use crate::hosting::Log;

/// One DXGI adapter. `index` is the `IDXGIFactory1::EnumAdapters1` index, which ONNX Runtime's
/// DirectML EP takes as `device_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DxgiAdapter {
    pub index: i32,
    pub description: String,
    pub dedicated_video_memory: u64,
    pub flags: u32,
    pub vendor_id: u32,
    pub device_id: u32,
}

impl DxgiAdapter {
    pub const FLAG_SOFTWARE: u32 = 2; // DXGI_ADAPTER_FLAG_SOFTWARE
    pub const VENDOR_INTEL: u32 = 0x8086;
    pub const VENDOR_MICROSOFT: u32 = 0x1414;

    pub fn is_software(&self) -> bool {
        (self.flags & Self::FLAG_SOFTWARE) != 0
    }

    pub fn is_basic_render(&self) -> bool {
        self.vendor_id == Self::VENDOR_MICROSOFT
            || self
                .description
                .to_ascii_lowercase()
                .contains("basic render")
    }

    /// Intel with under 512 MB dedicated memory: an integrated GPU (Arc discrete cards have GBs).
    pub fn is_intel_integrated(&self) -> bool {
        self.vendor_id == Self::VENDOR_INTEL
            && self.dedicated_video_memory < 512 * 1024 * 1024
    }
}

/// DXGI adapter enumeration for DirectML device selection.
pub struct DxgiAdapters;

impl DxgiAdapters {
    /// `"auto"`: the first adapter that is not software, not Basic Render Driver, and not an Intel
    /// integrated GPU. Otherwise the first adapter whose description contains the setting (case-insensitive).
    pub fn pick(setting: Option<&str>, adapters: &[DxgiAdapter]) -> Option<DxgiAdapter> {
        let s = setting.map(str::trim).unwrap_or("");
        if s.is_empty() || s.eq_ignore_ascii_case("auto") {
            return adapters
                .iter()
                .find(|a| !a.is_software() && !a.is_basic_render() && !a.is_intel_integrated())
                .cloned();
        }
        let needle = s.to_lowercase();
        adapters
            .iter()
            .find(|a| a.description.to_lowercase().contains(&needle))
            .cloned()
    }

    /// Every adapter in `EnumAdapters1` order; empty when DXGI is unavailable.
    pub fn enumerate() -> Vec<DxgiAdapter> {
        let mut list = Vec::new();
        let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
            Ok(f) => f,
            Err(e) => {
                Log::info(&format!("DXGI unavailable: {e}"));
                return list;
            }
        };
        for i in 0..64u32 {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
                break;
            };
            let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
                continue;
            };
            let end = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..end])
                .trim()
                .to_string();
            list.push(DxgiAdapter {
                index: i as i32,
                description: name,
                dedicated_video_memory: desc.DedicatedVideoMemory as u64,
                flags: desc.Flags,
                vendor_id: desc.VendorId,
                device_id: desc.DeviceId,
            });
        }
        list
    }
}
