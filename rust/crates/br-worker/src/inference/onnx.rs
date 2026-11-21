// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::HashMap;
use std::ffi::CStr;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use half::f16;
use ort::environment::Environment;
use ort::ep;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::{Session, SessionInputValue};
use ort::value::{Tensor, TensorElementType, ValueType};

use crate::hosting::Log;
use crate::inference::{
    DeviceInfo, DeviceKind, HostTensor, IInferenceBackend, IInferenceSession, TensorSpec,
    TensorType, find_native_dll, load_native_library,
};
use crate::protocol::WorkerError;

struct OrtRuntime {
    env: Arc<Environment>,
    version: String,
    providers: Vec<String>,
    _preload: Option<libloading::Library>,
}

fn global_ort() -> Result<&'static OrtRuntime, String> {
    static ORT: OnceLock<Result<OrtRuntime, String>> = OnceLock::new();
    ORT.get_or_init(|| {
        let dll = find_native_dll("onnxruntime.dll")
            .ok_or_else(|| "onnxruntime.dll not found".to_string())?;
        let preload = load_native_library(&dll)
            .map_err(|e| format!("failed to load onnxruntime.dll: {e}"))?;
        let base_getter: libloading::Symbol<'_, unsafe extern "C" fn() -> *const ort::sys::OrtApiBase> =
            unsafe { preload.get(b"OrtGetApiBase") }
                .map_err(|e| format!("OrtGetApiBase missing: {e}"))?;
        let version = unsafe {
            let base = base_getter();
            CStr::from_ptr(((*base).GetVersionString)())
                .to_string_lossy()
                .into_owned()
        };

        let builder = ort::init_from(&dll).map_err(|e| format!("ort init_from: {e}"))?;
        let env = builder.build().map_err(|e| format!("ort env build: {e}"))?;
        let providers = query_available_providers().unwrap_or_default();
        Log::debug(&format!(
            "ONNX Runtime {version}: providers [{}]",
            providers.join(",")
        ));
        Ok(OrtRuntime {
            env: Arc::new(env),
            version,
            providers,
            _preload: Some(preload),
        })
    })
    .as_ref()
    .map_err(Clone::clone)
}

fn query_available_providers() -> ort::Result<Vec<String>> {
    let api = ort::api();
    let mut ptr: *mut *mut core::ffi::c_char = core::ptr::null_mut();
    let mut count: core::ffi::c_int = 0;
    let status = unsafe { (api.GetAvailableProviders)(&mut ptr, &mut count) };
    if !status.0.is_null() {
        unsafe {
            (api.ReleaseStatus)(status.0);
        }
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    if !ptr.is_null() && count > 0 {
        for i in 0..count as isize {
            let p = unsafe { *ptr.offset(i) };
            if !p.is_null() {
                out.push(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned());
            }
        }
        let rel = unsafe { (api.ReleaseAvailableProviders)(ptr, count) };
        if !rel.0.is_null() {
            unsafe {
                (api.ReleaseStatus)(rel.0);
            }
        }
    }
    Ok(out)
}

/// ONNX Runtime: CPU always, QNN (NPU) when the QNN provider is present, DirectML (GPU) otherwise.
/// Used for the embedder on non-Intel machines, for Silero VAD, and for the NSFW gate on the CPU or a DirectML GPU.
pub struct OnnxBackend {
    env: Arc<Environment>,
    version: String,
    devices: Vec<DeviceInfo>,
}

impl OnnxBackend {
    /// `cpu_only`: VAD and the CPU slot of the NSFW gate never touch a GPU or NPU.
    pub fn new(cpu_only: bool) -> Result<Self, WorkerError> {
        let rt = global_ort().map_err(WorkerError::DeviceFailed)?;
        let mut devices = Vec::new();
        if !cpu_only {
            if rt.providers.iter().any(|p| p == "QNNExecutionProvider") {
                devices.push(DeviceInfo::new(
                    DeviceKind::Npu,
                    "QNN",
                    "Qualcomm NPU (QNN HTP)",
                    "QNN",
                    None,
                ));
            }
            if rt.providers.iter().any(|p| p == "DmlExecutionProvider") {
                devices.push(DeviceInfo::new(
                    DeviceKind::Gpu,
                    "DML:0",
                    "DirectML default adapter",
                    "DirectML",
                    None,
                ));
            }
        }
        devices.push(DeviceInfo::new(DeviceKind::Cpu, "CPU", "CPU", "CPU", None));
        Ok(Self {
            env: rt.env.clone(),
            version: rt.version.clone(),
            devices,
        })
    }

    pub fn dml_available() -> bool {
        match global_ort() {
            Ok(rt) => rt.providers.iter().any(|p| p == "DmlExecutionProvider"),
            Err(e) => {
                Log::warn(&format!("ONNX Runtime providers: {e}"));
                false
            }
        }
    }

    /// `"DML:<n>"` -> `n` (the DXGI `EnumAdapters1` index DirectML takes as `device_id`); `0` for anything else.
    pub fn dml_device_id(id: &str) -> i32 {
        let Some(rest) = id.strip_prefix("DML:") else {
            return 0;
        };
        if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
            return 0;
        }
        rest.parse::<i32>().unwrap_or(0)
    }
}

impl IInferenceBackend for OnnxBackend {
    fn name(&self) -> &str {
        "ONNX Runtime"
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn npu_driver_version(&self) -> &str {
        ""
    }

    fn devices(&self) -> &[DeviceInfo] {
        &self.devices
    }

    fn is_onnx_backend(&self) -> bool {
        true
    }

    fn load(
        &self,
        model_path: &Path,
        device: &DeviceInfo,
        _cache_dir: Option<&Path>,
        _static_shapes: Option<&HashMap<String, Vec<i64>>>,
    ) -> Result<Box<dyn IInferenceSession>, WorkerError> {
        if model_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("xml"))
        {
            let file_name = model_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Err(WorkerError::DeviceFailed(format!(
                "{} ONNX Runtime cannot read OpenVINO IR ({file_name}); the model has no ONNX graph",
                device.label()
            )));
        }

        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let threads = (cpus / 4).clamp(2, 4);

        let mut builder = Session::builder(&self.env)
            .map_err(|e| ort_err(device, e))?
            .with_intra_threads(threads)
            .map_err(|e| ort_err(device, e))?
            .with_inter_threads(1)
            .map_err(|e| ort_err(device, e))?
            .with_parallel_execution(false)
            .map_err(|e| ort_err(device, e))?
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(|e| ort_err(device, e))?
            .with_config_entry("session.intra_op.allow_spinning", "0")
            .map_err(|e| ort_err(device, e))?;

        match device.ep.as_str() {
            "QNN" => {
                let qnn = ep::QNN::default()
                    .with_backend_path("QnnHtp.dll")
                    .with_performance_mode(ep::qnn::PerformanceMode::LowPowerSaver)
                    .build()
                    .error_on_failure();
                builder = builder
                    .with_execution_providers([qnn])
                    .map_err(|e| ort_err(device, e))?
                    .with_config_entry("session.disable_cpu_ep_fallback", "1")
                    .map_err(|e| ort_err(device, e))?;
            }
            "DirectML" => {
                let dml = ep::DirectML::default()
                    .with_device_id(Self::dml_device_id(&device.id))
                    .build()
                    .error_on_failure();
                builder = builder
                    .with_memory_pattern(false)
                    .map_err(|e| ort_err(device, e))?
                    .with_execution_providers([dml])
                    .map_err(|e| ort_err(device, e))?;
            }
            _ => {}
        }

        let session = builder
            .commit_from_file(model_path)
            .map_err(|e| ort_err(device, e))?;

        let inputs = session
            .inputs()
            .iter()
            .map(|o| map_outlet(o.name(), o.dtype()))
            .collect();
        let outputs = session
            .outputs()
            .iter()
            .map(|o| map_outlet(o.name(), o.dtype()))
            .collect();

        Ok(Box::new(OrtSession {
            device: device.clone(),
            session: Mutex::new(session),
            inputs,
            outputs,
        }))
    }
}

fn ort_err<R>(device: &DeviceInfo, e: ort::Error<R>) -> WorkerError {
    let s = e.to_string();
    let first = s.lines().next().unwrap_or("").trim();
    WorkerError::DeviceFailed(format!("{} {first}", device.label()))
}

fn map_outlet(name: &str, dtype: &ValueType) -> TensorSpec {
    match dtype {
        ValueType::Tensor { ty, shape, .. } => {
            let mapped = match ty {
                TensorElementType::Float32 => TensorType::Float32,
                TensorElementType::Int64 => TensorType::Int64,
                TensorElementType::Int32 => TensorType::Int32,
                TensorElementType::Float16 => TensorType::Float16,
                _ => TensorType::Other,
            };
            TensorSpec {
                name: name.to_string(),
                ty: mapped,
                shape: shape.iter().copied().collect(),
            }
        }
        _ => TensorSpec {
            name: name.to_string(),
            ty: TensorType::Other,
            shape: Vec::new(),
        },
    }
}

struct OrtSession {
    device: DeviceInfo,
    session: Mutex<Session>,
    inputs: Vec<TensorSpec>,
    outputs: Vec<TensorSpec>,
}

impl IInferenceSession for OrtSession {
    fn device(&self) -> &DeviceInfo {
        &self.device
    }

    fn inputs(&self) -> &[TensorSpec] {
        &self.inputs
    }

    fn outputs(&self) -> &[TensorSpec] {
        &self.outputs
    }

    fn run(&self, feed: &[HostTensor]) -> Result<HashMap<String, HostTensor>, WorkerError> {
        let mut ort_inputs: Vec<(String, SessionInputValue<'_>)> = Vec::with_capacity(feed.len());
        for t in feed {
            let spec = self
                .inputs
                .iter()
                .find(|s| s.name == t.name)
                .ok_or_else(|| WorkerError::DeviceFailed(format!("unknown input {}", t.name)))?;
            let dims: Vec<usize> = t
                .shape
                .iter()
                .map(|&d| {
                    usize::try_from(d).map_err(|_| {
                        WorkerError::DeviceFailed(format!("invalid dimension {d} on {}", t.name))
                    })
                })
                .collect::<Result<_, _>>()?;

            let val: SessionInputValue<'_> = match spec.ty {
                TensorType::Float32 => {
                    let data = t
                        .floats
                        .as_ref()
                        .ok_or_else(|| missing_input(&t.name))?
                        .clone();
                    Tensor::from_array((dims, data))
                        .map_err(|e| ort_err(&self.device, e))?
                        .into()
                }
                TensorType::Float16 => {
                    let src = t.floats.as_ref().ok_or_else(|| missing_input(&t.name))?;
                    let data: Vec<f16> = src.iter().map(|&x| f16::from_f32(x)).collect();
                    Tensor::from_array((dims, data))
                        .map_err(|e| ort_err(&self.device, e))?
                        .into()
                }
                TensorType::Int64 => {
                    let data = t
                        .longs
                        .as_ref()
                        .ok_or_else(|| missing_input(&t.name))?
                        .clone();
                    Tensor::from_array((dims, data))
                        .map_err(|e| ort_err(&self.device, e))?
                        .into()
                }
                TensorType::Int32 => {
                    let src = t.longs.as_ref().ok_or_else(|| missing_input(&t.name))?;
                    let data: Vec<i32> = src
                        .iter()
                        .map(|&x| {
                            i32::try_from(x).map_err(|_| {
                                WorkerError::DeviceFailed(format!(
                                    "input {} value out of Int32 range",
                                    t.name
                                ))
                            })
                        })
                        .collect::<Result<_, _>>()?;
                    Tensor::from_array((dims, data))
                        .map_err(|e| ort_err(&self.device, e))?
                        .into()
                }
                TensorType::Other => {
                    return Err(WorkerError::DeviceFailed(format!(
                        "input {} has an unsupported element type",
                        t.name
                    )));
                }
            };
            ort_inputs.push((t.name.clone(), val));
        }

        let mut sess = self.session.lock().expect("ort session lock");
        let outputs = sess.run(ort_inputs).map_err(|e| ort_err(&self.device, e))?;
        let mut map = HashMap::new();
        for (name, val) in outputs.iter() {
            match val.dtype() {
                ValueType::Tensor {
                    ty: TensorElementType::Float32,
                    ..
                } => {
                    let (shape, data) = val
                        .try_extract_tensor::<f32>()
                        .map_err(|e| ort_err(&self.device, e))?;
                    map.insert(
                        name.to_string(),
                        HostTensor::f(name, data.to_vec(), shape),
                    );
                }
                ValueType::Tensor {
                    ty: TensorElementType::Float16,
                    ..
                } => {
                    let (shape, data) = val
                        .try_extract_tensor::<f16>()
                        .map_err(|e| ort_err(&self.device, e))?;
                    let floats: Vec<f32> = data.iter().map(|x| x.to_f32()).collect();
                    map.insert(name.to_string(), HostTensor::f(name, floats, shape));
                }
                _ => {}
            }
        }
        Ok(map)
    }
}

fn missing_input(name: &str) -> WorkerError {
    WorkerError::DeviceFailed(format!("input {name} has the wrong size or type"))
}
