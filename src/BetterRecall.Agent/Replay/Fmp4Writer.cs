// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Text;

namespace BetterRecall.Agent.Replay;

internal enum TrackKind { Video, Audio }

internal sealed record VideoFormat(VideoCodec Codec, int Width, int Height, byte[] Config);

/// <summary>One track to mux. Video: Codec is the sample entry fourcc (av01, hev1, avc1), Config is av1C/hvcC/avcC.
/// Audio: Codec "mp4a", Config is the AudioSpecificConfig.</summary>
internal sealed record MuxTrack(
    TrackKind Kind,
    string Codec,
    VideoCodec? VideoCodec,
    int Width,
    int Height,
    int SampleRate,
    int Channels,
    byte[] Config,
    long DefaultDuration100ns,
    RingSnapshot Samples,
    IReadOnlyDictionary<int, VideoFormat>? Formats = null,
    int Fps = 0);

/// <summary>
/// Fragmented MP4 (ISO/IEC 14496-12): ftyp, moov (with mvex), then one moof+mdat per track per segment.
/// Segments follow the first video track's keyframes. No re-encoding: samples are copied, with only
/// container-level rewrites (AV1 temporal delimiters dropped, Annex B start codes to length prefixes).
/// </summary>
internal static unsafe class Fmp4Writer
{
    public const int VideoTimescale = 90_000;

    public static int VideoTimescaleFor(int fps)
    {
        if (fps <= 0 || 90_000 % fps == 0) return 90_000;
        long g = Gcd(90_000, fps);
        return (int)((90_000L / g) * fps);
    }

    private static long Gcd(long a, long b)
    {
        while (b != 0) { long t = b; b = a % b; a = t; }
        return a;
    }

    private static IReadOnlyDictionary<int, VideoFormat> EffectiveFormats(MuxTrack t)
    {
        if (t.Formats != null) return t.Formats;
        if (t.VideoCodec.HasValue) return new Dictionary<int, VideoFormat> { [0] = new(t.VideoCodec.Value, t.Width, t.Height, t.Config) };
        return new Dictionary<int, VideoFormat>();
    }

    /// <summary>Returns the duration in milliseconds that was written.</summary>
    public static int Write(Stream output, IReadOnlyList<MuxTrack> tracks)
    {
        if (tracks.Count == 0) throw new ArgumentException("No tracks.");
        // Common start: the latest first-keyframe across video tracks, so every video track starts decodable.
        long start = long.MinValue;
        foreach (var t in tracks)
        {
            if (t.Kind != TrackKind.Video || t.Samples.Packets.Count == 0) continue;
            var fm = EffectiveFormats(t);
            for (int k = 0; k < t.Samples.Packets.Count; k++)
            {
                if (fm.ContainsKey(t.Samples.Packets[k].Format))
                {
                    start = Math.Max(start, t.Samples.Packets[k].Pts);
                    break;
                }
            }
        }
        if (start == long.MinValue) start = tracks.Where(t => t.Samples.Packets.Count > 0).Select(t => t.Samples.Packets[0].Pts).DefaultIfEmpty(0).Min();

        var plans = new List<TrackPlan>();
        for (int i = 0; i < tracks.Count; i++)
        {
            var t = tracks[i];
            var plan = new TrackPlan(i + 1, t, t.Kind == TrackKind.Video ? VideoTimescaleFor(t.Fps) : t.SampleRate, EffectiveFormats(t));
            var pk = t.Samples.Packets;
            int first = 0;
            if (t.Kind == TrackKind.Video)
            {
                // First keyframe at or after the common start.
                first = -1;
                for (int k = 0; k < pk.Count; k++)
                {
                    if (!plan.FormatMap.ContainsKey(pk[k].Format)) continue;
                    if (pk[k].Key && pk[k].Pts >= start) { first = k; break; }
                }
                if (first < 0) continue;
            }
            else
            {
                while (first < pk.Count && pk[first].Pts < start) first++;
            }
            for (int k = first; k < pk.Count; k++)
            {
                if (t.Kind == TrackKind.Video && !plan.FormatMap.ContainsKey(pk[k].Format)) continue;
                plan.Indices.Add(k);
                if (t.Kind == TrackKind.Video)
                {
                    int fmtId = pk[k].Format;
                    if (!plan.FormatIndices.ContainsKey(fmtId))
                    {
                        plan.UsedFormats.Add(fmtId);
                        plan.FormatIndices[fmtId] = (uint)plan.UsedFormats.Count;
                    }
                }
            }
            if (plan.Indices.Count == 0) continue;
            // Times in track timescale relative to start; durations from neighbours, last one default.
            plan.Times = new long[plan.Indices.Count];
            for (int k = 0; k < plan.Indices.Count; k++)
                plan.Times[k] = ToScale(pk[plan.Indices[k]].Pts - start, plan.Timescale);
            plan.Durations = new uint[plan.Indices.Count];
            for (int k = 0; k < plan.Indices.Count; k++)
            {
                long d = k + 1 < plan.Indices.Count ? plan.Times[k + 1] - plan.Times[k] : ToScale(t.DefaultDuration100ns, plan.Timescale);
                plan.Durations[k] = (uint)Math.Max(1, d);
            }
            plans.Add(plan);
        }
        if (plans.Count == 0) throw new InvalidOperationException("Nothing to write.");
        // Renumber track ids densely.
        for (int i = 0; i < plans.Count; i++) plans[i].TrackId = i + 1;

        long durationMs = 0;
        foreach (var p in plans)
        {
            long end = p.Times[^1] + p.Durations[^1];
            durationMs = Math.Max(durationMs, end * 1000 / p.Timescale);
        }

        WriteBytes(output, BuildFtyp(plans));
        WriteBytes(output, BuildMoov(plans, durationMs));

        // Segment boundaries in 100 ns from the first video plan's keyframes.
        var lead = plans.FirstOrDefault(p => p.Track.Kind == TrackKind.Video) ?? plans[0];
        var bounds = new List<long>();
        var lpk = lead.Track.Samples.Packets;
        foreach (var idx in lead.Indices)
            if (lpk[idx].Key || lead.Track.Kind == TrackKind.Audio) bounds.Add(lpk[idx].Pts);
        if (lead.Track.Kind == TrackKind.Audio)
        {
            // Audio-only: one segment per ~second.
            var b2 = new List<long>();
            foreach (var b in bounds) if (b2.Count == 0 || b - b2[^1] >= 10_000_000) b2.Add(b);
            bounds = b2;
        }
        bounds[0] = long.MinValue;

        uint sequence = 1;
        int maxSample = tracks.SelectMany(t => t.Samples.Packets).Select(p => p.Length).DefaultIfEmpty(0).Max();
        byte* scratch = (byte*)NativeMemory.Alloc((nuint)Math.Max(1, maxSample * 2 + 64));
        try
        {
            for (int s = 0; s < bounds.Count; s++)
            {
                long segStart = bounds[s];
                long segEnd = s + 1 < bounds.Count ? bounds[s + 1] : long.MaxValue;
                foreach (var p in plans)
                {
                    var pk = p.Track.Samples.Packets;
                    int from = p.Cursor;
                    int to = from;
                    while (to < p.Indices.Count && pk[p.Indices[to]].Pts < segEnd && pk[p.Indices[to]].Pts >= segStart) to++;
                    // Samples before segStart (should not happen after the first segment) are pulled into this one.
                    while (to < p.Indices.Count && pk[p.Indices[to]].Pts < segStart) to++;
                    if (to == from) continue;
                    if (p.Track.Kind == TrackKind.Video)
                    {
                        int runStart = from;
                        while (runStart < to)
                        {
                            int fmt = pk[p.Indices[runStart]].Format;
                            int runEnd = runStart + 1;
                            while (runEnd < to && pk[p.Indices[runEnd]].Format == fmt) runEnd++;
                            WriteFragment(output, p, runStart, runEnd, sequence++, scratch, maxSample * 2 + 64);
                            runStart = runEnd;
                        }
                    }
                    else
                    {
                        WriteFragment(output, p, from, to, sequence++, scratch, maxSample * 2 + 64);
                    }
                    p.Cursor = to;
                }
            }
            // Anything left (timestamps beyond the last boundary on other tracks).
            foreach (var p in plans)
                if (p.Cursor < p.Indices.Count)
                {
                    if (p.Track.Kind == TrackKind.Video)
                    {
                        var pk = p.Track.Samples.Packets;
                        int runStart = p.Cursor;
                        int to = p.Indices.Count;
                        while (runStart < to)
                        {
                            int fmt = pk[p.Indices[runStart]].Format;
                            int runEnd = runStart + 1;
                            while (runEnd < to && pk[p.Indices[runEnd]].Format == fmt) runEnd++;
                            WriteFragment(output, p, runStart, runEnd, sequence++, scratch, maxSample * 2 + 64);
                            runStart = runEnd;
                        }
                    }
                    else
                    {
                        WriteFragment(output, p, p.Cursor, p.Indices.Count, sequence++, scratch, maxSample * 2 + 64);
                    }
                    p.Cursor = p.Indices.Count;
                }
        }
        finally
        {
            NativeMemory.Clear(scratch, (nuint)Math.Max(1, maxSample * 2 + 64));
            NativeMemory.Free(scratch);
        }
        return (int)Math.Min(int.MaxValue, durationMs);
    }

    private static long ToScale(long ticks100ns, int timescale) => (long)Math.Round(ticks100ns * (double)timescale / 10_000_000.0);

    private sealed class TrackPlan(int trackId, MuxTrack track, int timescale, IReadOnlyDictionary<int, VideoFormat> formatMap)
    {
        public int TrackId = trackId;
        public readonly MuxTrack Track = track;
        public readonly int Timescale = timescale;
        public readonly IReadOnlyDictionary<int, VideoFormat> FormatMap = formatMap;
        public readonly List<int> Indices = [];
        public readonly List<int> UsedFormats = [];
        public readonly Dictionary<int, uint> FormatIndices = [];
        public long[] Times = [];
        public uint[] Durations = [];
        public int Cursor;
    }

    private static void WriteFragment(Stream output, TrackPlan p, int from, int to, uint sequence, byte* scratch, int scratchSize)
    {
        var pk = p.Track.Samples.Packets;
        int n = to - from;
        var sizes = new int[n];
        VideoCodec? fragCodec = p.Track.Kind == TrackKind.Video ? p.FormatMap[pk[p.Indices[from]].Format].Codec : null;
        // Transform pass 1: compute sizes (transform into scratch and keep only the size).
        for (int i = 0; i < n; i++)
            sizes[i] = Transform(fragCodec, p.Track.Samples.Data(pk[p.Indices[from + i]]), new Span<byte>(scratch, scratchSize));

        var moof = new BoxBuilder();
        int moofStart = moof.Begin("moof");
        moof.FullBox("mfhd", 0, 0, b => b.U32(sequence));
        int trafStart = moof.Begin("traf");
        uint sampleDescIndex = p.Track.Kind == TrackKind.Audio ? 1 : p.FormatIndices[pk[p.Indices[from]].Format];
        moof.FullBox("tfhd", 0, 0x020000 | 0x000002, b =>
        {
            b.U32((uint)p.TrackId);
            b.U32(sampleDescIndex);
        });
        moof.FullBox("tfdt", 1, 0, b => b.U64((ulong)Math.Max(0, p.Times[from])));
        const uint TrunFlags = 0x000001 | 0x000100 | 0x000200 | 0x000400;
        int trunStart = moof.Begin("trun", 0, TrunFlags);
        moof.U32((uint)n);
        int dataOffsetPos = moof.Position;
        moof.U32(0); // patched
        for (int i = 0; i < n; i++)
        {
            var pkt = pk[p.Indices[from + i]];
            moof.U32(p.Durations[from + i]);
            moof.U32((uint)sizes[i]);
            uint flags = p.Track.Kind == TrackKind.Audio || pkt.Key ? 0x02000000u : 0x01010000u;
            moof.U32(flags);
        }
        moof.End(trunStart);
        moof.End(trafStart);
        moof.End(moofStart);
        var moofBytes = moof.ToArray();
        BinaryPrimitives.WriteUInt32BigEndian(moofBytes.AsSpan(dataOffsetPos), (uint)(moofBytes.Length + 8));
        WriteBytes(output, moofBytes);

        long mdatSize = 8;
        foreach (var s in sizes) mdatSize += s;
        Span<byte> hdr = stackalloc byte[8];
        BinaryPrimitives.WriteUInt32BigEndian(hdr, (uint)mdatSize);
        "mdat"u8.CopyTo(hdr[4..]);
        output.Write(hdr);
        for (int i = 0; i < n; i++)
        {
            int len = Transform(fragCodec, p.Track.Samples.Data(pk[p.Indices[from + i]]), new Span<byte>(scratch, scratchSize));
            output.Write(new ReadOnlySpan<byte>(scratch, len));
        }
    }

    private static int Transform(VideoCodec? codec, ReadOnlySpan<byte> src, Span<byte> dst)
    {
        switch (codec)
        {
            case VideoCodec.Av1: return Av1.StripForMp4(src, dst);
            case VideoCodec.H264:
            case VideoCodec.Hevc: return AnnexB.ToLengthPrefixed(src, dst, codec.Value);
            default:
                src.CopyTo(dst);
                return src.Length;
        }
    }

    private static void WriteBytes(Stream s, byte[] b) => s.Write(b, 0, b.Length);

    private static byte[] BuildFtyp(List<TrackPlan> plans)
    {
        var b = new BoxBuilder();
        int f = b.Begin("ftyp");
        b.Fourcc("iso6");
        b.U32(0);
        b.Fourcc("iso6"); b.Fourcc("iso5"); b.Fourcc("isom"); b.Fourcc("mp41");
        bool hasAv1 = plans.Any(p => p.Track.Kind == TrackKind.Video && p.UsedFormats.Any(fmtId => p.FormatMap.TryGetValue(fmtId, out var fmt) && fmt.Codec == VideoCodec.Av1));
        if (hasAv1) b.Fourcc("av01");
        b.End(f);
        return b.ToArray();
    }

    private static byte[] BuildMoov(List<TrackPlan> plans, long durationMs)
    {
        var b = new BoxBuilder();
        int moov = b.Begin("moov");
        b.FullBox("mvhd", 0, 0, x =>
        {
            x.U32(0); x.U32(0);
            x.U32(1000);
            x.U32((uint)durationMs);
            x.U32(0x00010000); // rate
            x.U16(0x0100);     // volume
            x.U16(0); x.U32(0); x.U32(0);
            Matrix(x);
            for (int i = 0; i < 6; i++) x.U32(0);
            x.U32((uint)(plans.Count + 1));
        });
        foreach (var p in plans) Trak(b, p, durationMs);
        int mvex = b.Begin("mvex");
        b.FullBox("mehd", 0, 0, x => x.U32((uint)durationMs));
        foreach (var p in plans)
            b.FullBox("trex", 0, 0, x => { x.U32((uint)p.TrackId); x.U32(1); x.U32(0); x.U32(0); x.U32(0); });
        b.End(mvex);
        b.End(moov);
        return b.ToArray();
    }

    private static void Matrix(BoxBuilder x)
    {
        x.U32(0x00010000); x.U32(0); x.U32(0);
        x.U32(0); x.U32(0x00010000); x.U32(0);
        x.U32(0); x.U32(0); x.U32(0x40000000);
    }

    private static void Trak(BoxBuilder b, TrackPlan p, long durationMs)
    {
        var t = p.Track;
        bool video = t.Kind == TrackKind.Video;
        int trackW = 0, trackH = 0;
        if (video)
        {
            var lastPkt = p.Track.Samples.Packets[p.Indices[^1]];
            if (p.FormatMap.TryGetValue(lastPkt.Format, out var lastFmt))
            {
                trackW = lastFmt.Width;
                trackH = lastFmt.Height;
            }
        }
        int trak = b.Begin("trak");
        b.FullBox("tkhd", 0, 3, x =>
        {
            x.U32(0); x.U32(0);
            x.U32((uint)p.TrackId);
            x.U32(0);
            x.U32((uint)durationMs);
            x.U32(0); x.U32(0);
            x.U16(0); // layer
            x.U16(0); // alternate group
            x.U16(video ? (ushort)0 : (ushort)0x0100);
            x.U16(0);
            Matrix(x);
            x.U32(video ? (uint)trackW << 16 : 0);
            x.U32(video ? (uint)trackH << 16 : 0);
        });
        int mdia = b.Begin("mdia");
        b.FullBox("mdhd", 0, 0, x =>
        {
            x.U32(0); x.U32(0);
            x.U32((uint)p.Timescale);
            x.U32(0);
            x.U16(0x55C4); // 'und'
            x.U16(0);
        });
        b.FullBox("hdlr", 0, 0, x =>
        {
            x.U32(0);
            x.Fourcc(video ? "vide" : "soun");
            x.U32(0); x.U32(0); x.U32(0);
            x.Bytes(Encoding.ASCII.GetBytes(video ? "BetterRecall video\0" : "BetterRecall audio\0"));
        });
        int minf = b.Begin("minf");
        if (video) b.FullBox("vmhd", 0, 1, x => { x.U16(0); x.U16(0); x.U16(0); x.U16(0); });
        else b.FullBox("smhd", 0, 0, x => { x.U16(0); x.U16(0); });
        int dinf = b.Begin("dinf");
        b.FullBox("dref", 0, 0, x =>
        {
            x.U32(1);
            x.FullBox("url ", 0, 1, _ => { });
        });
        b.End(dinf);
        int stbl = b.Begin("stbl");
        b.FullBox("stsd", 0, 0, x =>
        {
            if (video)
            {
                x.U32((uint)p.UsedFormats.Count);
                foreach (int fmtId in p.UsedFormats)
                {
                    VisualEntry(x, p.FormatMap[fmtId]);
                }
            }
            else
            {
                x.U32(1);
                AudioEntry(x, t);
            }
        });
        b.FullBox("stts", 0, 0, x => x.U32(0));
        b.FullBox("stsc", 0, 0, x => x.U32(0));
        b.FullBox("stsz", 0, 0, x => { x.U32(0); x.U32(0); });
        b.FullBox("stco", 0, 0, x => x.U32(0));
        b.End(stbl);
        b.End(minf);
        b.End(mdia);
        b.End(trak);
    }

    private static void VisualEntry(BoxBuilder x, VideoFormat fmt)
    {
        string fourcc = fmt.Codec switch { VideoCodec.Av1 => "av01", VideoCodec.Hevc => "hev1", _ => "avc1" };
        int e = x.Begin(fourcc);
        x.Zero(6); x.U16(1);
        x.U16(0); x.U16(0); x.Zero(12);
        x.U16((ushort)fmt.Width); x.U16((ushort)fmt.Height);
        x.U32(0x00480000); x.U32(0x00480000);
        x.U32(0);
        x.U16(1);
        var name = new byte[32];
        var label = "BetterRecall"u8;
        name[0] = (byte)label.Length;
        label.CopyTo(name.AsSpan(1));
        x.Bytes(name);
        x.U16(0x0018);
        x.U16(0xFFFF);
        string cfg = fmt.Codec switch { VideoCodec.Av1 => "av1C", VideoCodec.Hevc => "hvcC", _ => "avcC" };
        int c = x.Begin(cfg);
        x.Bytes(fmt.Config);
        x.End(c);
        x.End(e);
    }

    private static void AudioEntry(BoxBuilder x, MuxTrack t)
    {
        int e = x.Begin("mp4a");
        x.Zero(6); x.U16(1);
        x.U32(0); x.U32(0);
        x.U16((ushort)t.Channels);
        x.U16(16);
        x.U16(0); x.U16(0);
        x.U32((uint)t.SampleRate << 16);
        x.FullBox("esds", 0, 0, z =>
        {
            var asc = t.Config;
            var dsi = Descriptor(0x05, asc);
            var dcd = new List<byte> { 0x40, 0x15, 0, 0x18, 0 }; // AAC, audio stream, 6144 byte buffer
            dcd.AddRange(new byte[] { 0, 0x02, 0xEE, 0x00 });     // max bitrate 192000
            dcd.AddRange(new byte[] { 0, 0x02, 0xEE, 0x00 });     // avg bitrate
            dcd.AddRange(dsi);
            var es = new List<byte> { 0, 1, 0 };                  // ES_ID 1, flags 0
            es.AddRange(Descriptor(0x04, dcd.ToArray()));
            es.AddRange(Descriptor(0x06, [0x02]));
            z.Bytes(Descriptor(0x03, es.ToArray()));
        });
        x.End(e);
    }

    private static byte[] Descriptor(byte tag, byte[] body)
    {
        var o = new List<byte> { tag };
        int len = body.Length;
        o.Add((byte)(0x80 | ((len >> 21) & 0x7F)));
        o.Add((byte)(0x80 | ((len >> 14) & 0x7F)));
        o.Add((byte)(0x80 | ((len >> 7) & 0x7F)));
        o.Add((byte)(len & 0x7F));
        o.AddRange(body);
        return o.ToArray();
    }
}

/// <summary>Big-endian box builder for metadata boxes (no captured content passes through it).</summary>
internal sealed class BoxBuilder
{
    private readonly MemoryStream _ms = new();
    public int Position => (int)_ms.Position;

    public int Begin(string type)
    {
        int start = Position;
        U32(0);
        Fourcc(type);
        return start;
    }

    public int Begin(string type, byte version, uint flags)
    {
        int start = Begin(type);
        U32(((uint)version << 24) | (flags & 0xFFFFFF));
        return start;
    }

    public void End(int start)
    {
        long end = _ms.Position;
        _ms.Position = start;
        U32((uint)(end - start));
        _ms.Position = end;
    }

    public void FullBox(string type, byte version, uint flags, Action<BoxBuilder> body)
    {
        int s = Begin(type, version, flags);
        body(this);
        End(s);
    }

    public void U16(ushort v) { Span<byte> b = stackalloc byte[2]; BinaryPrimitives.WriteUInt16BigEndian(b, v); _ms.Write(b); }
    public void U32(uint v) { Span<byte> b = stackalloc byte[4]; BinaryPrimitives.WriteUInt32BigEndian(b, v); _ms.Write(b); }
    public void U64(ulong v) { Span<byte> b = stackalloc byte[8]; BinaryPrimitives.WriteUInt64BigEndian(b, v); _ms.Write(b); }
    public void Fourcc(string s) { Span<byte> b = stackalloc byte[4]; Encoding.ASCII.GetBytes(s.AsSpan(0, 4), b); _ms.Write(b); }
    public void Bytes(ReadOnlySpan<byte> b) => _ms.Write(b);
    public void Zero(int n) { for (int i = 0; i < n; i++) _ms.WriteByte(0); }
    public byte[] ToArray() => _ms.ToArray();
}
