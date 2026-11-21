// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use crate::inference::dxgi::DxgiAdapter;
use crate::inference::{DeviceInfo, DeviceKind};

/// Which GPU the embedder uses when `inference.device` is `"gpu"` (`inference.gpuAdapter`),
/// and the GPU list of the benchmark.
pub struct GpuSelection;

impl GpuSelection {
    pub fn is_auto(setting: Option<&str>) -> bool {
        match setting {
            None => true,
            Some(s) => {
                let t = s.trim();
                t.is_empty() || t.eq_ignore_ascii_case("auto")
            }
        }
    }

    /// The GPU `"auto"` means: the main backend's first GPU (OpenVINO Intel GPU, or DirectML default).
    pub fn auto(main: &[DeviceInfo]) -> Option<DeviceInfo> {
        main.iter().find(|d| d.kind == DeviceKind::Gpu).cloned()
    }

    pub fn dml(a: &DxgiAdapter) -> DeviceInfo {
        DeviceInfo::new(
            DeviceKind::Gpu,
            format!("DML:{}", a.index),
            &a.description,
            "DirectML",
            None,
        )
    }

    fn is_hardware(a: &DxgiAdapter) -> bool {
        !a.is_software() && !a.is_basic_render()
    }

    /// The OpenVINO GPU that is the same physical device as an Intel DXGI adapter.
    pub fn open_vino_twin(a: &DxgiAdapter, main: &[DeviceInfo]) -> Option<DeviceInfo> {
        if a.vendor_id != DxgiAdapter::VENDOR_INTEL {
            return None;
        }
        let ov: Vec<&DeviceInfo> = main
            .iter()
            .filter(|d| d.kind == DeviceKind::Gpu && d.ep == "OpenVINO")
            .collect();
        let wanted = Some(a.is_intel_integrated());
        ov.iter()
            .find(|d| d.integrated == wanted)
            .copied()
            .cloned()
            .or_else(|| {
                if ov.len() == 1 {
                    Some(ov[0].clone())
                } else {
                    None
                }
            })
    }

    /// Every GPU once: the main backend's OpenVINO GPUs, then a DirectML device per hardware adapter
    /// without an OpenVINO twin.
    pub fn all(main: &[DeviceInfo], adapters: &[DxgiAdapter], dml: bool) -> Vec<DeviceInfo> {
        let mut list: Vec<DeviceInfo> = main
            .iter()
            .filter(|d| d.kind == DeviceKind::Gpu && d.ep == "OpenVINO")
            .cloned()
            .collect();
        if dml {
            for a in adapters.iter().filter(|a| Self::is_hardware(a)) {
                if Self::open_vino_twin(a, main).is_none() {
                    list.push(Self::dml(a));
                }
            }
        }
        if !list.iter().any(|d| d.ep == "DirectML") {
            list.extend(
                main.iter()
                    .filter(|d| d.kind == DeviceKind::Gpu && d.ep == "DirectML")
                    .cloned(),
            );
        }
        list
    }

    /// Resolves `inference.gpuAdapter` to a device plus an optional warning when falling back to auto.
    pub fn resolve(
        setting: Option<&str>,
        main: &[DeviceInfo],
        adapters: &[DxgiAdapter],
        dml: bool,
    ) -> (Option<DeviceInfo>, Option<String>) {
        let auto = Self::auto(main);
        if Self::is_auto(setting) {
            return (auto, None);
        }
        let s = setting.unwrap().trim();
        let needle = s.to_lowercase();
        if let Some(ov) = main.iter().find(|d| {
            d.kind == DeviceKind::Gpu
                && d.ep == "OpenVINO"
                && d.name.to_lowercase().contains(&needle)
        }) {
            return (Some(ov.clone()), None);
        }
        if let Some(a) = adapters
            .iter()
            .find(|x| Self::is_hardware(x) && x.description.to_lowercase().contains(&needle))
        {
            if let Some(twin) = Self::open_vino_twin(a, main) {
                return (Some(twin), None);
            }
            if dml {
                return (Some(Self::dml(a)), None);
            }
            return (
                auto,
                Some(format!(
                    "inference.gpuAdapter '{s}' matches {}, but ONNX Runtime has no DirectML provider; using the automatic GPU",
                    a.description
                )),
            );
        }
        (
            auto,
            Some(format!(
                "inference.gpuAdapter '{s}' matches no GPU; using the automatic GPU"
            )),
        )
    }
}
