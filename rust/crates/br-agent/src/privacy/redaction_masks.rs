// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Per-monitor desktop redaction rectangles for the replay compositor (`Privacy\RedactionMasks.cs`).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::native::Rect;
use crate::privacy::redactor::PixelBox;

struct MasksState {
    by_monitor: HashMap<i32, Vec<Rect>>,
    all: Vec<Rect>,
}

/// Redaction boxes of the newest kept keyframe of each monitor, in desktop pixels. Thread-safe.
pub struct RedactionMasks {
    state: Mutex<MasksState>,
}

impl Default for RedactionMasks {
    fn default() -> Self {
        Self::new()
    }
}

impl RedactionMasks {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MasksState {
                by_monitor: HashMap::new(),
                all: Vec::new(),
            }),
        }
    }

    pub fn set(&self, monitor: i32, desktop: Vec<Rect>) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if desktop.is_empty() {
            st.by_monitor.remove(&monitor);
        } else {
            st.by_monitor.insert(monitor, desktop);
        }
        st.all = st.by_monitor.values().flatten().copied().collect();
    }

    pub fn clear(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.by_monitor.clear();
        st.all.clear();
    }

    pub fn all(&self) -> Vec<Rect> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .all
            .clone()
    }

    /// Keyframe boxes (`kf_w x kf_h` image of `monitor`) to desktop rectangles, rounded outwards
    /// and clipped to `monitor`.
    pub fn to_desktop(boxes: &[PixelBox], kf_w: i32, kf_h: i32, monitor: Rect) -> Vec<Rect> {
        if kf_w <= 0 || kf_h <= 0 {
            return Vec::new();
        }
        let sx = monitor.width() as f64 / kf_w as f64;
        let sy = monitor.height() as f64 / kf_h as f64;
        let mut out = Vec::with_capacity(boxes.len());
        for b in boxes {
            let l = (monitor.left + (b.x as f64 * sx).floor() as i32).max(monitor.left);
            let t = (monitor.top + (b.y as f64 * sy).floor() as i32).max(monitor.top);
            let r = (monitor.left + (b.right() as f64 * sx).ceil() as i32).min(monitor.right);
            let btm = (monitor.top + (b.bottom() as f64 * sy).ceil() as i32).min(monitor.bottom);
            if r > l && btm > t {
                out.push(Rect::new(l, t, r, btm));
            }
        }
        out
    }

    /// Desktop rectangles to keyframe boxes of `monitor` (`kf_w x kf_h`), clipped; off-monitor
    /// rectangles are dropped.
    pub fn to_keyframe(desktop: &[Rect], monitor: Rect, kf_w: i32, kf_h: i32) -> Vec<PixelBox> {
        if monitor.width() <= 0 || monitor.height() <= 0 {
            return Vec::new();
        }
        let sx = kf_w as f64 / monitor.width() as f64;
        let sy = kf_h as f64 / monitor.height() as f64;
        let mut out = Vec::with_capacity(desktop.len());
        for d in desktop {
            let l = d.left.max(monitor.left);
            let t = d.top.max(monitor.top);
            let r = d.right.min(monitor.right);
            let b = d.bottom.min(monitor.bottom);
            if r <= l || b <= t {
                continue;
            }
            let x0 = ((l - monitor.left) as f64 * sx).floor() as i32;
            let y0 = ((t - monitor.top) as f64 * sy).floor() as i32;
            let x1 = ((r - monitor.left) as f64 * sx).ceil() as i32;
            let y1 = ((b - monitor.top) as f64 * sy).ceil() as i32;
            out.push(PixelBox::new(x0, y0, (x1 - x0).max(1), (y1 - y0).max(1)));
        }
        out
    }

    /// Fills `boxes` opaque black (`0xFF000000`) in a BGRA buffer, clipped to `width x height`.
    ///
    /// # Safety
    /// `bgra` must point to at least `height * stride` writable bytes with `width * 4 <= stride`.
    pub unsafe fn fill(bgra: *mut u8, width: i32, height: i32, stride: i32, boxes: &[PixelBox]) {
        if bgra.is_null() || width <= 0 || height <= 0 || stride < width * 4 {
            return;
        }
        for b in boxes {
            let x0 = b.x.max(0);
            let y0 = b.y.max(0);
            let x1 = b.right().min(width);
            let y1 = b.bottom().min(height);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let count = (x1 - x0) as usize;
            for y in y0..y1 {
                unsafe {
                    let row = bgra.add((y as usize) * (stride as usize)).cast::<u32>().add(x0 as usize);
                    std::slice::from_raw_parts_mut(row, count).fill(0xFF00_0000);
                }
            }
        }
    }
}
