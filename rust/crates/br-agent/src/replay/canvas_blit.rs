// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Pure CPU NV12 compositing helpers (`Replay\CanvasBlit.cs`).

use crate::capture::output_capture::DirtyMapper;
use crate::native::Rect;

/// Mutable NV12 surface view (Y plane, then interleaved UV plane at `pitch * plane_height`).
pub struct Nv12Mut {
    pub ptr: *mut u8,
    pub pitch: usize,
    pub plane_height: usize,
}

/// Read-only NV12 surface view.
pub struct Nv12Ref {
    pub ptr: *const u8,
    pub pitch: usize,
    pub plane_height: usize,
}

/// Pure CPU helpers on NV12 (Y plane, then interleaved UV plane at `pitch * plane_height`).
pub struct CanvasBlit;

impl CanvasBlit {
    /// Copies rect `r` (source-local, even-aligned) of an NV12 image into an NV12 canvas at
    /// offset `(dx, dy)` (even): Y rows `r.top..r.bottom`, UV rows `r.top/2..r.bottom/2`,
    /// bytes `r.left..r.right` in both planes.
    ///
    /// # Safety
    /// `src` and `dst` must point to valid NV12 buffers covering `r` and `(dx, dy) + r`.
    pub unsafe fn copy_rect(src: Nv12Ref, dst: Nv12Mut, r: Rect, dx: i32, dy: i32) {
        if DirtyMapper::is_empty(r) {
            return;
        }
        let w = (r.right - r.left) as usize;
        let h = (r.bottom - r.top) as usize;
        if w == 0 || h == 0 {
            return;
        }
        let src_left = r.left as usize;
        let src_top = r.top as usize;
        let dst_left = (dx + r.left) as usize;
        let dst_top = (dy + r.top) as usize;

        for y in 0..h {
            unsafe {
                let s_line = src.ptr.add((src_top + y) * src.pitch + src_left);
                let d_line = dst.ptr.add((dst_top + y) * dst.pitch + dst_left);
                std::ptr::copy_nonoverlapping(s_line, d_line, w);
            }
        }

        let src_uv_base = unsafe { src.ptr.add(src.pitch * src.plane_height) };
        let dst_uv_base = unsafe { dst.ptr.add(dst.pitch * dst.plane_height) };
        let uv_rows = h / 2;
        let src_uv_top = src_top / 2;
        let dst_uv_top = dst_top / 2;
        for y in 0..uv_rows {
            unsafe {
                let s_line = src_uv_base.add((src_uv_top + y) * src.pitch + src_left);
                let d_line = dst_uv_base.add((dst_uv_top + y) * dst.pitch + dst_left);
                std::ptr::copy_nonoverlapping(s_line, d_line, w);
            }
        }
    }

    /// Paints rect `r` (canvas coordinates, even-aligned) black: Y = 16, UV = 128.
    ///
    /// # Safety
    /// `nv12` must point to a valid NV12 buffer covering `r`.
    pub unsafe fn fill_black(nv12: Nv12Mut, r: Rect) {
        if DirtyMapper::is_empty(r) {
            return;
        }
        let w = (r.right - r.left) as usize;
        let h = (r.bottom - r.top) as usize;
        if w == 0 || h == 0 {
            return;
        }
        let left = r.left as usize;
        let top = r.top as usize;
        let bottom = r.bottom as usize;

        for y in top..bottom {
            unsafe {
                let line = nv12.ptr.add(y * nv12.pitch + left);
                std::ptr::write_bytes(line, 16, w);
            }
        }

        let uv_base = unsafe { nv12.ptr.add(nv12.pitch * nv12.plane_height) };
        let uv_top = top / 2;
        let uv_bottom = bottom / 2;
        for y in uv_top..uv_bottom {
            unsafe {
                let line = uv_base.add(y * nv12.pitch + left);
                std::ptr::write_bytes(line, 128, w);
            }
        }
    }

    /// Whole canvas black: all Y = 16, all UV = 128.
    ///
    /// # Safety
    /// `nv12` must point to a valid NV12 buffer of at least `width` by `plane_height`.
    pub unsafe fn clear(nv12: Nv12Mut, width: usize) {
        if width == 0 || nv12.plane_height == 0 {
            return;
        }
        for y in 0..nv12.plane_height {
            unsafe {
                let line = nv12.ptr.add(y * nv12.pitch);
                std::ptr::write_bytes(line, 16, width);
            }
        }
        let uv_base = unsafe { nv12.ptr.add(nv12.pitch * nv12.plane_height) };
        let uv_height = nv12.plane_height / 2;
        for y in 0..uv_height {
            unsafe {
                let line = uv_base.add(y * nv12.pitch);
                std::ptr::write_bytes(line, 128, width);
            }
        }
    }

    /// Benchmark content: 16x16 tiles covering the top-left quarter of the canvas that scroll
    /// one tile per frame so encoders see steady motion.
    ///
    /// # Safety
    /// `nv12` must point to a valid NV12 buffer of at least `width` by `height`.
    pub unsafe fn paint_motion(nv12: Nv12Mut, width: usize, height: usize, frame: i64) {
        let w = (width / 2) & !15;
        let h = (height / 2) & !15;
        if w == 0 || h == 0 {
            return;
        }
        let uv_base = unsafe { nv12.ptr.add(nv12.pitch * nv12.plane_height) };
        for ty in 0..(h / 16) {
            for tx in 0..(w / 16) {
                let v = hash(tx as u32, (ty as i64 + frame) as u32);
                let y_val = (16 + v % 220) as u8;
                let u_val = (16 + (v >> 8) % 225) as u8;
                let v_val = (16 + (v >> 16) % 225) as u8;
                for r in 0..16 {
                    unsafe {
                        let line = nv12.ptr.add((ty * 16 + r) * nv12.pitch + tx * 16);
                        std::ptr::write_bytes(line, y_val, 16);
                    }
                }
                for r in 0..8 {
                    unsafe {
                        let line = uv_base.add((ty * 8 + r) * nv12.pitch + tx * 16);
                        let slice = std::slice::from_raw_parts_mut(line, 16);
                        for pair in slice.as_chunks_mut::<2>().0 {
                            pair[0] = u_val;
                            pair[1] = v_val;
                        }
                    }
                }
            }
        }
    }
}

const fn hash(x: u32, y: u32) -> u32 {
    let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h
}
