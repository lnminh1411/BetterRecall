// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Replay;

internal enum VideoCodec { Av1, Hevc, H264 }

/// <summary>MSB-first bit reader for codec headers.</summary>
internal ref struct BitReader
{
    private readonly ReadOnlySpan<byte> _data;
    private int _bit;

    public BitReader(ReadOnlySpan<byte> data) { _data = data; _bit = 0; }

    public int BitsLeft => _data.Length * 8 - _bit;

    public uint Read(int n)
    {
        uint v = 0;
        for (int i = 0; i < n; i++)
        {
            if (_bit >= _data.Length * 8) throw new InvalidDataException("Bitstream ended early.");
            int b = (_data[_bit >> 3] >> (7 - (_bit & 7))) & 1;
            v = (v << 1) | (uint)b;
            _bit++;
        }
        return v;
    }

    public bool Flag() => Read(1) == 1;
    public void Skip(int n) { for (int i = 0; i < n; i++) Read(1); }

    /// <summary>Exp-Golomb ue(v) (H.264/HEVC).</summary>
    public uint Ue()
    {
        int zeros = 0;
        while (Read(1) == 0) { zeros++; if (zeros > 31) throw new InvalidDataException("Bad ue(v)."); }
        return zeros == 0 ? 0 : ((1u << zeros) - 1) + Read(zeros);
    }

    /// <summary>AV1 uvlc().</summary>
    public uint Uvlc()
    {
        int leading = 0;
        while (Read(1) == 0) { leading++; if (leading >= 32) return uint.MaxValue; }
        return leading == 0 ? 0 : Read(leading) + ((1u << leading) - 1);
    }
}

/// <summary>AV1 OBU helpers and the av1C record (AV1-ISOBMFF section 2.3).</summary>
internal static class Av1
{
    public const int ObuSequenceHeader = 1, ObuTemporalDelimiter = 2, ObuPadding = 15;

    public readonly record struct Obu(int Type, int Start, int Length, int PayloadStart, int PayloadLength);

    public static List<Obu> Split(ReadOnlySpan<byte> data)
    {
        var list = new List<Obu>();
        int pos = 0;
        while (pos < data.Length)
        {
            int start = pos;
            byte h = data[pos++];
            int type = (h >> 3) & 0xF;
            bool ext = (h & 0x04) != 0;
            bool hasSize = (h & 0x02) != 0;
            if (ext) pos++;
            long size;
            if (hasSize)
            {
                size = 0;
                for (int i = 0; i < 8; i++)
                {
                    if (pos >= data.Length) throw new InvalidDataException("Truncated leb128.");
                    byte b = data[pos++];
                    size |= (long)(b & 0x7F) << (7 * i);
                    if ((b & 0x80) == 0) break;
                }
            }
            else size = data.Length - pos;
            if (pos + size > data.Length) throw new InvalidDataException("Truncated OBU.");
            list.Add(new Obu(type, start, (int)(pos + size - start), pos, (int)size));
            pos += (int)size;
        }
        return list;
    }

    /// <summary>Copies the temporal unit without temporal delimiter and padding OBUs (MP4 sample format).</summary>
    public static int StripForMp4(ReadOnlySpan<byte> src, Span<byte> dst)
    {
        int w = 0;
        foreach (var o in Split(src))
        {
            if (o.Type is ObuTemporalDelimiter or ObuPadding) continue;
            src.Slice(o.Start, o.Length).CopyTo(dst[w..]);
            w += o.Length;
        }
        return w;
    }

    /// <summary>Finds the sequence header OBU (with header bytes) in a temporal unit.</summary>
    public static byte[]? FindSequenceHeader(ReadOnlySpan<byte> tu)
    {
        foreach (var o in Split(tu))
            if (o.Type == ObuSequenceHeader) return tu.Slice(o.Start, o.Length).ToArray();
        return null;
    }

    public sealed record SeqInfo(int Profile, int Level, int Tier, bool HighBitdepth, bool TwelveBit, bool Mono, int SubX, int SubY, int ChromaSamplePosition, int MaxWidth, int MaxHeight);

    public static SeqInfo ParseSequenceHeader(byte[] obu)
    {
        var o = Split(obu)[0];
        var r = new BitReader(obu.AsSpan(o.PayloadStart, o.PayloadLength));
        int profile = (int)r.Read(3);
        r.Read(1); // still_picture
        bool reduced = r.Flag();
        int level0 = 0, tier0 = 0;
        if (reduced)
        {
            level0 = (int)r.Read(5);
        }
        else
        {
            bool timing = r.Flag();
            bool decoderModel = false;
            int bufferDelayLen = 0;
            if (timing)
            {
                r.Read(32); r.Read(32);
                if (r.Flag()) r.Uvlc();
                decoderModel = r.Flag();
                if (decoderModel)
                {
                    bufferDelayLen = (int)r.Read(5) + 1;
                    r.Read(32);
                    r.Read(5); r.Read(5);
                }
            }
            bool initialDisplayDelay = r.Flag();
            int opCount = (int)r.Read(5) + 1;
            for (int i = 0; i < opCount; i++)
            {
                r.Read(12);
                int lvl = (int)r.Read(5);
                int tier = lvl > 7 ? (int)r.Read(1) : 0;
                if (decoderModel && r.Flag()) { r.Read(bufferDelayLen); r.Read(bufferDelayLen); r.Read(1); }
                if (initialDisplayDelay && r.Flag()) r.Read(4);
                if (i == 0) { level0 = lvl; tier0 = tier; }
            }
        }
        int wBits = (int)r.Read(4) + 1, hBits = (int)r.Read(4) + 1;
        int maxW = (int)r.Read(wBits) + 1, maxH = (int)r.Read(hBits) + 1;
        bool orderHint = false;
        if (!reduced && r.Flag()) { r.Read(4); r.Read(3); }
        r.Read(3); // 128x128 sb, filter intra, intra edge
        if (!reduced)
        {
            r.Read(4); // interintra, masked, warped, dual filter
            orderHint = r.Flag();
            if (orderHint) r.Read(2);
            int forceSct = r.Flag() ? 2 : (int)r.Read(1);
            if (forceSct > 0) { if (!r.Flag()) r.Read(1); }
            if (orderHint) r.Read(3);
        }
        r.Read(3); // superres, cdef, restoration
        bool high = r.Flag();
        bool twelve = profile == 2 && high && r.Flag();
        bool mono = profile != 1 && r.Flag();
        int cp = 2, tc = 2, mc = 2;
        if (r.Flag()) { cp = (int)r.Read(8); tc = (int)r.Read(8); mc = (int)r.Read(8); }
        int sx = 1, sy = 1, csp = 0;
        if (mono) { r.Read(1); }
        else if (cp == 1 && tc == 13 && mc == 0) { sx = 0; sy = 0; }
        else
        {
            r.Read(1); // color_range
            if (profile == 0) { sx = 1; sy = 1; }
            else if (profile == 1) { sx = 0; sy = 0; }
            else if (twelve) { sx = (int)r.Read(1); sy = sx == 1 ? (int)r.Read(1) : 0; }
            else { sx = 1; sy = 0; }
            if (sx == 1 && sy == 1) csp = (int)r.Read(2);
        }
        return new SeqInfo(profile, level0, tier0, high, twelve, mono, sx, sy, csp, maxW, maxH);
    }

    public static byte[] BuildAv1C(byte[] seqHeaderObu)
    {
        var s = ParseSequenceHeader(seqHeaderObu);
        var c = new byte[4 + seqHeaderObu.Length];
        c[0] = 0x81; // marker 1, version 1
        c[1] = (byte)((s.Profile << 5) | (s.Level & 0x1F));
        c[2] = (byte)((s.Tier << 7) | ((s.HighBitdepth ? 1 : 0) << 6) | ((s.TwelveBit ? 1 : 0) << 5) | ((s.Mono ? 1 : 0) << 4)
                      | (s.SubX << 3) | (s.SubY << 2) | (s.ChromaSamplePosition & 3));
        c[3] = 0; // no initial_presentation_delay
        seqHeaderObu.CopyTo(c, 4);
        return c;
    }
}

/// <summary>Annex B NAL helpers for H.264 and HEVC.</summary>
internal static class AnnexB
{
    public readonly record struct Nal(int Start, int Length);

    /// <summary>NAL units without start codes.</summary>
    public static List<Nal> Split(ReadOnlySpan<byte> data)
    {
        var list = new List<Nal>();
        int i = 0, n = data.Length;
        int start = -1;
        while (i + 2 < n)
        {
            if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1)
            {
                if (start >= 0) list.Add(new Nal(start, TrimEnd(data, start, i)));
                i += 3;
                start = i;
                continue;
            }
            i++;
        }
        if (start >= 0 && start < n) list.Add(new Nal(start, n - start));
        else if (start < 0 && n > 0) list.Add(new Nal(0, n)); // already length-free single NAL
        return list;
    }

    private static int TrimEnd(ReadOnlySpan<byte> d, int start, int end)
    {
        // Drop trailing zero bytes that belong to a 4-byte start code.
        while (end > start && d[end - 1] == 0) end--;
        return end - start;
    }

    public static int H264Type(ReadOnlySpan<byte> nal) => nal[0] & 0x1F;
    public static int HevcType(ReadOnlySpan<byte> nal) => (nal[0] >> 1) & 0x3F;

    /// <summary>Annex B to 4-byte length prefixes, dropping access unit delimiters. Returns bytes written.</summary>
    public static int ToLengthPrefixed(ReadOnlySpan<byte> src, Span<byte> dst, VideoCodec codec)
    {
        int w = 0;
        foreach (var nal in Split(src))
        {
            var body = src.Slice(nal.Start, nal.Length);
            if (body.Length == 0) continue;
            if (codec == VideoCodec.H264 && H264Type(body) == 9) continue;
            if (codec == VideoCodec.Hevc && HevcType(body) == 35) continue;
            dst[w++] = (byte)(body.Length >> 24);
            dst[w++] = (byte)(body.Length >> 16);
            dst[w++] = (byte)(body.Length >> 8);
            dst[w++] = (byte)body.Length;
            body.CopyTo(dst[w..]);
            w += body.Length;
        }
        return w;
    }

    /// <summary>Removes emulation prevention bytes (00 00 03).</summary>
    public static byte[] Rbsp(ReadOnlySpan<byte> nal)
    {
        var o = new List<byte>(nal.Length);
        int zeros = 0;
        foreach (var b in nal)
        {
            if (zeros >= 2 && b == 3) { zeros = 0; continue; }
            o.Add(b);
            zeros = b == 0 ? zeros + 1 : 0;
        }
        return o.ToArray();
    }

    public static byte[]? BuildAvcC(ReadOnlySpan<byte> keyframe)
    {
        byte[]? sps = null, pps = null;
        foreach (var nal in Split(keyframe))
        {
            var body = keyframe.Slice(nal.Start, nal.Length);
            if (body.Length == 0) continue;
            int t = H264Type(body);
            if (t == 7 && sps is null) sps = body.ToArray();
            else if (t == 8 && pps is null) pps = body.ToArray();
        }
        if (sps is null || pps is null || sps.Length < 4) return null;
        var c = new List<byte> { 1, sps[1], sps[2], sps[3], 0xFF, 0xE1, (byte)(sps.Length >> 8), (byte)sps.Length };
        c.AddRange(sps);
        c.Add(1);
        c.Add((byte)(pps.Length >> 8));
        c.Add((byte)pps.Length);
        c.AddRange(pps);
        if (sps[1] is 100 or 110 or 122 or 144)
        {
            c.Add(0xFC | 1);  // chroma_format_idc 4:2:0
            c.Add(0xF8);      // bit_depth_luma_minus8 0
            c.Add(0xF8);      // bit_depth_chroma_minus8 0
            c.Add(0);         // numOfSequenceParameterSetExt
        }
        return c.ToArray();
    }

    public static byte[]? BuildHvcC(ReadOnlySpan<byte> keyframe)
    {
        byte[]? vps = null, sps = null, pps = null;
        foreach (var nal in Split(keyframe))
        {
            var body = keyframe.Slice(nal.Start, nal.Length);
            if (body.Length < 2) continue;
            int t = HevcType(body);
            if (t == 32 && vps is null) vps = body.ToArray();
            else if (t == 33 && sps is null) sps = body.ToArray();
            else if (t == 34 && pps is null) pps = body.ToArray();
        }
        if (vps is null || sps is null || pps is null) return null;

        var rbsp = Rbsp(sps.AsSpan(2));
        var r = new BitReader(rbsp);
        r.Read(4); // sps_video_parameter_set_id
        int maxSubLayersMinus1 = (int)r.Read(3);
        bool temporalIdNested = r.Flag();
        int profileSpace = (int)r.Read(2);
        int tier = (int)r.Read(1);
        int profileIdc = (int)r.Read(5);
        uint compat = r.Read(32);
        ulong constraint = ((ulong)r.Read(16) << 32) | r.Read(32);
        int levelIdc = (int)r.Read(8);
        // Sub-layer profile/level info.
        var subProfile = new bool[8];
        var subLevel = new bool[8];
        for (int i = 0; i < maxSubLayersMinus1; i++) { subProfile[i] = r.Flag(); subLevel[i] = r.Flag(); }
        if (maxSubLayersMinus1 > 0) for (int i = maxSubLayersMinus1; i < 8; i++) r.Read(2);
        for (int i = 0; i < maxSubLayersMinus1; i++)
        {
            if (subProfile[i]) { r.Read(32); r.Read(32); r.Read(24); }
            if (subLevel[i]) r.Read(8);
        }
        r.Ue(); // sps_seq_parameter_set_id
        int chroma = (int)r.Ue();
        if (chroma == 3) r.Read(1);
        r.Ue(); r.Ue(); // width, height
        if (r.Flag()) { r.Ue(); r.Ue(); r.Ue(); r.Ue(); }
        int bitDepthLuma = (int)r.Ue();
        int bitDepthChroma = (int)r.Ue();

        var c = new List<byte>
        {
            1,
            (byte)((profileSpace << 6) | (tier << 5) | profileIdc),
            (byte)(compat >> 24), (byte)(compat >> 16), (byte)(compat >> 8), (byte)compat,
            (byte)(constraint >> 40), (byte)(constraint >> 32), (byte)(constraint >> 24),
            (byte)(constraint >> 16), (byte)(constraint >> 8), (byte)constraint,
            (byte)levelIdc,
            0xF0, 0x00,                       // min_spatial_segmentation_idc 0
            0xFC,                             // parallelismType 0
            (byte)(0xFC | chroma),
            (byte)(0xF8 | bitDepthLuma),
            (byte)(0xF8 | bitDepthChroma),
            0, 0,                             // avgFrameRate
            (byte)((0 << 6) | ((maxSubLayersMinus1 + 1) << 3) | ((temporalIdNested ? 1 : 0) << 2) | 3),
            3,                                // numOfArrays
        };
        foreach (var (type, nal) in new[] { (32, vps), (33, sps), (34, pps) })
        {
            c.Add((byte)(0x80 | type));
            c.Add(0); c.Add(1);
            c.Add((byte)(nal.Length >> 8)); c.Add((byte)nal.Length);
            c.AddRange(nal);
        }
        return c.ToArray();
    }
}
