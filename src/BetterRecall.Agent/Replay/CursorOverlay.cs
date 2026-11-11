// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Agent.Native;

namespace BetterRecall.Agent.Replay;

/// <summary>Mouse pointer state in desktop (physical) pixels. Desktop Duplication leaves the pointer out of frames.</summary>
internal readonly record struct CursorState(bool Visible, int X, int Y, nint Handle)
{
    public static readonly CursorState Hidden = default;
}

/// <summary>Cursor image pre-converted to BT.709 limited-range YUV with straight alpha; Invert marks XOR pixels (I-beam).</summary>
internal sealed class CursorShape
{
    public required int Width { get; init; }
    public required int Height { get; init; }
    public required int HotX { get; init; }
    public required int HotY { get; init; }
    public required byte[] Y { get; init; }
    public required byte[] U { get; init; }
    public required byte[] V { get; init; }
    public required byte[] A { get; init; }
    public required bool[] Invert { get; init; }

    /// <summary>Builds a shape from two BGRA renders of the cursor: over black and over white.</summary>
    public static CursorShape FromRenders(ReadOnlySpan<byte> onBlack, ReadOnlySpan<byte> onWhite, int w, int h, int hotX, int hotY)
    {
        int n = w * h;
        var s = new CursorShape
        {
            Width = w, Height = h, HotX = hotX, HotY = hotY,
            Y = new byte[n], U = new byte[n], V = new byte[n], A = new byte[n], Invert = new bool[n],
        };
        for (int i = 0; i < n; i++)
        {
            int bb = onBlack[i * 4], bg = onBlack[i * 4 + 1], br = onBlack[i * 4 + 2];
            int wb = onWhite[i * 4], wg = onWhite[i * 4 + 1], wr = onWhite[i * 4 + 2];
            int diff = ((wb - bb) + (wg - bg) + (wr - br)) / 3;
            if (diff < -32) { s.Invert[i] = true; s.A[i] = 255; continue; } // white on black, black on white
            int a = Math.Clamp(255 - diff, 0, 255);
            s.A[i] = (byte)a;
            if (a == 0) continue;
            // Un-premultiply the render over black.
            double r = Math.Min(255, br * 255.0 / a), g = Math.Min(255, bg * 255.0 / a), b = Math.Min(255, bb * 255.0 / a);
            s.Y[i] = (byte)Math.Clamp(Math.Round(16 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219 / 255), 16, 235);
            s.U[i] = (byte)Math.Clamp(Math.Round(128 + (-0.1146 * r - 0.3854 * g + 0.5 * b) * 224 / 255), 16, 240);
            s.V[i] = (byte)Math.Clamp(Math.Round(128 + (0.5 * r - 0.4542 * g - 0.0458 * b) * 224 / 255), 16, 240);
        }
        return s;
    }
}

/// <summary>Reads the system pointer through GDI and caches rendered shapes by cursor handle.</summary>
internal static unsafe partial class CursorProbe
{
    private static readonly Dictionary<nint, CursorShape?> Cache = [];
    private static readonly object Gate = new();

    public static CursorState Current()
    {
        var ci = new CURSORINFO { cbSize = (uint)sizeof(CURSORINFO) };
        if (!GetCursorInfo(&ci) || (ci.flags & CURSOR_SHOWING) == 0 || ci.hCursor == 0) return CursorState.Hidden;
        return new CursorState(true, ci.x, ci.y, ci.hCursor);
    }

    public static CursorShape? Shape(nint handle)
    {
        lock (Gate)
        {
            if (Cache.TryGetValue(handle, out var s)) return s;
            try { s = Render(handle); }
            catch { s = null; }
            if (Cache.Count > 64) Cache.Clear();
            Cache[handle] = s;
            return s;
        }
    }

    private static CursorShape? Render(nint hCursor)
    {
        ICONINFO ii;
        if (!GetIconInfo(hCursor, &ii)) return null;
        try
        {
            BITMAP bm;
            nint basis = ii.hbmColor != 0 ? ii.hbmColor : ii.hbmMask;
            if (GetObjectW(basis, sizeof(BITMAP), &bm) == 0) return null;
            int w = bm.bmWidth, h = ii.hbmColor != 0 ? bm.bmHeight : bm.bmHeight / 2;
            if (w <= 0 || h <= 0 || w > 256 || h > 256) return null;

            var black = RenderOver(hCursor, w, h, 0x00);
            var white = RenderOver(hCursor, w, h, 0xFF);
            if (black is null || white is null) return null;
            return CursorShape.FromRenders(black, white, w, h, (int)ii.xHotspot, (int)ii.yHotspot);
        }
        finally
        {
            if (ii.hbmColor != 0) DeleteObject(ii.hbmColor);
            if (ii.hbmMask != 0) DeleteObject(ii.hbmMask);
        }
    }

    private static byte[]? RenderOver(nint hCursor, int w, int h, byte fill)
    {
        nint dc = CreateCompatibleDC(0);
        if (dc == 0) return null;
        try
        {
            var bi = new BITMAPINFOHEADER
            {
                biSize = (uint)sizeof(BITMAPINFOHEADER), biWidth = w, biHeight = -h, biPlanes = 1, biBitCount = 32,
            };
            void* bits;
            nint dib = CreateDIBSection(dc, &bi, 0, &bits, 0, 0);
            if (dib == 0) return null;
            try
            {
                nint old = SelectObject(dc, dib);
                new Span<byte>(bits, w * h * 4).Fill(fill);
                bool ok = DrawIconEx(dc, 0, 0, hCursor, w, h, 0, 0, DI_NORMAL);
                GdiFlush();
                SelectObject(dc, old);
                return ok ? new Span<byte>(bits, w * h * 4).ToArray() : null;
            }
            finally { DeleteObject(dib); }
        }
        finally { DeleteDC(dc); }
    }

    private const uint CURSOR_SHOWING = 0x1;
    private const uint DI_NORMAL = 0x3;

    [StructLayout(LayoutKind.Sequential)]
    private struct CURSORINFO { public uint cbSize; public uint flags; public nint hCursor; public int x; public int y; }

    [StructLayout(LayoutKind.Sequential)]
    private struct ICONINFO { public int fIcon; public uint xHotspot; public uint yHotspot; public nint hbmMask; public nint hbmColor; }

    [StructLayout(LayoutKind.Sequential)]
    private struct BITMAP { public int bmType; public int bmWidth; public int bmHeight; public int bmWidthBytes; public ushort bmPlanes; public ushort bmBitsPixel; public nint bmBits; }

    [StructLayout(LayoutKind.Sequential)]
    private struct BITMAPINFOHEADER
    {
        public uint biSize; public int biWidth; public int biHeight; public ushort biPlanes; public ushort biBitCount;
        public uint biCompression; public uint biSizeImage; public int biXPelsPerMeter; public int biYPelsPerMeter; public uint biClrUsed; public uint biClrImportant;
    }

    [LibraryImport("user32.dll")] [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetCursorInfo(CURSORINFO* pci);
    [LibraryImport("user32.dll")] [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetIconInfo(nint hIcon, ICONINFO* info);
    [LibraryImport("user32.dll")] [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool DrawIconEx(nint hdc, int x, int y, nint hIcon, int cx, int cy, uint step, nint brush, uint flags);
    [LibraryImport("gdi32.dll")]
    private static partial int GetObjectW(nint h, int c, void* pv);
    [LibraryImport("gdi32.dll")]
    private static partial nint CreateCompatibleDC(nint hdc);
    [LibraryImport("gdi32.dll")] [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool DeleteDC(nint hdc);
    [LibraryImport("gdi32.dll")]
    private static partial nint CreateDIBSection(nint hdc, BITMAPINFOHEADER* bmi, uint usage, void** bits, nint section, uint offset);
    [LibraryImport("gdi32.dll")]
    private static partial nint SelectObject(nint hdc, nint h);
    [LibraryImport("gdi32.dll")] [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool DeleteObject(nint h);
    [LibraryImport("gdi32.dll")] [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GdiFlush();
}

/// <summary>
/// Draws the pointer into an NV12 canvas and puts the pixels underneath back before the next update,
/// so dirty-rect compositing never leaves pointer trails.
/// </summary>
internal sealed unsafe class CursorPainter
{
    private RECT _patch;
    private byte[] _saved = [];
    public bool HasPatch { get; private set; }

    /// <summary>Restores the canvas under the last drawn pointer.</summary>
    public void Restore(byte* nv12, int pitch, int planeHeight)
    {
        if (!HasPatch) return;
        Copy(nv12, pitch, planeHeight, _patch, _saved, save: false);
        HasPatch = false;
    }

    /// <summary>
    /// Draws shape scaled by (sx, sy) with its hotspot at canvas point (cx, cy), clipped to clip (even-aligned).
    /// </summary>
    public void Draw(byte* nv12, int pitch, int planeHeight, CursorShape s, double cx, double cy, double sx, double sy, RECT clip)
    {
        int dw = Math.Max(1, (int)Math.Round(s.Width * sx)), dh = Math.Max(1, (int)Math.Round(s.Height * sy));
        int left = (int)Math.Round(cx - s.HotX * sx), top = (int)Math.Round(cy - s.HotY * sy);
        int l = Math.Max(clip.Left, left) & ~1, t = Math.Max(clip.Top, top) & ~1;
        int r = Math.Min(clip.Right, (left + dw + 1) & ~1), b = Math.Min(clip.Bottom, (top + dh + 1) & ~1);
        if (r <= l || b <= t) return;
        _patch = new RECT(l, t, r, b);
        int need = (r - l) * (b - t) * 3 / 2;
        if (_saved.Length < need) _saved = new byte[need];
        Copy(nv12, pitch, planeHeight, _patch, _saved, save: true);
        HasPatch = true;

        byte* uvBase = nv12 + (long)pitch * planeHeight;
        for (int by = t; by < b; by += 2)
        {
            for (int bx = l; bx < r; bx += 2)
            {
                int aSum = 0, uSum = 0, vSum = 0;
                for (int k = 0; k < 4; k++)
                {
                    int x = bx + (k & 1), y = by + (k >> 1);
                    int ix = (int)((x - left) / sx), iy = (int)((y - top) / sy);
                    if (x < left || y < top || ix < 0 || iy < 0 || ix >= s.Width || iy >= s.Height) continue;
                    int i = iy * s.Width + ix;
                    byte* py = nv12 + (long)y * pitch + x;
                    if (s.Invert[i]) { *py = (byte)(251 - *py); continue; }
                    int a = s.A[i];
                    if (a == 0) continue;
                    *py = (byte)((s.Y[i] * a + *py * (255 - a) + 127) / 255);
                    aSum += a; uSum += s.U[i] * a; vSum += s.V[i] * a;
                }
                if (aSum == 0) continue;
                byte* uv = uvBase + (long)(by / 2) * pitch + bx;
                int ab = aSum / 4; // block coverage
                uv[0] = (byte)((uSum / aSum * ab + uv[0] * (255 - ab) + 127) / 255);
                uv[1] = (byte)((vSum / aSum * ab + uv[1] * (255 - ab) + 127) / 255);
            }
        }
    }

    private static void Copy(byte* nv12, int pitch, int planeHeight, RECT r, byte[] buf, bool save)
    {
        int w = r.Right - r.Left, h = r.Bottom - r.Top, o = 0;
        fixed (byte* p = buf)
        {
            for (int y = r.Top; y < r.Bottom; y++, o += w)
            {
                byte* line = nv12 + (long)y * pitch + r.Left;
                if (save) Buffer.MemoryCopy(line, p + o, w, w); else Buffer.MemoryCopy(p + o, line, w, w);
            }
            byte* uvBase = nv12 + (long)pitch * planeHeight;
            for (int y = r.Top / 2; y < r.Bottom / 2; y++, o += w)
            {
                byte* line = uvBase + (long)y * pitch + r.Left;
                if (save) Buffer.MemoryCopy(line, p + o, w, w); else Buffer.MemoryCopy(p + o, line, w, w);
            }
        }
    }
}
