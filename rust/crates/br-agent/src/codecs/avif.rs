// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Dynamic FFI binding for `avif.dll` (libavif 1.4: aom encoder, dav1d decoder, libyuv; `Native\Avif.cs`).

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

use libloading::Library;
use zeroize::Zeroize;

use crate::codecs::{find_agent_native_dll, load_native_library, resolve_sym};

const RESULT_OK: c_int = 0;
const PIXEL_YUV420: c_int = 3;
const RGB_FORMAT_BGRA: c_int = 4;
const RGB_IMAGE_SIZE: usize = 64;

// avifRGBImage offsets (libavif 1.4 x64): width 0, height 4, depth 8, format 12,
// ignoreAlpha 28, maxThreads 40, pixels 48, rowBytes 56.
const RGB_DEPTH: usize = 8;
const RGB_FORMAT: usize = 12;
const RGB_IGNORE_ALPHA: usize = 28;
const RGB_MAX_THREADS: usize = 40;
const RGB_PIXELS: usize = 48;
const RGB_ROW_BYTES: usize = 56;

// avifEncoder offsets: codecChoice 0, maxThreads 4, speed 8, ..., quality 32.
const ENC_MAX_THREADS: usize = 4;
const ENC_SPEED: usize = 8;
const ENC_QUALITY: usize = 32;

// avifDecoder offsets: codecChoice 0, maxThreads 4.
const DEC_MAX_THREADS: usize = 4;

#[repr(C)]
struct RwData {
    data: *mut u8,
    size: usize,
}

type FnImageCreate = unsafe extern "C" fn(c_uint, c_uint, c_uint, c_int) -> *mut c_void;
type FnImageCreateEmpty = unsafe extern "C" fn() -> *mut c_void;
type FnImageDestroy = unsafe extern "C" fn(*mut c_void);
type FnRgbSetDefaults = unsafe extern "C" fn(*mut u8, *mut c_void);
type FnRgbToYuv = unsafe extern "C" fn(*mut c_void, *mut u8) -> c_int;
type FnYuvToRgb = unsafe extern "C" fn(*mut c_void, *mut u8) -> c_int;
type FnEncoderCreate = unsafe extern "C" fn() -> *mut c_void;
type FnEncoderDestroy = unsafe extern "C" fn(*mut c_void);
type FnEncoderWrite = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut RwData) -> c_int;
type FnRwDataFree = unsafe extern "C" fn(*mut RwData);
type FnSetCodecOption = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type FnDecoderCreate = unsafe extern "C" fn() -> *mut c_void;
type FnDecoderDestroy = unsafe extern "C" fn(*mut c_void);
type FnDecoderReadMemory =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *const u8, usize) -> c_int;

struct AvifApi {
    _lib: Library,
    image_create: FnImageCreate,
    image_create_empty: FnImageCreateEmpty,
    image_destroy: FnImageDestroy,
    rgb_set_defaults: FnRgbSetDefaults,
    rgb_to_yuv: FnRgbToYuv,
    yuv_to_rgb: FnYuvToRgb,
    encoder_create: FnEncoderCreate,
    encoder_destroy: FnEncoderDestroy,
    encoder_write: FnEncoderWrite,
    rw_data_free: FnRwDataFree,
    set_codec_option: FnSetCodecOption,
    decoder_create: FnDecoderCreate,
    decoder_destroy: FnDecoderDestroy,
    decoder_read_memory: FnDecoderReadMemory,
}

unsafe impl Send for AvifApi {}
unsafe impl Sync for AvifApi {}

static API: OnceLock<Result<AvifApi, String>> = OnceLock::new();

fn load_avif() -> Result<AvifApi, String> {
    let path = find_agent_native_dll("avif.dll")
        .ok_or_else(|| "avif.dll not found".to_string())?;
    let lib = load_native_library(&path)
        .map_err(|e| format!("failed to load {}: {e}", path.display()))?;
    Ok(AvifApi {
        image_create: resolve_sym(&lib, b"avifImageCreate\0")?,
        image_create_empty: resolve_sym(&lib, b"avifImageCreateEmpty\0")?,
        image_destroy: resolve_sym(&lib, b"avifImageDestroy\0")?,
        rgb_set_defaults: resolve_sym(&lib, b"avifRGBImageSetDefaults\0")?,
        rgb_to_yuv: resolve_sym(&lib, b"avifImageRGBToYUV\0")?,
        yuv_to_rgb: resolve_sym(&lib, b"avifImageYUVToRGB\0")?,
        encoder_create: resolve_sym(&lib, b"avifEncoderCreate\0")?,
        encoder_destroy: resolve_sym(&lib, b"avifEncoderDestroy\0")?,
        encoder_write: resolve_sym(&lib, b"avifEncoderWrite\0")?,
        rw_data_free: resolve_sym(&lib, b"avifRWDataFree\0")?,
        set_codec_option: resolve_sym(&lib, b"avifEncoderSetCodecSpecificOption\0")?,
        decoder_create: resolve_sym(&lib, b"avifDecoderCreate\0")?,
        decoder_destroy: resolve_sym(&lib, b"avifDecoderDestroy\0")?,
        decoder_read_memory: resolve_sym(&lib, b"avifDecoderReadMemory\0")?,
        _lib: lib,
    })
}

fn api() -> Result<&'static AvifApi, String> {
    match API.get_or_init(load_avif) {
        Ok(a) => Ok(a),
        Err(e) => Err(e.clone()),
    }
}

fn default_threads() -> c_int {
    let n = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(2);
    ((n / 2).clamp(1, 4)) as c_int
}

struct ImageGuard<'a>(&'a AvifApi, *mut c_void);
impl Drop for ImageGuard<'_> {
    fn drop(&mut self) {
        if !self.1.is_null() {
            unsafe { (self.0.image_destroy)(self.1) };
        }
    }
}

struct EncoderGuard<'a>(&'a AvifApi, *mut c_void);
impl Drop for EncoderGuard<'_> {
    fn drop(&mut self) {
        if !self.1.is_null() {
            unsafe { (self.0.encoder_destroy)(self.1) };
        }
    }
}

struct DecoderGuard<'a>(&'a AvifApi, *mut c_void);
impl Drop for DecoderGuard<'_> {
    fn drop(&mut self) {
        if !self.1.is_null() {
            unsafe { (self.0.decoder_destroy)(self.1) };
        }
    }
}

struct RwDataGuard<'a>(&'a AvifApi, RwData);
impl Drop for RwDataGuard<'_> {
    fn drop(&mut self) {
        if !self.1.data.is_null() {
            unsafe { (self.0.rw_data_free)(&mut self.1) };
        }
    }
}

/// Parameters for [`Avif::encode`].
#[derive(Debug, Clone, Copy)]
pub struct AvifEncodeParams<'a> {
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub quality: i32,
    pub speed: i32,
    pub threads: i32,
    pub tune_content: Option<&'a str>,
}

/// Dynamic binding to `avif.dll`.
pub struct Avif;

impl Avif {
    pub fn try_load() -> bool {
        api().is_ok()
    }

    pub fn load_error() -> Option<String> {
        API.get().and_then(|r| r.as_ref().err().cloned())
    }

    /// 8-bit 4:2:0 AVIF from BGRA (alpha ignored). `quality` is libavif's 0..100; `speed` 0..10.
    /// `threads`: 0 = default (half logical CPUs, at most 4); 1 keeps work on calling thread.
    /// `tune_content`: libaom `tune-content` (`None` leaves libaom's own screen-content detection).
    ///
    /// # Safety
    /// `bgra` must point to at least `height * stride` readable bytes with `width * 4 <= stride`.
    pub unsafe fn encode(bgra: *const u8, params: AvifEncodeParams<'_>) -> Result<Vec<u8>, String> {
        let AvifEncodeParams {
            width,
            height,
            stride,
            quality,
            speed,
            threads,
            tune_content,
        } = params;
        let a = api().map_err(|_| "AVIF codec unavailable.".to_string())?;
        if width <= 0 || height <= 0 || stride < width * 4 || bgra.is_null() {
            return Err("Invalid AVIF input dimensions.".to_string());
        }
        let t = if threads > 0 { threads } else { default_threads() };
        let image = ImageGuard(
            a,
            unsafe { (a.image_create)(width as c_uint, height as c_uint, 8, PIXEL_YUV420) },
        );
        if image.1.is_null() {
            return Err("avifImageCreate failed.".to_string());
        }

        let mut rgb = [0u8; RGB_IMAGE_SIZE];
        unsafe {
            (a.rgb_set_defaults)(rgb.as_mut_ptr(), image.1);
            *rgb.as_mut_ptr().add(RGB_FORMAT).cast::<c_int>() = RGB_FORMAT_BGRA;
            *rgb.as_mut_ptr().add(RGB_IGNORE_ALPHA).cast::<c_int>() = 1;
            *rgb.as_mut_ptr().add(RGB_MAX_THREADS).cast::<c_int>() = t;
            *rgb.as_mut_ptr().add(RGB_PIXELS).cast::<*const u8>() = bgra;
            *rgb.as_mut_ptr().add(RGB_ROW_BYTES).cast::<c_uint>() = stride as c_uint;
            if (a.rgb_to_yuv)(image.1, rgb.as_mut_ptr()) != RESULT_OK {
                return Err("AVIF colour conversion failed.".to_string());
            }
        }

        let enc = EncoderGuard(a, unsafe { (a.encoder_create)() });
        if enc.1.is_null() {
            return Err("avifEncoderCreate failed.".to_string());
        }
        unsafe {
            *enc.1.cast::<u8>().add(ENC_MAX_THREADS).cast::<c_int>() = t;
            *enc.1.cast::<u8>().add(ENC_SPEED).cast::<c_int>() = speed.clamp(0, 10);
            *enc.1.cast::<u8>().add(ENC_QUALITY).cast::<c_int>() = quality.clamp(0, 100);
        }
        if let Some(tc) = tune_content {
            set_option(a, enc.1, "tune-content", tc)?;
        }

        let mut output = RwDataGuard(
            a,
            RwData {
                data: std::ptr::null_mut(),
                size: 0,
            },
        );
        let status = unsafe { (a.encoder_write)(enc.1, image.1, &mut output.1) };
        if status != RESULT_OK || output.1.data.is_null() {
            return Err("AVIF encode failed.".to_string());
        }
        let slice = unsafe { std::slice::from_raw_parts(output.1.data, output.1.size) };
        Ok(slice.to_vec())
    }

    /// Decodes AVIF to BGRA (stride `width * 4`). Caller should zeroize the returned buffer after use.
    pub fn decode(data: &[u8]) -> Result<(Vec<u8>, i32, i32), String> {
        let a = api().map_err(|_| "AVIF codec unavailable.".to_string())?;
        let dec = DecoderGuard(a, unsafe { (a.decoder_create)() });
        let image = ImageGuard(a, unsafe { (a.image_create_empty)() });
        if dec.1.is_null() || image.1.is_null() {
            return Err("avifDecoderCreate failed.".to_string());
        }
        let t = default_threads();
        unsafe {
            *dec.1.cast::<u8>().add(DEC_MAX_THREADS).cast::<c_int>() = t;
            if (a.decoder_read_memory)(dec.1, image.1, data.as_ptr(), data.len()) != RESULT_OK {
                return Err("AVIF decode failed.".to_string());
            }
        }
        let width = unsafe { *image.1.cast::<c_uint>() } as i32;
        let height = unsafe { *image.1.cast::<u8>().add(4).cast::<c_uint>() } as i32;
        if width <= 0 || height <= 0 || (width as i64) * (height as i64) > 64 * 1024 * 1024 {
            return Err("AVIF size.".to_string());
        }
        let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
        let mut rgb = [0u8; RGB_IMAGE_SIZE];
        let status = unsafe {
            (a.rgb_set_defaults)(rgb.as_mut_ptr(), image.1);
            *rgb.as_mut_ptr().add(RGB_DEPTH).cast::<c_uint>() = 8;
            *rgb.as_mut_ptr().add(RGB_FORMAT).cast::<c_int>() = RGB_FORMAT_BGRA;
            *rgb.as_mut_ptr().add(RGB_MAX_THREADS).cast::<c_int>() = t;
            *rgb.as_mut_ptr().add(RGB_PIXELS).cast::<*mut u8>() = pixels.as_mut_ptr();
            *rgb.as_mut_ptr().add(RGB_ROW_BYTES).cast::<c_uint>() = (width * 4) as c_uint;
            (a.yuv_to_rgb)(image.1, rgb.as_mut_ptr())
        };
        if status != RESULT_OK {
            pixels.zeroize();
            return Err("AVIF colour conversion failed.".to_string());
        }
        Ok((pixels, width, height))
    }
}

fn set_option(a: &AvifApi, enc: *mut c_void, key: &str, value: &str) -> Result<(), String> {
    let mut k = [0u8; 64];
    let mut v = [0u8; 64];
    if key.len() >= k.len() || value.len() >= v.len() {
        return Err(format!("AVIF option {key}={value} too long."));
    }
    k[..key.len()].copy_from_slice(key.as_bytes());
    v[..value.len()].copy_from_slice(value.as_bytes());
    let rc = unsafe { (a.set_codec_option)(enc, k.as_ptr().cast(), v.as_ptr().cast()) };
    if rc != RESULT_OK {
        return Err(format!("AVIF option {key}={value} rejected."));
    }
    Ok(())
}
