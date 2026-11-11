// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Native;

namespace BetterRecall.Agent.Replay;

/// <summary>
/// Pure CPU helpers on NV12 (Y plane, then interleaved UV plane at pitch * planeHeight).
/// </summary>
internal static unsafe class CanvasBlit
{
    /// <summary>
    /// Copies rect r (source-local, even-aligned) of an NV12 image into an NV12 canvas at offset (dx, dy) (even):
    /// Y rows r.Top..r.Bottom, UV rows r.Top/2..r.Bottom/2, bytes r.Left..r.Right in both planes.
    /// </summary>
    public static void CopyRect(byte* src, int srcPitch, int srcPlaneHeight, byte* dst, int dstPitch, int dstPlaneHeight, RECT r, int dx, int dy)
    {
        if (DirtyMapper.IsEmpty(r)) return;
        int w = r.Right - r.Left;
        int h = r.Bottom - r.Top;
        if (w <= 0 || h <= 0) return;

        // Y plane
        int dstLeft = dx + r.Left;
        int dstTop = dy + r.Top;
        for (int y = 0; y < h; y++)
        {
            byte* sLine = src + (long)(r.Top + y) * srcPitch + r.Left;
            byte* dLine = dst + (long)(dstTop + y) * dstPitch + dstLeft;
            Buffer.MemoryCopy(sLine, dLine, w, w);
        }

        // UV plane
        byte* srcUvBase = src + (long)srcPitch * srcPlaneHeight;
        byte* dstUvBase = dst + (long)dstPitch * dstPlaneHeight;
        int uvRows = h / 2;
        int srcUvTop = r.Top / 2;
        int dstUvTop = dstTop / 2;
        for (int y = 0; y < uvRows; y++)
        {
            byte* sLine = srcUvBase + (long)(srcUvTop + y) * srcPitch + r.Left;
            byte* dLine = dstUvBase + (long)(dstUvTop + y) * dstPitch + dstLeft;
            Buffer.MemoryCopy(sLine, dLine, w, w);
        }
    }

    /// <summary>
    /// Paints rect r (canvas coordinates, even-aligned) black: Y = 16, UV = 128.
    /// </summary>
    public static void FillBlack(byte* nv12, int pitch, int planeHeight, RECT r)
    {
        if (DirtyMapper.IsEmpty(r)) return;
        int w = r.Right - r.Left;
        int h = r.Bottom - r.Top;
        if (w <= 0 || h <= 0) return;

        // Y plane
        for (int y = r.Top; y < r.Bottom; y++)
        {
            byte* line = nv12 + (long)y * pitch + r.Left;
            new Span<byte>(line, w).Fill(16);
        }

        // UV plane
        byte* uvBase = nv12 + (long)pitch * planeHeight;
        int uvTop = r.Top / 2;
        int uvBottom = r.Bottom / 2;
        for (int y = uvTop; y < uvBottom; y++)
        {
            byte* line = uvBase + (long)y * pitch + r.Left;
            new Span<byte>(line, w).Fill(128);
        }
    }

    /// <summary>
    /// Whole canvas black: all Y = 16, all UV = 128.
    /// </summary>
    public static void Clear(byte* nv12, int pitch, int planeHeight, int width)
    {
        if (width <= 0 || planeHeight <= 0) return;

        // Y plane
        for (int y = 0; y < planeHeight; y++)
        {
            byte* line = nv12 + (long)y * pitch;
            new Span<byte>(line, width).Fill(16);
        }

        // UV plane
        byte* uvBase = nv12 + (long)pitch * planeHeight;
        int uvHeight = planeHeight / 2;
        for (int y = 0; y < uvHeight; y++)
        {
            byte* line = uvBase + (long)y * pitch;
            new Span<byte>(line, width).Fill(128);
        }
    }

    /// <summary>
    /// Benchmark content: a block of 16x16 tiles covering the top-left quarter of the canvas that scrolls one tile per
    /// frame, so encoders see steady motion instead of a static desktop. Flat tiles keep the CPU cost of painting small.
    /// </summary>
    public static void PaintMotion(byte* nv12, int pitch, int planeHeight, int width, int height, long frame)
    {
        int w = (width / 2) & ~15, h = (height / 2) & ~15;
        if (w <= 0 || h <= 0) return;
        byte* uvBase = nv12 + (long)pitch * planeHeight;
        for (int ty = 0; ty < h / 16; ty++)
        {
            for (int tx = 0; tx < w / 16; tx++)
            {
                uint v = Hash((uint)tx, (uint)(ty + frame));
                byte y = (byte)(16 + v % 220), u = (byte)(16 + (v >> 8) % 225), vv = (byte)(16 + (v >> 16) % 225);
                for (int r = 0; r < 16; r++)
                    new Span<byte>(nv12 + (long)(ty * 16 + r) * pitch + tx * 16, 16).Fill(y);
                for (int r = 0; r < 8; r++)
                {
                    byte* line = uvBase + (long)(ty * 8 + r) * pitch + tx * 16;
                    for (int c = 0; c < 16; c += 2) { line[c] = u; line[c + 1] = vv; }
                }
            }
        }
    }

    private static uint Hash(uint x, uint y)
    {
        uint h = x * 0x9E3779B1u ^ y * 0x85EBCA77u;
        h ^= h >> 15; h *= 0x2C1B3C6Du; h ^= h >> 12;
        return h;
    }
}
