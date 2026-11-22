// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Canvas layout, scaling, rate selection, and dirty-rect mapping (`Replay\ReplayLayout.cs`).

use br_contracts::ReplaySettings;

pub use crate::capture::output_capture::{DirtyMapper, FramePacer};
use crate::native::Rect;

pub struct ReplayScaling;

impl ReplayScaling {
    pub const RESOLUTIONS: &'static [&'static str] = &[
        "native", "2160p", "1440p", "1080p", "900p", "720p", "540p", "scale",
    ];

    pub fn target_height(h: i32, resolution: Option<&str>, scale_percent: i32) -> i32 {
        let target = match resolution.map(|s| s.to_ascii_lowercase()).as_deref() {
            Some("2160p") => 2160,
            Some("1440p") => 1440,
            Some("1080p") => 1080,
            Some("900p") => 900,
            Some("720p") => 720,
            Some("540p") => 540,
            Some("scale") => h * scale_percent.clamp(25, 100) / 100,
            _ => h,
        };
        h.min(target)
    }

    pub fn fit(
        w: i32,
        h: i32,
        resolution: Option<&str>,
        scale_percent: i32,
        max_w: i32,
        max_h: i32,
    ) -> (i32, i32) {
        let mut h0 = Self::target_height(h, resolution, scale_percent);
        let mut w0 = ((w as f64) * (h0 as f64) / (h as f64)).round() as i32;
        if w0 > max_w {
            w0 = max_w;
            h0 = ((h as f64) * (max_w as f64) / (w as f64)).round() as i32;
        }
        if h0 > max_h {
            h0 = max_h;
            w0 = ((w as f64) * (max_h as f64) / (h as f64)).round() as i32;
        }
        ((w0 & !1).max(2), (h0 & !1).max(2))
    }
}

pub struct ReplayRate;

impl ReplayRate {
    pub fn normalize_refresh(hz: i32) -> i32 {
        if hz <= 1 {
            return 60;
        }
        if ReplaySettings::FPS_CHOICES.contains(&(hz + 1)) {
            return hz + 1;
        }
        hz
    }

    pub fn effective(requested_fps: i32, max_refresh_hz: i32) -> i32 {
        let req = if ReplaySettings::FPS_CHOICES.contains(&requested_fps) {
            requested_fps
        } else {
            30
        };
        let limit = if max_refresh_hz <= 0 {
            req
        } else {
            max_refresh_hz
        };
        req.min(limit).max(10)
    }
}

/// Monitor placement in desktop pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorPlacement {
    pub index: i32,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

/// Destination rectangle for one monitor in canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorDest {
    pub index: i32,
    pub rect: Rect,
}

/// Composited canvas layout for one or more monitors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasLayout {
    pub width: i32,
    pub height: i32,
    pub origin_left: i32,
    pub origin_top: i32,
    pub source_width: i32,
    pub source_height: i32,
    pub dests: Vec<MonitorDest>,
}

impl CanvasLayout {
    pub fn compute(
        monitors: &[MonitorPlacement],
        resolution: Option<&str>,
        scale_percent: i32,
        max_w: i32,
        max_h: i32,
    ) -> Result<Self, String> {
        if monitors.is_empty() {
            return Err("Empty monitor list.".to_string());
        }
        let mut min_left = i32::MAX;
        let mut min_top = i32::MAX;
        let mut max_right = i32::MIN;
        let mut max_bottom = i32::MIN;
        for m in monitors {
            min_left = min_left.min(m.left);
            min_top = min_top.min(m.top);
            max_right = max_right.max(m.left + m.width);
            max_bottom = max_bottom.max(m.top + m.height);
        }
        let bbox_w = max_right - min_left;
        let bbox_h = max_bottom - min_top;
        let (w, h) = ReplayScaling::fit(bbox_w, bbox_h, resolution, scale_percent, max_w, max_h);
        let mut dests = Vec::with_capacity(monitors.len());
        for m in monitors {
            let mut x0 = (((m.left - min_left) as i64 * w as i64) / bbox_w as i64) as i32 & !1;
            let mut y0 = (((m.top - min_top) as i64 * h as i64) / bbox_h as i64) as i32 & !1;
            let mut x1 =
                (((m.left + m.width - min_left) as i64 * w as i64) / bbox_w as i64) as i32 & !1;
            let mut y1 =
                (((m.top + m.height - min_top) as i64 * h as i64) / bbox_h as i64) as i32 & !1;

            x0 = x0.clamp(0, w);
            y0 = y0.clamp(0, h);
            x1 = x1.clamp(0, w);
            y1 = y1.clamp(0, h);

            if x1 - x0 < 2 {
                x1 = w.min(x1 + 2);
                if x1 - x0 < 2 {
                    x0 = 0.max(x1 - 2);
                }
            }
            if y1 - y0 < 2 {
                y1 = h.min(y1 + 2);
                if y1 - y0 < 2 {
                    y0 = 0.max(y1 - 2);
                }
            }
            dests.push(MonitorDest {
                index: m.index,
                rect: Rect::new(x0, y0, x1, y1),
            });
        }
        Ok(Self {
            width: w,
            height: h,
            origin_left: min_left,
            origin_top: min_top,
            source_width: bbox_w,
            source_height: bbox_h,
            dests,
        })
    }

    pub fn for_monitor(&self, index: i32) -> Option<MonitorDest> {
        self.dests.iter().copied().find(|d| d.index == index)
    }
}

/// Maps a source-local dirty rect to even-aligned destination pixels (`DirtyMapper.ToDest`).
pub fn dirty_to_dest(
    dirty: Rect,
    src_w: i32,
    src_h: i32,
    dst_w: i32,
    dst_h: i32,
    pad: i32,
) -> Rect {
    if DirtyMapper::is_empty(dirty) {
        return Rect::default();
    }
    let l = (dirty.left as i64 * dst_w as i64) / src_w as i64 - pad as i64;
    let t = (dirty.top as i64 * dst_h as i64) / src_h as i64 - pad as i64;
    let r = (dirty.right as i64 * dst_w as i64 + src_w as i64 - 1) / src_w as i64 + pad as i64;
    let b = (dirty.bottom as i64 * dst_h as i64 + src_h as i64 - 1) / src_h as i64 + pad as i64;

    let dest_l = (l.max(0) as i32) & !1;
    let dest_t = (t.max(0) as i32) & !1;
    let dest_r = ((r + 1) & !1).min(dst_w as i64) as i32;
    let dest_b = ((b + 1) & !1).min(dst_h as i64) as i32;
    Rect::new(dest_l, dest_t, dest_r, dest_b)
}
