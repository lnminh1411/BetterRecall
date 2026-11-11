// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Replay;
using Xunit;

namespace BetterRecall.Agent.Tests;

public class CanvasBlitTests
{
    [Fact]
    public unsafe void Dirty_region_update()
    {
        // a 64x32 canvas (pitch 64, plane height 32, NV12 = 64*48 bytes) filled 0xAA
        int dstPitch = 64;
        int dstPlaneHeight = 32;
        byte[] canvas = new byte[dstPitch * 48];
        Array.Fill(canvas, (byte)0xAA);

        // a 16x8 source (pitch 16, plane height 8) filled 0x55
        int srcPitch = 16;
        int srcPlaneHeight = 8;
        byte[] source = new byte[srcPitch * 12];
        Array.Fill(source, (byte)0x55);

        // CopyRect with r = (4,2,10,6) at (20,10)
        var r = new RECT(4, 2, 10, 6);
        fixed (byte* src = source)
        fixed (byte* dst = canvas)
        {
            CanvasBlit.CopyRect(src, srcPitch, srcPlaneHeight, dst, dstPitch, dstPlaneHeight, r, 20, 10);
        }

        // exactly the Y bytes rows 12..15 x 24..29 and UV rows 6..7 x 24..29 changed to 0x55, every other byte still 0xAA
        for (int y = 0; y < dstPlaneHeight; y++)
        {
            for (int x = 0; x < dstPitch; x++)
            {
                bool expectedChanged = (y >= 12 && y <= 15) && (x >= 24 && x <= 29);
                byte expected = expectedChanged ? (byte)0x55 : (byte)0xAA;
                Assert.Equal(expected, canvas[y * dstPitch + x]);
            }
        }

        int uvBase = dstPitch * dstPlaneHeight;
        for (int y = 0; y < dstPlaneHeight / 2; y++)
        {
            for (int x = 0; x < dstPitch; x++)
            {
                bool expectedChanged = (y >= 6 && y <= 7) && (x >= 24 && x <= 29);
                byte expected = expectedChanged ? (byte)0x55 : (byte)0xAA;
                Assert.Equal(expected, canvas[uvBase + y * dstPitch + x]);
            }
        }
    }

    [Fact]
    public unsafe void FillBlack_updates_rect_and_empty_rect_does_nothing()
    {
        int pitch = 64;
        int planeHeight = 32;
        byte[] canvas = new byte[pitch * 48];
        Array.Fill(canvas, (byte)0xAA);

        var r = new RECT(2, 2, 6, 6);
        fixed (byte* ptr = canvas)
        {
            CanvasBlit.FillBlack(ptr, pitch, planeHeight, r);
        }

        // Y rows 2..5 cols 2..5 = 16, UV rows 1..2 cols 2..5 = 128, nothing else changed
        for (int y = 0; y < planeHeight; y++)
        {
            for (int x = 0; x < pitch; x++)
            {
                bool inY = (y >= 2 && y <= 5) && (x >= 2 && x <= 5);
                byte expected = inY ? (byte)16 : (byte)0xAA;
                Assert.Equal(expected, canvas[y * pitch + x]);
            }
        }

        int uvBase = pitch * planeHeight;
        for (int y = 0; y < planeHeight / 2; y++)
        {
            for (int x = 0; x < pitch; x++)
            {
                bool inUv = (y >= 1 && y <= 2) && (x >= 2 && x <= 5);
                byte expected = inUv ? (byte)128 : (byte)0xAA;
                Assert.Equal(expected, canvas[uvBase + y * pitch + x]);
            }
        }

        // Empty rect: no change
        byte[] before = (byte[])canvas.Clone();
        fixed (byte* ptr = canvas)
        {
            CanvasBlit.FillBlack(ptr, pitch, planeHeight, new RECT(0, 0, 0, 0));
            CanvasBlit.FillBlack(ptr, pitch, planeHeight, new RECT(10, 10, 10, 10));
            CanvasBlit.FillBlack(ptr, pitch, planeHeight, new RECT(10, 10, 8, 8));
        }
        Assert.Equal(before, canvas);
    }

    [Fact]
    public unsafe void Clear_sets_all_Y_to_16_and_all_UV_to_128()
    {
        int pitch = 64;
        int planeHeight = 32;
        int width = 64;
        byte[] canvas = new byte[pitch * 48];
        Array.Fill(canvas, (byte)0);

        fixed (byte* ptr = canvas)
        {
            CanvasBlit.Clear(ptr, pitch, planeHeight, width);
        }

        // all Y = 16, all UV = 128
        for (int y = 0; y < planeHeight; y++)
        {
            for (int x = 0; x < width; x++)
            {
                Assert.Equal(16, canvas[y * pitch + x]);
            }
        }

        int uvBase = pitch * planeHeight;
        for (int y = 0; y < planeHeight / 2; y++)
        {
            for (int x = 0; x < width; x++)
            {
                Assert.Equal(128, canvas[uvBase + y * pitch + x]);
            }
        }
    }
}

public unsafe class CursorPainterTests
{
    private const int W = 64, H = 32, Pitch = 64;

    private static byte[] Canvas()
    {
        var c = new byte[Pitch * H * 3 / 2];
        var rnd = new Random(7);
        rnd.NextBytes(c);
        return c;
    }

    private static BetterRecall.Agent.Replay.CursorShape Arrow()
    {
        // 4x4: top-left 2x2 opaque white, one inverted pixel, rest transparent.
        int n = 16;
        var black = new byte[n * 4];
        var white = new byte[n * 4];
        for (int i = 0; i < n; i++) { white[i * 4] = white[i * 4 + 1] = white[i * 4 + 2] = 255; }
        foreach (int i in new[] { 0, 1, 4, 5 }) { black[i * 4] = black[i * 4 + 1] = black[i * 4 + 2] = 255; }
        black[10 * 4] = black[10 * 4 + 1] = black[10 * 4 + 2] = 255; white[10 * 4] = white[10 * 4 + 1] = white[10 * 4 + 2] = 0;
        return BetterRecall.Agent.Replay.CursorShape.FromRenders(black, white, 4, 4, 0, 0);
    }

    [Fact]
    public void ShapeFromRendersFindsAlphaAndInvert()
    {
        var s = Arrow();
        Assert.Equal(255, s.A[0]);
        Assert.Equal(235, s.Y[0]);
        Assert.Equal(0, s.A[2]);
        Assert.True(s.Invert[10]);
    }

    [Fact]
    public void DrawThenRestoreGivesBackTheCanvas()
    {
        var c = Canvas();
        var before = (byte[])c.Clone();
        var p = new BetterRecall.Agent.Replay.CursorPainter();
        fixed (byte* ptr = c)
        {
            p.Draw(ptr, Pitch, H, Arrow(), 9, 7, 1, 1, new BetterRecall.Agent.Native.RECT(0, 0, W, H));
            Assert.True(p.HasPatch);
            Assert.Equal(235, c[7 * Pitch + 9]);                    // opaque white pixel
            Assert.Equal((byte)(251 - before[9 * Pitch + 11]), c[9 * Pitch + 11]); // inverted pixel (row 2, col 2)
            Assert.Equal(before[7 * Pitch + 11], c[7 * Pitch + 11]); // transparent pixel untouched
            p.Restore(ptr, Pitch, H);
        }
        Assert.False(p.HasPatch);
        Assert.Equal(before, c);
    }

    [Fact]
    public void DrawIsClippedToTheMonitor()
    {
        var c = Canvas();
        var before = (byte[])c.Clone();
        var p = new BetterRecall.Agent.Replay.CursorPainter();
        fixed (byte* ptr = c)
        {
            p.Draw(ptr, Pitch, H, Arrow(), 30, 30, 1, 1, new BetterRecall.Agent.Native.RECT(0, 0, 32, 32));
            Assert.Equal(before[30 * Pitch + 32], c[30 * Pitch + 32]); // right of clip untouched
            p.Restore(ptr, Pitch, H);
        }
        Assert.Equal(before, c);
    }
}
