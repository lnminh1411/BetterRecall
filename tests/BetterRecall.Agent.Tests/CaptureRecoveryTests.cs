// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Privacy;
using Vortice.Direct3D11;
using Vortice.DXGI;
using Xunit.Abstractions;

namespace BetterRecall.Agent.Tests;

/// <summary>A3: rotated (portrait) outputs. Texture pixels are in scan-out orientation; the desktop is the texture
/// turned clockwise by the rotation (as in the Desktop Duplication sample: ROTATE90 puts the texture's top-left corner
/// at the desktop's top-right).</summary>
public class OutputRotationTests(ITestOutputHelper output)
{
    [Theory]
    [InlineData(ModeRotation.Unspecified, 0)]
    [InlineData(ModeRotation.Identity, 0)]
    [InlineData(ModeRotation.Rotate90, 90)]
    [InlineData(ModeRotation.Rotate180, 180)]
    [InlineData(ModeRotation.Rotate270, 270)]
    public void Degrees_from_dxgi(ModeRotation r, int expected) => Assert.Equal(expected, OutputRotation.Degrees(r));

    [Fact]
    public void Sizes_swap_for_quarter_turns()
    {
        Assert.Equal((1080, 1920), OutputRotation.DesktopSize(1920, 1080, 90));
        Assert.Equal((1080, 1920), OutputRotation.DesktopSize(1920, 1080, 270));
        Assert.Equal((1920, 1080), OutputRotation.DesktopSize(1920, 1080, 180));
        Assert.Equal((1920, 1080), OutputRotation.TextureSize(1080, 1920, 90));
        // Mode reported unrotated (usual) or already in desktop orientation: the texture size is the unrotated one.
        Assert.Equal((1920, 1080), OutputRotation.TextureSizeFor(1920, 1080, 1080, 1920, 90));
        Assert.Equal((1920, 1080), OutputRotation.TextureSizeFor(1080, 1920, 1080, 1920, 90));
        Assert.Equal((2560, 1440), OutputRotation.TextureSizeFor(2560, 1440, 2560, 1440, 0));
    }

    [Fact]
    public void Quarter_turn_corners_match_the_duplication_sample()
    {
        const int w = 1920, h = 1080;
        Assert.Equal((h - 1, 0), OutputRotation.TexturePixelToDesktop(0, 0, w, h, 90));         // top-left -> top-right
        Assert.Equal((h - 1, w - 1), OutputRotation.TexturePixelToDesktop(w - 1, 0, w, h, 90)); // top-right -> bottom-right
        Assert.Equal((0, 0), OutputRotation.TexturePixelToDesktop(0, h - 1, w, h, 90));         // bottom-left -> top-left
        Assert.Equal((0, w - 1), OutputRotation.TexturePixelToDesktop(0, 0, w, h, 270));        // top-left -> bottom-left
        Assert.Equal((w - 1, h - 1), OutputRotation.TexturePixelToDesktop(0, 0, w, h, 180));
    }

    [Theory]
    [InlineData(0)]
    [InlineData(90)]
    [InlineData(180)]
    [InlineData(270)]
    public void Rect_mapping_agrees_with_pixels_and_inverts(int deg)
    {
        const int w = 7, h = 4;
        var (dw, dh) = OutputRotation.DesktopSize(w, h, deg);
        for (int y = 0; y < h; y++)
            for (int x = 0; x < w; x++)
            {
                var (px, py) = OutputRotation.TexturePixelToDesktop(x, y, w, h, deg);
                Assert.InRange(px, 0, dw - 1);
                Assert.InRange(py, 0, dh - 1);
                var d = OutputRotation.TextureToDesktop(new RECT(x, y, x + 1, y + 1), w, h, deg);
                Assert.Equal(new RECT(px, py, px + 1, py + 1), d);
                Assert.Equal(new RECT(x, y, x + 1, y + 1), OutputRotation.DesktopToTexture(d, w, h, deg));
            }
        var r = new RECT(1, 0, 6, 3);
        var m = OutputRotation.TextureToDesktop(r, w, h, deg);
        Assert.Equal(r.Width * r.Height, m.Width * m.Height);
        Assert.True(m.Left >= 0 && m.Top >= 0 && m.Right <= dw && m.Bottom <= dh);
        Assert.Equal(r, OutputRotation.DesktopToTexture(m, w, h, deg));
        Assert.Equal(new RECT(0, 0, dw, dh), OutputRotation.TextureToDesktop(new RECT(0, 0, w, h), w, h, deg));
    }

    /// <summary>Keyframes of a portrait output come out upright (desktop orientation, 1080 wide), so desktop masks map
    /// with the plain desktop-to-keyframe scale: a password field and a redaction box land where they are on screen.</summary>
    [Fact]
    public void Portrait_output_masks_map_to_the_upright_keyframe_and_back()
    {
        var monitor = new RECT(1920, -420, 1920 + 1080, -420 + 1920);   // right of a landscape primary, rotated
        int kfW = 540, kfH = 960;
        var field = new RECT(1920 + 108, -420 + 192, 1920 + 108 + 216, -420 + 192 + 96);
        var box = Assert.Single(RedactionMasks.ToKeyframe([field], monitor, kfW, kfH));
        Assert.Equal(new BetterRecall.Agent.Privacy.Box(54, 96, 108, 48), box);
        var back = Assert.Single(RedactionMasks.ToDesktop([box], kfW, kfH, monitor));
        Assert.Equal(field, back);
        // The old code used the unrotated 1920 x 1080 size: the same field went off the image or to the wrong place.
        var wrong = RedactionMasks.ToKeyframe([field], new RECT(1920, -420, 1920 + 1920, -420 + 1080), kfW, kfH);
        Assert.NotEqual(box, Assert.Single(wrong));
    }

    /// <summary>The video processor's Rotation90 is clockwise, the sense OutputRotation uses. Runs on the first hardware
    /// adapter whose video processor can rotate; skipped otherwise.</summary>
    [Fact]
    public unsafe void Video_processor_rotation_matches_the_mapping()
    {
        var adapter = GpuDevices.Adapters().FirstOrDefault(a => !a.Software);
        if (adapter is null) { output.WriteLine("no hardware adapter: skipped"); return; }
        GpuDevice dev;
        try { dev = GpuDevices.CreateDevice(adapter.Luid); }
        catch (Exception ex) { output.WriteLine($"no device: {ex.Message}"); return; }
        using (dev)
        {
            const int w = 64, h = 32;
            // Four quadrants: TL red, TR green, BL blue, BR white (BGRA).
            var px = new uint[w * h];
            for (int y = 0; y < h; y++)
                for (int x = 0; x < w; x++)
                    px[y * w + x] = (x < w / 2, y < h / 2) switch
                    {
                        (true, true) => 0xFFFF0000u,
                        (false, true) => 0xFF00FF00u,
                        (true, false) => 0xFF0000FFu,
                        _ => 0xFFFFFFFFu,
                    };
            using var src = dev.Device.CreateTexture2D(new Texture2DDescription
            {
                Width = w, Height = h, MipLevels = 1, ArraySize = 1, Format = Format.B8G8R8A8_UNorm,
                SampleDescription = new SampleDescription(1, 0), Usage = ResourceUsage.Default,
                BindFlags = BindFlags.ShaderResource | BindFlags.RenderTarget,
            });
            dev.Context.UpdateSubresource(px, src, 0, w * 4, 0);
            foreach (int deg in new[] { 90, 180, 270 })
            {
                var (dw, dh) = OutputRotation.DesktopSize(w, h, deg);
                VideoScaler scaler;
                try { scaler = new VideoScaler(dev, w, h, dw, dh, Format.B8G8R8A8_UNorm, withStaging: true, rotationDegrees: deg); }
                catch (NotSupportedException) { output.WriteLine($"{adapter.Name}: video processor cannot rotate, skipped"); return; }
                using (scaler)
                {
                    scaler.Run(src, copyToStaging: true);
                    var m = dev.Context.Map(scaler.Staging!, 0, MapMode.Read, Vortice.Direct3D11.MapFlags.None);
                    try
                    {
                        // Sample the middle of each texture quadrant where it lands on the desktop.
                        foreach (var (tx, ty) in new[] { (w / 4, h / 4), (3 * w / 4, h / 4), (w / 4, 3 * h / 4), (3 * w / 4, 3 * h / 4) })
                        {
                            var (dx, dy) = OutputRotation.TexturePixelToDesktop(tx, ty, w, h, deg);
                            uint got = *(uint*)((byte*)m.DataPointer + (long)dy * m.RowPitch + dx * 4);
                            uint want = px[ty * w + tx];
                            for (int c = 0; c < 24; c += 8)
                                Assert.InRange(Math.Abs((int)((got >> c) & 0xFF) - (int)((want >> c) & 0xFF)), 0, 24);
                        }
                    }
                    finally { dev.Context.Unmap(scaler.Staging!, 0); }
                }
            }
            output.WriteLine($"{adapter.Name}: rotation 90/180/270 checked");
        }
    }
}

/// <summary>A1: GPU device loss with two outputs on one adapter.</summary>
public class GpuDeviceRecoveryTests
{
    [Fact]
    public void Device_disposed_by_another_output_is_reported_gone_without_throwing()
    {
        var adapter = GpuDevices.Adapters().FirstOrDefault(a => !a.Software);
        if (adapter is null) return;
        using var cache = new GpuDeviceCache();
        GpuDevice shared;
        try { shared = cache.Get(adapter.Luid); }
        catch { return; }
        Assert.True(cache.IsCurrent(shared));
        Assert.False(cache.InvalidateIfRemoved(shared));   // healthy device stays
        cache.Invalidate(adapter.Luid);                    // output A's recovery drops and disposes it
        Assert.True(shared.IsDisposed);
        Assert.False(cache.IsCurrent(shared));
        // Output B still holds it. Vortice threw NullReferenceException here, out of the capture thread's catch.
        Assert.True(cache.InvalidateIfRemoved(shared));
        shared.Dispose();   // idempotent
        var fresh = cache.Get(adapter.Luid);
        Assert.NotSame(shared, fresh);
        Assert.True(cache.IsCurrent(fresh));
    }
}

/// <summary>A4: keyframes act only on a snapshot scanned after the current foreground window came up.</summary>
public class GuardFreshnessTests
{
    private static GuardSnapshot Snap(nint fg, long epoch) => new(Environment.TickCount64, new WindowInfo(fg, 1, @"C:\Tools\notepad.exe", "Doc", "Notepad"),
        GuardDecision.Allowed, BadgeResult.NotBrowser, false, [], new Dictionary<int, MonitorTop>(), FgEpoch: epoch);

    [Fact]
    public void Snapshot_of_the_current_foreground_is_used()
    {
        var s = Snap(100, 7);
        Assert.Same(s, GuardrailService.FreshForGrab(s, 7, 100));
    }

    [Fact]
    public void Foreground_change_after_the_scan_started_denies()
    {
        var s = Snap(100, 7);
        var r = GuardrailService.FreshForGrab(s, 8, 100);   // a foreground event arrived since
        Assert.False(r.Decision.Allow);
        Assert.Equal(GuardReason.ForegroundChanged, r.Decision.Reason);
        r = GuardrailService.FreshForGrab(s, 7, 200);       // event not delivered yet, but another window is in front
        Assert.False(r.Decision.Allow);
        Assert.Equal(GuardReason.ForegroundChanged, r.Decision.Reason);
    }

    [Fact]
    public void Closed_snapshot_stays_closed()
        => Assert.Same(GuardSnapshot.Closed, GuardrailService.FreshForGrab(GuardSnapshot.Closed, 0, 0));
}
