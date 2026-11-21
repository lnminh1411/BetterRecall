// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! DXGI adapter/output enumeration and shared D3D11 device cache (`Capture\GpuDevices.cs`).

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use br_contracts::ReplaySettings;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11VideoContext,
    ID3D11VideoDevice,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1,
};
use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
use windows::core::{Interface, PCWSTR};

use crate::native::to_wide_z;
use crate::util::AgentLog;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    pub index: i32,
    pub device_name: String,
    pub adapter_luid: i64,
    pub vendor_id: u32,
    pub adapter_name: String,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub monitor: isize,
    pub refresh_hz: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    pub luid: i64,
    pub vendor_id: u32,
    pub name: String,
    pub software: bool,
    pub integrated: bool,
    pub vram_mb: i64,
    pub device_id: u32,
}

/// One D3D11 device per adapter, shared by every output on it (multithread protected).
pub struct GpuDevice {
    pub luid: i64,
    pub vendor_id: u32,
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub multithread: ID3D11Multithread,
    pub video_device: ID3D11VideoDevice,
    pub video_context: ID3D11VideoContext,
    /// Serialises immediate-context use across threads that share this device.
    pub gate: Mutex<()>,
    disposed: AtomicBool,
}

// SAFETY: D3D11 device has `SetMultithreadProtected(true)` and immediate-context calls are gated by `gate`.
unsafe impl Send for GpuDevice {}
unsafe impl Sync for GpuDevice {}

impl GpuDevice {
    pub fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::Acquire)
    }

    /// Marks the device disposed (idempotent). COM objects release when the last `Arc` drops.
    pub fn mark_disposed(&self) {
        self.disposed.store(true, Ordering::Release);
    }
}

/// DXGI enumeration and D3D11 device creation (`GpuDevices`).
pub struct GpuDevices;

impl GpuDevices {
    pub const VENDOR_INTEL: u32 = 0x8086;
    pub const VENDOR_AMD: u32 = 0x1002;
    pub const VENDOR_NVIDIA: u32 = 0x10DE;

    pub const fn luid_to_i64(luid: LUID) -> i64 {
        ((luid.HighPart as i64) << 32) | (luid.LowPart as i64)
    }

    /// PDH instance token for an adapter LUID, e.g. `luid_0x00000000_0x0000D1A3`.
    pub fn pdh_luid_token(luid: i64) -> String {
        let hi = (luid >> 32) as u32;
        let lo = luid as u32;
        format!("luid_0x{hi:08X}_0x{lo:08X}")
    }

    pub fn adapters() -> Vec<AdapterInfo> {
        let mut list = Vec::new();
        let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
            return list;
        };
        let mut i = 0u32;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
            i += 1;
            if let Ok(d) = unsafe { adapter.GetDesc1() } {
                let vram = (d.DedicatedVideoMemory / 1_048_576) as i64;
                let software = (d.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32)) != 0;
                list.push(AdapterInfo {
                    luid: Self::luid_to_i64(d.AdapterLuid),
                    vendor_id: d.VendorId,
                    name: wide_z_to_string(&d.Description),
                    software,
                    integrated: !software && vram <= 1024,
                    vram_mb: vram,
                    device_id: d.DeviceId,
                });
            }
        }
        list
    }

    /// Outputs attached to the desktop, in adapter then output order (`capture.monitors` index).
    pub fn outputs() -> Vec<OutputInfo> {
        let mut list = Vec::new();
        let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
            return list;
        };
        let mut index = 0i32;
        let mut a = 0u32;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
            a += 1;
            let Ok(ad) = (unsafe { adapter.GetDesc1() }) else {
                continue;
            };
            if (ad.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32)) != 0 {
                continue;
            }
            let adapter_name = wide_z_to_string(&ad.Description);
            let luid = Self::luid_to_i64(ad.AdapterLuid);
            let mut o = 0u32;
            while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
                o += 1;
                let Ok(od) = (unsafe { output.GetDesc() }) else {
                    continue;
                };
                if !od.AttachedToDesktop.as_bool() {
                    continue;
                }
                let r = od.DesktopCoordinates;
                let dev_name = wide_z_to_string(&od.DeviceName);
                let hz = normalize_refresh(refresh_hz(&dev_name));
                list.push(OutputInfo {
                    index,
                    device_name: dev_name,
                    adapter_luid: luid,
                    vendor_id: ad.VendorId,
                    adapter_name: adapter_name.clone(),
                    left: r.left,
                    top: r.top,
                    width: r.right - r.left,
                    height: r.bottom - r.top,
                    monitor: od.Monitor.0 as isize,
                    refresh_hz: hz,
                });
                index += 1;
            }
        }
        list
    }

    pub fn find_adapter(factory: &IDXGIFactory1, luid: i64) -> Option<IDXGIAdapter1> {
        let mut i = 0u32;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
            i += 1;
            if let Ok(d) = unsafe { adapter.GetDesc1() }
                && Self::luid_to_i64(d.AdapterLuid) == luid
            {
                return Some(adapter);
            }
        }
        None
    }

    pub fn create_device(luid: i64) -> Result<Arc<GpuDevice>, String> {
        let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }
            .map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
        let adapter =
            Self::find_adapter(&factory, luid).ok_or_else(|| "Adapter not found.".to_string())?;
        let desc = unsafe { adapter.GetDesc1() }.map_err(|e| format!("GetDesc1: {e}"))?;
        let base_adapter: IDXGIAdapter = adapter
            .cast()
            .map_err(|e| format!("IDXGIAdapter cast: {e}"))?;
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                &base_adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
        let device = device.ok_or_else(|| "D3D11CreateDevice returned null device".to_string())?;
        let context =
            context.ok_or_else(|| "D3D11CreateDevice returned null context".to_string())?;
        let multithread: ID3D11Multithread = device
            .cast()
            .map_err(|e| format!("ID3D11Multithread: {e}"))?;
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|e| format!("ID3D11VideoDevice: {e}"))?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .map_err(|e| format!("ID3D11VideoContext: {e}"))?;
        AgentLog::info(&format!("d3d11 device on vendor 0x{:04X}", desc.VendorId));
        Ok(Arc::new(GpuDevice {
            luid,
            vendor_id: desc.VendorId,
            device,
            context,
            multithread,
            video_device,
            video_context,
            gate: Mutex::new(()),
            disposed: AtomicBool::new(false),
        }))
    }
}

/// Shares [`GpuDevice`] instances per adapter LUID (`GpuDeviceCache`).
#[derive(Default)]
pub struct GpuDeviceCache {
    devices: Mutex<HashMap<i64, Arc<GpuDevice>>>,
}

impl GpuDeviceCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, luid: i64) -> Result<Arc<GpuDevice>, String> {
        let mut map = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = map.get(&luid) {
            return Ok(d.clone());
        }
        let d = GpuDevices::create_device(luid)?;
        map.insert(luid, d.clone());
        Ok(d)
    }

    pub fn invalidate(&self, luid: i64) {
        let removed = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&luid);
        if let Some(d) = removed {
            dispose_shared(&d);
        }
    }

    /// True while `dev` is the live device for its adapter (not disposed, not replaced).
    pub fn is_current(&self, dev: &Arc<GpuDevice>) -> bool {
        if dev.is_disposed() {
            return false;
        }
        let map = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&dev.luid).is_some_and(|d| Arc::ptr_eq(d, dev))
    }

    /// Drops `dev` when the driver removed or reset it (TDR, driver update).
    /// Returns `true` when the caller must stop using `dev`. Never panics.
    pub fn invalidate_if_removed(&self, dev: &Arc<GpuDevice>) -> bool {
        if !self.is_current(dev) {
            return true;
        }
        let removed = unsafe { dev.device.GetDeviceRemovedReason() }.is_err();
        if !removed {
            return false;
        }
        let dropped = {
            let mut map = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            if map.get(&dev.luid).is_some_and(|d| Arc::ptr_eq(d, dev)) {
                map.remove(&dev.luid)
            } else {
                None
            }
        };
        if let Some(d) = dropped {
            dispose_shared(&d);
        }
        AgentLog::warn(&format!(
            "d3d11 device on adapter {:X} was removed; recreating",
            dev.luid
        ));
        true
    }

    pub fn clear(&self) {
        let all: Vec<Arc<GpuDevice>> = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, d)| d)
            .collect();
        for d in all {
            dispose_shared(&d);
        }
    }
}

impl Drop for GpuDeviceCache {
    fn drop(&mut self) {
        self.clear();
    }
}

fn dispose_shared(d: &GpuDevice) {
    let _g = d.gate.lock().unwrap_or_else(|e| e.into_inner());
    d.mark_disposed();
}

pub fn refresh_hz(device_name: &str) -> i32 {
    let wide = to_wide_z(device_name);
    let mut dm = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let ok = unsafe { EnumDisplaySettingsW(PCWSTR(wide.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) };
    if ok.as_bool() {
        dm.dmDisplayFrequency as i32
    } else {
        0
    }
}

pub fn normalize_refresh(hz: i32) -> i32 {
    if hz <= 1 {
        return 60;
    }
    if ReplaySettings::FPS_CHOICES.contains(&(hz + 1)) {
        return hz + 1;
    }
    hz
}

fn wide_z_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..len])
        .to_string_lossy()
        .into_owned()
}
