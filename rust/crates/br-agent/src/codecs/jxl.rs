// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Dynamic FFI binding for `jxl.dll` + `jxl_threads.dll` (libjxl 0.12; `Native\Jxl.cs`).

use std::ffi::{c_float, c_int, c_longlong, c_uint, c_void};
use std::sync::OnceLock;

use libloading::Library;
use zeroize::Zeroize;

use crate::codecs::{find_agent_native_dll, load_native_library, resolve_sym};

const ENC_SUCCESS: c_int = 0;
const ENC_NEED_MORE_OUTPUT: c_int = 2;
const DEC_SUCCESS: c_int = 0;
const DEC_ERROR: c_int = 1;
const DEC_NEED_MORE_INPUT: c_int = 2;
const DEC_NEED_IMAGE_OUT_BUFFER: c_int = 5;
const DEC_BASIC_INFO: c_int = 0x40;
const DEC_FULL_IMAGE: c_int = 0x1000;
const TYPE_UINT8: c_int = 2;
const SETTING_EFFORT: c_int = 0;
const BASIC_INFO_SIZE: usize = 512;
const COLOR_ENCODING_SIZE: usize = 512;

// JxlBasicInfo offsets (x64): xsize 4, ysize 8, bits_per_sample 12,
// uses_original_profile 36, num_color_channels 52, alpha_bits 60.
const BI_XSIZE: usize = 4;
const BI_YSIZE: usize = 8;
const BI_BITS: usize = 12;
const BI_ORIGINAL_PROFILE: usize = 36;
const BI_COLOR_CHANNELS: usize = 52;
const BI_ALPHA_BITS: usize = 60;

#[repr(C)]
struct PixelFormat {
    num_channels: c_uint,
    data_type: c_int,
    endianness: c_int,
    align: usize,
}

type FnEncoderCreate = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type FnEncoderDestroy = unsafe extern "C" fn(*mut c_void);
type FnEncoderInitBasicInfo = unsafe extern "C" fn(*mut u8);
type FnEncoderSetBasicInfo = unsafe extern "C" fn(*mut c_void, *const u8) -> c_int;
type FnColorEncodingSetToSrgb = unsafe extern "C" fn(*mut u8, c_int);
type FnEncoderSetColorEncoding = unsafe extern "C" fn(*mut c_void, *const u8) -> c_int;
type FnFrameSettingsCreate = unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void;
type FnSetFrameDistance = unsafe extern "C" fn(*mut c_void, c_float) -> c_int;
type FnDistanceFromQuality = unsafe extern "C" fn(c_float) -> c_float;
type FnFrameSettingsSetOption = unsafe extern "C" fn(*mut c_void, c_int, c_longlong) -> c_int;
type FnAddImageFrame =
    unsafe extern "C" fn(*mut c_void, *const PixelFormat, *const c_void, usize) -> c_int;
type FnCloseInput = unsafe extern "C" fn(*mut c_void);
type FnProcessOutput = unsafe extern "C" fn(*mut c_void, *mut *mut u8, *mut usize) -> c_int;

type FnDecoderCreate = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type FnDecoderDestroy = unsafe extern "C" fn(*mut c_void);
type FnSubscribeEvents = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type FnDecoderSetParallelRunner =
    unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void) -> c_int;
type FnSetInput = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> c_int;
type FnDecoderCloseInput = unsafe extern "C" fn(*mut c_void);
type FnProcessInput = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnGetBasicInfo = unsafe extern "C" fn(*mut c_void, *mut u8) -> c_int;
type FnImageOutBufferSize =
    unsafe extern "C" fn(*mut c_void, *const PixelFormat, *mut usize) -> c_int;
type FnSetImageOutBuffer =
    unsafe extern "C" fn(*mut c_void, *const PixelFormat, *mut c_void, usize) -> c_int;

type FnRunnerCreate = unsafe extern "C" fn(*const c_void, usize) -> *mut c_void;
type FnRunnerDestroy = unsafe extern "C" fn(*mut c_void);

struct JxlApi {
    _lib: Library,
    _thr: Library,
    encoder_create: FnEncoderCreate,
    encoder_destroy: FnEncoderDestroy,
    encoder_init_basic_info: FnEncoderInitBasicInfo,
    encoder_set_basic_info: FnEncoderSetBasicInfo,
    color_encoding_set_to_srgb: FnColorEncodingSetToSrgb,
    encoder_set_color_encoding: FnEncoderSetColorEncoding,
    frame_settings_create: FnFrameSettingsCreate,
    set_frame_distance: FnSetFrameDistance,
    distance_from_quality: FnDistanceFromQuality,
    frame_settings_set_option: FnFrameSettingsSetOption,
    add_image_frame: FnAddImageFrame,
    close_input: FnCloseInput,
    process_output: FnProcessOutput,
    decoder_create: FnDecoderCreate,
    decoder_destroy: FnDecoderDestroy,
    subscribe_events: FnSubscribeEvents,
    decoder_set_parallel_runner: FnDecoderSetParallelRunner,
    set_input: FnSetInput,
    decoder_close_input: FnDecoderCloseInput,
    process_input: FnProcessInput,
    get_basic_info: FnGetBasicInfo,
    image_out_buffer_size: FnImageOutBufferSize,
    set_image_out_buffer: FnSetImageOutBuffer,
    runner_create: FnRunnerCreate,
    runner_destroy: FnRunnerDestroy,
    runner_fn: *const c_void,
}

unsafe impl Send for JxlApi {}
unsafe impl Sync for JxlApi {}

static API: OnceLock<Result<JxlApi, String>> = OnceLock::new();

fn load_jxl() -> Result<JxlApi, String> {
    let jxl_path = find_agent_native_dll("jxl.dll")
        .ok_or_else(|| "jxl.dll not found".to_string())?;
    let thr_path = find_agent_native_dll("jxl_threads.dll")
        .ok_or_else(|| "jxl_threads.dll not found".to_string())?;
    let lib = load_native_library(&jxl_path)
        .map_err(|e| format!("failed to load {}: {e}", jxl_path.display()))?;
    let thr = load_native_library(&thr_path)
        .map_err(|e| format!("failed to load {}: {e}", thr_path.display()))?;
    let runner_fn: *const c_void = unsafe {
        thr.get::<unsafe extern "C" fn()>(b"JxlThreadParallelRunner\0")
            .map(|s| (*s) as *const c_void)
            .map_err(|e| format!("missing export JxlThreadParallelRunner: {e}"))?
    };
    Ok(JxlApi {
        encoder_create: resolve_sym(&lib, b"JxlEncoderCreate\0")?,
        encoder_destroy: resolve_sym(&lib, b"JxlEncoderDestroy\0")?,
        encoder_init_basic_info: resolve_sym(&lib, b"JxlEncoderInitBasicInfo\0")?,
        encoder_set_basic_info: resolve_sym(&lib, b"JxlEncoderSetBasicInfo\0")?,
        color_encoding_set_to_srgb: resolve_sym(&lib, b"JxlColorEncodingSetToSRGB\0")?,
        encoder_set_color_encoding: resolve_sym(&lib, b"JxlEncoderSetColorEncoding\0")?,
        frame_settings_create: resolve_sym(&lib, b"JxlEncoderFrameSettingsCreate\0")?,
        set_frame_distance: resolve_sym(&lib, b"JxlEncoderSetFrameDistance\0")?,
        distance_from_quality: resolve_sym(&lib, b"JxlEncoderDistanceFromQuality\0")?,
        frame_settings_set_option: resolve_sym(&lib, b"JxlEncoderFrameSettingsSetOption\0")?,
        add_image_frame: resolve_sym(&lib, b"JxlEncoderAddImageFrame\0")?,
        close_input: resolve_sym(&lib, b"JxlEncoderCloseInput\0")?,
        process_output: resolve_sym(&lib, b"JxlEncoderProcessOutput\0")?,
        decoder_create: resolve_sym(&lib, b"JxlDecoderCreate\0")?,
        decoder_destroy: resolve_sym(&lib, b"JxlDecoderDestroy\0")?,
        subscribe_events: resolve_sym(&lib, b"JxlDecoderSubscribeEvents\0")?,
        decoder_set_parallel_runner: resolve_sym(&lib, b"JxlDecoderSetParallelRunner\0")?,
        set_input: resolve_sym(&lib, b"JxlDecoderSetInput\0")?,
        decoder_close_input: resolve_sym(&lib, b"JxlDecoderCloseInput\0")?,
        process_input: resolve_sym(&lib, b"JxlDecoderProcessInput\0")?,
        get_basic_info: resolve_sym(&lib, b"JxlDecoderGetBasicInfo\0")?,
        image_out_buffer_size: resolve_sym(&lib, b"JxlDecoderImageOutBufferSize\0")?,
        set_image_out_buffer: resolve_sym(&lib, b"JxlDecoderSetImageOutBuffer\0")?,
        runner_create: resolve_sym(&thr, b"JxlThreadParallelRunnerCreate\0")?,
        runner_destroy: resolve_sym(&thr, b"JxlThreadParallelRunnerDestroy\0")?,
        runner_fn,
        _lib: lib,
        _thr: thr,
    })
}

fn api() -> Result<&'static JxlApi, String> {
    match API.get_or_init(load_jxl) {
        Ok(a) => Ok(a),
        Err(e) => Err(e.clone()),
    }
}

struct EncGuard<'a>(&'a JxlApi, *mut c_void);
impl Drop for EncGuard<'_> {
    fn drop(&mut self) {
        if !self.1.is_null() {
            unsafe { (self.0.encoder_destroy)(self.1) };
        }
    }
}

struct DecGuard<'a>(&'a JxlApi, *mut c_void);
impl Drop for DecGuard<'_> {
    fn drop(&mut self) {
        if !self.1.is_null() {
            unsafe { (self.0.decoder_destroy)(self.1) };
        }
    }
}

struct RunnerGuard<'a>(&'a JxlApi, *mut c_void);
impl Drop for RunnerGuard<'_> {
    fn drop(&mut self) {
        if !self.1.is_null() {
            unsafe { (self.0.runner_destroy)(self.1) };
        }
    }
}

fn check(status: c_int, what: &str) -> Result<(), String> {
    if status != ENC_SUCCESS {
        Err(format!("JPEG XL encoder: {what} failed."))
    } else {
        Ok(())
    }
}

/// Dynamic binding to `jxl.dll` + `jxl_threads.dll`.
pub struct Jxl;

impl Jxl {
    pub fn try_load() -> bool {
        api().is_ok()
    }

    pub fn load_error() -> Option<String> {
        API.get().and_then(|r| r.as_ref().err().cloned())
    }

    /// Lossy JPEG XL from BGRA (alpha dropped). `quality` 30..95 maps through
    /// `JxlEncoderDistanceFromQuality`. Runs on the calling thread (no parallel runner) so the
    /// keyframe CPU budget measures all of its work (A6).
    ///
    /// # Safety
    /// `bgra` must point to at least `height * stride` readable bytes with `width * 4 <= stride`.
    pub unsafe fn encode(
        bgra: *const u8,
        width: i32,
        height: i32,
        stride: i32,
        quality: i32,
        effort: i32,
    ) -> Result<Vec<u8>, String> {
        let a = api().map_err(|_| "JPEG XL codec unavailable.".to_string())?;
        if width <= 0 || height <= 0 || stride < width * 4 || bgra.is_null() {
            return Err("Invalid JPEG XL input dimensions.".to_string());
        }
        let w = width as usize;
        let h = height as usize;
        let rgb_len = w
            .checked_mul(h)
            .and_then(|n| n.checked_mul(3))
            .ok_or_else(|| "JPEG XL size overflow.".to_string())?;
        let mut rgb = vec![0u8; rgb_len];
        for y in 0..h {
            let s = unsafe { bgra.add(y * stride as usize) };
            let d = &mut rgb[y * w * 3..(y + 1) * w * 3];
            for x in 0..w {
                unsafe {
                    d[x * 3] = *s.add(x * 4 + 2);
                    d[x * 3 + 1] = *s.add(x * 4 + 1);
                    d[x * 3 + 2] = *s.add(x * 4);
                }
            }
        }

        let res = (|| -> Result<Vec<u8>, String> {
            let enc = EncGuard(a, unsafe { (a.encoder_create)(std::ptr::null()) });
            if enc.1.is_null() {
                return Err("JxlEncoderCreate failed.".to_string());
            }
            let mut basic = [0u8; BASIC_INFO_SIZE];
            unsafe {
                (a.encoder_init_basic_info)(basic.as_mut_ptr());
                *basic.as_mut_ptr().add(BI_XSIZE).cast::<c_uint>() = width as c_uint;
                *basic.as_mut_ptr().add(BI_YSIZE).cast::<c_uint>() = height as c_uint;
                *basic.as_mut_ptr().add(BI_BITS).cast::<c_uint>() = 8;
                *basic.as_mut_ptr().add(BI_ORIGINAL_PROFILE).cast::<c_int>() = 0;
                *basic.as_mut_ptr().add(BI_COLOR_CHANNELS).cast::<c_uint>() = 3;
                *basic.as_mut_ptr().add(BI_ALPHA_BITS).cast::<c_uint>() = 0;
            }
            check(
                unsafe { (a.encoder_set_basic_info)(enc.1, basic.as_ptr()) },
                "basic info",
            )?;

            let mut color = [0u8; COLOR_ENCODING_SIZE];
            unsafe { (a.color_encoding_set_to_srgb)(color.as_mut_ptr(), 0) };
            check(
                unsafe { (a.encoder_set_color_encoding)(enc.1, color.as_ptr()) },
                "color",
            )?;

            let settings = unsafe { (a.frame_settings_create)(enc.1, std::ptr::null()) };
            let dist = unsafe { (a.distance_from_quality)(quality as c_float) };
            check(unsafe { (a.set_frame_distance)(settings, dist) }, "distance")?;
            check(
                unsafe {
                    (a.frame_settings_set_option)(
                        settings,
                        SETTING_EFFORT,
                        effort.clamp(1, 9) as c_longlong,
                    )
                },
                "effort",
            )?;

            let fmt = PixelFormat {
                num_channels: 3,
                data_type: TYPE_UINT8,
                endianness: 0,
                align: 0,
            };
            check(
                unsafe { (a.add_image_frame)(settings, &fmt, rgb.as_ptr().cast(), rgb.len()) },
                "frame",
            )?;
            unsafe { (a.close_input)(enc.1) };

            let mut output = vec![0u8; (64 * 1024).max(rgb_len / 8)];
            let mut used = 0usize;
            loop {
                let mut next = unsafe { output.as_mut_ptr().add(used) };
                let mut avail = output.len() - used;
                let status = unsafe { (a.process_output)(enc.1, &mut next, &mut avail) };
                used = (next as usize) - (output.as_ptr() as usize);
                if status == ENC_SUCCESS {
                    break;
                }
                if status != ENC_NEED_MORE_OUTPUT {
                    output.zeroize();
                    return Err("JPEG XL encode failed.".to_string());
                }
                let mut bigger = vec![0u8; output.len() * 2];
                bigger[..used].copy_from_slice(&output[..used]);
                output.zeroize();
                output = bigger;
            }
            let result = output[..used].to_vec();
            output.zeroize();
            Ok(result)
        })();
        rgb.zeroize();
        res
    }

    /// Decodes JPEG XL to BGRA (stride `width * 4`). Caller should zeroize pixels after use.
    pub fn decode(data: &[u8]) -> Result<(Vec<u8>, i32, i32), String> {
        let a = api().map_err(|_| "JPEG XL codec unavailable.".to_string())?;
        let dec = DecGuard(a, unsafe { (a.decoder_create)(std::ptr::null()) });
        if dec.1.is_null() {
            return Err("JxlDecoderCreate failed.".to_string());
        }
        let threads = (std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(2)
            / 2)
            .clamp(1, 4);
        let runner = RunnerGuard(a, unsafe { (a.runner_create)(std::ptr::null(), threads) });
        if !runner.1.is_null() {
            unsafe {
                (a.decoder_set_parallel_runner)(dec.1, a.runner_fn, runner.1);
            }
        }
        if unsafe { (a.subscribe_events)(dec.1, DEC_BASIC_INFO | DEC_FULL_IMAGE) } != DEC_SUCCESS {
            return Err("JPEG XL decoder setup failed.".to_string());
        }
        if unsafe { (a.set_input)(dec.1, data.as_ptr(), data.len()) } != DEC_SUCCESS {
            return Err("JPEG XL input rejected.".to_string());
        }
        unsafe { (a.decoder_close_input)(dec.1) };

        let fmt = PixelFormat {
            num_channels: 4,
            data_type: TYPE_UINT8,
            endianness: 0,
            align: 0,
        };
        let mut basic = [0u8; BASIC_INFO_SIZE];
        let mut width = 0i32;
        let mut height = 0i32;
        let mut pixels: Option<Vec<u8>> = None;

        let res = (|| -> Result<(Vec<u8>, i32, i32), String> {
            loop {
                let st = unsafe { (a.process_input)(dec.1) };
                if st == DEC_ERROR || st == DEC_NEED_MORE_INPUT {
                    return Err("JPEG XL decode failed.".to_string());
                }
                if st == DEC_BASIC_INFO {
                    basic.fill(0);
                    if unsafe { (a.get_basic_info)(dec.1, basic.as_mut_ptr()) } != DEC_SUCCESS {
                        return Err("JPEG XL header unreadable.".to_string());
                    }
                    width = unsafe { *basic.as_ptr().add(BI_XSIZE).cast::<c_uint>() } as i32;
                    height = unsafe { *basic.as_ptr().add(BI_YSIZE).cast::<c_uint>() } as i32;
                    if width <= 0
                        || height <= 0
                        || (width as i64) * (height as i64) > 64 * 1024 * 1024
                    {
                        return Err("JPEG XL size.".to_string());
                    }
                } else if st == DEC_NEED_IMAGE_OUT_BUFFER {
                    let mut size = 0usize;
                    if unsafe { (a.image_out_buffer_size)(dec.1, &fmt, &mut size) } != DEC_SUCCESS {
                        return Err("JPEG XL buffer size.".to_string());
                    }
                    if (size as i64) != (width as i64) * (height as i64) * 4 {
                        return Err("JPEG XL buffer size.".to_string());
                    }
                    let mut buf = vec![0u8; size];
                    if unsafe {
                        (a.set_image_out_buffer)(dec.1, &fmt, buf.as_mut_ptr().cast(), size)
                    } != DEC_SUCCESS
                    {
                        buf.zeroize();
                        return Err("JPEG XL output rejected.".to_string());
                    }
                    pixels = Some(buf);
                } else if st == DEC_FULL_IMAGE || st == DEC_SUCCESS {
                    let mut px = pixels
                        .take()
                        .ok_or_else(|| "JPEG XL has no image.".to_string())?;
                    for chunk in px.chunks_exact_mut(4) {
                        chunk.swap(0, 2);
                    }
                    return Ok((px, width, height));
                }
            }
        })();
        if let Some(mut px) = pixels {
            px.zeroize();
        }
        res
    }
}
