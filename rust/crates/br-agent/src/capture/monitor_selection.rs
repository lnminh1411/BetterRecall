// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Stable monitor-id pinning across hot-plug renumbering (`Capture\MonitorSelection.cs`).

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;

use windows::Win32::Graphics::Gdi::{DISPLAY_DEVICEW, EnumDisplayDevicesW};
use windows::core::PCWSTR;

const EDD_GET_DEVICE_INTERFACE_NAME: u32 = 1;

use crate::capture::gpu_devices::OutputInfo;
use crate::native::to_wide_z;
use crate::util::AgentLog;

/// Settings index list and the stable monitor IDs it resolved to at that time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorPin {
    pub indices: Vec<i32>,
    pub ids: Vec<String>,
}

/// Pins `capture.monitors` indices to stable monitor device interface paths (A12).
pub struct MonitorSelection;

impl MonitorSelection {
    pub const FILE_NAME: &'static str = "capture-monitors.txt";

    /// Resolves which outputs to capture and whether a new pin must be saved. Pure.
    pub fn resolve<F>(
        setting: &[i32],
        outputs: &[OutputInfo],
        id_of: F,
        pinned: Option<&MonitorPin>,
    ) -> (Vec<OutputInfo>, Option<MonitorPin>)
    where
        F: Fn(&OutputInfo) -> String,
    {
        if setting.is_empty() {
            return (outputs.to_vec(), None);
        }
        if let Some(pin) = pinned
            && pin.indices == setting
        {
            let ids: HashSet<String> = pin.ids.iter().map(|s| s.to_ascii_lowercase()).collect();
            let sel = outputs
                .iter()
                .filter(|o| ids.contains(&id_of(o).to_ascii_lowercase()))
                .cloned()
                .collect();
            return (sel, None);
        }
        let chosen: Vec<OutputInfo> = outputs
            .iter()
            .filter(|o| setting.contains(&o.index))
            .cloned()
            .collect();
        let pin = MonitorPin {
            indices: setting.to_vec(),
            ids: chosen.iter().map(&id_of).collect(),
        };
        (chosen, Some(pin))
    }

    pub fn load(path: Option<&Path>) -> Option<MonitorPin> {
        let p = path?;
        if !p.is_file() {
            return None;
        }
        let text = match fs::read_to_string(p) {
            Ok(t) => t,
            Err(e) => {
                AgentLog::warn_err("capture monitor ids unreadable", &e.to_string());
                return None;
            }
        };
        let mut lines = text.lines();
        let first = lines.next()?;
        let mut indices = Vec::new();
        for part in first.split(',') {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            match trimmed.parse::<i32>() {
                Ok(v) => indices.push(v),
                Err(e) => {
                    AgentLog::warn_err("capture monitor ids unreadable", &e.to_string());
                    return None;
                }
            }
        }
        let ids: Vec<String> = lines
            .filter(|l| !l.is_empty())
            .map(ToString::to_string)
            .collect();
        Some(MonitorPin { indices, ids })
    }

    pub fn save(path: Option<&Path>, pin: &MonitorPin) {
        let Some(p) = path else {
            return;
        };
        let mut content = pin
            .indices
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        for id in &pin.ids {
            content.push('\n');
            content.push_str(id);
        }
        content.push('\n');
        let tmp = p.with_extension("txt.tmp");
        if let Err(e) = fs::write(&tmp, content).and_then(|()| fs::rename(&tmp, p)) {
            AgentLog::warn_err("capture monitor ids not saved", &e.to_string());
        }
    }

    /// Stable ID of the monitor on a DXGI output (`\\.\DISPLAYn`): its device interface path,
    /// falling back to the GDI device name.
    pub fn stable_id(o: &OutputInfo) -> String {
        device_interface_path(&o.device_name).unwrap_or_else(|| o.device_name.clone())
    }
}

fn device_interface_path(adapter_device: &str) -> Option<String> {
    let wide = to_wide_z(adapter_device);
    let mut dd = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        EnumDisplayDevicesW(
            PCWSTR(wide.as_ptr()),
            0,
            &mut dd,
            EDD_GET_DEVICE_INTERFACE_NAME,
        )
    };
    if !ok.as_bool() {
        return None;
    }
    let len = dd
        .DeviceID
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(dd.DeviceID.len());
    if len == 0 {
        return None;
    }
    Some(
        OsString::from_wide(&dd.DeviceID[..len])
            .to_string_lossy()
            .into_owned(),
    )
}
