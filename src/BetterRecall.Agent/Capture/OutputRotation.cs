// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Native;
using Vortice.Direct3D11;
using Vortice.DXGI;

namespace BetterRecall.Agent.Capture;

/// <summary>
/// Desktop Duplication hands over the image in the panel's scan-out orientation and reports how the desktop is rotated
/// from it (DXGI_OUTDUPL_DESC.Rotation). Degrees clockwise: the desktop is the texture turned that far clockwise, the
/// same sense as D3D11_VIDEO_PROCESSOR_ROTATION. Texture and its dirty or move rectangles are in texture pixels;
/// everything after the capture thread (keyframes, replay, masks, pointer) works in desktop orientation.
/// </summary>
internal static class OutputRotation
{
    public static int Degrees(ModeRotation r) => r switch
    {
        ModeRotation.Rotate90 => 90,
        ModeRotation.Rotate180 => 180,
        ModeRotation.Rotate270 => 270,
        _ => 0,   // Unspecified, Identity
    };

    public static VideoProcessorRotation ToVideoProcessor(int degrees) => degrees switch
    {
        90 => VideoProcessorRotation.Rotation90,
        180 => VideoProcessorRotation.Rotation180,
        270 => VideoProcessorRotation.Rotation270,
        _ => VideoProcessorRotation.Identity,
    };

    public static bool SwapsAxes(int degrees) => degrees is 90 or 270;

    /// <summary>Desktop size of a texW x texH texture.</summary>
    public static (int W, int H) DesktopSize(int texW, int texH, int degrees) => SwapsAxes(degrees) ? (texH, texW) : (texW, texH);

    /// <summary>Texture size for a deskW x deskH desktop.</summary>
    public static (int W, int H) TextureSize(int deskW, int deskH, int degrees) => SwapsAxes(degrees) ? (deskH, deskW) : (deskW, deskH);

    /// <summary>
    /// Texture size from the duplication description. ModeDescription is the scan-out mode, which for a rotated output
    /// is normally the unrotated panel size; when a driver reports it in desktop orientation instead, the output's
    /// desktop size decides.
    /// </summary>
    public static (int W, int H) TextureSizeFor(int modeW, int modeH, int deskW, int deskH, int degrees)
    {
        if (deskW <= 0 || deskH <= 0) return (modeW, modeH);
        var expect = TextureSize(deskW, deskH, degrees);
        if (modeW == expect.W && modeH == expect.H) return (modeW, modeH);
        if (modeW == expect.H && modeH == expect.W) return expect;
        return (modeW, modeH);
    }

    /// <summary>Rectangle in texture pixels to desktop pixels (relative to the output's top-left).</summary>
    public static RECT TextureToDesktop(RECT r, int texW, int texH, int degrees) => degrees switch
    {
        90 => new RECT(texH - r.Bottom, r.Left, texH - r.Top, r.Right),
        180 => new RECT(texW - r.Right, texH - r.Bottom, texW - r.Left, texH - r.Top),
        270 => new RECT(r.Top, texW - r.Right, r.Bottom, texW - r.Left),
        _ => r,
    };

    /// <summary>Rectangle in desktop pixels (relative to the output's top-left) to texture pixels.</summary>
    public static RECT DesktopToTexture(RECT r, int texW, int texH, int degrees) => degrees switch
    {
        90 => new RECT(r.Top, texH - r.Right, r.Bottom, texH - r.Left),
        180 => new RECT(texW - r.Right, texH - r.Bottom, texW - r.Left, texH - r.Top),
        270 => new RECT(texW - r.Bottom, r.Left, texW - r.Top, r.Right),
        _ => r,
    };

    /// <summary>Texture pixel to the desktop pixel it shows (relative to the output's top-left).</summary>
    public static (int X, int Y) TexturePixelToDesktop(int x, int y, int texW, int texH, int degrees) => degrees switch
    {
        90 => (texH - 1 - y, x),
        180 => (texW - 1 - x, texH - 1 - y),
        270 => (y, texW - 1 - x),
        _ => (x, y),
    };
}
