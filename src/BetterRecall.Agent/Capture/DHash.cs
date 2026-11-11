// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Numerics;

namespace BetterRecall.Agent.Capture;

/// <summary>64-bit difference hash over a 9x8 grayscale box-averaged thumbnail.</summary>
internal static class DHash
{
    /// <summary>bgra: 32-bit BGRA pixels, stride in bytes.</summary>
    public static ulong Compute(ReadOnlySpan<byte> bgra, int width, int height, int stride)
    {
        if (width < 9 || height < 8) throw new ArgumentException("Image too small for dHash.");
        Span<double> gray = stackalloc double[72];
        // Sample a sparse grid inside each cell: enough for a hash, cheap on large frames.
        const int Samples = 8;
        for (int cy = 0; cy < 8; cy++)
        {
            int y0 = cy * height / 8, y1 = (cy + 1) * height / 8;
            for (int cx = 0; cx < 9; cx++)
            {
                int x0 = cx * width / 9, x1 = (cx + 1) * width / 9;
                double sum = 0;
                int n = 0;
                for (int sy = 0; sy < Samples; sy++)
                {
                    int y = y0 + (int)((y1 - y0) * (sy + 0.5) / Samples);
                    int row = y * stride;
                    for (int sx = 0; sx < Samples; sx++)
                    {
                        int x = x0 + (int)((x1 - x0) * (sx + 0.5) / Samples);
                        int p = row + x * 4;
                        sum += 0.114 * bgra[p] + 0.587 * bgra[p + 1] + 0.299 * bgra[p + 2];
                        n++;
                    }
                }
                gray[cy * 9 + cx] = sum / n;
            }
        }
        ulong hash = 0;
        int bit = 0;
        for (int cy = 0; cy < 8; cy++)
            for (int cx = 0; cx < 8; cx++, bit++)
                if (gray[cy * 9 + cx] < gray[cy * 9 + cx + 1]) hash |= 1UL << bit;
        return hash;
    }

    public static int Distance(ulong a, ulong b) => BitOperations.PopCount(a ^ b);
}

/// <summary>Per-monitor state of the last stored keyframe: dHash, app + title context and the change map.</summary>
internal sealed class DedupFilter
{
    private ulong? _lastHash;
    private string? _lastContext;
    private ChangeMap? _lastMap;

    public ulong? LastHash => _lastHash;

    public bool ContextDiffers(string context) => _lastContext is not null && !string.Equals(_lastContext, context, StringComparison.Ordinal);

    /// <summary>Change of a new grab against the last stored keyframe and the smart-capture decision.</summary>
    public (KeepReason Reason, ChangeResult Change) Decide(ChangeMap map, ulong hash, KeyframeRequest req, int dhashThreshold, double dirtyMinPercent)
    {
        var change = ChangeMap.Compare(_lastMap, map);
        int distance = _lastHash is ulong last ? DHash.Distance(last, hash) : 64;
        var reason = CapturePolicy.Decide(req.Force, req.Trigger, ContextDiffers(req.Context), change, distance, dhashThreshold, dirtyMinPercent);
        return (reason, change);
    }

    /// <summary>The grab was stored: it becomes the reference. Takes ownership of map.</summary>
    public void Accept(ulong hash, string context, ChangeMap map)
    {
        _lastHash = hash;
        _lastContext = context;
        _lastMap?.Clear();
        _lastMap = map;
    }

    public void Reset()
    {
        _lastHash = null;
        _lastContext = null;
        _lastMap?.Clear();
        _lastMap = null;
    }
}
