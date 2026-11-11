// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Capture;

/// <summary>Change of a grab against the previous stored keyframe of its monitor.</summary>
/// <param name="ChangedPercent">Share of 32 px tiles that changed.</param>
/// <param name="TextPercent">Share of tiles that changed and look like text (in either frame).</param>
/// <param name="Score">Change score: changed area with text-like tiles counted TextWeight times, capped at 100.</param>
/// <param name="Comparable">False when there is no previous map or the frame size changed.</param>
internal readonly record struct ChangeResult(double ChangedPercent, double TextPercent, double Score, bool Comparable)
{
    public static readonly ChangeResult First = new(100, 0, 100, false);
}

/// <summary>
/// Tile signature of a keyframe for smart capture (v0.2 batch 2): 32 px tiles, each with 4x4 cell luma means and a
/// text-like flag (share of strong horizontal edges between 6% and 50% on the sampled rows). Cheap: every second row,
/// all columns, about 2 ms at 1280x720. Holds a coarse 8 px luma image of the screen: cleared after use.
/// </summary>
internal sealed class ChangeMap
{
    public const int Tile = 32, Cells = 4, CellPx = Tile / Cells;
    public const int CellDelta = 6;        // luma mean change of one cell that marks the tile changed
    public const int EdgeStep = 48;        // luma step between neighbours counted as a strong edge
    public const double TextEdgeMin = 0.06, TextEdgeMax = 0.5;
    public const double TextWeight = 5;

    public int Width { get; private init; }
    public int Height { get; private init; }
    public int Cols { get; private init; }
    public int Rows { get; private init; }
    public byte[] Means { get; private init; } = [];   // Cols * Rows * 16
    public bool[] Text { get; private init; } = [];     // Cols * Rows

    public int Tiles => Cols * Rows;

    /// <summary>bgra: 32-bit BGRA pixels, stride in bytes.</summary>
    public static ChangeMap Compute(ReadOnlySpan<byte> bgra, int width, int height, int stride)
    {
        if (width <= 0 || height <= 0) throw new ArgumentException("Empty image.");
        int cols = (width + Tile - 1) / Tile, rows = (height + Tile - 1) / Tile;
        int tiles = cols * rows;
        var sums = new int[tiles * Cells * Cells];
        var counts = new int[tiles * Cells * Cells];
        var edges = new int[tiles];
        var pairs = new int[tiles];
        for (int y = 0; y < height; y += 2)
        {
            int row = y * stride;
            int ty = y / Tile, cy = (y % Tile) / CellPx;
            int prevL = -1;
            for (int x = 0; x < width; x++)
            {
                int p = row + x * 4;
                int l = (29 * bgra[p] + 150 * bgra[p + 1] + 77 * bgra[p + 2]) >> 8;
                int tx = x / Tile;
                int t = ty * cols + tx;
                int c = t * 16 + cy * Cells + (x % Tile) / CellPx;
                sums[c] += l;
                counts[c]++;
                if (prevL >= 0 && x % Tile != 0)
                {
                    pairs[t]++;
                    if (Math.Abs(l - prevL) >= EdgeStep) edges[t]++;
                }
                prevL = l;
            }
        }
        var means = new byte[sums.Length];
        for (int i = 0; i < sums.Length; i++) means[i] = counts[i] == 0 ? (byte)0 : (byte)(sums[i] / counts[i]);
        var text = new bool[tiles];
        for (int t = 0; t < tiles; t++)
        {
            if (pairs[t] == 0) continue;
            double d = edges[t] / (double)pairs[t];
            text[t] = d >= TextEdgeMin && d <= TextEdgeMax;
        }
        Array.Clear(sums);
        return new ChangeMap { Width = width, Height = height, Cols = cols, Rows = rows, Means = means, Text = text };
    }

    /// <summary>Compares cur with the previous stored map. No previous map, or another size: ChangeResult.First.</summary>
    public static ChangeResult Compare(ChangeMap? prev, ChangeMap cur)
    {
        if (prev is null || prev.Width != cur.Width || prev.Height != cur.Height) return ChangeResult.First;
        int changed = 0, textChanged = 0;
        for (int t = 0; t < cur.Tiles; t++)
        {
            bool diff = false;
            for (int c = t * 16, end = c + 16; c < end; c++)
                if (Math.Abs(prev.Means[c] - cur.Means[c]) > CellDelta) { diff = true; break; }
            if (!diff) continue;
            changed++;
            if (prev.Text[t] || cur.Text[t]) textChanged++;
        }
        double n = cur.Tiles;
        double changedPct = changed * 100.0 / n;
        double textPct = textChanged * 100.0 / n;
        double score = Math.Min(100, (changed - textChanged) * 100.0 / n + TextWeight * textPct);
        return new ChangeResult(changedPct, textPct, score, true);
    }

    public void Clear()
    {
        Array.Clear(Means);
        Array.Clear(Text);
    }
}

/// <summary>Why a grab is kept (or skipped).</summary>
internal enum KeepReason { Skip, Force, First, Switch, Trigger, Change, Hash }

/// <summary>
/// Smart capture rule (v0.2 batch 2). A grab is kept when forced; when it is the monitor's first since start or
/// panic, or the window/app (app + title) changed; for an event trigger (foreground change, typing pause, scroll
/// stop) when anything visible changed (score at least TriggerMinScore, or dHash distance at least the threshold);
/// on the timer when the change score reaches capture.dirtyMinPercent, or the dHash distance reaches
/// capture.dhashThreshold (0 turns the dHash test off).
/// </summary>
internal static class CapturePolicy
{
    public const double TriggerMinScore = 0.5;

    public static KeepReason Decide(bool force, bool trigger, bool contextChanged, ChangeResult change, int hashDistance, int dhashThreshold, double dirtyMinPercent)
    {
        if (force) return KeepReason.Force;
        if (!change.Comparable) return KeepReason.First;
        if (contextChanged) return KeepReason.Switch;
        bool hash = dhashThreshold > 0 && hashDistance >= dhashThreshold;
        if (trigger) return change.Score >= TriggerMinScore || hash ? KeepReason.Trigger : KeepReason.Skip;
        if (change.Score > 0 && change.Score >= dirtyMinPercent) return KeepReason.Change;
        return hash ? KeepReason.Hash : KeepReason.Skip;
    }
}
