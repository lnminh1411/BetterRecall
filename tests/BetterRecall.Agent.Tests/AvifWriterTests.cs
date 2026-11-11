// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;

namespace BetterRecall.Agent.Tests;

/// <summary>AvifWriter: AV1 OBUs (here from libaom) wrapped as an AVIF still.</summary>
public unsafe class AvifWriterTests
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
}
