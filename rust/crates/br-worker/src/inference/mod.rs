// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

pub mod device_selection;
pub mod dxgi;
pub mod gpu_selection;
pub mod nsfw_plan;
pub mod onnx;
pub mod openvino;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use libloading::os::windows::Library as WinLibrary;
use serde::{Deserialize, Serialize};
use windows::Win32::System::LibraryLoader::{
    LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

use crate::protocol::WorkerError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceKind {
    Npu,
    Gpu,
    Cpu,
}

/// A device a backend can compile for. `id` is backend specific (`"NPU"`, `"GPU.0"`, `"CPU"`, `"DML:0"`, `"QNN"`).
/// `integrated`: OpenVINO GPU `DEVICE_TYPE` (`"integrated"` = `Some(true)`, `"discrete"` = `Some(false)`),
/// `None` when unknown or not a GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub kind: DeviceKind,
    pub id: String,
    pub name: String,
    pub ep: String,
    pub integrated: Option<bool>,
}

impl DeviceInfo {
    pub fn new(
        kind: DeviceKind,
        id: impl Into<String>,
        name: impl Into<String>,
        ep: impl Into<String>,
        integrated: Option<bool>,
    ) -> Self {
        Self {
            kind,
            id: id.into(),
            name: name.into(),
            ep: ep.into(),
            integrated,
        }
    }

    pub fn label(&self) -> &'static str {
        match self.kind {
            DeviceKind::Npu => "NPU",
            DeviceKind::Gpu => "GPU",
            DeviceKind::Cpu => "CPU",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TensorType {
    Float32,
    Int64,
    Int32,
    Float16,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TensorSpec {
    pub name: String,
    pub ty: TensorType,
    pub shape: Vec<i64>,
}

impl TensorSpec {
    pub fn is_static(&self) -> bool {
        self.shape.iter().all(|&d| d > 0)
    }

    pub fn elements(&self) -> i64 {
        self.shape.iter().fold(1i64, |a, &b| a * b.max(1))
    }
}

/// Host tensor. Exactly one of `floats` / `longs` is set.
#[derive(Debug, Clone)]
pub struct HostTensor {
    pub name: String,
    pub shape: Vec<i64>,
    pub floats: Option<Vec<f32>>,
    pub longs: Option<Vec<i64>>,
}

impl HostTensor {
    pub fn f(name: impl Into<String>, data: Vec<f32>, shape: &[i64]) -> Self {
        Self {
            name: name.into(),
            shape: shape.to_vec(),
            floats: Some(data),
            longs: None,
        }
    }

    pub fn l(name: impl Into<String>, data: Vec<i64>, shape: &[i64]) -> Self {
        Self {
            name: name.into(),
            shape: shape.to_vec(),
            floats: None,
            longs: Some(data),
        }
    }
}

pub trait IInferenceSession: Send + Sync {
    fn device(&self) -> &DeviceInfo;
    fn inputs(&self) -> &[TensorSpec];
    fn outputs(&self) -> &[TensorSpec];

    /// Runs once. Outputs come back as float32 (int outputs are not used by the worker's graphs).
    fn run(&self, inputs: &[HostTensor]) -> Result<HashMap<String, HostTensor>, WorkerError>;
}

/// An inference runtime: OpenVINO (primary on Intel) or ONNX Runtime (CPU, DirectML, QNN).
pub trait IInferenceBackend: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    /// NPU driver version, empty when the backend sees no NPU.
    fn npu_driver_version(&self) -> &str;
    fn devices(&self) -> &[DeviceInfo];
    fn is_onnx_backend(&self) -> bool {
        false
    }
    fn is_openvino_backend(&self) -> bool {
        false
    }

    /// Loads and compiles one graph. `cache_dir` (compiled blobs) may be `None`.
    /// `static_shapes` fixes dynamic inputs (by name) before compiling.
    fn load(
        &self,
        model_path: &Path,
        device: &DeviceInfo,
        cache_dir: Option<&Path>,
        static_shapes: Option<&HashMap<String, Vec<i64>>>,
    ) -> Result<Box<dyn IInferenceSession>, WorkerError>;
}

/// Locates a native runtime DLL next to the worker exe first, falling back to C# build outputs
/// or the NuGet cache only when running outside a staged worker directory (e.g. `cargo test`).
pub fn find_native_dll(file_name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let local = exe_dir.join(file_name);
    if local.is_file() {
        return Some(local);
    }
    // Staged/installed BetterRecall.Worker.exe must never load DLLs from NuGet/repo artifacts.
    if exe
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("BetterRecall.Worker.exe"))
    {
        return None;
    }

    if let Some(dir) = std::env::var_os("BR_NATIVE_DIR") {
        let p = PathBuf::from(dir).join(file_name);
        if p.is_file() {
            return Some(p);
        }
    }

    // Check repo artifacts\bin\BetterRecall.Worker\<pivot>\<file_name>
    for anchor in [Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))), Some(exe_dir.to_path_buf())]
        .into_iter()
        .flatten()
    {
        let mut cur: Option<&Path> = Some(anchor.as_path());
        while let Some(d) = cur {
            let bin_dir = d.join("artifacts").join("bin").join("BetterRecall.Worker");
            if bin_dir.is_dir()
                && let Ok(entries) = std::fs::read_dir(&bin_dir)
            {
                for entry in entries.flatten() {
                    let cand = entry.path().join(file_name);
                    if cand.is_file() {
                        return Some(cand);
                    }
                }
            }
            cur = d.parent();
        }
    }

    // NuGet package cache / dev native fallback for cargo test only: a release worker loads natives
    // from its own folder and never from a user-writable package cache or repo native staging dir.
    #[cfg(debug_assertions)]
    {
        for anchor in [
            Some(PathBuf::from(env!("CARGO_MANIFEST_DIR"))),
            Some(exe_dir.to_path_buf()),
        ]
        .into_iter()
        .flatten()
        {
            let mut cur: Option<&Path> = Some(anchor.as_path());
            while let Some(d) = cur {
                let cand = d
                    .join("artifacts")
                    .join("native")
                    .join("openvino-genai")
                    .join(file_name);
                if cand.is_file() {
                    return Some(cand);
                }
                cur = d.parent();
            }
        }
    }
    #[cfg(debug_assertions)]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let pkgs = PathBuf::from(profile).join(".nuget").join("packages");
        if file_name.eq_ignore_ascii_case("onnxruntime.dll") {
            let cand = pkgs
                .join("microsoft.ml.onnxruntime.directml")
                .join("1.24.4")
                .join("runtimes")
                .join("win-x64")
                .join("native")
                .join(file_name);
            if cand.is_file() {
                return Some(cand);
            }
        } else if file_name.eq_ignore_ascii_case("openvino_c.dll") {
            let ov_pkg = pkgs.join("openvino.runtime.win");
            if let Ok(entries) = std::fs::read_dir(&ov_pkg) {
                let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
                dirs.sort();
                for ver in dirs.into_iter().rev() {
                    let cand = ver
                        .join("runtimes")
                        .join("win-x64")
                        .join("native")
                        .join(file_name);
                    if cand.is_file() {
                        return Some(cand);
                    }
                }
            }
        }
    }
    None
}

/// Loads a native DLL with `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS`
/// so sibling DLLs (DirectML.dll, openvino.dll, tbb12.dll, plugins) resolve from the DLL's folder
/// instead of System32.
pub fn load_native_library(path: &Path) -> Result<libloading::Library, libloading::Error> {
    // In cargo test (when DLL is not next to exe), also add its dir to the search order for late plugin loads.
    #[cfg(debug_assertions)]
    if let Ok(exe) = std::env::current_exe()
        && let (Some(exe_dir), Some(dll_dir)) = (exe.parent(), path.parent())
        && exe_dir != dll_dir
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::System::LibraryLoader::SetDllDirectoryW;
        use windows::core::PCWSTR;
        let wide: Vec<u16> = dll_dir.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let _ = SetDllDirectoryW(PCWSTR(wide.as_ptr()));
        }
    }
    let flags = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR.0 | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS.0;
    let win_lib = unsafe { WinLibrary::load_with_flags(path, flags) }?;
    Ok(win_lib.into())
}
