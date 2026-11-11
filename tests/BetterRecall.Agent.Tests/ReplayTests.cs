// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.Text;
using BetterRecall.Agent.Replay;

namespace BetterRecall.Agent.Tests;

public class PacketRingTests
{
    private const long Sec = 10_000_000;

    private static byte[] Packet(int len, byte fill) => Enumerable.Repeat(fill, len).ToArray();

    [Fact]
    public void Cut_starts_at_last_keyframe_at_or_before_request()
    {
        using var ring = new PacketRing(1 << 20, TimeSpan.FromSeconds(60), lockMemory: false);
        // 1 s GOPs at 4 fps: keyframes at 0, 1, 2, 3 s.
        for (int i = 0; i < 16; i++) ring.Write(Packet(100, (byte)i), i * Sec / 4, key: i % 4 == 0);
        using (var snap = ring.Snapshot(fromPts: 2 * Sec + Sec / 2))
        {
            Assert.Equal(2 * Sec, snap.Packets[0].Pts);
            Assert.True(snap.Packets[0].Key);
            Assert.Equal(8, snap.Packets.Count);
            Assert.Equal((byte)8, snap.Data(snap.Packets[0])[0]);
            Assert.Equal((byte)15, snap.Data(snap.Packets[^1])[0]);
        }
        // Request before the oldest packet: start at the first keyframe.
        using (var snap = ring.Snapshot(-5 * Sec)) Assert.Equal(0, snap.Packets[0].Pts);
        // Exactly on a keyframe.
        using (var snap = ring.Snapshot(Sec)) Assert.Equal(Sec, snap.Packets[0].Pts);
    }

    [Fact]
    public void Time_eviction_keeps_whole_gops()
    {
        using var ring = new PacketRing(1 << 20, TimeSpan.FromSeconds(2), lockMemory: false);
        for (int i = 0; i < 40; i++) ring.Write(Packet(50, 1), i * Sec / 4, key: i % 4 == 0);
        // Newest is 9.75 s; cutoff 7.75 s; oldest kept must be the keyframe at 7 s.
        Assert.Equal(7 * Sec, ring.OldestPts);
        using var snap = ring.Snapshot(0);
        Assert.True(snap.Packets[0].Key);
    }

    [Fact]
    public void Wraps_and_evicts_by_space_without_corruption()
    {
        using var ring = new PacketRing(1 << 20, TimeSpan.Zero, lockMemory: false); // capacity 1 MiB, no time eviction
        var rnd = new Random(3);
        long pts = 0;
        for (int i = 0; i < 2000; i++)
        {
            int len = rnd.Next(1000, 9000);
            var p = new byte[len];
            BinaryPrimitives.WriteInt32LittleEndian(p, i);
            p[^1] = (byte)i;
            Assert.True(ring.Write(p, pts, key: i % 15 == 0));
            pts += Sec / 15;
        }
        Assert.True(ring.LiveBytes <= ring.Capacity);
        using var snap = ring.Snapshot(0);
        int prev = -1;
        foreach (var p in snap.Packets)
        {
            var d = snap.Data(p);
            int idx = BinaryPrimitives.ReadInt32LittleEndian(d);
            Assert.True(idx > prev);
            Assert.Equal((byte)idx, d[^1]);
            prev = idx;
        }
        Assert.Equal(1999, prev);
        Assert.True(ring.CommittedBytes <= ring.Capacity);
    }

    [Fact]
    public void Clear_zeroes_and_forgets()
    {
        using var ring = new PacketRing(1 << 20, TimeSpan.FromSeconds(10), lockMemory: false);
        ring.Write(Packet(500, 0xAB), 0, true);
        ring.Clear();
        Assert.Equal(0, ring.Count);
        using var snap = ring.Snapshot(0);
        Assert.Empty(snap.Packets);
    }

    [Fact]
    public void Commits_on_demand()
    {
        using var ring = new PacketRing(64L << 20, TimeSpan.FromSeconds(120), lockMemory: false);
        for (int i = 0; i < 10; i++) ring.Write(Packet(1000, 1), i * Sec, true);
        Assert.True(ring.CommittedBytes <= 1 << 20, $"committed {ring.CommittedBytes}");
    }

    [Fact]
    public void Rejects_oversized_packet()
    {
        using var ring = new PacketRing(1 << 20, TimeSpan.FromSeconds(10), lockMemory: false);
        Assert.False(ring.Write(new byte[(1 << 20) + 1], 0, true));
    }
}

/// <summary>Minimal ISO BMFF box parser used to check the muxer output.</summary>
internal sealed record Box(string Type, long Offset, long Size, List<Box> Children)
{
    private static readonly HashSet<string> Containers = ["moov", "trak", "mdia", "minf", "stbl", "dinf", "mvex", "moof", "traf"];

    public static List<Box> Parse(byte[] data, long start, long end)
    {
        var list = new List<Box>();
        long pos = start;
        while (pos + 8 <= end)
        {
            long size = BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan((int)pos));
            var type = Encoding.ASCII.GetString(data, (int)pos + 4, 4);
            if (size < 8 || pos + size > end) throw new InvalidDataException($"Bad box {type} at {pos} size {size}");
            var children = Containers.Contains(type) ? Parse(data, pos + 8, pos + size) : [];
            list.Add(new Box(type, pos, size, children));
            pos += size;
        }
        if (pos != end) throw new InvalidDataException($"Trailing bytes at {pos}");
        return list;
    }

    public Box Child(string type) => Children.Single(c => c.Type == type);
}

public class Fmp4WriterTests
{
    private const long Sec = 10_000_000;

    // A tiny AV1 sequence header OBU (profile 0, level 8, 1280x720, 4:2:0) with obu_has_size_field.
    internal static byte[] Av1SeqHeaderObu()
    {
        // Build the payload with a bit writer.
        var bits = new List<int>();
        void W(long v, int n) { for (int i = n - 1; i >= 0; i--) bits.Add((int)((v >> i) & 1)); }
        W(0, 3); W(0, 1); W(0, 1);          // profile 0, still_picture 0, reduced 0
        W(0, 1);                            // timing_info_present 0
        W(0, 1);                            // initial_display_delay_present 0
        W(0, 5);                            // operating_points_cnt_minus_1 = 0
        W(0, 12); W(8, 5);                  // idc, seq_level_idx 8 (no tier bit since <= 7? 8 > 7 -> tier bit)
        W(0, 1);                            // seq_tier
        W(15, 4); W(15, 4);                 // frame width/height bits minus 1 = 15
        W(1279, 16); W(719, 16);            // max frame size minus 1
        W(0, 1);                            // frame_id_numbers_present
        W(0, 1); W(1, 1); W(1, 1);          // sb128, filter intra, intra edge
        W(0, 1); W(0, 1); W(0, 1); W(0, 1); // interintra, masked, warped, dual filter
        W(1, 1); W(0, 1); W(0, 1);          // order hint on, jnt_comp, ref_frame_mvs
        W(1, 1);                            // seq_choose_screen_content_tools -> 2
        W(1, 1);                            // seq_choose_integer_mv
        W(6, 3);                            // order_hint_bits_minus_1
        W(0, 1); W(1, 1); W(1, 1);          // superres, cdef, restoration
        W(0, 1);                            // high_bitdepth
        W(0, 1);                            // mono_chrome
        W(0, 1);                            // color_description_present
        W(0, 1);                            // color_range
        W(0, 2);                            // chroma_sample_position
        W(0, 1);                            // film_grain_params_present
        W(1, 1);                            // trailing one bit
        while (bits.Count % 8 != 0) bits.Add(0);
        var payload = new byte[bits.Count / 8];
        for (int i = 0; i < bits.Count; i++) if (bits[i] == 1) payload[i / 8] |= (byte)(0x80 >> (i % 8));
        var obu = new List<byte> { (byte)((1 << 3) | 0x02), (byte)payload.Length };
        obu.AddRange(payload);
        return obu.ToArray();
    }

    internal static readonly byte[] TemporalDelimiter = [(2 << 3) | 0x02, 0];

    private static PacketRing VideoRing(byte[] seq, int seconds, int fps)
    {
        var ring = new PacketRing(1 << 20, TimeSpan.Zero, false);
        for (int i = 0; i < seconds * fps; i++)
        {
            bool key = i % fps == 0;
            var frame = new List<byte>(TemporalDelimiter);
            if (key) frame.AddRange(seq);
            frame.AddRange(new byte[] { (6 << 3) | 0x02, 4, 1, 2, 3, (byte)i }); // OBU_FRAME with 4 bytes
            ring.Write(frame.ToArray(), 1000 * Sec + i * Sec / fps, key);
        }
        return ring;
    }

    [Fact]
    public void Av1C_record_from_sequence_header()
    {
        var seq = Av1SeqHeaderObu();
        var info = Av1.ParseSequenceHeader(seq);
        Assert.Equal(0, info.Profile);
        Assert.Equal(8, info.Level);
        Assert.Equal(1280, info.MaxWidth);
        Assert.Equal(720, info.MaxHeight);
        Assert.Equal(1, info.SubX);
        Assert.Equal(1, info.SubY);
        var c = Av1.BuildAv1C(seq);
        Assert.Equal(0x81, c[0]);
        Assert.Equal((0 << 5) | 8, c[1]);
        Assert.Equal(seq, c[4..]);
    }

    [Fact]
    public void Writes_parseable_fragmented_mp4_with_video_and_audio()
    {
        var seq = Av1SeqHeaderObu();
        using var vring = VideoRing(seq, seconds: 3, fps: 10);
        using var aring = new PacketRing(1 << 20, TimeSpan.Zero, false);
        for (int i = 0; i < 3 * 48000 / 1024; i++) aring.Write(new byte[] { 0x21, 0x10, (byte)i }, 1000 * Sec + i * 1024L * Sec / 48000, true);
        using var vs = vring.Snapshot(0);
        using var aus = aring.Snapshot(0);
        var tracks = new List<MuxTrack>
        {
            new(TrackKind.Video, "av01", VideoCodec.Av1, 1280, 720, 0, 0, Av1.BuildAv1C(seq), Sec / 10, vs),
            new(TrackKind.Audio, "mp4a", null, 0, 0, 48000, 2, [0x11, 0x90], 1024L * Sec / 48000, aus),
        };
        var ms = new MemoryStream();
        int durationMs = Fmp4Writer.Write(ms, tracks);
        var data = ms.ToArray();
        var boxes = Box.Parse(data, 0, data.Length);

        Assert.Equal("ftyp", boxes[0].Type);
        Assert.Equal("moov", boxes[1].Type);
        var moov = boxes[1];
        Assert.Equal(2, moov.Children.Count(b => b.Type == "trak"));
        Assert.Equal(2, moov.Child("mvex").Children.Count(b => b.Type == "trex"));
        var stsd = moov.Children.First(b => b.Type == "trak").Child("mdia").Child("minf").Child("stbl").Child("stsd");
        Assert.Contains("av1C", Encoding.ASCII.GetString(data, (int)stsd.Offset, (int)stsd.Size));
        var audioStsd = moov.Children.Where(b => b.Type == "trak").Last().Child("mdia").Child("minf").Child("stbl").Child("stsd");
        Assert.Contains("esds", Encoding.ASCII.GetString(data, (int)audioStsd.Offset, (int)audioStsd.Size));

        var moofs = boxes.Where(b => b.Type == "moof").ToList();
        var mdats = boxes.Where(b => b.Type == "mdat").ToList();
        Assert.Equal(moofs.Count, mdats.Count);
        Assert.True(moofs.Count >= 3 * 2 - 1, $"fragments {moofs.Count}");
        // Every moof is directly followed by its mdat, and trun data offsets point into it.
        long videoSamples = 0;
        foreach (var moof in moofs)
        {
            var next = boxes[boxes.IndexOf(moof) + 1];
            Assert.Equal("mdat", next.Type);
            var traf = moof.Child("traf");
            var trun = traf.Child("trun");
            int count = (int)BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan((int)trun.Offset + 12));
            int dataOffset = (int)BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan((int)trun.Offset + 16));
            Assert.Equal(moof.Size + 8, dataOffset);
            long sum = 0;
            for (int i = 0; i < count; i++) sum += BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan((int)trun.Offset + 20 + i * 12 + 4));
            Assert.Equal(next.Size - 8, sum);
            uint trackId = BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan((int)traf.Child("tfhd").Offset + 12));
            if (trackId == 1)
            {
                videoSamples += count;
                // First sample of each video fragment is a sync sample.
                uint flags = BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan((int)trun.Offset + 20 + 8));
                Assert.Equal(0x02000000u, flags);
                // Temporal delimiters were stripped: the first OBU is not type 2.
                Assert.NotEqual(2, (data[next.Offset + 8] >> 3) & 0xF);
            }
        }
        Assert.Equal(30, videoSamples);
        Assert.InRange(durationMs, 2900, 3100);
        Assert.Equal(durationMs, ReplayLibrary.ReadMvhdDurationMs(new MemoryStream(data)));
    }

    [Fact]
    public void Annex_b_to_length_prefixed_and_avcC()
    {
        byte[] sps = [0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9];
        byte[] pps = [0x68, 0xEB, 0xE3];
        byte[] idr = [0x65, 0x88, 0x84, 0x00];
        var au = new List<byte> { 0, 0, 0, 1, 0x09, 0xF0 }; // AUD, dropped
        au.AddRange([0, 0, 0, 1]); au.AddRange(sps);
        au.AddRange([0, 0, 1]); au.AddRange(pps);
        au.AddRange([0, 0, 0, 1]); au.AddRange(idr);
        var src = au.ToArray();
        var dst = new byte[src.Length * 2];
        int n = AnnexB.ToLengthPrefixed(src, dst, VideoCodec.H264);
        Assert.Equal(4 + sps.Length + 4 + pps.Length + 4 + idr.Length, n);
        Assert.Equal(sps.Length, BinaryPrimitives.ReadInt32BigEndian(dst));
        var avcc = AnnexB.BuildAvcC(src)!;
        Assert.Equal(1, avcc[0]);
        Assert.Equal(0x64, avcc[1]);
        Assert.Equal(0xFF, avcc[4]);
        Assert.Equal(0xE1, avcc[5]);
    }
}
