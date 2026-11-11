// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Util;
using Vortice.Direct3D11;
using Vortice.DXGI;

namespace BetterRecall.Agent.Capture;

/// <summary>
/// AVIF keyframes on the iGPU's AV1 hardware encoder (oneVPL, the replay encoder in intra-only constant-QP mode).
/// The CPU turns the BGRA grab into NV12 (BT.709, limited range) in a staging texture on that GPU; one encoder per
/// keyframe size, every frame a key frame; the packet is wrapped by AvifWriter. Hardware is 4:2:0 8-bit only.
/// Not thread-safe: the keyframe pipeline calls it from one task.
/// </summary>
internal sealed unsafe class HwAvifEncoder : IDisposable
{
    private const int MaxEncoders = 4;
    private const int WaitMs = 3000;

    private readonly GpuDevice _dev;
    private readonly Dictionary<(int W, int H, int Qp), Entry> _entries = new();
    private long _pts, _use;

    /// <summary>Encoder and adapter, e.g. "oneVPL AV1 VDENC on Intel(R) Graphics".</summary>
    public string Name { get; private set; }
    public string Adapter { get; }

    private sealed class Entry : IDisposable
    {
        public VplAv1Encoder? Encoder;
        public ID3D11Texture2D? Staging;
        public readonly ManualResetEventSlim Done = new();
        public byte[]? Packet;
        public long LastUse;

        public void Dispose()
        {
            Encoder?.Dispose();
            Staging?.Dispose();
            Done.Dispose();
            if (Packet is not null) Array.Clear(Packet);
        }
    }

    private HwAvifEncoder(GpuDevice dev, string adapter)
    {
        _dev = dev;
        Adapter = adapter;
        Name = $"oneVPL AV1 on {adapter}";
    }

    /// <summary>keyframes.quality (30-95) to the AV1 q index (1-255) of the hardware encoder. Spike 2026-11-12 (docs/v0.2-plan.md):
    /// QP 120 matches libaom at quality 75 on text-block PSNR for UI screenshots, QP 160 matches 60, QP 80-100 matches 90.</summary>
    public static int QpFor(int quality) => Math.Clamp((int)Math.Round(QpAt75 - (quality - 75) * QpPerQuality), 1, 255);

    /// <summary>Tuned by the spike (BR_HWAVIF_SPIKE); internal so the spike can sweep it.</summary>
    internal static double QpAt75 = 120, QpPerQuality = 3;

    /// <summary>The first hardware adapter with a oneVPL AV1 encoder, integrated GPU first. Null with a reason otherwise.</summary>
    public static HwAvifEncoder? Create(GpuDeviceCache devices, out string? why)
    {
        why = null;
        try
        {
            var adapter = GpuDevices.Adapters()
                .Where(a => !a.Software && a.VendorId == GpuDevices.VendorIntel)
                .OrderByDescending(a => a.Integrated)
                .FirstOrDefault(a => EncoderFactory.Available(a, VideoCodec.Av1, "vpl"));
            if (adapter is null) { why = "no GPU with a oneVPL AV1 encoder"; return null; }
            return new HwAvifEncoder(devices.Get(adapter.Luid), adapter.Name);
        }
        catch (Exception ex)
        {
            why = ex.Message;
            return null;
        }
    }

    /// <summary>AVIF bytes, or null when the hardware cannot encode this frame (odd size, encoder failure): the caller
    /// falls back to software. Error says why.</summary>
    public byte[]? Encode(byte* bgra, int width, int height, int stride, int quality, out string? error)
    {
        error = null;
        if ((width & 1) != 0 || (height & 1) != 0) { error = $"odd size {width}x{height}"; return null; }
        int qp = QpFor(quality);
        var e = Get(width, height, qp, out error);
        if (e is null) return null;
        e.LastUse = ++_use;
        lock (_dev.Gate)
        {
            var m = _dev.Context.Map(e.Staging!, 0, MapMode.Write, Vortice.Direct3D11.MapFlags.None);
            try { Nv12.FromBgra(bgra, width, height, stride, (byte*)m.DataPointer, (int)m.RowPitch, e.Encoder!.SurfaceWidth, e.Encoder.SurfaceHeight); }
            finally { _dev.Context.Unmap(e.Staging!, 0); }
        }
        e.Done.Reset();
        e.Packet = null;
        if (!e.Encoder!.TrySubmit(e.Staging!, ++_pts * 10_000_000, forceKey: true))
        {
            error = e.Encoder.Error ?? "encoder busy";
            Drop((width, height, qp));
            return null;
        }
        if (!e.Done.Wait(WaitMs) || e.Packet is null)
        {
            error = e.Encoder.Error ?? "no packet within 3 s";
            Drop((width, height, qp));
            return null;
        }
        var packet = e.Packet;
        e.Packet = null;
        try
        {
            var avif = AvifWriter.Write(packet, width, height);
            if (avif is null) error = "packet without a sequence header or frame";
            return avif;
        }
        finally { Array.Clear(packet); }
    }

    private Entry? Get(int w, int h, int qp, out string? error)
    {
        error = null;
        if (_entries.TryGetValue((w, h, qp), out var e)) return e;
        while (_entries.Count >= MaxEncoders) Drop(_entries.MinBy(kv => kv.Value.LastUse).Key);
        e = new Entry();
        var entry = e;
        var enc = VplAv1Encoder.Create(_dev, w, h, 30, 0, (data, _, _) =>
        {
            entry.Packet = data.ToArray();
            entry.Done.Set();
        }, out error, intraQp: qp);
        if (enc is null) { e.Dispose(); return null; }
        e.Encoder = enc;
        Name = $"{enc.Name} on {Adapter}";
        lock (_dev.Gate)
            e.Staging = _dev.Device.CreateTexture2D(new Texture2DDescription
            {
                Width = (uint)enc.SurfaceWidth,
                Height = (uint)enc.SurfaceHeight,
                MipLevels = 1,
                ArraySize = 1,
                Format = Format.NV12,
                SampleDescription = new SampleDescription(1, 0),
                Usage = ResourceUsage.Staging,
                CPUAccessFlags = CpuAccessFlags.Read | CpuAccessFlags.Write,
            });
        _entries[(w, h, qp)] = e;
        return e;
    }

    private void Drop((int, int, int) key)
    {
        if (_entries.Remove(key, out var e))
            lock (_dev.Gate) e.Dispose();
    }

    public void Dispose()
    {
        foreach (var k in _entries.Keys.ToList()) Drop(k);
    }
}

/// <summary>BGRA to NV12, BT.709 limited range, 2x2 chroma average. Rows and columns past the picture repeat its edge.</summary>
internal static unsafe class Nv12
{
    public static void FromBgra(byte* src, int w, int h, int stride, byte* dst, int pitch, int surfW, int surfH)
    {
        // Y = 16 + (47 R + 157 G + 16 B) / 256; Cb = 128 + (-26 R - 86 G + 112 B) / 256; Cr = 128 + (112 R - 102 G - 10 B) / 256
        for (int y = 0; y < surfH; y++)
        {
            byte* s = src + (long)Math.Min(y, h - 1) * stride;
            byte* d = dst + (long)y * pitch;
            for (int x = 0; x < w; x++)
            {
                byte* p = s + x * 4;
                d[x] = (byte)((47 * p[2] + 157 * p[1] + 16 * p[0] + 128 + (16 << 8)) >> 8);
            }
            for (int x = w; x < surfW; x++) d[x] = d[w - 1];
        }
        byte* uv = dst + (long)pitch * surfH;
        for (int y = 0; y < surfH / 2; y++)
        {
            byte* s0 = src + (long)Math.Min(2 * y, h - 1) * stride;
            byte* s1 = src + (long)Math.Min(2 * y + 1, h - 1) * stride;
            byte* d = uv + (long)y * pitch;
            for (int x = 0; x < surfW / 2; x++)
            {
                int x0 = Math.Min(2 * x, w - 1) * 4, x1 = Math.Min(2 * x + 1, w - 1) * 4;
                int b = s0[x0] + s0[x1] + s1[x0] + s1[x1];
                int g = s0[x0 + 1] + s0[x1 + 1] + s1[x0 + 1] + s1[x1 + 1];
                int r = s0[x0 + 2] + s0[x1 + 2] + s1[x0 + 2] + s1[x1 + 2];
                d[2 * x] = (byte)Math.Clamp((-26 * r - 86 * g + 112 * b + 512 + (128 << 10)) >> 10, 0, 255);
                d[2 * x + 1] = (byte)Math.Clamp((112 * r - 102 * g - 10 * b + 512 + (128 << 10)) >> 10, 0, 255);
            }
        }
    }
}
