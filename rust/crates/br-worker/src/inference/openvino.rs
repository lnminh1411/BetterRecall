// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use half::f16;
use libloading::Library;

use crate::hosting::Log;
use crate::inference::{
    DeviceInfo, DeviceKind, HostTensor, IInferenceBackend, IInferenceSession, TensorSpec,
    TensorType, find_native_dll, load_native_library,
};
use crate::protocol::WorkerError;

const OV_OK: c_int = 0;

// ov_element_type_e (ov_common.h, 2026.4): DYNAMIC=0, BOOLEAN, BF16, F16, F32, F64, I4, I8, I16, I32, I64
const ET_BF16: c_int = 2;
const ET_F16: c_int = 3;
const ET_F32: c_int = 4;
const ET_I32: c_int = 9;
const ET_I64: c_int = 10;

#[repr(C)]
#[derive(Clone, Copy)]
struct OvShape {
    rank: i64,
    dims: *mut i64,
}

#[repr(C)]
struct OvVersion {
    build_number: *const c_char,
    description: *const c_char,
}

#[repr(C)]
struct OvAvailableDevices {
    devices: *const *const c_char,
    size: usize,
}

#[repr(C)]
struct OvProperty {
    key: *const c_char,
    value: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct OvDimension {
    min: i64,
    max: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct OvPartialShape {
    rank: OvDimension,
    dims: *mut OvDimension,
}

/// Hand-written dynamic FFI to OpenVINO 2026.4 C API (`openvino_c.dll`), mirroring `OvNative.cs`.
struct OvApi {
    ov_get_openvino_version: unsafe extern "C" fn(*mut OvVersion) -> c_int,
    ov_version_free: unsafe extern "C" fn(*mut OvVersion),
    ov_get_last_err_msg: unsafe extern "C" fn() -> *const c_char,
    ov_free: unsafe extern "C" fn(*const c_char),
    ov_core_create: unsafe extern "C" fn(*mut *mut c_void) -> c_int,
    ov_core_free: unsafe extern "C" fn(*mut c_void),
    ov_core_get_available_devices: unsafe extern "C" fn(*mut c_void, *mut OvAvailableDevices) -> c_int,
    ov_available_devices_free: unsafe extern "C" fn(*mut OvAvailableDevices),
    ov_core_get_property:
        unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *mut *mut c_char) -> c_int,
    ov_core_read_model_unicode:
        unsafe extern "C" fn(*mut c_void, *const u16, *const u16, *mut *mut c_void) -> c_int,
    ov_core_compile_model_props: unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        *const c_char,
        usize,
        *const OvProperty,
        *mut *mut c_void,
    ) -> c_int,
    ov_model_free: unsafe extern "C" fn(*mut c_void),
    ov_model_inputs_size: unsafe extern "C" fn(*mut c_void, *mut usize) -> c_int,
    ov_model_outputs_size: unsafe extern "C" fn(*mut c_void, *mut usize) -> c_int,
    ov_model_const_input_by_index:
        unsafe extern "C" fn(*mut c_void, usize, *mut *mut c_void) -> c_int,
    ov_model_const_output_by_index:
        unsafe extern "C" fn(*mut c_void, usize, *mut *mut c_void) -> c_int,
    ov_port_get_any_name: unsafe extern "C" fn(*mut c_void, *mut *mut c_char) -> c_int,
    ov_const_port_get_shape: unsafe extern "C" fn(*mut c_void, *mut OvShape) -> c_int,
    ov_port_get_element_type: unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int,
    ov_output_const_port_free: unsafe extern "C" fn(*mut c_void),
    ov_compiled_model_free: unsafe extern "C" fn(*mut c_void),
    ov_compiled_model_create_infer_request:
        unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int,
    ov_infer_request_free: unsafe extern "C" fn(*mut c_void),
    ov_infer_request_set_tensor:
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> c_int,
    ov_infer_request_infer: unsafe extern "C" fn(*mut c_void) -> c_int,
    ov_infer_request_get_output_tensor_by_index:
        unsafe extern "C" fn(*mut c_void, usize, *mut *mut c_void) -> c_int,
    ov_partial_shape_create_static:
        unsafe extern "C" fn(i64, *const i64, *mut OvPartialShape) -> c_int,
    ov_partial_shape_free: unsafe extern "C" fn(*mut OvPartialShape),
    ov_model_reshape_input_by_name:
        unsafe extern "C" fn(*mut c_void, *const c_char, OvPartialShape) -> c_int,
    ov_shape_create: unsafe extern "C" fn(i64, *const i64, *mut OvShape) -> c_int,
    ov_shape_free: unsafe extern "C" fn(*mut OvShape) -> c_int,
    ov_tensor_create: unsafe extern "C" fn(c_int, OvShape, *mut *mut c_void) -> c_int,
    ov_tensor_get_shape: unsafe extern "C" fn(*mut c_void, *mut OvShape) -> c_int,
    ov_tensor_get_element_type: unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int,
    ov_tensor_get_size: unsafe extern "C" fn(*mut c_void, *mut usize) -> c_int,
    ov_tensor_data: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int,
    ov_tensor_free: unsafe extern "C" fn(*mut c_void),
    _lib: Library,
}

unsafe impl Send for OvApi {}
unsafe impl Sync for OvApi {}

macro_rules! load_sym {
    ($lib:expr, $name:literal) => {
        *unsafe { $lib.get($name)? }
    };
}

impl OvApi {
    fn load(path: &Path) -> Result<Self, libloading::Error> {
        let lib = load_native_library(path)?;
        Ok(Self {
            ov_get_openvino_version: load_sym!(lib, b"ov_get_openvino_version\0"),
            ov_version_free: load_sym!(lib, b"ov_version_free\0"),
            ov_get_last_err_msg: load_sym!(lib, b"ov_get_last_err_msg\0"),
            ov_free: load_sym!(lib, b"ov_free\0"),
            ov_core_create: load_sym!(lib, b"ov_core_create\0"),
            ov_core_free: load_sym!(lib, b"ov_core_free\0"),
            ov_core_get_available_devices: load_sym!(lib, b"ov_core_get_available_devices\0"),
            ov_available_devices_free: load_sym!(lib, b"ov_available_devices_free\0"),
            ov_core_get_property: load_sym!(lib, b"ov_core_get_property\0"),
            ov_core_read_model_unicode: load_sym!(lib, b"ov_core_read_model_unicode\0"),
            ov_core_compile_model_props: load_sym!(lib, b"ov_core_compile_model_props\0"),
            ov_model_free: load_sym!(lib, b"ov_model_free\0"),
            ov_model_inputs_size: load_sym!(lib, b"ov_model_inputs_size\0"),
            ov_model_outputs_size: load_sym!(lib, b"ov_model_outputs_size\0"),
            ov_model_const_input_by_index: load_sym!(lib, b"ov_model_const_input_by_index\0"),
            ov_model_const_output_by_index: load_sym!(lib, b"ov_model_const_output_by_index\0"),
            ov_port_get_any_name: load_sym!(lib, b"ov_port_get_any_name\0"),
            ov_const_port_get_shape: load_sym!(lib, b"ov_const_port_get_shape\0"),
            ov_port_get_element_type: load_sym!(lib, b"ov_port_get_element_type\0"),
            ov_output_const_port_free: load_sym!(lib, b"ov_output_const_port_free\0"),
            ov_compiled_model_free: load_sym!(lib, b"ov_compiled_model_free\0"),
            ov_compiled_model_create_infer_request: load_sym!(
                lib,
                b"ov_compiled_model_create_infer_request\0"
            ),
            ov_infer_request_free: load_sym!(lib, b"ov_infer_request_free\0"),
            ov_infer_request_set_tensor: load_sym!(lib, b"ov_infer_request_set_tensor\0"),
            ov_infer_request_infer: load_sym!(lib, b"ov_infer_request_infer\0"),
            ov_infer_request_get_output_tensor_by_index: load_sym!(
                lib,
                b"ov_infer_request_get_output_tensor_by_index\0"
            ),
            ov_partial_shape_create_static: load_sym!(lib, b"ov_partial_shape_create_static\0"),
            ov_partial_shape_free: load_sym!(lib, b"ov_partial_shape_free\0"),
            ov_model_reshape_input_by_name: load_sym!(lib, b"ov_model_reshape_input_by_name\0"),
            ov_shape_create: load_sym!(lib, b"ov_shape_create\0"),
            ov_shape_free: load_sym!(lib, b"ov_shape_free\0"),
            ov_tensor_create: load_sym!(lib, b"ov_tensor_create\0"),
            ov_tensor_get_shape: load_sym!(lib, b"ov_tensor_get_shape\0"),
            ov_tensor_get_element_type: load_sym!(lib, b"ov_tensor_get_element_type\0"),
            ov_tensor_get_size: load_sym!(lib, b"ov_tensor_get_size\0"),
            ov_tensor_data: load_sym!(lib, b"ov_tensor_data\0"),
            ov_tensor_free: load_sym!(lib, b"ov_tensor_free\0"),
            _lib: lib,
        })
    }

    fn last_error(&self, status: c_int) -> String {
        let p = unsafe { (self.ov_get_last_err_msg)() };
        let raw = if p.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        };
        let msg = raw.trim();
        if msg.is_empty() {
            return format!("status {status} (status {status})");
        }
        let line = msg
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("Check '") && !l.contains(".cpp:"))
            .unwrap_or_else(|| msg.lines().next().unwrap_or(""));
        format!("{line} (status {status})")
    }

    fn check(&self, status: c_int, what: &str) -> Result<(), String> {
        if status == OV_OK {
            Ok(())
        } else {
            Err(format!("{what}: {}", self.last_error(status)))
        }
    }

    unsafe fn take_string(&self, p: *const c_char) -> String {
        if p.is_null() {
            return String::new();
        }
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { (self.ov_free)(p) };
        s
    }
}

struct OvCore {
    ptr: *mut c_void,
    api: Arc<OvApi>,
}

unsafe impl Send for OvCore {}
unsafe impl Sync for OvCore {}

impl Drop for OvCore {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.api.ov_core_free)(self.ptr) };
            self.ptr = std::ptr::null_mut();
        }
    }
}

/// OpenVINO through its C API. NPU, Intel GPU (only devices whose name contains `Intel`), CPU.
pub struct OpenVinoBackend {
    core: Arc<OvCore>,
    version: String,
    npu_driver: String,
    devices: Vec<DeviceInfo>,
    supported: Mutex<HashMap<String, HashSet<String>>>,
}

impl OpenVinoBackend {
    /// Returns `None` when `openvino_c.dll` cannot be loaded or initialized.
    pub fn try_create() -> Option<Self> {
        let dll = find_native_dll("openvino_c.dll")?;
        let api = match OvApi::load(&dll) {
            Ok(a) => Arc::new(a),
            Err(e) => {
                Log::info(&format!("OpenVINO unavailable: {e}"));
                return None;
            }
        };

        let mut ver = OvVersion {
            build_number: std::ptr::null(),
            description: std::ptr::null(),
        };
        if let Err(e) = api.check(unsafe { (api.ov_get_openvino_version)(&mut ver) }, "version") {
            Log::info(&format!("OpenVINO unavailable: {e}"));
            return None;
        }
        let version = if ver.build_number.is_null() {
            "unknown".to_string()
        } else {
            unsafe { CStr::from_ptr(ver.build_number) }
                .to_string_lossy()
                .into_owned()
        };
        unsafe { (api.ov_version_free)(&mut ver) };

        let mut core_ptr: *mut c_void = std::ptr::null_mut();
        if let Err(e) = api.check(unsafe { (api.ov_core_create)(&mut core_ptr) }, "core") {
            Log::info(&format!("OpenVINO unavailable: {e}"));
            return None;
        }
        let core = Arc::new(OvCore {
            ptr: core_ptr,
            api: api.clone(),
        });

        let mut names = Vec::new();
        let mut devs = OvAvailableDevices {
            devices: std::ptr::null(),
            size: 0,
        };
        if unsafe { (api.ov_core_get_available_devices)(core.ptr, &mut devs) } == OV_OK {
            for i in 0..devs.size {
                let p = unsafe { *devs.devices.add(i) };
                if !p.is_null() {
                    names.push(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned());
                }
            }
            unsafe { (api.ov_available_devices_free)(&mut devs) };
        }

        let mut devices = Vec::new();
        let mut npu_driver = String::new();
        for n in names {
            let full = get_prop(&api, core.ptr, &n, "FULL_DEVICE_NAME").unwrap_or_else(|| n.clone());
            if n.starts_with("NPU") {
                devices.push(DeviceInfo::new(
                    DeviceKind::Npu,
                    n.clone(),
                    full,
                    "OpenVINO",
                    None,
                ));
                npu_driver = get_prop(&api, core.ptr, &n, "NPU_DRIVER_VERSION").unwrap_or_default();
            } else if n.starts_with("GPU") {
                if full.to_ascii_lowercase().contains("intel") {
                    let ty = get_prop(&api, core.ptr, &n, "DEVICE_TYPE");
                    let integrated = ty.map(|t| t.trim().eq_ignore_ascii_case("integrated"));
                    devices.push(DeviceInfo::new(
                        DeviceKind::Gpu,
                        n,
                        full,
                        "OpenVINO",
                        integrated,
                    ));
                }
            } else if n == "CPU" {
                devices.push(DeviceInfo::new(
                    DeviceKind::Cpu,
                    n,
                    full,
                    "OpenVINO",
                    None,
                ));
            }
        }

        let dev_summary: Vec<String> = devices
            .iter()
            .map(|d| match d.integrated {
                Some(true) => format!("{}(integrated)", d.id),
                Some(false) => format!("{}(discrete)", d.id),
                None => d.id.clone(),
            })
            .collect();
        Log::info(&format!(
            "OpenVINO {version}: devices [{}] npuDriver={npu_driver}",
            dev_summary.join(", ")
        ));

        Some(Self {
            core,
            version,
            npu_driver,
            devices,
            supported: Mutex::new(HashMap::new()),
        })
    }

    fn supported_props(&self, device: &str) -> HashSet<String> {
        let mut cache = self.supported.lock().expect("ov supported lock");
        if let Some(s) = cache.get(device) {
            return s.clone();
        }
        let raw = get_prop(&self.core.api, self.core.ptr, device, "SUPPORTED_PROPERTIES")
            .unwrap_or_default();
        let set: HashSet<String> = raw
            .split([' ', ',', '\t', '\n', '\r'])
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        cache.insert(device.to_string(), set.clone());
        set
    }

    /// Background-friendly compile properties per device (research.md 2.3).
    pub fn properties_for(kind: DeviceKind, cache_dir: Option<&Path>) -> Vec<(&'static str, String)> {
        let mut p = vec![
            ("PERFORMANCE_HINT", "LATENCY".to_string()),
            ("PERFORMANCE_HINT_NUM_REQUESTS", "1".to_string()),
        ];
        match kind {
            DeviceKind::Npu => {
                p.push(("NPU_TURBO", "NO".to_string()));
                p.push(("WORKLOAD_TYPE", "EFFICIENT".to_string()));
                p.push(("MODEL_PRIORITY", "LOW".to_string()));
            }
            DeviceKind::Gpu => {
                p.push(("GPU_QUEUE_THROTTLE", "LOW".to_string()));
                p.push(("GPU_QUEUE_PRIORITY", "LOW".to_string()));
                p.push(("MODEL_PRIORITY", "LOW".to_string()));
                // EmbeddingGemma 2: float16 gives NaN (model card).
                p.push(("INFERENCE_PRECISION_HINT", "f32".to_string()));
            }
            DeviceKind::Cpu => {
                let cpus = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4);
                p.push(("INFERENCE_NUM_THREADS", (cpus / 4).clamp(2, 4).to_string()));
                p.push(("SCHEDULING_CORE_TYPE", "ECORE_ONLY".to_string()));
                p.push(("INFERENCE_PRECISION_HINT", "f32".to_string()));
                p.push(("ENABLE_CPU_PINNING", "NO".to_string()));
            }
        }
        if let Some(dir) = cache_dir {
            let s = dir.to_string_lossy();
            if !s.is_empty() {
                p.push(("CACHE_DIR", s.into_owned()));
            }
        }
        p
    }
}

impl IInferenceBackend for OpenVinoBackend {
    fn name(&self) -> &str {
        "OpenVINO"
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn npu_driver_version(&self) -> &str {
        &self.npu_driver
    }

    fn devices(&self) -> &[DeviceInfo] {
        &self.devices
    }

    fn is_openvino_backend(&self) -> bool {
        true
    }

    fn load(
        &self,
        model_path: &Path,
        device: &DeviceInfo,
        cache_dir: Option<&Path>,
        static_shapes: Option<&HashMap<String, Vec<i64>>>,
    ) -> Result<Box<dyn IInferenceSession>, WorkerError> {
        let api = &self.core.api;
        let model_w = wide_null(model_path);
        let bin_path = if model_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("xml"))
        {
            let b = model_path.with_extension("bin");
            if b.exists() { Some(b) } else { None }
        } else {
            None
        };
        let bin_w = bin_path.as_deref().map(wide_null);

        let mut model_ptr: *mut c_void = std::ptr::null_mut();
        api.check(
            unsafe {
                (api.ov_core_read_model_unicode)(
                    self.core.ptr,
                    model_w.as_ptr(),
                    bin_w.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
                    &mut model_ptr,
                )
            },
            "read model",
        )
        .map_err(|e| WorkerError::DeviceFailed(format!("{} {e}", device.label())))?;

        let _model_guard = ModelGuard {
            ptr: model_ptr,
            api: api.clone(),
        };

        if let Some(shapes) = static_shapes {
            for (name, dims) in shapes {
                let mut ps = OvPartialShape {
                    rank: OvDimension { min: 0, max: 0 },
                    dims: std::ptr::null_mut(),
                };
                api.check(
                    unsafe {
                        (api.ov_partial_shape_create_static)(
                            dims.len() as i64,
                            dims.as_ptr(),
                            &mut ps,
                        )
                    },
                    "partial shape",
                )
                .map_err(|e| WorkerError::DeviceFailed(format!("{} {e}", device.label())))?;
                let c_name = CString::new(name.as_str()).map_err(|_| {
                    WorkerError::DeviceFailed(format!("{} invalid tensor name", device.label()))
                })?;
                let res = unsafe { (api.ov_model_reshape_input_by_name)(model_ptr, c_name.as_ptr(), ps) };
                unsafe { (api.ov_partial_shape_free)(&mut ps) };
                api.check(res, &format!("reshape {name}"))
                    .map_err(|e| WorkerError::DeviceFailed(format!("{} {e}", device.label())))?;
            }
        }

        let inputs = ports(api, model_ptr, true)
            .map_err(|e| WorkerError::DeviceFailed(format!("{} {e}", device.label())))?;
        let outputs = ports(api, model_ptr, false)
            .map_err(|e| WorkerError::DeviceFailed(format!("{} {e}", device.label())))?;

        // NPU needs static inputs and outputs; inner nodes may still look dynamic after reshape.
        if device.kind == DeviceKind::Npu
            && inputs
                .iter()
                .chain(outputs.iter())
                .any(|p| p.shape.iter().any(|&d| d < 0))
        {
            return Err(WorkerError::DeviceFailed(format!(
                "{} NPU needs a static-shape model",
                device.label()
            )));
        }

        if let Some(cd) = cache_dir {
            let _ = std::fs::create_dir_all(cd);
        }

        let supported = self.supported_props(&device.id);
        let props: Vec<(CString, CString)> = Self::properties_for(device.kind, cache_dir)
            .into_iter()
            .filter(|(k, _)| supported.is_empty() || supported.contains(*k))
            .filter_map(|(k, v)| Some((CString::new(k).ok()?, CString::new(v).ok()?)))
            .collect();
        let native_props: Vec<OvProperty> = props
            .iter()
            .map(|(k, v)| OvProperty {
                key: k.as_ptr(),
                value: v.as_ptr(),
            })
            .collect();

        let dev_c = CString::new(device.id.as_str()).map_err(|_| {
            WorkerError::DeviceFailed(format!("{} invalid device id", device.label()))
        })?;
        let mut compiled_ptr: *mut c_void = std::ptr::null_mut();
        let st = unsafe {
            (api.ov_core_compile_model_props)(
                self.core.ptr,
                model_ptr,
                dev_c.as_ptr(),
                native_props.len(),
                native_props.as_ptr(),
                &mut compiled_ptr,
            )
        };
        if st != OV_OK {
            return Err(WorkerError::DeviceFailed(format!(
                "{} compile failed: {}",
                device.label(),
                api.last_error(st)
            )));
        }

        let mut req_ptr: *mut c_void = std::ptr::null_mut();
        if let Err(e) = api.check(
            unsafe { (api.ov_compiled_model_create_infer_request)(compiled_ptr, &mut req_ptr) },
            "infer request",
        ) {
            unsafe { (api.ov_compiled_model_free)(compiled_ptr) };
            return Err(WorkerError::DeviceFailed(format!("{} {e}", device.label())));
        }

        Ok(Box::new(OvSession {
            device: device.clone(),
            inputs,
            outputs,
            state: Mutex::new(OvSessionState {
                core: self.core.clone(),
                compiled: compiled_ptr,
                request: req_ptr,
            }),
        }))
    }
}

struct ModelGuard {
    ptr: *mut c_void,
    api: Arc<OvApi>,
}

impl Drop for ModelGuard {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.api.ov_model_free)(self.ptr) };
        }
    }
}

struct TensorGuard {
    ptr: *mut c_void,
    api: Arc<OvApi>,
}

impl Drop for TensorGuard {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (self.api.ov_tensor_free)(self.ptr) };
        }
    }
}

struct OvSessionState {
    request: *mut c_void,
    compiled: *mut c_void,
    core: Arc<OvCore>,
}

unsafe impl Send for OvSessionState {}

impl Drop for OvSessionState {
    fn drop(&mut self) {
        let api = &self.core.api;
        if !self.request.is_null() {
            unsafe { (api.ov_infer_request_free)(self.request) };
            self.request = std::ptr::null_mut();
        }
        if !self.compiled.is_null() {
            unsafe { (api.ov_compiled_model_free)(self.compiled) };
            self.compiled = std::ptr::null_mut();
        }
    }
}

struct OvSession {
    device: DeviceInfo,
    inputs: Vec<TensorSpec>,
    outputs: Vec<TensorSpec>,
    state: Mutex<OvSessionState>,
}

impl IInferenceSession for OvSession {
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
        let st = self.state.lock().expect("ov session lock");
        let api = &st.core.api;
        let mut created: Vec<TensorGuard> = Vec::with_capacity(feed.len());

        for t in feed {
            let spec = self
                .inputs
                .iter()
                .find(|s| s.name == t.name)
                .ok_or_else(|| WorkerError::DeviceFailed(format!("unknown input {}", t.name)))?;
            set_input(api, &self.device, st.request, t, spec, &mut created)?;
        }

        let infer_st = unsafe { (api.ov_infer_request_infer)(st.request) };
        if infer_st != OV_OK {
            return Err(WorkerError::DeviceFailed(format!(
                "{} infer: {}",
                self.device.label(),
                api.last_error(infer_st)
            )));
        }

        let mut result = HashMap::new();
        for (i, out_spec) in self.outputs.iter().enumerate() {
            let mut out_tensor: *mut c_void = std::ptr::null_mut();
            api.check(
                unsafe { (api.ov_infer_request_get_output_tensor_by_index)(st.request, i, &mut out_tensor) },
                "output",
            )
            .map_err(|e| WorkerError::DeviceFailed(format!("{} {e}", self.device.label())))?;
            let guard = TensorGuard {
                ptr: out_tensor,
                api: api.clone(),
            };
            let ht = read_float(api, &out_spec.name, guard.ptr)
                .map_err(|e| WorkerError::DeviceFailed(format!("{} {e}", self.device.label())))?;
            result.insert(out_spec.name.clone(), ht);
        }
        Ok(result)
    }
}

fn set_input(
    api: &Arc<OvApi>,
    device: &DeviceInfo,
    request: *mut c_void,
    t: &HostTensor,
    spec: &TensorSpec,
    created: &mut Vec<TensorGuard>,
) -> Result<(), WorkerError> {
    let et = match spec.ty {
        TensorType::Float32 => ET_F32,
        TensorType::Int64 => ET_I64,
        TensorType::Int32 => ET_I32,
        TensorType::Float16 => ET_F16,
        TensorType::Other => {
            return Err(WorkerError::DeviceFailed(format!(
                "input {} has an unsupported element type",
                t.name
            )));
        }
    };
    let mut shape = OvShape {
        rank: 0,
        dims: std::ptr::null_mut(),
    };
    api.check(
        unsafe { (api.ov_shape_create)(t.shape.len() as i64, t.shape.as_ptr(), &mut shape) },
        "shape",
    )
    .map_err(|e| WorkerError::DeviceFailed(format!("{} {e}", device.label())))?;

    let mut tensor_ptr: *mut c_void = std::ptr::null_mut();
    let cr = unsafe { (api.ov_tensor_create)(et, shape, &mut tensor_ptr) };
    unsafe { (api.ov_shape_free)(&mut shape) };
    api.check(cr, "tensor")
        .map_err(|e| WorkerError::DeviceFailed(format!("{} {e}", device.label())))?;
    created.push(TensorGuard {
        ptr: tensor_ptr,
        api: api.clone(),
    });

    let mut data_ptr: *mut c_void = std::ptr::null_mut();
    api.check(
        unsafe { (api.ov_tensor_data)(tensor_ptr, &mut data_ptr) },
        "tensor data",
    )
    .map_err(|e| WorkerError::DeviceFailed(format!("{} {e}", device.label())))?;

    let n = t
        .shape
        .iter()
        .try_fold(1usize, |a, &b| usize::try_from(b).ok().and_then(|u| a.checked_mul(u)))
        .ok_or_else(|| WorkerError::DeviceFailed(format!("invalid input shape for {}", t.name)))?;

    match spec.ty {
        TensorType::Float32 => {
            let src = need_slice(t.floats.as_deref(), n, &t.name)?;
            let dst = unsafe { std::slice::from_raw_parts_mut(data_ptr.cast::<f32>(), n) };
            dst.copy_from_slice(src);
        }
        TensorType::Float16 => {
            let src = need_slice(t.floats.as_deref(), n, &t.name)?;
            let dst = unsafe { std::slice::from_raw_parts_mut(data_ptr.cast::<u16>(), n) };
            for (d, &s) in dst.iter_mut().zip(src.iter()) {
                *d = f16::from_f32(s).to_bits();
            }
        }
        TensorType::Int64 => {
            let src = need_slice(t.longs.as_deref(), n, &t.name)?;
            let dst = unsafe { std::slice::from_raw_parts_mut(data_ptr.cast::<i64>(), n) };
            dst.copy_from_slice(src);
        }
        TensorType::Int32 => {
            let src = need_slice(t.longs.as_deref(), n, &t.name)?;
            let dst = unsafe { std::slice::from_raw_parts_mut(data_ptr.cast::<i32>(), n) };
            for (d, &s) in dst.iter_mut().zip(src.iter()) {
                *d = i32::try_from(s).map_err(|_| {
                    WorkerError::DeviceFailed(format!("input {} value out of Int32 range", t.name))
                })?;
            }
        }
        TensorType::Other => unreachable!(),
    }

    let c_name = CString::new(t.name.as_str())
        .map_err(|_| WorkerError::DeviceFailed(format!("invalid input name {}", t.name)))?;
    api.check(
        unsafe { (api.ov_infer_request_set_tensor)(request, c_name.as_ptr(), tensor_ptr) },
        &format!("set input {}", t.name),
    )
    .map_err(|e| WorkerError::DeviceFailed(format!("{} {e}", device.label())))?;
    Ok(())
}

fn need_slice<'a, T>(opt: Option<&'a [T]>, n: usize, name: &str) -> Result<&'a [T], WorkerError> {
    let s = opt.ok_or_else(|| {
        WorkerError::DeviceFailed(format!("input {name} has the wrong size or type"))
    })?;
    if s.len() < n {
        return Err(WorkerError::DeviceFailed(format!(
            "input {name} has the wrong size or type"
        )));
    }
    Ok(&s[..n])
}

fn read_float(api: &OvApi, name: &str, tensor: *mut c_void) -> Result<HostTensor, String> {
    let mut shape = OvShape {
        rank: 0,
        dims: std::ptr::null_mut(),
    };
    api.check(unsafe { (api.ov_tensor_get_shape)(tensor, &mut shape) }, "output shape")?;
    let dims = unsafe { std::slice::from_raw_parts(shape.dims, shape.rank as usize) }.to_vec();
    unsafe { (api.ov_shape_free)(&mut shape) };

    let mut et: c_int = 0;
    api.check(
        unsafe { (api.ov_tensor_get_element_type)(tensor, &mut et) },
        "output type",
    )?;
    let mut size: usize = 0;
    api.check(unsafe { (api.ov_tensor_get_size)(tensor, &mut size) }, "output size")?;
    let mut data_ptr: *mut c_void = std::ptr::null_mut();
    api.check(unsafe { (api.ov_tensor_data)(tensor, &mut data_ptr) }, "output data")?;

    let mut floats = vec![0.0f32; size];
    match et {
        ET_F32 => {
            let src = unsafe { std::slice::from_raw_parts(data_ptr.cast::<f32>(), size) };
            floats.copy_from_slice(src);
        }
        ET_F16 => {
            let src = unsafe { std::slice::from_raw_parts(data_ptr.cast::<u16>(), size) };
            for (d, &s) in floats.iter_mut().zip(src.iter()) {
                *d = f16::from_bits(s).to_f32();
            }
        }
        ET_BF16 => {
            let src = unsafe { std::slice::from_raw_parts(data_ptr.cast::<u16>(), size) };
            for (d, &s) in floats.iter_mut().zip(src.iter()) {
                *d = f32::from_bits((s as u32) << 16);
            }
        }
        _ => return Err(format!("output {name} is not floating point")),
    }
    Ok(HostTensor::f(name, floats, &dims))
}

fn ports(api: &OvApi, model: *mut c_void, input: bool) -> Result<Vec<TensorSpec>, String> {
    let mut n: usize = 0;
    let st = if input {
        unsafe { (api.ov_model_inputs_size)(model, &mut n) }
    } else {
        unsafe { (api.ov_model_outputs_size)(model, &mut n) }
    };
    api.check(st, "ports")?;
    let mut list = Vec::with_capacity(n);
    for i in 0..n {
        let mut port: *mut c_void = std::ptr::null_mut();
        let pst = if input {
            unsafe { (api.ov_model_const_input_by_index)(model, i, &mut port) }
        } else {
            unsafe { (api.ov_model_const_output_by_index)(model, i, &mut port) }
        };
        api.check(pst, "port")?;
        let mut name_ptr: *mut c_char = std::ptr::null_mut();
        let name = if unsafe { (api.ov_port_get_any_name)(port, &mut name_ptr) } == OV_OK {
            unsafe { api.take_string(name_ptr) }
        } else {
            format!("{}{i}", if input { "in" } else { "out" })
        };
        let mut et: c_int = 0;
        unsafe { (api.ov_port_get_element_type)(port, &mut et) };
        let mut shape = OvShape {
            rank: 0,
            dims: std::ptr::null_mut(),
        };
        let dims = if unsafe { (api.ov_const_port_get_shape)(port, &mut shape) } == OV_OK {
            let d = unsafe { std::slice::from_raw_parts(shape.dims, shape.rank as usize) }.to_vec();
            unsafe { (api.ov_shape_free)(&mut shape) };
            d
        } else {
            vec![-1]
        };
        unsafe { (api.ov_output_const_port_free)(port) };
        list.push(TensorSpec {
            name,
            ty: map_element_type(et),
            shape: dims,
        });
    }
    Ok(list)
}

pub fn map_element_type(et: c_int) -> TensorType {
    match et {
        ET_F32 => TensorType::Float32,
        ET_I64 => TensorType::Int64,
        ET_I32 => TensorType::Int32,
        ET_F16 => TensorType::Float16,
        _ => TensorType::Other,
    }
}

fn get_prop(api: &OvApi, core: *mut c_void, device: &str, key: &str) -> Option<String> {
    let dev_c = CString::new(device).ok()?;
    let key_c = CString::new(key).ok()?;
    let mut val_ptr: *mut c_char = std::ptr::null_mut();
    if unsafe { (api.ov_core_get_property)(core, dev_c.as_ptr(), key_c.as_ptr(), &mut val_ptr) }
        == OV_OK
    {
        Some(unsafe { api.take_string(val_ptr) })
    } else {
        None
    }
}

fn wide_null(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}
