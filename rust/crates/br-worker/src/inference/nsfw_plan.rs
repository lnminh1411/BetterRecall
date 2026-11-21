// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use crate::inference::{DeviceInfo, DeviceKind};

/// Device order of the NSFW gate (classifier and detector), setting `privacy.nsfwDevice`.
pub struct NsfwDevicePlan;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NsfwSlot {
    Npu,
    IntegratedGpu,
    DiscreteGpu,
    Cpu,
}

impl NsfwDevicePlan {
    pub const VALUES: [&'static str; 5] = ["auto", "npu", "igpu", "gpu", "cpu"];

    /// Slot of a device, or `None` when the gate never uses it.
    pub fn slot_of(d: &DeviceInfo) -> Option<NsfwSlot> {
        match d.kind {
            DeviceKind::Npu if d.ep == "OpenVINO" => Some(NsfwSlot::Npu),
            DeviceKind::Gpu if d.ep == "OpenVINO" && d.integrated == Some(true) => {
                Some(NsfwSlot::IntegratedGpu)
            }
            DeviceKind::Gpu if d.ep == "DirectML" => Some(NsfwSlot::DiscreteGpu),
            DeviceKind::Cpu if d.ep == "CPU" => Some(NsfwSlot::Cpu),
            _ => None,
        }
    }

    pub fn slots(setting: &str) -> &'static [NsfwSlot] {
        match setting {
            "npu" => &[NsfwSlot::Npu, NsfwSlot::Cpu],
            "igpu" => &[NsfwSlot::IntegratedGpu, NsfwSlot::Cpu],
            "gpu" => &[NsfwSlot::DiscreteGpu, NsfwSlot::Cpu],
            "cpu" => &[NsfwSlot::Cpu],
            _ => &[
                NsfwSlot::Npu,
                NsfwSlot::IntegratedGpu,
                NsfwSlot::DiscreteGpu,
                NsfwSlot::Cpu,
            ],
        }
    }

    /// Candidates in try order: for each slot of the setting, the first available device in that slot.
    pub fn order(setting: &str, available: &[DeviceInfo]) -> Vec<DeviceInfo> {
        let mut list = Vec::new();
        for &slot in Self::slots(setting) {
            if let Some(dev) = available.iter().find(|d| Self::slot_of(d) == Some(slot)) {
                list.push(dev.clone());
            }
        }
        list
    }

    /// Name shown in `hello.nsfwDevice` and logs: `NPU`, `iGPU`, `GPU (adapter name)` or `CPU`.
    pub fn label(d: &DeviceInfo) -> String {
        match Self::slot_of(d) {
            Some(NsfwSlot::Npu) => "NPU".to_string(),
            Some(NsfwSlot::IntegratedGpu) => "iGPU".to_string(),
            Some(NsfwSlot::DiscreteGpu) => format!("GPU ({})", d.name),
            _ => "CPU".to_string(),
        }
    }
}
