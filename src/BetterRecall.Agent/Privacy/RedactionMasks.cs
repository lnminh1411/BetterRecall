// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Native;

namespace BetterRecall.Agent.Privacy;

/// <summary>Redaction boxes of the newest kept keyframe of each monitor, in desktop pixels, for the replay compositor:
/// it fills them black before encoding. Each monitor's boxes stay until that monitor's next kept keyframe replaces
/// them, so new sensitive text shows in the replay for up to one capture interval (plus the OCR time) before its box
/// arrives. Thread-safe.</summary>
internal sealed class RedactionMasks
{
    private readonly Lock _gate = new();
    private readonly Dictionary<int, RECT[]> _byMonitor = new();
    private RECT[] _all = [];

    public void Set(int monitor, RECT[] desktop)
    {
        lock (_gate)
        {
            if (desktop.Length == 0) _byMonitor.Remove(monitor); else _byMonitor[monitor] = desktop;
            _all = _byMonitor.Values.SelectMany(x => x).ToArray();
        }
    }

    public void Clear()
    {
        lock (_gate) { _byMonitor.Clear(); _all = []; }
    }

    public RECT[] All() { lock (_gate) return _all; }

    /// <summary>Keyframe boxes (kfW x kfH image of the monitor) to desktop rectangles, rounded outwards and clipped to
    /// the monitor.</summary>
    public static RECT[] ToDesktop(IEnumerable<Box> boxes, int kfW, int kfH, RECT monitor)
    {
        if (kfW <= 0 || kfH <= 0) return [];
        double sx = (double)monitor.Width / kfW, sy = (double)monitor.Height / kfH;
        var list = new List<RECT>();
        foreach (var b in boxes)
        {
            int l = monitor.Left + (int)Math.Floor(b.X * sx), t = monitor.Top + (int)Math.Floor(b.Y * sy);
            int r = monitor.Left + (int)Math.Ceiling(b.Right * sx), btm = monitor.Top + (int)Math.Ceiling(b.Bottom * sy);
            l = Math.Max(l, monitor.Left); t = Math.Max(t, monitor.Top);
            r = Math.Min(r, monitor.Right); btm = Math.Min(btm, monitor.Bottom);
            if (r > l && btm > t) list.Add(new RECT(l, t, r, btm));
        }
        return list.ToArray();
    }

    /// <summary>Desktop rectangles to keyframe boxes of a monitor (kfW x kfH), clipped; rectangles off the monitor are
    /// dropped.</summary>
    public static Box[] ToKeyframe(IEnumerable<RECT> desktop, RECT monitor, int kfW, int kfH)
    {
        if (monitor.Width <= 0 || monitor.Height <= 0) return [];
        double sx = (double)kfW / monitor.Width, sy = (double)kfH / monitor.Height;
        var list = new List<Box>();
        foreach (var d in desktop)
        {
            int l = Math.Max(d.Left, monitor.Left), t = Math.Max(d.Top, monitor.Top);
            int r = Math.Min(d.Right, monitor.Right), b = Math.Min(d.Bottom, monitor.Bottom);
            if (r <= l || b <= t) continue;
            int x0 = (int)Math.Floor((l - monitor.Left) * sx), y0 = (int)Math.Floor((t - monitor.Top) * sy);
            int x1 = (int)Math.Ceiling((r - monitor.Left) * sx), y1 = (int)Math.Ceiling((b - monitor.Top) * sy);
            list.Add(new Box(x0, y0, Math.Max(1, x1 - x0), Math.Max(1, y1 - y0)));
        }
        return list.ToArray();
    }

    /// <summary>Fills boxes black (opaque) in a BGRA image, clipped to it.</summary>
    public static unsafe void Fill(byte* bgra, int width, int height, int stride, IEnumerable<Box> boxes)
    {
        foreach (var b in boxes)
        {
            int x0 = Math.Max(0, b.X), y0 = Math.Max(0, b.Y), x1 = Math.Min(width, b.Right), y1 = Math.Min(height, b.Bottom);
            if (x1 <= x0 || y1 <= y0) continue;
            for (int y = y0; y < y1; y++)
            {
                uint* row = (uint*)(bgra + (long)y * stride) + x0;
                new Span<uint>(row, x1 - x0).Fill(0xFF000000);
            }
        }
    }
}
