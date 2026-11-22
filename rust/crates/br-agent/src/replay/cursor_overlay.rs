// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Mouse pointer probe and NV12 cursor painter (`Replay\CursorOverlay.cs`).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetObjectW, HGDIOBJ, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON, ICONINFO,
};

use crate::native::Rect;
use crate::replay::canvas_blit::Nv12Mut;

/// Mouse pointer state in desktop (physical) pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorState {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub handle: isize,
}

impl CursorState {
    pub const HIDDEN: Self = Self {
        visible: false,
        x: 0,
        y: 0,
        handle: 0,
    };
}

/// Cursor image pre-converted to BT.709 limited-range YUV with straight alpha; `invert` marks XOR pixels.
#[derive(Debug, Clone)]
pub struct CursorShape {
    pub width: i32,
    pub height: i32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub a: Vec<u8>,
    pub invert: Vec<bool>,
}

impl CursorShape {
    /// Builds a shape from two BGRA renders of the cursor: over black and over white.
    pub fn from_renders(
        on_black: &[u8],
        on_white: &[u8],
        w: i32,
        h: i32,
        hot_x: i32,
        hot_y: i32,
    ) -> Self {
        let n = (w * h) as usize;
        let mut y_buf = vec![0u8; n];
        let mut u_buf = vec![0u8; n];
        let mut v_buf = vec![0u8; n];
        let mut a_buf = vec![0u8; n];
        let mut invert = vec![false; n];
        for i in 0..n {
            let bb = on_black[i * 4] as i32;
            let bg = on_black[i * 4 + 1] as i32;
            let br = on_black[i * 4 + 2] as i32;
            let wb = on_white[i * 4] as i32;
            let wg = on_white[i * 4 + 1] as i32;
            let wr = on_white[i * 4 + 2] as i32;
            let diff = ((wb - bb) + (wg - bg) + (wr - br)) / 3;
            if diff < -32 {
                invert[i] = true;
                a_buf[i] = 255;
                continue;
            }
            let a = (255 - diff).clamp(0, 255);
            a_buf[i] = a as u8;
            if a == 0 {
                continue;
            }
            let af = a as f64;
            let r = (br as f64 * 255.0 / af).min(255.0);
            let g = (bg as f64 * 255.0 / af).min(255.0);
            let b = (bb as f64 * 255.0 / af).min(255.0);
            y_buf[i] = (16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0)
                .round()
                .clamp(16.0, 235.0) as u8;
            u_buf[i] = (128.0 + (-0.1146 * r - 0.3854 * g + 0.5 * b) * 224.0 / 255.0)
                .round()
                .clamp(16.0, 240.0) as u8;
            v_buf[i] = (128.0 + (0.5 * r - 0.4542 * g - 0.0458 * b) * 224.0 / 255.0)
                .round()
                .clamp(16.0, 240.0) as u8;
        }
        Self {
            width: w,
            height: h,
            hot_x,
            hot_y,
            y: y_buf,
            u: u_buf,
            v: v_buf,
            a: a_buf,
            invert,
        }
    }
}

/// Reads the system pointer through GDI and caches rendered shapes by cursor handle.
pub struct CursorProbe;

impl CursorProbe {
    fn cache() -> &'static Mutex<HashMap<isize, Option<Arc<CursorShape>>>> {
        static CACHE: OnceLock<Mutex<HashMap<isize, Option<Arc<CursorShape>>>>> = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub fn current() -> CursorState {
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetCursorInfo(&mut ci) }.is_err()
            || (ci.flags.0 & CURSOR_SHOWING.0) == 0
            || ci.hCursor.is_invalid()
            || ci.hCursor.0.is_null()
        {
            return CursorState::HIDDEN;
        }
        CursorState {
            visible: true,
            x: ci.ptScreenPos.x,
            y: ci.ptScreenPos.y,
            handle: ci.hCursor.0 as isize,
        }
    }

    pub fn shape(handle: isize) -> Option<Arc<CursorShape>> {
        let mut map = Self::cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = map.get(&handle) {
            return s.clone();
        }
        let rendered = render_cursor(handle).map(Arc::new);
        if map.len() > 64 {
            map.clear();
        }
        map.insert(handle, rendered.clone());
        rendered
    }
}

fn render_cursor(h_cursor: isize) -> Option<CursorShape> {
    let icon = HICON(h_cursor as *mut c_void);
    let mut ii = ICONINFO::default();
    if unsafe { GetIconInfo(icon, &mut ii) }.is_err() {
        return None;
    }
    let res = (|| {
        let basis = if !ii.hbmColor.is_invalid() && !ii.hbmColor.0.is_null() {
            HGDIOBJ(ii.hbmColor.0)
        } else {
            HGDIOBJ(ii.hbmMask.0)
        };
        let mut bm = BITMAP::default();
        let got = unsafe {
            GetObjectW(
                basis,
                std::mem::size_of::<BITMAP>() as i32,
                Some((&raw mut bm).cast::<c_void>()),
            )
        };
        if got == 0 {
            return None;
        }
        let w = bm.bmWidth;
        let h = if !ii.hbmColor.is_invalid() && !ii.hbmColor.0.is_null() {
            bm.bmHeight
        } else {
            bm.bmHeight / 2
        };
        if w <= 0 || h <= 0 || w > 256 || h > 256 {
            return None;
        }
        let black = render_over(icon, w, h, 0x00)?;
        let white = render_over(icon, w, h, 0xFF)?;
        Some(CursorShape::from_renders(
            &black,
            &white,
            w,
            h,
            ii.xHotspot as i32,
            ii.yHotspot as i32,
        ))
    })();
    unsafe {
        if !ii.hbmColor.is_invalid() && !ii.hbmColor.0.is_null() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
        }
        if !ii.hbmMask.is_invalid() && !ii.hbmMask.0.is_null() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
        }
    }
    res
}

fn render_over(h_cursor: HICON, w: i32, h: i32, fill: u8) -> Option<Vec<u8>> {
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.is_invalid() || dc.0.is_null() {
        return None;
    }
    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let dib = match unsafe { CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) } {
        Ok(b) if !b.is_invalid() && !bits.is_null() => b,
        _ => {
            unsafe {
                let _ = DeleteDC(dc);
            }
            return None;
        }
    };
    let byte_len = (w as usize) * (h as usize) * 4;
    unsafe {
        let old = SelectObject(dc, HGDIOBJ(dib.0));
        std::ptr::write_bytes(bits.cast::<u8>(), fill, byte_len);
        let ok = DrawIconEx(dc, 0, 0, h_cursor, w, h, 0, None, DI_NORMAL).is_ok();
        let _ = GdiFlush();
        SelectObject(dc, old);
        let res = if ok {
            Some(std::slice::from_raw_parts(bits.cast::<u8>(), byte_len).to_vec())
        } else {
            None
        };
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(dc);
        res
    }
}

/// Parameters for [`CursorPainter::draw`].
#[derive(Debug, Clone, Copy)]
pub struct CursorDrawParams {
    pub cx: f64,
    pub cy: f64,
    pub sx: f64,
    pub sy: f64,
    pub clip: Rect,
}

/// Draws the pointer into an NV12 canvas and restores pixels underneath before the next update.
#[derive(Default)]
pub struct CursorPainter {
    patch: Rect,
    saved: Vec<u8>,
    has_patch: bool,
}

impl CursorPainter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn has_patch(&self) -> bool {
        self.has_patch
    }

    /// Restores the canvas under the last drawn pointer.
    ///
    /// # Safety
    /// `nv12` must point to a valid NV12 buffer matching the previous `draw` call.
    pub unsafe fn restore(&mut self, nv12: Nv12Mut) {
        if !self.has_patch {
            return;
        }
        unsafe {
            copy_patch(nv12, self.patch, &mut self.saved, false);
        }
        self.has_patch = false;
    }

    /// Draws `s` scaled by `(sx, sy)` with its hotspot at canvas point `(cx, cy)`, clipped to `clip`.
    ///
    /// # Safety
    /// `nv12` must point to a valid NV12 buffer covering `params.clip`.
    pub unsafe fn draw(&mut self, nv12: Nv12Mut, s: &CursorShape, params: CursorDrawParams) {
        let CursorDrawParams {
            cx,
            cy,
            sx,
            sy,
            clip,
        } = params;
        let dw = ((s.width as f64 * sx).round() as i32).max(1);
        let dh = ((s.height as f64 * sy).round() as i32).max(1);
        let left = (cx - s.hot_x as f64 * sx).round() as i32;
        let top = (cy - s.hot_y as f64 * sy).round() as i32;
        let l = clip.left.max(left) & !1;
        let t = clip.top.max(top) & !1;
        let r = clip.right.min((left + dw + 1) & !1);
        let b = clip.bottom.min((top + dh + 1) & !1);
        if r <= l || b <= t {
            return;
        }
        self.patch = Rect::new(l, t, r, b);
        let need = ((r - l) as usize) * ((b - t) as usize) * 3 / 2;
        if self.saved.len() < need {
            self.saved.resize(need, 0);
        }
        unsafe {
            copy_patch(
                Nv12Mut {
                    ptr: nv12.ptr,
                    pitch: nv12.pitch,
                    plane_height: nv12.plane_height,
                },
                self.patch,
                &mut self.saved,
                true,
            );
        }
        self.has_patch = true;

        let uv_base = unsafe { nv12.ptr.add(nv12.pitch * nv12.plane_height) };
        let mut by = t;
        while by < b {
            let mut bx = l;
            while bx < r {
                let mut a_sum = 0i32;
                let mut u_sum = 0i32;
                let mut v_sum = 0i32;
                for k in 0..4 {
                    let x = bx + (k & 1);
                    let y = by + (k >> 1);
                    if x < left || y < top {
                        continue;
                    }
                    let ix = ((x - left) as f64 / sx) as i32;
                    let iy = ((y - top) as f64 / sy) as i32;
                    if ix < 0 || iy < 0 || ix >= s.width || iy >= s.height {
                        continue;
                    }
                    let i = (iy * s.width + ix) as usize;
                    let py = unsafe { nv12.ptr.add((y as usize) * nv12.pitch + (x as usize)) };
                    if s.invert[i] {
                        unsafe {
                            *py = 251u8.wrapping_sub(*py);
                        }
                        continue;
                    }
                    let a = s.a[i] as i32;
                    if a == 0 {
                        continue;
                    }
                    unsafe {
                        let cur = *py as i32;
                        *py = ((s.y[i] as i32 * a + cur * (255 - a) + 127) / 255) as u8;
                    }
                    a_sum += a;
                    u_sum += s.u[i] as i32 * a;
                    v_sum += s.v[i] as i32 * a;
                }
                if a_sum > 0 {
                    let uv = unsafe {
                        uv_base.add(((by / 2) as usize) * nv12.pitch + (bx as usize))
                    };
                    let ab = a_sum / 4;
                    unsafe {
                        let cur_u = *uv as i32;
                        let cur_v = *uv.add(1) as i32;
                        *uv = (((u_sum / a_sum) * ab + cur_u * (255 - ab) + 127) / 255) as u8;
                        *uv.add(1) =
                            (((v_sum / a_sum) * ab + cur_v * (255 - ab) + 127) / 255) as u8;
                    }
                }
                bx += 2;
            }
            by += 2;
        }
    }
}

unsafe fn copy_patch(nv12: Nv12Mut, r: Rect, buf: &mut [u8], save: bool) {
    let w = (r.right - r.left) as usize;
    let left = r.left as usize;
    let mut o = 0usize;
    for y in (r.top as usize)..(r.bottom as usize) {
        unsafe {
            let line = nv12.ptr.add(y * nv12.pitch + left);
            if save {
                std::ptr::copy_nonoverlapping(line, buf.as_mut_ptr().add(o), w);
            } else {
                std::ptr::copy_nonoverlapping(buf.as_ptr().add(o), line, w);
            }
        }
        o += w;
    }
    let uv_base = unsafe { nv12.ptr.add(nv12.pitch * nv12.plane_height) };
    for y in ((r.top / 2) as usize)..((r.bottom / 2) as usize) {
        unsafe {
            let line = uv_base.add(y * nv12.pitch + left);
            if save {
                std::ptr::copy_nonoverlapping(line, buf.as_mut_ptr().add(o), w);
            } else {
                std::ptr::copy_nonoverlapping(buf.as_ptr().add(o), line, w);
            }
        }
        o += w;
    }
}
