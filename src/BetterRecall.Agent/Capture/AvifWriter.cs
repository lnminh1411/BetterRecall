// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.Text;
using BetterRecall.Agent.Replay;

namespace BetterRecall.Agent.Capture;

/// <summary>
/// Minimal AVIF (HEIF still image) writer for one AV1 intra frame from a video encoder: ftyp, meta (hdlr pict, pitm,
/// iloc, iinf with one av01 item, iprp with ispe, av1C, pixi, colr nclx and clap when the coded size is larger than
/// the picture), mdat. Temporal delimiter and padding OBUs are dropped; the sequence header stays in the item data and
/// in av1C. The video encoder leaves still_picture 0, which AVIF only recommends.
/// </summary>
internal static class AvifWriter
{
    /// <summary>Colour signalling of the YUV the encoder received: BT.709 primaries and matrix, sRGB transfer, limited range.</summary>
    public const int Primaries709 = 1, TransferSrgb = 13, Matrix709 = 1;

    /// <summary>Wraps an encoded AV1 temporal unit (Section 5 OBUs with size fields). Width and Height are the picture size;
    /// the coded size comes from the sequence header. Null when the unit has no sequence header or no frame.</summary>
    public static byte[]? Write(ReadOnlySpan<byte> temporalUnit, int width, int height)
    {
        var obus = Av1.Split(temporalUnit);
        byte[]? seq = null;
        var item = new MemoryStream(temporalUnit.Length);
        bool frame = false;
        foreach (var o in obus)
        {
            if (o.Type is Av1.ObuTemporalDelimiter or Av1.ObuPadding) continue;
            var bytes = temporalUnit.Slice(o.Start, o.Length);
            if (o.Type == Av1.ObuSequenceHeader) seq ??= bytes.ToArray();
            if (o.Type is 3 or 4 or 6) frame = true; // frame header, tile group, frame
            item.Write(bytes);
        }
        if (seq is null || !frame) return null;
        var info = Av1.ParseSequenceHeader(seq);
        if (info.Mono || info.HighBitdepth) return null;
        int codedW = info.MaxWidth, codedH = info.MaxHeight;
        if (width > codedW || height > codedH) return null;

        var props = new List<byte[]>
        {
            FullBox("ispe", 0, 0, U32(codedW), U32(codedH)),
            Box("av1C", Av1.BuildAv1C(seq)),
            FullBox("pixi", 0, 0, [3, 8, 8, 8]),
            Box("colr", Ascii("nclx"), U16(Primaries709), U16(TransferSrgb), U16(Matrix709), [0x00]), // full_range 0
        };
        bool crop = width != codedW || height != codedH;
        if (crop)
            props.Add(Box("clap", U32(width), U32(1), U32(height), U32(1),
                I32(width - codedW), U32(2), I32(height - codedH), U32(2))); // offsets from the centre
        var assoc = new List<byte> { 0, 1, (byte)props.Count };
        for (int i = 1; i <= props.Count; i++) assoc.Add((byte)(i == 2 ? 0x80 | i : i)); // av1C essential
        if (crop) assoc[^1] |= 0x80;                                                         // clap essential (transformative)
        var ipma = FullBox("ipma", 0, 0, U32(1), [.. assoc]);
        var iprp = Box("iprp", Box("ipco", [.. props.SelectMany(p => p)]), ipma);

        byte[] data = item.ToArray();
        var ftyp = Box("ftyp", Ascii("avif"), U32(0), Ascii("avif"), Ascii("mif1"), Ascii("miaf"));
        byte[] Meta(uint offset) => FullBox("meta", 0, 0,
            FullBox("hdlr", 0, 0, U32(0), Ascii("pict"), U32(0), U32(0), U32(0), [0]),
            FullBox("pitm", 0, 0, U16(1)),
            FullBox("iloc", 0, 0, [0x44, 0x00], U16(1), U16(1), U16(0), U16(1), U32(offset), U32(data.Length)),
            FullBox("iinf", 0, 0, U16(1), FullBox("infe", 2, 0, U16(1), U16(0), Ascii("av01"), [0])),
            iprp);
        int metaLen = Meta(0).Length;
        uint mdatData = (uint)(ftyp.Length + metaLen + 8);
        var mdat = Box("mdat", data);
        var file = new byte[ftyp.Length + metaLen + mdat.Length];
        ftyp.CopyTo(file, 0);
        Meta(mdatData).CopyTo(file, ftyp.Length);
        mdat.CopyTo(file, ftyp.Length + metaLen);
        Array.Clear(data);
        return file;
    }

    private static byte[] Box(string type, params byte[][] parts)
    {
        int len = 8 + parts.Sum(p => p.Length);
        var b = new byte[len];
        BinaryPrimitives.WriteUInt32BigEndian(b, (uint)len);
        Encoding.ASCII.GetBytes(type, b.AsSpan(4));
        int o = 8;
        foreach (var p in parts) { p.CopyTo(b, o); o += p.Length; }
        return b;
    }

    private static byte[] FullBox(string type, byte version, uint flags, params byte[][] parts) =>
        Box(type, [[version, (byte)(flags >> 16), (byte)(flags >> 8), (byte)flags], .. parts]);

    private static byte[] U16(int v) { var b = new byte[2]; BinaryPrimitives.WriteUInt16BigEndian(b, (ushort)v); return b; }
    private static byte[] U32(long v) { var b = new byte[4]; BinaryPrimitives.WriteUInt32BigEndian(b, (uint)v); return b; }
    private static byte[] I32(int v) { var b = new byte[4]; BinaryPrimitives.WriteInt32BigEndian(b, v); return b; }
    private static byte[] Ascii(string s) => Encoding.ASCII.GetBytes(s);
}
