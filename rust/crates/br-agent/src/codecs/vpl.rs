// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Dynamic FFI bindings for `libvpl.dll` (oneVPL dispatcher, Intel, MIT) (`Native\Vpl.cs`).

use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use libloading::Library;
use libloading::os::windows::Library as WinLibrary;
use windows::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_SYSTEM32;

use crate::codecs::{find_agent_native_dll, load_native_library, resolve_sym};
use crate::util::AgentLog;

/// `mfxVariant` passed by value to `MFXSetConfigFilterProperty` (16 bytes on x64).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MfxVariant {
    pub version: u16,
    pub ty: i32,
    pub data: u64,
}

impl MfxVariant {
    pub const fn u32_val(v: u32) -> Self {
        Self {
            version: 0x0100,
            ty: 5,
            data: v as u64,
        }
    }
}

type FnMfxLoad = unsafe extern "C" fn() -> *mut c_void;
type FnMfxUnload = unsafe extern "C" fn(*mut c_void);
type FnMfxCreateConfig = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnMfxSetConfigFilterProperty =
    unsafe extern "C" fn(*mut c_void, *const u8, MfxVariant) -> i32;
type FnMfxEnumImplementations =
    unsafe extern "C" fn(*mut c_void, u32, u32, *mut *mut c_void) -> i32;
type FnMfxDispReleaseImplDescription = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
type FnMfxCreateSession = unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void) -> i32;
type FnMfxClose = unsafe extern "C" fn(*mut c_void) -> i32;
type FnMfxVideoCoreSetHandle = unsafe extern "C" fn(*mut c_void, i32, *mut c_void) -> i32;
type FnMfxVideoCoreSyncOperation = unsafe extern "C" fn(*mut c_void, *mut c_void, u32) -> i32;
type FnMfxVideoEncodeQuery = unsafe extern "C" fn(*mut c_void, *mut u8, *mut u8) -> i32;
type FnMfxVideoEncodeInit = unsafe extern "C" fn(*mut c_void, *mut u8) -> i32;
type FnMfxVideoEncodeClose = unsafe extern "C" fn(*mut c_void) -> i32;
type FnMfxVideoEncodeGetVideoParam = unsafe extern "C" fn(*mut c_void, *mut u8) -> i32;
type FnMfxVideoEncodeEncodeFrameAsync =
    unsafe extern "C" fn(*mut c_void, *mut u8, *mut u8, *mut u8, *mut *mut c_void) -> i32;
type FnMfxMemoryGetSurfaceForEncode = unsafe extern "C" fn(*mut c_void, *mut *mut u8) -> i32;

pub struct VplApi {
    _lib: Library,
    pub loaded_from: String,
    pub mfx_load: FnMfxLoad,
    pub mfx_unload: FnMfxUnload,
    pub mfx_create_config: FnMfxCreateConfig,
    pub mfx_set_config_filter_property: FnMfxSetConfigFilterProperty,
    pub mfx_enum_implementations: FnMfxEnumImplementations,
    pub mfx_disp_release_impl_description: FnMfxDispReleaseImplDescription,
    pub mfx_create_session: FnMfxCreateSession,
    pub mfx_close: FnMfxClose,
    pub mfx_video_core_set_handle: FnMfxVideoCoreSetHandle,
    pub mfx_video_core_sync_operation: FnMfxVideoCoreSyncOperation,
    pub mfx_video_encode_query: FnMfxVideoEncodeQuery,
    pub mfx_video_encode_init: FnMfxVideoEncodeInit,
    pub mfx_video_encode_close: FnMfxVideoEncodeClose,
    pub mfx_video_encode_get_video_param: FnMfxVideoEncodeGetVideoParam,
    pub mfx_video_encode_encode_frame_async: FnMfxVideoEncodeEncodeFrameAsync,
    pub mfx_memory_get_surface_for_encode: FnMfxMemoryGetSurfaceForEncode,
}

unsafe impl Send for VplApi {}
unsafe impl Sync for VplApi {}

static VPL_STATE: OnceLock<Result<VplApi, String>> = OnceLock::new();
static LOAD_GATE: Mutex<()> = Mutex::new(());

/// oneVPL dispatcher constants, struct offsets (x64 oneVPL 2.x), and helpers (`Native.Vpl`).
pub struct Vpl;

impl Vpl {
    pub const ERR_NONE: i32 = 0;
    pub const ERR_UNSUPPORTED: i32 = -3;
    pub const ERR_NOT_FOUND: i32 = -9;
    pub const ERR_MORE_DATA: i32 = -10;
    pub const ERR_DEVICE_LOST: i32 = -13;
    pub const WRN_IN_EXECUTION: i32 = 1;
    pub const WRN_DEVICE_BUSY: i32 = 2;

    pub const IMPL_TYPE_HARDWARE: u32 = 2;
    pub const ACCEL_D3D11: u32 = 0x0300;
    pub const IMPL_CAPS_DEVICE_ID_EXTENDED: u32 = 4;
    pub const HANDLE_D3D11_DEVICE: i32 = 3;
    pub const CODEC_AV1: u32 =
        (b'A' as u32) | ((b'V' as u32) << 8) | ((b'1' as u32) << 16) | ((b' ' as u32) << 24);
    pub const FOURCC_NV12: u32 =
        (b'N' as u32) | ((b'V' as u32) << 8) | ((b'1' as u32) << 16) | ((b'2' as u32) << 24);
    pub const FRAME_TYPE_I: u16 = 0x1;
    pub const FRAME_TYPE_REF: u16 = 0x40;
    pub const FRAME_TYPE_IDR: u16 = 0x80;
    pub const RESOURCE_DX11_TEXTURE: i32 = 5;
    pub const MAP_WRITE: u32 = 2;

    // mfxVideoParam (208 bytes) offsets; mfxInfoMFX starts at 16, its mfxFrameInfo at 48.
    pub const PAR_SIZE: usize = 208;
    pub const P_ASYNC_DEPTH: usize = 14;
    pub const P_LOW_POWER: usize = 44;
    pub const P_BRC_MULTIPLIER: usize = 46;
    pub const P_BIT_DEPTH_LUMA: usize = 66;
    pub const P_BIT_DEPTH_CHROMA: usize = 68;
    pub const P_FOURCC: usize = 80;
    pub const P_WIDTH: usize = 84;
    pub const P_HEIGHT: usize = 86;
    pub const P_CROP_X: usize = 88;
    pub const P_CROP_Y: usize = 90;
    pub const P_CROP_W: usize = 92;
    pub const P_CROP_H: usize = 94;
    pub const P_FRAME_RATE_N: usize = 96;
    pub const P_FRAME_RATE_D: usize = 100;
    pub const P_ASPECT_W: usize = 106;
    pub const P_ASPECT_H: usize = 108;
    pub const P_PIC_STRUCT: usize = 110;
    pub const P_CHROMA_FORMAT: usize = 112;
    pub const P_CODEC_ID: usize = 116;
    pub const P_CODEC_PROFILE: usize = 120;
    pub const P_CODEC_LEVEL: usize = 122;
    pub const P_TARGET_USAGE: usize = 126;
    pub const P_GOP_PIC_SIZE: usize = 128;
    pub const P_GOP_REF_DIST: usize = 130;
    pub const P_GOP_OPT_FLAG: usize = 132;
    pub const P_IDR_INTERVAL: usize = 134;
    pub const P_RATE_CONTROL: usize = 136;
    pub const P_BUFFER_SIZE_KB: usize = 140;
    pub const P_TARGET_KBPS: usize = 142;
    pub const P_MAX_KBPS: usize = 144;
    pub const P_NUM_REF_FRAME: usize = 148;
    pub const P_IO_PATTERN: usize = 186;

    // mfxBitstream (72 bytes)
    pub const BS_SIZE: usize = 72;
    pub const BS_DATA: usize = 40;
    pub const BS_DATA_OFFSET: usize = 48;
    pub const BS_DATA_LENGTH: usize = 52;
    pub const BS_MAX_LENGTH: usize = 56;
    pub const BS_FRAME_TYPE: usize = 62;

    // mfxEncodeCtrl (56 bytes)
    pub const CTRL_SIZE: usize = 56;
    pub const CTRL_FRAME_TYPE: usize = 32;

    // mfxFrameSurface1
    pub const S_PITCH_HIGH: usize = 118;
    pub const S_PITCH_LOW: usize = 134;
    pub const S_Y: usize = 136;
    pub const S_UV: usize = 144;

    // mfxFrameSurfaceInterface
    pub const FI_RELEASE: usize = 24;
    pub const FI_MAP: usize = 40;
    pub const FI_UNMAP: usize = 48;
    pub const FI_GET_NATIVE_HANDLE: usize = 56;

    /// Loads `libvpl.dll` from the bundled native resolver first, then `System32`.
    pub fn api() -> Result<&'static VplApi, String> {
        let _g = LOAD_GATE.lock().unwrap_or_else(|e| e.into_inner());
        VPL_STATE.get_or_init(load_vpl).as_ref().map_err(Clone::clone)
    }

    pub fn try_load() -> bool {
        Self::api().is_ok()
    }

    pub fn load_error() -> Option<String> {
        Self::api().err()
    }

    /// Sets a oneVPL config filter property by ASCII name.
    ///
    /// # Safety
    /// `config` must be a valid `mfxConfig` handle returned by `MFXCreateConfig`.
    pub unsafe fn set_filter(
        api: &VplApi,
        config: *mut c_void,
        name: &str,
        value: MfxVariant,
    ) -> i32 {
        let mut bytes = Vec::with_capacity(name.len() + 1);
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        unsafe { (api.mfx_set_config_filter_property)(config, bytes.as_ptr(), value) }
    }

    /// Index of the hardware implementation matching `luid` (or first Intel/any implementation).
    /// Returns `(index_or_neg1, count)`.
    ///
    /// # Safety
    /// `loader` must be a valid `mfxLoader` handle returned by `MFXLoad`.
    pub unsafe fn find_implementation(
        api: &VplApi,
        loader: *mut c_void,
        luid: i64,
    ) -> (i32, i32) {
        let mut count = 0i32;
        let mut first_intel = -1i32;
        let want = luid.to_le_bytes();
        for i in 0..16u32 {
            let mut desc: *mut c_void = std::ptr::null_mut();
            let st = unsafe {
                (api.mfx_enum_implementations)(
                    loader,
                    i,
                    Self::IMPL_CAPS_DEVICE_ID_EXTENDED,
                    &mut desc,
                )
            };
            if st == Self::ERR_NOT_FOUND {
                break;
            }
            count += 1;
            if st < 0 || desc.is_null() {
                if first_intel < 0 {
                    first_intel = i as i32;
                }
                continue;
            }
            let matched = {
                let bytes = unsafe { std::slice::from_raw_parts(desc.cast::<u8>(), 64) };
                let vendor = u16::from_le_bytes([bytes[2], bytes[3]]);
                if vendor == 0x8086 && first_intel < 0 {
                    first_intel = i as i32;
                }
                bytes.windows(8).any(|w| w == want)
            };
            unsafe {
                let _ = (api.mfx_disp_release_impl_description)(loader, desc);
            }
            if matched {
                return (i as i32, count);
            }
        }
        (first_intel, count)
    }

    /// Writes a little-endian `u16` at byte offset `off`.
    ///
    /// # Safety
    /// `p.add(off)` must be valid for 2-byte write.
    pub unsafe fn w16(p: *mut u8, off: usize, v: u16) {
        unsafe {
            std::ptr::write_unaligned(p.add(off).cast::<u16>(), v);
        }
    }

    /// Writes a little-endian `u32` at byte offset `off`.
    ///
    /// # Safety
    /// `p.add(off)` must be valid for 4-byte write.
    pub unsafe fn w32(p: *mut u8, off: usize, v: u32) {
        unsafe {
            std::ptr::write_unaligned(p.add(off).cast::<u32>(), v);
        }
    }

    /// Reads a little-endian `u16` at byte offset `off`.
    ///
    /// # Safety
    /// `p.add(off)` must be valid for 2-byte read.
    pub unsafe fn r16(p: *const u8, off: usize) -> u16 {
        unsafe { std::ptr::read_unaligned(p.add(off).cast::<u16>()) }
    }

    /// Reads a little-endian `u32` at byte offset `off`.
    ///
    /// # Safety
    /// `p.add(off)` must be valid for 4-byte read.
    pub unsafe fn r32(p: *const u8, off: usize) -> u32 {
        unsafe { std::ptr::read_unaligned(p.add(off).cast::<u32>()) }
    }

    /// Invokes `mfxFrameSurfaceInterface::Map(surface, flags)`.
    ///
    /// # Safety
    /// `surface` must be a valid `mfxFrameSurface1*` with a valid `FrameInterface` vtable.
    pub unsafe fn surface_call_map(surface: *mut u8, flags: u32) -> i32 {
        unsafe {
            let iface = *surface.cast::<*const u8>();
            let fn_ptr: unsafe extern "C" fn(*mut u8, u32) -> i32 =
                std::mem::transmute(*(iface.add(Self::FI_MAP).cast::<*const c_void>()));
            fn_ptr(surface, flags)
        }
    }

    /// Invokes a 1-arg `mfxFrameSurfaceInterface` method at byte offset `slot` (`FI_RELEASE`, `FI_UNMAP`).
    ///
    /// # Safety
    /// `surface` must be a valid `mfxFrameSurface1*` with a valid `FrameInterface` vtable.
    pub unsafe fn surface_call(surface: *mut u8, slot: usize) -> i32 {
        unsafe {
            let iface = *surface.cast::<*const u8>();
            let fn_ptr: unsafe extern "C" fn(*mut u8) -> i32 =
                std::mem::transmute(*(iface.add(slot).cast::<*const c_void>()));
            fn_ptr(surface)
        }
    }

    /// Invokes `mfxFrameSurfaceInterface::GetNativeHandle(surface, &mut res, &mut ty)`.
    ///
    /// # Safety
    /// `surface` must be a valid `mfxFrameSurface1*` with a valid `FrameInterface` vtable.
    pub unsafe fn surface_get_native_handle(
        surface: *mut u8,
        res: &mut *mut c_void,
        ty: &mut i32,
    ) -> i32 {
        unsafe {
            let iface = *surface.cast::<*const u8>();
            let fn_ptr: unsafe extern "C" fn(*mut u8, *mut *mut c_void, *mut i32) -> i32 =
                std::mem::transmute(*(iface.add(Self::FI_GET_NATIVE_HANDLE).cast::<*const c_void>()));
            fn_ptr(surface, res, ty)
        }
    }
}

fn load_vpl() -> Result<VplApi, String> {
    let (lib, loaded_from) = if let Some(path) = find_agent_native_dll("libvpl.dll") {
        let lib = load_native_library(&path)
            .map_err(|e| format!("libvpl.dll load failed ({}): {e}", path.display()))?;
        (lib, path.display().to_string())
    } else {
        // System32 fallback matching Native\Vpl.cs
        // SAFETY: Loading system DLL `libvpl.dll` restricted to System32.
        let win_lib = unsafe {
            WinLibrary::load_with_flags("libvpl.dll", LOAD_LIBRARY_SEARCH_SYSTEM32.0)
        }
        .map_err(|_| "libvpl.dll not found".to_string())?;
        (win_lib.into(), "system32".to_string())
    };

    let api = VplApi {
        mfx_load: resolve_sym(&lib, b"MFXLoad\0")?,
        mfx_unload: resolve_sym(&lib, b"MFXUnload\0")?,
        mfx_create_config: resolve_sym(&lib, b"MFXCreateConfig\0")?,
        mfx_set_config_filter_property: resolve_sym(&lib, b"MFXSetConfigFilterProperty\0")?,
        mfx_enum_implementations: resolve_sym(&lib, b"MFXEnumImplementations\0")?,
        mfx_disp_release_impl_description: resolve_sym(&lib, b"MFXDispReleaseImplDescription\0")?,
        mfx_create_session: resolve_sym(&lib, b"MFXCreateSession\0")?,
        mfx_close: resolve_sym(&lib, b"MFXClose\0")?,
        mfx_video_core_set_handle: resolve_sym(&lib, b"MFXVideoCORE_SetHandle\0")?,
        mfx_video_core_sync_operation: resolve_sym(&lib, b"MFXVideoCORE_SyncOperation\0")?,
        mfx_video_encode_query: resolve_sym(&lib, b"MFXVideoENCODE_Query\0")?,
        mfx_video_encode_init: resolve_sym(&lib, b"MFXVideoENCODE_Init\0")?,
        mfx_video_encode_close: resolve_sym(&lib, b"MFXVideoENCODE_Close\0")?,
        mfx_video_encode_get_video_param: resolve_sym(&lib, b"MFXVideoENCODE_GetVideoParam\0")?,
        mfx_video_encode_encode_frame_async: resolve_sym(&lib, b"MFXVideoENCODE_EncodeFrameAsync\0")?,
        mfx_memory_get_surface_for_encode: resolve_sym(&lib, b"MFXMemory_GetSurfaceForEncode\0")?,
        loaded_from: loaded_from.clone(),
        _lib: lib,
    };
    AgentLog::info(&format!("oneVPL dispatcher loaded ({loaded_from})"));
    Ok(api)
}
