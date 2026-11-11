// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Replay;
using Xunit;

namespace BetterRecall.Agent.Tests;

public class ReplayLayoutTests
{
    [Fact]
    public void ReplayScaling_Fit_options()
    {
        int maxW = 7680, maxH = 4320;
        Assert.Equal((2560, 1440), ReplayScaling.Fit(2560, 1440, "native", 100, maxW, maxH));
        Assert.Equal((2560, 1440), ReplayScaling.Fit(2560, 1440, "2160p", 100, maxW, maxH));
        Assert.Equal((2560, 1440), ReplayScaling.Fit(2560, 1440, "1440p", 100, maxW, maxH));
        Assert.Equal((1920, 1080), ReplayScaling.Fit(2560, 1440, "1080p", 100, maxW, maxH));
        Assert.Equal((1600, 900), ReplayScaling.Fit(2560, 1440, "900p", 100, maxW, maxH));
        Assert.Equal((1280, 720), ReplayScaling.Fit(2560, 1440, "720p", 100, maxW, maxH));
        Assert.Equal((960, 540), ReplayScaling.Fit(2560, 1440, "540p", 100, maxW, maxH));
        Assert.Equal((1280, 720), ReplayScaling.Fit(2560, 1440, "scale", 50, maxW, maxH));
        Assert.Equal((640, 360), ReplayScaling.Fit(2560, 1440, "scale", 10, maxW, maxH));

        // Also 1280x1024 720p -> 900x720; 1921x1081 native -> 1920x1080; 8000x2000 native with max 7680x4320 -> 7680x1920
        Assert.Equal((900, 720), ReplayScaling.Fit(1280, 1024, "720p", 100, maxW, maxH));
        Assert.Equal((1920, 1080), ReplayScaling.Fit(1921, 1081, "native", 100, maxW, maxH));
        Assert.Equal((7680, 1920), ReplayScaling.Fit(8000, 2000, "native", 100, maxW, maxH));

        // Results always even
        var (w, h) = ReplayScaling.Fit(1921, 1081, "native", 100, maxW, maxH);
        Assert.True(w % 2 == 0 && h % 2 == 0);
    }

    [Fact]
    public void CanvasLayout_computations()
    {
        // (a) monitors (0, -1280,0,1280,1024) and (1, 0,0,1920,1080), native -> 3200x1080, origin (-1280,0), dests (0,0,1280,1024) and (1280,0,3200,1080)
        var layoutA = CanvasLayout.Compute(
            [new(0, -1280, 0, 1280, 1024), new(1, 0, 0, 1920, 1080)],
            "native", 100, 7680, 4320);
        Assert.Equal(3200, layoutA.Width);
        Assert.Equal(1080, layoutA.Height);
        Assert.Equal(-1280, layoutA.OriginLeft);
        Assert.Equal(0, layoutA.OriginTop);
        Assert.Equal(new RECT(0, 0, 1280, 1024), layoutA.For(0)!.Value.Rect);
        Assert.Equal(new RECT(1280, 0, 3200, 1080), layoutA.For(1)!.Value.Rect);

        // (b) mixed sizes with vertical offset: (0, 0,0,1920,1080), (1, 1920,-200,1280,1024) -> 3200x1280, dests (0,200,1920,1280) and (1920,0,3200,1024)
        var layoutB = CanvasLayout.Compute(
            [new(0, 0, 0, 1920, 1080), new(1, 1920, -200, 1280, 1024)],
            "native", 100, 7680, 4320);
        Assert.Equal(3200, layoutB.Width);
        Assert.Equal(1280, layoutB.Height);
        Assert.Equal(0, layoutB.OriginLeft);
        Assert.Equal(-200, layoutB.OriginTop);
        Assert.Equal(new RECT(0, 200, 1920, 1280), layoutB.For(0)!.Value.Rect);
        Assert.Equal(new RECT(1920, 0, 3200, 1024), layoutB.For(1)!.Value.Rect);

        // (c) downscale: three 3840x2160 side by side (x 0, 3840, 7680), native, max 7680x4320 -> 7680x1440, dests (0,0,2560,1440),(2560,0,5120,1440),(5120,0,7680,1440)
        var layoutC = CanvasLayout.Compute(
            [new(0, 0, 0, 3840, 2160), new(1, 3840, 0, 3840, 2160), new(2, 7680, 0, 3840, 2160)],
            "native", 100, 7680, 4320);
        Assert.Equal(7680, layoutC.Width);
        Assert.Equal(1440, layoutC.Height);
        Assert.Equal(new RECT(0, 0, 2560, 1440), layoutC.For(0)!.Value.Rect);
        Assert.Equal(new RECT(2560, 0, 5120, 1440), layoutC.For(1)!.Value.Rect);
        Assert.Equal(new RECT(5120, 0, 7680, 1440), layoutC.For(2)!.Value.Rect);

        // (d) 1080p on (a) -> height 1080 unchanged (already 1080)
        var layoutD = CanvasLayout.Compute(
            [new(0, -1280, 0, 1280, 1024), new(1, 0, 0, 1920, 1080)],
            "1080p", 100, 7680, 4320);
        Assert.Equal(1080, layoutD.Height);
        Assert.Equal(3200, layoutD.Width);

        // (e) single monitor 2560x1440 at (100,50) with 720p -> 1280x720, dest (0,0,1280,720)
        var layoutE = CanvasLayout.Compute(
            [new(0, 100, 50, 2560, 1440)],
            "720p", 100, 7680, 4320);
        Assert.Equal(1280, layoutE.Width);
        Assert.Equal(720, layoutE.Height);
        Assert.Equal(new RECT(0, 0, 1280, 720), layoutE.For(0)!.Value.Rect);

        Assert.Null(layoutE.For(999));
        Assert.Throws<ArgumentException>(() => CanvasLayout.Compute([], "native", 100, 1920, 1080));
    }

    [Fact]
    public void DirtyMapper_empty_and_scaling()
    {
        Assert.True(DirtyMapper.IsEmpty(DirtyMapper.ToDest(new RECT(0, 0, 0, 0), 1920, 1080, 960, 540)));
        Assert.True(DirtyMapper.IsEmpty(DirtyMapper.ToDest(new RECT(10, 10, 5, 5), 1920, 1080, 960, 540)));

        var full = DirtyMapper.ToDest(new RECT(0, 0, 1920, 1080), 1920, 1080, 960, 540);
        Assert.Equal(new RECT(0, 0, 960, 540), full);

        var mapped = DirtyMapper.ToDest(new RECT(100, 100, 110, 110), 1920, 1080, 960, 540);
        Assert.Equal(new RECT(48, 48, 58, 58), mapped);

        Assert.Equal(0, mapped.Left % 2);
        Assert.Equal(0, mapped.Top % 2);
        Assert.Equal(0, mapped.Right % 2);
        Assert.Equal(0, mapped.Bottom % 2);
        Assert.InRange(mapped.Left, 0, 960);
        Assert.InRange(mapped.Right, 0, 960);
        Assert.InRange(mapped.Top, 0, 540);
        Assert.InRange(mapped.Bottom, 0, 540);

        var r = new RECT(10, 20, 30, 40);
        var empty = new RECT(0, 0, 0, 0);
        Assert.Equal(r, DirtyMapper.Union(r, empty));
        Assert.Equal(r, DirtyMapper.Union(empty, r));
    }

    [Theory]
    [InlineData(24)]
    [InlineData(25)]
    [InlineData(144)]
    [InlineData(240)]
    public void FramePacer_pacing_without_drift(int fps)
    {
        long freq = 10_000_000;
        var pacer = new FramePacer(fps, freq);
        var ptsList = new List<long>();
        long start = 1_000_000;
        long end = start + freq;
        for (long now = start; now <= end; now += 50)
        {
            if (pacer.TryTick(now, out long pts))
            {
                ptsList.Add(pts);
            }
        }
        Assert.Equal(fps + 1, ptsList.Count);
        double expectedStep = (double)freq / fps;
        for (int i = 0; i < ptsList.Count - 1; i++)
        {
            Assert.True(ptsList[i + 1] > ptsList[i]);
            long diff = ptsList[i + 1] - ptsList[i];
            Assert.InRange(diff, (long)Math.Floor(expectedStep - 1), (long)Math.Ceiling(expectedStep + 1));
        }
        Assert.Equal(10_000_000, ptsList[fps] - ptsList[0]);
    }

    [Fact]
    public void FramePacer_late_resets_and_skips_without_bunching()
    {
        long freq = 10_000_000;
        var pacer = new FramePacer(25, freq);
        long now = 1_000_000;
        var ptsList = new List<long>();
        for (int i = 0; i < 50000; i++)
        {
            if (pacer.TryTick(now, out long pts)) ptsList.Add(pts);
            now += 50;
        }
        Assert.True(ptsList.Count >= 2);
        long lastPts = ptsList[^1];

        // jump now by 2 s
        now += 2 * freq;
        Assert.True(pacer.TryTick(now, out long latePts));
        Assert.True(latePts > lastPts);

        long period = freq / 25; // 400_000
        for (long t = 1; t < period; t += 50)
        {
            Assert.False(pacer.TryTick(now + t, out _));
        }
        Assert.True(pacer.TryTick(now + period, out _));
    }

    [Fact]
    public void Fmp4Writer_VideoTimescaleFor()
    {
        Assert.Equal(90000, Fmp4Writer.VideoTimescaleFor(30));
        Assert.Equal(90000, Fmp4Writer.VideoTimescaleFor(24));
        Assert.Equal(90000, Fmp4Writer.VideoTimescaleFor(144));
        Assert.Equal(90000, Fmp4Writer.VideoTimescaleFor(240));
        Assert.Equal(990000, Fmp4Writer.VideoTimescaleFor(165));
        Assert.Equal(90000, Fmp4Writer.VideoTimescaleFor(0));
    }

    [Fact]
    public void ReplayRate_normalize_and_effective()
    {
        Assert.Equal(60, ReplayRate.NormalizeRefresh(59));
        Assert.Equal(144, ReplayRate.NormalizeRefresh(143));
        Assert.Equal(60, ReplayRate.NormalizeRefresh(60));
        Assert.Equal(60, ReplayRate.NormalizeRefresh(0));

        Assert.Equal(60, ReplayRate.Effective(144, 60));
        Assert.Equal(60, ReplayRate.Effective(60, 144));
        Assert.Equal(30, ReplayRate.Effective(7, 60));
        Assert.Equal(240, ReplayRate.Effective(240, 240));
    }

    private static byte[] MakeFrame(byte[] seq, bool key, int index)
    {
        var frame = new List<byte>(Fmp4WriterTests.TemporalDelimiter);
        if (key) frame.AddRange(seq);
        frame.AddRange([(byte)((6 << 3) | 0x02), 4, 1, 2, 3, (byte)index]);
        return frame.ToArray();
    }

    [Fact]
    public void VideoStream_continuity_and_multi_format_fmp4()
    {
        var seq = Fmp4WriterTests.Av1SeqHeaderObu();
        using var stream = new VideoStream("cam", 16 << 20, TimeSpan.FromSeconds(60), lockMemory: false);

        int s1 = stream.BeginSession(VideoCodec.Av1, 1280, 720, 30);
        Assert.Equal(1, s1);

        // a non-key packet first (dropped, DroppedBeforeKey == 1)
        stream.OnPacket(s1, MakeFrame(seq, key: false, 0), 1000 * 10_000_000L, key: false);
        Assert.Equal(1, stream.DroppedBeforeKey);

        // then 60 frames at 30 fps, key every 30
        long pts = 1000 * 10_000_000L;
        for (int i = 0; i < 60; i++)
        {
            bool key = i % 30 == 0;
            pts += 10_000_000L / 30;
            stream.OnPacket(s1, MakeFrame(seq, key, i), pts, key);
        }

        int s2 = stream.BeginSession(VideoCodec.Av1, 1920, 1080, 30);
        Assert.Equal(2, s2);

        // a late packet for s1 is dropped
        pts += 10_000_000L / 30;
        stream.OnPacket(s1, MakeFrame(seq, key: false, 60), pts, key: false);

        // s2 non-key dropped
        pts += 10_000_000L / 30;
        stream.OnPacket(s2, MakeFrame(seq, key: false, 61), pts, key: false);
        Assert.Equal(2, stream.DroppedBeforeKey);

        // then 60 frames of s2 starting with a key, pts continuing after s1
        for (int i = 0; i < 60; i++)
        {
            bool key = i % 30 == 0;
            pts += 10_000_000L / 30;
            stream.OnPacket(s2, MakeFrame(seq, key, 62 + i), pts, key);
        }

        var formats = stream.Formats();
        Assert.Equal(2, formats.Count);
        Assert.True(formats.ContainsKey(1));
        Assert.True(formats.ContainsKey(2));

        using var snap = stream.Ring.Snapshot(long.MinValue);
        var track = stream.ToMuxTrack(snap);
        Assert.NotNull(track);

        using var ms = new MemoryStream();
        Fmp4Writer.Write(ms, [track]);
        byte[] data = ms.ToArray();
        var boxes = Box.Parse(data, 0, data.Length);

        var moov = boxes.Single(b => b.Type == "moov");
        var trak = moov.Children.Single(b => b.Type == "trak");
        var stsd = trak.Child("mdia").Child("minf").Child("stbl").Child("stsd");
        uint entryCount = BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan((int)stsd.Offset + 12, 4));
        Assert.Equal(2u, entryCount);

        var moofs = boxes.Where(b => b.Type == "moof").ToList();
        Assert.NotEmpty(moofs);

        var sampleDescIndices = new List<uint>();
        foreach (var moof in moofs)
        {
            var tfhd = moof.Child("traf").Child("tfhd");
            uint flags = BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan((int)tfhd.Offset + 8, 4)) & 0x00FFFFFF;
            Assert.True((flags & 2) != 0, "tfhd must have flags & 2 set");

            uint sampleDescIdx = BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan((int)tfhd.Offset + 16, 4));
            sampleDescIndices.Add(sampleDescIdx);
        }

        int firstTwo = sampleDescIndices.IndexOf(2u);
        Assert.True(firstTwo > 0, "Expected at least one fragment with sample_description_index 1 before 2");
        for (int i = 0; i < firstTwo; i++) Assert.Equal(1u, sampleDescIndices[i]);
        for (int i = firstTwo; i < sampleDescIndices.Count; i++) Assert.Equal(2u, sampleDescIndices[i]);

        var firstTwoMoof = moofs[firstTwo];
        var trun = firstTwoMoof.Child("traf").Child("trun");
        uint firstSampleFlags = BinaryPrimitives.ReadUInt32BigEndian(data.AsSpan((int)trun.Offset + 20 + 8, 4));
        Assert.Equal(0x02000000u, firstSampleFlags);
    }

    [Fact]
    public void PacketRing_preserves_format_and_reports_in_use()
    {
        using var ring = new PacketRing(1 << 20, TimeSpan.FromSeconds(10), lockMemory: false);
        ring.Write(new byte[] { 1, 2, 3 }, 1000, key: true, format: 7);
        using var snap = ring.Snapshot(0);
        Assert.Single(snap.Packets);
        Assert.Equal(7, snap.Packets[0].Format);
        Assert.Contains(7, ring.FormatsInUse());
    }
}
