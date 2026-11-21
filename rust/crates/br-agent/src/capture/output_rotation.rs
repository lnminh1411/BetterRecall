// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Desktop Duplication scan-out rotation math (`Capture\OutputRotation.cs`).

use windows::Win32::Graphics::Direct3D11::{
    D3D11_VIDEO_PROCESSOR_ROTATION, D3D11_VIDEO_PROCESSOR_ROTATION_90,
    D3D11_VIDEO_PROCESSOR_ROTATION_180, D3D11_VIDEO_PROCESSOR_ROTATION_270,
    D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180,
    DXGI_MODE_ROTATION_ROTATE270,
};

use crate::native::Rect;

/// Converts between scan-out (duplication texture) and desktop orientations.
pub struct OutputRotation;

impl OutputRotation {
    pub fn degrees(r: DXGI_MODE_ROTATION) -> i32 {
        if r == DXGI_MODE_ROTATION_ROTATE90 {
            90
        } else if r == DXGI_MODE_ROTATION_ROTATE180 {
            180
        } else if r == DXGI_MODE_ROTATION_ROTATE270 {
            270
        } else {
            0
        }
    }

    pub fn to_video_processor(degrees: i32) -> D3D11_VIDEO_PROCESSOR_ROTATION {
        match degrees {
            90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
            180 => D3D11_VIDEO_PROCESSOR_ROTATION_180,
            270 => D3D11_VIDEO_PROCESSOR_ROTATION_270,
            _ => D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
        }
    }

    pub const fn swaps_axes(degrees: i32) -> bool {
        matches!(degrees, 90 | 270)
    }

    /// Desktop size of a `tex_w x tex_h` texture.
    pub const fn desktop_size(tex_w: i32, tex_h: i32, degrees: i32) -> (i32, i32) {
        if Self::swaps_axes(degrees) {
            (tex_h, tex_w)
        } else {
            (tex_w, tex_h)
        }
    }

    /// Texture size for a `desk_w x desk_h` desktop.
    pub const fn texture_size(desk_w: i32, desk_h: i32, degrees: i32) -> (i32, i32) {
        if Self::swaps_axes(degrees) {
            (desk_h, desk_w)
        } else {
            (desk_w, desk_h)
        }
    }

    /// Texture size from the duplication description, disambiguating drivers that report
    /// `ModeDesc` in desktop orientation.
    pub const fn texture_size_for(
        mode_w: i32,
        mode_h: i32,
        desk_w: i32,
        desk_h: i32,
        degrees: i32,
    ) -> (i32, i32) {
        if desk_w <= 0 || desk_h <= 0 {
            return (mode_w, mode_h);
        }
        let expect = Self::texture_size(desk_w, desk_h, degrees);
        if mode_w == expect.0 && mode_h == expect.1 {
            return (mode_w, mode_h);
        }
        if mode_w == expect.1 && mode_h == expect.0 {
            return expect;
        }
        (mode_w, mode_h)
    }

    /// Rectangle in texture pixels to desktop pixels (relative to the output's top-left).
    pub const fn texture_to_desktop(r: Rect, tex_w: i32, tex_h: i32, degrees: i32) -> Rect {
        match degrees {
            90 => Rect::new(tex_h - r.bottom, r.left, tex_h - r.top, r.right),
            180 => Rect::new(
                tex_w - r.right,
                tex_h - r.bottom,
                tex_w - r.left,
                tex_h - r.top,
            ),
            270 => Rect::new(r.top, tex_w - r.right, r.bottom, tex_w - r.left),
            _ => r,
        }
    }

    /// Rectangle in desktop pixels (relative to the output's top-left) to texture pixels.
    pub const fn desktop_to_texture(r: Rect, tex_w: i32, tex_h: i32, degrees: i32) -> Rect {
        match degrees {
            90 => Rect::new(r.top, tex_h - r.right, r.bottom, tex_h - r.left),
            180 => Rect::new(
                tex_w - r.right,
                tex_h - r.bottom,
                tex_w - r.left,
                tex_h - r.top,
            ),
            270 => Rect::new(tex_w - r.bottom, r.left, tex_w - r.top, r.right),
            _ => r,
        }
    }

    /// Texture pixel to desktop pixel (relative to the output's top-left).
    pub const fn texture_pixel_to_desktop(
        x: i32,
        y: i32,
        tex_w: i32,
        tex_h: i32,
        degrees: i32,
    ) -> (i32, i32) {
        match degrees {
            90 => (tex_h - 1 - y, x),
            180 => (tex_w - 1 - x, tex_h - 1 - y),
            270 => (y, tex_w - 1 - x),
            _ => (x, y),
        }
    }
}
