// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Native;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Replay;

internal static class ReplayScaling
{
    public static readonly string[] Resolutions = ["native", "2160p", "1440p", "1080p", "900p", "720p", "540p", "scale"];

    public static int TargetHeight(int h, string? resolution, int scalePercent)
    {
        int target = resolution?.ToLowerInvariant() switch
        {
            "2160p" => 2160,
            "1440p" => 1440,
            "1080p" => 1080,
            "900p" => 900,
            "720p" => 720,
            "540p" => 540,
            "scale" => h * Math.Clamp(scalePercent, 25, 100) / 100,
            _ => h,
        };
        return Math.Min(h, target);
    }

    public static (int W, int H) Fit(int w, int h, string? resolution, int scalePercent, int maxW, int maxH)
    {
        int H0 = TargetHeight(h, resolution, scalePercent);
        int W0 = (int)Math.Round((double)w * H0 / h);
        if (W0 > maxW) { W0 = maxW; H0 = (int)Math.Round((double)h * maxW / w); }
        if (H0 > maxH) { H0 = maxH; W0 = (int)Math.Round((double)w * maxH / h); }
        return (Math.Max(2, W0 & ~1), Math.Max(2, H0 & ~1));
    }
}

internal static class ReplayRate
{
    public static int NormalizeRefresh(int hz)
    {
        if (hz <= 1) return 60;
        if (Array.IndexOf(ReplaySettings.FpsChoices, hz + 1) >= 0) return hz + 1;
        return hz;
    }

    public static int Effective(int requestedFps, int maxRefreshHz)
    {
        int req = Array.IndexOf(ReplaySettings.FpsChoices, requestedFps) >= 0 ? requestedFps : 30;
        int limit = maxRefreshHz <= 0 ? req : maxRefreshHz;
        return Math.Max(10, Math.Min(req, limit));
    }
}

internal readonly record struct MonitorPlacement(int Index, int Left, int Top, int Width, int Height); // desktop pixels
internal readonly record struct MonitorDest(int Index, RECT Rect); // canvas pixels

internal sealed record CanvasLayout(int Width, int Height, int OriginLeft, int OriginTop, int SourceWidth, int SourceHeight, IReadOnlyList<MonitorDest> Dests)
{
    public static CanvasLayout Compute(IReadOnlyList<MonitorPlacement> monitors, string? resolution, int scalePercent, int maxW, int maxH)
    {
        if (monitors.Count == 0) throw new ArgumentException("Empty monitor list.", nameof(monitors));
        int minLeft = int.MaxValue;
        int minTop = int.MaxValue;
        int maxRight = int.MinValue;
        int maxBottom = int.MinValue;
        foreach (var m in monitors)
        {
            if (m.Left < minLeft) minLeft = m.Left;
            if (m.Top < minTop) minTop = m.Top;
            int r = m.Left + m.Width;
            int b = m.Top + m.Height;
            if (r > maxRight) maxRight = r;
            if (b > maxBottom) maxBottom = b;
        }

        int bboxW = maxRight - minLeft;
        int bboxH = maxBottom - minTop;
        var (w, h) = ReplayScaling.Fit(bboxW, bboxH, resolution, scalePercent, maxW, maxH);
        var dests = new List<MonitorDest>(monitors.Count);
        foreach (var m in monitors)
        {
            int x0 = (int)(((long)(m.Left - minLeft) * w) / bboxW) & ~1;
            int y0 = (int)(((long)(m.Top - minTop) * h) / bboxH) & ~1;
            int x1 = (int)(((long)(m.Left + m.Width - minLeft) * w) / bboxW) & ~1;
            int y1 = (int)(((long)(m.Top + m.Height - minTop) * h) / bboxH) & ~1;

            x0 = Math.Clamp(x0, 0, w);
            y0 = Math.Clamp(y0, 0, h);
            x1 = Math.Clamp(x1, 0, w);
            y1 = Math.Clamp(y1, 0, h);

            if (x1 - x0 < 2)
            {
                x1 = Math.Min(w, x1 + 2);
                if (x1 - x0 < 2) x0 = Math.Max(0, x1 - 2);
            }
            if (y1 - y0 < 2)
            {
                y1 = Math.Min(h, y1 + 2);
                if (y1 - y0 < 2) y0 = Math.Max(0, y1 - 2);
            }

            dests.Add(new MonitorDest(m.Index, new RECT(x0, y0, x1, y1)));
        }

        return new CanvasLayout(w, h, minLeft, minTop, bboxW, bboxH, dests);
    }

    public MonitorDest? For(int index)
    {
        for (int i = 0; i < Dests.Count; i++)
        {
            if (Dests[i].Index == index) return Dests[i];
        }
        return null;
    }
}

internal static class DirtyMapper
{
    public static bool IsEmpty(RECT r) => r.Right <= r.Left || r.Bottom <= r.Top;

    public static RECT Union(RECT a, RECT b)
    {
        if (IsEmpty(a)) return b;
        if (IsEmpty(b)) return a;
        return new RECT(
            Math.Min(a.Left, b.Left),
            Math.Min(a.Top, b.Top),
            Math.Max(a.Right, b.Right),
            Math.Max(a.Bottom, b.Bottom));
    }

    public static RECT ToDest(RECT dirty, int srcW, int srcH, int dstW, int dstH, int pad = 2)
    {
        if (IsEmpty(dirty)) return new RECT(0, 0, 0, 0);

        long l = ((long)dirty.Left * dstW) / srcW - pad;
        long t = ((long)dirty.Top * dstH) / srcH - pad;
        long r = (((long)dirty.Right * dstW) + srcW - 1) / srcW + pad;
        long b = (((long)dirty.Bottom * dstH) + srcH - 1) / srcH + pad;

        int destL = (int)Math.Max(0, l) & ~1;
        int destT = (int)Math.Max(0, t) & ~1;
        int destR = (int)Math.Min(dstW, (r + 1) & ~1);
        int destB = (int)Math.Min(dstH, (b + 1) & ~1);

        return new RECT(destL, destT, destR, destB);
    }
}

/// <summary>Paces ticks at fps on a Stopwatch-tick clock without drift: deadline n = Base + n * Frequency / Fps.</summary>
internal sealed class FramePacer
{
    public FramePacer(int fps, long frequency)
    {
        Fps = fps;
        Frequency = frequency;
    }

    public int Fps { get; }
    public long Frequency { get; }
    public long Base { get; private set; }   // 0 = not started
    public long Index { get; private set; }
    public long NextDeadline => Base + (long)((Int128)Index * Frequency / Fps);

    public void Reset(long now)
    {
        Base = now;
        Index = 0;
    }

    public bool TryTick(long now, out long pts100ns)
    {
        if (Base == 0) Reset(now);
        if (now < NextDeadline)
        {
            pts100ns = 0;
            return false;
        }
        if (now - NextDeadline > 4 * Frequency / Fps)
        {
            Reset(now);
        }
        pts100ns = ToHundredNs(Base, Frequency) + (long)(((Int128)Index * 20_000_000 + Fps) / (2 * Fps));
        Index++;
        return true;
    }

    public static long ToHundredNs(long ticks, long frequency) => (long)((Int128)ticks * 10_000_000 / frequency);
}
