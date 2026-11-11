// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;
using BetterRecall.Contracts;
using Xunit.Abstractions;

namespace BetterRecall.Agent.Tests;

/// <summary>AVIF from a video encoder: AvifWriter (any PC), the iGPU AV1 encoder (skipped without one), encoder choice.</summary>
public unsafe class HwAvifTests(ITestOutputHelper output)
{
    private static byte[] Ui(int w, int h)
    {
        var px = Screens.Flat(w, h, 250);
        Screens.Rect(px, w, 0, 0, w, 40, 60, 40, 30);
        Screens.Text(px, w, 40, 80, w - 80, h - 200, 5);
        Screens.Gradient(px, w, w / 2, h - 110, w / 2 - 10, 100);
        return px;
    }

    private static double MeanAbsDiff(byte[] a, byte[] b)
    {
        long sum = 0;
        int n = 0;
        for (int i = 0; i < a.Length; i += 4)
            for (int c = 0; c < 3; c++, n++) sum += Math.Abs(a[i + c] - b[i + c]);
        return sum / (double)n;
    }

    /// <summary>Payload of the top-level mdat box (libavif puts only the colour item there for an opaque image).</summary>
    private static byte[] Mdat(byte[] file)
    {
        int pos = 0;
        while (pos + 8 <= file.Length)
        {
            int len = (int)BinaryPrimitives.ReadUInt32BigEndian(file.AsSpan(pos));
            if (System.Text.Encoding.ASCII.GetString(file, pos + 4, 4) == "mdat") return file[(pos + 8)..(pos + len)];
            pos += len;
        }
        throw new InvalidDataException("no mdat");
    }

    private static bool HasBox(byte[] file, string type) => System.Text.Encoding.ASCII.GetString(file).Contains(type, StringComparison.Ordinal);

    [Fact]
    public void Writer_wraps_av1_obus_into_a_decodable_avif()
    {
        if (!Avif.TryLoad()) return;
        const int w = 640, h = 360;
        var px = Ui(w, h);
        byte[] reference;
        fixed (byte* p = px) reference = ImageCodec.EncodeBgra(p, w, h, w * 4, "avif", 75);
        // A video temporal unit: temporal delimiter, the OBUs, a padding OBU.
        byte[] tu = [0x12, 0x00, .. Mdat(reference), 0x7A, 0x02, 0xAA, 0xBB];

        var avif = AvifWriter.Write(tu, w, h);
        Assert.NotNull(avif);
        Assert.Equal("ftyp", System.Text.Encoding.ASCII.GetString(avif!, 4, 4));
        foreach (var box in new[] { "avif", "hdlr", "pict", "pitm", "iloc", "iinf", "infe", "av01", "ispe", "av1C", "pixi", "colr", "nclx", "ipma", "mdat" })
            Assert.True(HasBox(avif, box), box);
        Assert.False(HasBox(avif, "clap"));
        var expected = new byte[tu.Length];                // libavif keeps a temporal delimiter in its own mdat
        int n = BetterRecall.Agent.Replay.Av1.StripForMp4(tu, expected);
        Assert.Equal(expected[..n], Mdat(avif));          // delimiters and padding dropped, nothing else changed
        Assert.DoesNotContain(BetterRecall.Agent.Replay.Av1.Split(Mdat(avif)), o => o.Type is 2 or 15);

        var (dec, dw, dh) = ImageCodec.DecodeBgra(avif, "image/avif");
        Assert.Equal((w, h), (dw, dh));
        // libavif encoded BT.601 full-range YUV; our colr box says BT.709 limited (what Nv12 writes), so colours shift a little.
        Assert.True(MeanAbsDiff(px, dec) < 12, $"mad {MeanAbsDiff(px, dec):F2}");
    }

    [Fact]
    public void Writer_rejects_units_without_a_sequence_header_or_larger_pictures()
    {
        Assert.Null(AvifWriter.Write([0x12, 0x00], 64, 64));
        if (!Avif.TryLoad()) return;
        var px = Ui(128, 128);
        byte[] reference;
        fixed (byte* p = px) reference = ImageCodec.EncodeBgra(p, 128, 128, 512, "avif", 75);
        Assert.Null(AvifWriter.Write(Mdat(reference), 130, 128));
        var cropped = AvifWriter.Write(Mdat(reference), 126, 120);
        Assert.NotNull(cropped);
        Assert.True(HasBox(cropped!, "clap"));
    }

    [Fact]
    public void Nv12_is_bt709_limited_range()
    {
        const int w = 4, h = 2;
        var px = new byte[w * h * 4];
        Screens.Rect(px, w, 0, 0, 2, 2, 255, 255, 255);   // white
        Screens.Rect(px, w, 2, 0, 2, 2, 0, 0, 255);       // red (BGRA)
        var dst = new byte[16 * 2 + 16];
        fixed (byte* s = px) fixed (byte* d = dst) Nv12.FromBgra(s, w, h, w * 4, d, 16, 6, 2);
        Assert.InRange(dst[0], 234, 236);                  // white Y 235
        Assert.InRange(dst[2], 62, 64);                    // red Y 63
        Assert.Equal(dst[3], dst[5]);                      // right padding repeats the edge
        Assert.InRange(dst[32], 127, 129);                 // white Cb, Cr 128
        Assert.InRange(dst[33], 127, 129);
        Assert.InRange(dst[35], 238, 241);                 // red Cr 240
    }

    [Fact]
    public void Software_setting_and_missing_gpu_use_libaom()
    {
        if (!Avif.TryLoad()) return;
        var s = new Settings();
        var px = Ui(320, 180);
        using var noGpu = new KeyframeEncoder(() => null);
        fixed (byte* p = px)
        {
            var (bytes, format) = noGpu.Encode(p, 320, 180, 1280, s);
            Assert.Equal("avif", format);
            Assert.NotEmpty(bytes);
            Assert.Equal("libaom (software)", noGpu.Active);
            Assert.NotNull(noGpu.HardwareUnavailable);

            s.Keyframes.Encoder = "software";
            using var sw = new KeyframeEncoder(() => throw new InvalidOperationException("must not touch the GPU"));
            Assert.Equal("avif", sw.Encode(p, 320, 180, 1280, s).Format);
            Assert.Equal("libaom (software)", sw.Active);

            s.Keyframes.Format = "webp";
            s.Keyframes.Encoder = "hardware";
            Assert.Equal("webp", sw.Encode(p, 320, 180, 1280, s).Format);
        }
    }

    /// <summary>iGPU AV1 encoder: sizes that are and are not multiples of 16 decode at the picture size; odd sizes go to software.</summary>
    [Theory]
    [InlineData(1280, 720)]
    [InlineData(1280, 534)]
    [InlineData(1278, 718)]
    [InlineData(640, 1138)]
    public void Hardware_avif_decodes_at_the_picture_size(int w, int h)
    {
        if (!Avif.TryLoad()) return;
        using var devices = new GpuDeviceCache();
        using var hw = HwAvifEncoder.Create(devices, out var why);
        if (hw is null) { output.WriteLine($"skipped: {why}"); return; }
        var px = Ui(w, h);
        byte[]? avif;
        string? error;
        fixed (byte* p = px) avif = hw.Encode(p, w, h, w * 4, 75, out error);
        Assert.True(avif is not null, error);
        var (dec, dw, dh) = ImageCodec.DecodeBgra(avif!, "image/avif");
        output.WriteLine($"{hw.Name} {w}x{h}: {avif!.Length} bytes, clap {HasBox(avif, "clap")}, decoded {dw}x{dh}, mad {(dw == w && dh == h ? MeanAbsDiff(px, dec) : -1):F2}");
        Assert.Equal((w, h), (dw, dh));
        Assert.True(MeanAbsDiff(px, dec) < 8);

        fixed (byte* p = px) Assert.Null(hw.Encode(p, w - 1, h, w * 4, 75, out error));
        Assert.Contains("odd", error);
    }
}
