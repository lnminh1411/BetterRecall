// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Minimal dynamic FFI binding for `libSkiaSharp.dll` (SkiaSharp 3.119.1 C API):
//! lossy WebP encoding, 4:2:0 JPEG encoding (alpha ignored), Skia decoding to BGRA8888,
//! and pixmap scaling (Linear / Mipmap Linear).

use std::ffi::{c_float, c_int, c_void};
use std::sync::OnceLock;

use libloading::Library;
use zeroize::Zeroize;

use crate::codecs::{find_skia_dll, load_native_library, resolve_sym};

const SK_COLOR_TYPE_BGRA8888: c_int = 6;
const SK_ALPHA_TYPE_OPAQUE: c_int = 1;
const SK_ALPHA_TYPE_PREMUL: c_int = 2;

const SK_JPEG_DOWNSAMPLE_420: c_int = 0;
const SK_JPEG_ALPHA_IGNORE: c_int = 0;
const SK_WEBP_COMPRESSION_LOSSY: c_int = 0;

const SK_FILTER_LINEAR: c_int = 1;
const SK_MIPMAP_NONE: c_int = 0;
const SK_MIPMAP_LINEAR: c_int = 2;

const SK_CODEC_RESULT_SUCCESS: c_int = 0;
const SK_ZERO_INITIALIZED_NO: c_int = 1;

#[repr(C)]
struct SkImageInfo {
    colorspace: *mut c_void,
    width: i32,
    height: i32,
    color_type: c_int,
    alpha_type: c_int,
}

#[repr(C)]
struct SkJpegEncoderOptions {
    quality: i32,
    downsample: c_int,
    alpha_option: c_int,
    xmp_metadata: *const c_void,
    icc_profile: *const c_void,
    icc_profile_description: *const c_void,
}

#[repr(C)]
struct SkWebpEncoderOptions {
    compression: c_int,
    quality: c_float,
    icc_profile: *const c_void,
    icc_profile_description: *const c_void,
}

#[repr(C)]
struct SkSamplingOptions {
    max_aniso: i32,
    use_cubic: u8,
    cubic: [f32; 2],
    filter: c_int,
    mipmap: c_int,
}

#[repr(C)]
struct SkCodecOptions {
    zero_initialized: c_int,
    subset: *const c_void,
    frame_index: i32,
    prior_frame: i32,
}

type FnPixmapNewWithParams =
    unsafe extern "C" fn(*const SkImageInfo, *const c_void, usize) -> *mut c_void;
type FnPixmapDestructor = unsafe extern "C" fn(*mut c_void);
type FnPixmapScalePixels =
    unsafe extern "C" fn(*const c_void, *const c_void, *const SkSamplingOptions) -> bool;
type FnDynamicWStreamNew = unsafe extern "C" fn() -> *mut c_void;
type FnDynamicWStreamDetachAsData = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnDynamicWStreamDestroy = unsafe extern "C" fn(*mut c_void);
type FnJpegEncode =
    unsafe extern "C" fn(*mut c_void, *const c_void, *const SkJpegEncoderOptions) -> bool;
type FnWebpEncode =
    unsafe extern "C" fn(*mut c_void, *const c_void, *const SkWebpEncoderOptions) -> bool;
type FnDataNewWithCopy = unsafe extern "C" fn(*const c_void, usize) -> *mut c_void;
type FnDataGetData = unsafe extern "C" fn(*const c_void) -> *const u8;
type FnDataGetSize = unsafe extern "C" fn(*const c_void) -> usize;
type FnDataUnref = unsafe extern "C" fn(*mut c_void);
type FnCodecNewFromData = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnCodecDestroy = unsafe extern "C" fn(*mut c_void);
type FnCodecGetInfo = unsafe extern "C" fn(*mut c_void, *mut SkImageInfo);
type FnCodecGetPixels = unsafe extern "C" fn(
    *mut c_void,
    *const SkImageInfo,
    *mut c_void,
    usize,
    *const SkCodecOptions,
) -> c_int;

struct SkiaApi {
    _lib: Library,
    pixmap_new_with_params: FnPixmapNewWithParams,
    pixmap_destructor: FnPixmapDestructor,
    pixmap_scale_pixels: FnPixmapScalePixels,
    wstream_new: FnDynamicWStreamNew,
    wstream_detach_as_data: FnDynamicWStreamDetachAsData,
    wstream_destroy: FnDynamicWStreamDestroy,
    jpeg_encode: FnJpegEncode,
    webp_encode: FnWebpEncode,
    data_new_with_copy: FnDataNewWithCopy,
    data_get_data: FnDataGetData,
    data_get_size: FnDataGetSize,
    data_unref: FnDataUnref,
    codec_new_from_data: FnCodecNewFromData,
    codec_destroy: FnCodecDestroy,
    codec_get_info: FnCodecGetInfo,
    codec_get_pixels: FnCodecGetPixels,
}

unsafe impl Send for SkiaApi {}
unsafe impl Sync for SkiaApi {}

static API: OnceLock<Result<SkiaApi, String>> = OnceLock::new();

fn load_skia() -> Result<SkiaApi, String> {
    let path = find_skia_dll().ok_or_else(|| "libSkiaSharp.dll not found".to_string())?;
    let lib = load_native_library(&path)
        .map_err(|e| format!("failed to load {}: {e}", path.display()))?;
    Ok(SkiaApi {
        pixmap_new_with_params: resolve_sym(&lib, b"sk_pixmap_new_with_params\0")?,
        pixmap_destructor: resolve_sym(&lib, b"sk_pixmap_destructor\0")?,
        pixmap_scale_pixels: resolve_sym(&lib, b"sk_pixmap_scale_pixels\0")?,
        wstream_new: resolve_sym(&lib, b"sk_dynamicmemorywstream_new\0")?,
        wstream_detach_as_data: resolve_sym(&lib, b"sk_dynamicmemorywstream_detach_as_data\0")?,
        wstream_destroy: resolve_sym(&lib, b"sk_dynamicmemorywstream_destroy\0")?,
        jpeg_encode: resolve_sym(&lib, b"sk_jpegencoder_encode\0")?,
        webp_encode: resolve_sym(&lib, b"sk_webpencoder_encode\0")?,
        data_new_with_copy: resolve_sym(&lib, b"sk_data_new_with_copy\0")?,
        data_get_data: resolve_sym(&lib, b"sk_data_get_data\0")?,
        data_get_size: resolve_sym(&lib, b"sk_data_get_size\0")?,
        data_unref: resolve_sym(&lib, b"sk_data_unref\0")?,
        codec_new_from_data: resolve_sym(&lib, b"sk_codec_new_from_data\0")?,
        codec_destroy: resolve_sym(&lib, b"sk_codec_destroy\0")?,
        codec_get_info: resolve_sym(&lib, b"sk_codec_get_info\0")?,
        codec_get_pixels: resolve_sym(&lib, b"sk_codec_get_pixels\0")?,
        _lib: lib,
    })
}

fn api() -> Result<&'static SkiaApi, String> {
    match API.get_or_init(load_skia) {
        Ok(a) => Ok(a),
        Err(e) => Err(e.clone()),
    }
}

struct PixmapGuard<'a>(&'a SkiaApi, *mut c_void);
impl Drop for PixmapGuard<'_> {
    fn drop(&mut self) {
        if !self.1.is_null() {
            unsafe { (self.0.pixmap_destructor)(self.1) };
        }
    }
}

struct WStreamGuard<'a>(&'a SkiaApi, *mut c_void);
impl Drop for WStreamGuard<'_> {
    fn drop(&mut self) {
        if !self.1.is_null() {
            unsafe { (self.0.wstream_destroy)(self.1) };
        }
    }
}

struct DataGuard<'a>(&'a SkiaApi, *mut c_void);
impl Drop for DataGuard<'_> {
    fn drop(&mut self) {
        if !self.1.is_null() {
            unsafe { (self.0.data_unref)(self.1) };
        }
    }
}

struct CodecGuard<'a>(&'a SkiaApi, *mut c_void);
impl Drop for CodecGuard<'_> {
    fn drop(&mut self) {
        if !self.1.is_null() {
            unsafe { (self.0.codec_destroy)(self.1) };
        }
    }
}

/// Dynamic binding to `libSkiaSharp.dll`.
pub struct Skia;

impl Skia {
    pub fn try_load() -> bool {
        api().is_ok()
    }

    pub fn load_error() -> Option<String> {
        API.get().and_then(|r| r.as_ref().err().cloned())
    }

    /// Lossy WebP from opaque BGRA8888.
    ///
    /// # Safety
    /// `bgra` must point to at least `height * stride` readable bytes with `width * 4 <= stride`.
    pub unsafe fn encode_webp(
        bgra: *const u8,
        width: i32,
        height: i32,
        stride: i32,
        quality: i32,
    ) -> Result<Vec<u8>, String> {
        let a = api()?;
        if width <= 0 || height <= 0 || stride < width * 4 || bgra.is_null() {
            return Err("Invalid WebP input dimensions.".to_string());
        }
        let info = SkImageInfo {
            colorspace: std::ptr::null_mut(),
            width,
            height,
            color_type: SK_COLOR_TYPE_BGRA8888,
            alpha_type: SK_ALPHA_TYPE_OPAQUE,
        };
        let pm = PixmapGuard(a, unsafe {
            (a.pixmap_new_with_params)(&info, bgra.cast(), stride as usize)
        });
        if pm.1.is_null() {
            return Err("Image encode failed.".to_string());
        }
        let ws = WStreamGuard(a, unsafe { (a.wstream_new)() });
        if ws.1.is_null() {
            return Err("Image encode failed.".to_string());
        }
        let opts = SkWebpEncoderOptions {
            compression: SK_WEBP_COMPRESSION_LOSSY,
            quality: quality as f32,
            icc_profile: std::ptr::null(),
            icc_profile_description: std::ptr::null(),
        };
        if !unsafe { (a.webp_encode)(ws.1, pm.1, &opts) } {
            return Err("Image encode failed.".to_string());
        }
        detach_data(a, ws.1)
    }

    /// JPEG (4:2:0, alpha ignored) from opaque BGRA8888.
    ///
    /// # Safety
    /// `bgra` must point to at least `height * stride` readable bytes with `width * 4 <= stride`.
    pub unsafe fn encode_jpeg(
        bgra: *const u8,
        width: i32,
        height: i32,
        stride: i32,
        quality: i32,
    ) -> Result<Vec<u8>, String> {
        let a = api()?;
        if width <= 0 || height <= 0 || stride < width * 4 || bgra.is_null() {
            return Err("Invalid JPEG input dimensions.".to_string());
        }
        let info = SkImageInfo {
            colorspace: std::ptr::null_mut(),
            width,
            height,
            color_type: SK_COLOR_TYPE_BGRA8888,
            alpha_type: SK_ALPHA_TYPE_OPAQUE,
        };
        let pm = PixmapGuard(a, unsafe {
            (a.pixmap_new_with_params)(&info, bgra.cast(), stride as usize)
        });
        if pm.1.is_null() {
            return Err("Image encode failed.".to_string());
        }
        let ws = WStreamGuard(a, unsafe { (a.wstream_new)() });
        if ws.1.is_null() {
            return Err("Image encode failed.".to_string());
        }
        let opts = SkJpegEncoderOptions {
            quality,
            downsample: SK_JPEG_DOWNSAMPLE_420,
            alpha_option: SK_JPEG_ALPHA_IGNORE,
            xmp_metadata: std::ptr::null(),
            icc_profile: std::ptr::null(),
            icc_profile_description: std::ptr::null(),
        };
        if !unsafe { (a.jpeg_encode)(ws.1, pm.1, &opts) } {
            return Err("Image encode failed.".to_string());
        }
        detach_data(a, ws.1)
    }

    /// Decodes WebP / JPEG / PNG (any format Skia supports) to tightly packed BGRA8888 (`stride = w * 4`).
    pub fn decode_bgra(encoded: &[u8]) -> Result<(Vec<u8>, i32, i32), String> {
        let a = api()?;
        if encoded.is_empty() {
            return Err("Keyframe decode failed.".to_string());
        }
        let data = DataGuard(a, unsafe {
            (a.data_new_with_copy)(encoded.as_ptr().cast(), encoded.len())
        });
        if data.1.is_null() {
            return Err("Keyframe decode failed.".to_string());
        }
        let codec = CodecGuard(a, unsafe { (a.codec_new_from_data)(data.1) });
        if codec.1.is_null() {
            return Err("Keyframe decode failed.".to_string());
        }
        let mut info = SkImageInfo {
            colorspace: std::ptr::null_mut(),
            width: 0,
            height: 0,
            color_type: 0,
            alpha_type: 0,
        };
        unsafe { (a.codec_get_info)(codec.1, &mut info) };
        let w = info.width;
        let h = info.height;
        if w <= 0 || h <= 0 || (w as i64) * (h as i64) > 64 * 1024 * 1024 {
            return Err("Keyframe decode failed.".to_string());
        }
        let dst_info = SkImageInfo {
            colorspace: std::ptr::null_mut(),
            width: w,
            height: h,
            color_type: SK_COLOR_TYPE_BGRA8888,
            alpha_type: if info.alpha_type == SK_ALPHA_TYPE_OPAQUE {
                SK_ALPHA_TYPE_OPAQUE
            } else {
                SK_ALPHA_TYPE_PREMUL
            },
        };
        let row_bytes = (w as usize) * 4;
        let mut pixels = vec![0u8; row_bytes * (h as usize)];
        let opts = SkCodecOptions {
            zero_initialized: SK_ZERO_INITIALIZED_NO,
            subset: std::ptr::null(),
            frame_index: 0,
            prior_frame: -1,
        };
        let rc = unsafe {
            (a.codec_get_pixels)(
                codec.1,
                &dst_info,
                pixels.as_mut_ptr().cast(),
                row_bytes,
                &opts,
            )
        };
        if rc != SK_CODEC_RESULT_SUCCESS {
            pixels.zeroize();
            return Err("Keyframe decode failed.".to_string());
        }
        Ok((pixels, w, h))
    }

    /// Scales opaque BGRA8888 pixels with Skia's linear filter (and optional linear mipmaps).
    ///
    /// # Safety
    /// `src` must point to at least `src_h * src_stride` readable bytes with `src_w * 4 <= src_stride`.
    pub unsafe fn scale_bgra(
        src: *const u8,
        src_w: i32,
        src_h: i32,
        src_stride: i32,
        dst_w: i32,
        dst_h: i32,
        mipmap_linear: bool,
    ) -> Result<Vec<u8>, String> {
        let a = api()?;
        if src_w <= 0 || src_h <= 0 || dst_w <= 0 || dst_h <= 0 || src.is_null() {
            return Err("Resize failed.".to_string());
        }
        let src_info = SkImageInfo {
            colorspace: std::ptr::null_mut(),
            width: src_w,
            height: src_h,
            color_type: SK_COLOR_TYPE_BGRA8888,
            alpha_type: SK_ALPHA_TYPE_OPAQUE,
        };
        let dst_info = SkImageInfo {
            colorspace: std::ptr::null_mut(),
            width: dst_w,
            height: dst_h,
            color_type: SK_COLOR_TYPE_BGRA8888,
            alpha_type: SK_ALPHA_TYPE_OPAQUE,
        };
        let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
        let src_pm = PixmapGuard(a, unsafe {
            (a.pixmap_new_with_params)(&src_info, src.cast(), src_stride as usize)
        });
        let dst_pm = PixmapGuard(a, unsafe {
            (a.pixmap_new_with_params)(&dst_info, dst.as_mut_ptr().cast(), (dst_w as usize) * 4)
        });
        if src_pm.1.is_null() || dst_pm.1.is_null() {
            dst.zeroize();
            return Err("Resize failed.".to_string());
        }
        let sampling = SkSamplingOptions {
            max_aniso: 0,
            use_cubic: 0,
            cubic: [0.0, 0.0],
            filter: SK_FILTER_LINEAR,
            mipmap: if mipmap_linear {
                SK_MIPMAP_LINEAR
            } else {
                SK_MIPMAP_NONE
            },
        };
        if !unsafe { (a.pixmap_scale_pixels)(src_pm.1, dst_pm.1, &sampling) } {
            dst.zeroize();
            return Err("Resize failed.".to_string());
        }
        Ok(dst)
    }
}

fn detach_data(a: &SkiaApi, ws: *mut c_void) -> Result<Vec<u8>, String> {
    let data = DataGuard(a, unsafe { (a.wstream_detach_as_data)(ws) });
    if data.1.is_null() {
        return Err("Image encode failed.".to_string());
    }
    let ptr = unsafe { (a.data_get_data)(data.1) };
    let size = unsafe { (a.data_get_size)(data.1) };
    if ptr.is_null() || size == 0 {
        return Err("Image encode failed.".to_string());
    }
    Ok(unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec())
}
