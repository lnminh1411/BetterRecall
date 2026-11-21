// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Hardware inventory probe (`HardwareProbe.cs`).

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

use br_contracts::{GpuInfo, HardwareInfo, MonitorInfo, NpuInfo};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_PRESENT, SP_DEVINFO_DATA, SPDRP_DEVICEDESC, SPDRP_DRIVER, SPDRP_FRIENDLYNAME,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceRegistryPropertyW,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIDevice, IDXGIFactory1,
};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GlobalMemoryStatusEx, MEMORYSTATUSEX, RelationProcessorCore,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows::core::{GUID, Interface};

use crate::host::shell::reg_get_string;

pub const VENDOR_INTEL: u32 = 0x8086;
pub const VENDOR_AMD: u32 = 0x1002;
pub const VENDOR_NVIDIA: u32 = 0x10DE;
const VENDOR_MICROSOFT: u32 = 0x1414;

/// Output description passed into [`HardwareProbe::collect`].
#[derive(Debug, Clone)]
pub struct OutputDesc {
    pub index: i32,
    pub width: i32,
    pub height: i32,
    pub refresh_hz: i32,
    pub adapter_name: String,
}

pub struct HardwareProbe;

impl HardwareProbe {
    pub fn collect(outputs: &[OutputDesc]) -> HardwareInfo {
        let cpu = Self::probe_cpu();
        let cores = Self::probe_cores();
        let threads = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(1);
        let ram_mb = Self::probe_ram_mb();
        let has_battery = Self::probe_battery();
        let gpus = Self::probe_gpus();
        let npu = Self::probe_npu();
        let monitors = outputs
            .iter()
            .map(|o| MonitorInfo {
                index: o.index,
                width: o.width,
                height: o.height,
                refresh_hz: o.refresh_hz,
                adapter: o.adapter_name.clone(),
            })
            .collect();
        let os = probe_os();

        HardwareInfo {
            cpu,
            cores,
            threads,
            ram_mb,
            has_battery,
            gpus,
            npu,
            monitors,
            os,
        }
    }

    /// Returns the PDH LUID token (`luid_0xHIGH_0xLOW`) of the first Intel DXGI adapter, if any.
    pub fn intel_igpu_pdh_token() -> Option<String> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
        let mut idx = 0u32;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(idx) } {
            idx += 1;
            let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
                continue;
            };
            if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                continue;
            }
            if desc.VendorId == VENDOR_INTEL {
                let high = desc.AdapterLuid.HighPart as u32;
                let low = desc.AdapterLuid.LowPart;
                return Some(format!("luid_0x{high:08X}_0x{low:08X}"));
            }
        }
        None
    }

    pub fn probe_cpu() -> String {
        reg_get_string(
            HKEY_LOCAL_MACHINE,
            r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
            "ProcessorNameString",
        )
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
    }

    pub fn probe_cores() -> i32 {
        let fallback = std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(1);
        let mut len = 0u32;
        let _ = unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut len) };
        if len == 0 {
            return fallback;
        }
        let mut buf = vec![0u8; len as usize];
        let ok = unsafe {
            GetLogicalProcessorInformationEx(
                RelationProcessorCore,
                Some(buf.as_mut_ptr().cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()),
                &mut len,
            )
        };
        if ok.is_err() {
            return fallback;
        }
        let mut cores = 0i32;
        let mut offset = 0usize;
        while offset + 8 <= len as usize {
            let rel = u32::from_ne_bytes(buf[offset..offset + 4].try_into().unwrap());
            let size = u32::from_ne_bytes(buf[offset + 4..offset + 8].try_into().unwrap()) as usize;
            if size == 0 {
                break;
            }
            if rel == RelationProcessorCore.0 as u32 {
                cores += 1;
            }
            offset += size;
        }
        if cores > 0 { cores } else { fallback }
    }

    pub fn probe_ram_mb() -> i64 {
        let mut mem = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if unsafe { GlobalMemoryStatusEx(&mut mem) }.is_ok() {
            (mem.ullTotalPhys / (1024 * 1024)) as i64
        } else {
            0
        }
    }

    pub fn probe_battery() -> bool {
        let mut st = SYSTEM_POWER_STATUS::default();
        if unsafe { GetSystemPowerStatus(&mut st) }.is_ok() {
            st.BatteryFlag != 128 && st.BatteryFlag != 255
        } else {
            false
        }
    }

    pub fn probe_gpus() -> Vec<GpuInfo> {
        let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
            return Vec::new();
        };
        let mut list = Vec::new();
        let mut idx = 0u32;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(idx) } {
            idx += 1;
            let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
                continue;
            };
            let software = (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
            if software || desc.VendorId == VENDOR_MICROSOFT {
                continue;
            }
            let name_len = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            let name = OsString::from_wide(&desc.Description[..name_len])
                .to_string_lossy()
                .trim()
                .to_string();
            let vendor = match desc.VendorId {
                VENDOR_INTEL => "Intel".to_string(),
                VENDOR_AMD => "AMD".to_string(),
                VENDOR_NVIDIA => "NVIDIA".to_string(),
                v => format!("0x{v:X}"),
            };
            let vram_mb = (desc.DedicatedVideoMemory / (1024 * 1024)) as i64;
            let integrated = desc.DedicatedVideoMemory < 512 * 1024 * 1024;
            let luid = ((desc.AdapterLuid.HighPart as i64) << 32)
                | (desc.AdapterLuid.LowPart as i64);
            let driver = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }
                .map(|umd| {
                    let v = umd as u64;
                    format!(
                        "{}.{}.{}.{}",
                        (v >> 48) as u16,
                        (v >> 32) as u16,
                        (v >> 16) as u16,
                        v as u16
                    )
                })
                .unwrap_or_default();
            list.push(GpuInfo {
                name,
                vendor,
                integrated,
                vram_mb,
                luid: format!("0x{luid:016X}"),
                driver,
                codecs: Vec::new(), // A6 populates hardware video encoder codecs
            });
        }
        list
    }

    pub fn probe_npu() -> Option<NpuInfo> {
        // Compute Accelerator device class GUID: {f01a9d53-3ff6-48d2-9f97-c8a7004be10c}
        let guid = GUID::from_u128(0xf01a9d53_3ff6_48d2_9f97_c8a7004be10c);
        let set = unsafe { SetupDiGetClassDevsW(Some(&guid), None, None, DIGCF_PRESENT) }.ok()?;
        let mut data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        let found = unsafe { SetupDiEnumDeviceInfo(set, 0, &mut data) }.is_ok();
        if !found {
            unsafe {
                let _ = SetupDiDestroyDeviceInfoList(set);
            }
            return None;
        }
        let name = get_dev_prop(set, &data, SPDRP_FRIENDLYNAME)
            .or_else(|| get_dev_prop(set, &data, SPDRP_DEVICEDESC))
            .unwrap_or_default();
        let driver_key = get_dev_prop(set, &data, SPDRP_DRIVER);
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(set);
        }
        let driver_version = driver_key
            .and_then(|d| {
                reg_get_string(
                    HKEY_LOCAL_MACHINE,
                    &format!(r"SYSTEM\CurrentControlSet\Control\Class\{d}"),
                    "DriverVersion",
                )
            })
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        Some(NpuInfo {
            name,
            driver: driver_version,
        })
    }
}

fn get_dev_prop(
    set: windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO,
    dev: &SP_DEVINFO_DATA,
    prop: windows::Win32::Devices::DeviceAndDriverInstallation::SETUP_DI_REGISTRY_PROPERTY,
) -> Option<String> {
    let mut buf = vec![0u8; 512];
    let mut req = 0u32;
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(set, dev, prop, None, Some(&mut buf), Some(&mut req))
    };
    if ok.is_err() && req > 512 {
        buf.resize(req as usize, 0);
        unsafe {
            SetupDiGetDeviceRegistryPropertyW(set, dev, prop, None, Some(&mut buf), Some(&mut req))
        }
        .ok()?;
    } else if ok.is_err() {
        return None;
    }
    let words: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
        .collect();
    let len = words.iter().position(|&c| c == 0).unwrap_or(words.len());
    Some(
        OsString::from_wide(&words[..len])
            .to_string_lossy()
            .trim()
            .to_string(),
    )
}

fn probe_os() -> String {
    let name = reg_get_string(
        HKEY_LOCAL_MACHINE,
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        "ProductName",
    )
    .unwrap_or_else(|| "Windows".to_string());
    let build = reg_get_string(
        HKEY_LOCAL_MACHINE,
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        "CurrentBuildNumber",
    )
    .unwrap_or_default();
    if build.is_empty() {
        name
    } else {
        format!("{name} (build {build})")
    }
}
