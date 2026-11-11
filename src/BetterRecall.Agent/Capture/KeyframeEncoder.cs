// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;
using SkiaSharp;

namespace BetterRecall.Agent.Capture;

/// <summary>
/// Keyframe encoding at one EncodeGovernor level, on the calling thread only (libaom with one thread, so the CPU it
/// uses can be measured and capped). AVIF levels: Normal and Fast are libaom speeds, Reduced is the fastest speed on a
/// picture scaled to ReducedScale at a lower quality, WebP is the last resort. keyframes.format webp / jpeg / jxl use
/// their own encoder at Normal and Fast, the scaled picture at Reduced. Presets: spike 2026-11-12 (docs/v0.2-plan.md).
/// </summary>
internal static unsafe class KeyframeEncoder
{
    /// <summary>Spike 2026-11-12, 8 screens at 1280 px, one thread, quality 75: speed 7 127 ms CPU 18.5 KiB, speed 8 60 ms
    /// 27.6 KiB, speed 9 (= 10 for stills) 20 ms 40 KiB with soft text; WebP 31 ms 40 KiB. tune-content screen changed
    /// nothing at 7-8 (libaom detects screen content itself) and tripled the cost at 9.</summary>
    internal static int NormalSpeed = 7, FastSpeed = 8, ReducedSpeed = 9;
    internal static double ReducedScale = 0.75;
    internal const int ReducedQualityDrop = 10;
    /// <summary>libaom tune-content: null keeps libaom's own screen-content detection.</summary>
    internal static string? TuneContent = null;

    /// <summary>Expected CPU ms per level for a 1280 px frame before real encodes are measured (Normal, Fast, Reduced, WebP).</summary>
    internal static double[] InitialEstimateMs => [130, 65, 15, 35];

    public static string Describe(string format, EncodeLevel level) => (format, level) switch
    {
        (_, EncodeLevel.WebP) => "WebP (CPU budget fallback)",
        ("avif", EncodeLevel.Normal) => $"libaom speed {NormalSpeed}",
        ("avif", EncodeLevel.Fast) => $"libaom speed {FastSpeed} (CPU budget)",
        ("avif", EncodeLevel.Reduced) => $"libaom speed {ReducedSpeed}, {ReducedScale:P0} size (CPU budget)",
        (_, EncodeLevel.Reduced) => $"{format}, {ReducedScale:P0} size (CPU budget)",
        _ => format,
    };

    /// <summary>Encoded bytes, the format written and the stored size (smaller than the grab at Reduced).</summary>
    public static (byte[] Bytes, string Format, int Width, int Height) Encode(byte* bgra, int width, int height, int stride, Settings s, EncodeLevel level)
    {
        int quality = Math.Clamp(s.Keyframes.Quality, 30, 95);
        string format = level == EncodeLevel.WebP ? "webp" : ImageCodec.Resolve(s.Keyframes.Format);
        if (level != EncodeLevel.Reduced) return (EncodeAt(bgra, width, height, stride, format, quality, level == EncodeLevel.Fast ? FastSpeed : NormalSpeed), format, width, height);

        int w = Math.Max(2, (int)(width * ReducedScale) & ~1), h = Math.Max(2, (int)(height * ReducedScale) & ~1);
        var info = new SKImageInfo(w, h, SKColorType.Bgra8888, SKAlphaType.Opaque);
        using var src = new SKPixmap(new SKImageInfo(width, height, SKColorType.Bgra8888, SKAlphaType.Opaque), (nint)bgra, stride);
        using var small = new SKBitmap(info);
        try
        {
            using (var dst = small.PeekPixels())
                if (!src.ScalePixels(dst, new SKSamplingOptions(SKFilterMode.Linear, SKMipmapMode.None))) throw new InvalidOperationException("Keyframe scale failed.");
            var bytes = EncodeAt((byte*)small.GetPixels(), w, h, w * 4, format, Math.Max(30, quality - ReducedQualityDrop), ReducedSpeed);
            return (bytes, format, w, h);
        }
        finally { new Span<byte>((void*)small.GetPixels(), w * h * 4).Clear(); }
    }

    private static byte[] EncodeAt(byte* bgra, int w, int h, int stride, string format, int quality, int speed) => format == "avif"
        ? Avif.Encode(bgra, w, h, stride, Math.Clamp(quality - ImageCodec.AvifQualityOffset, 0, 100), speed, threads: 1, tuneContent: TuneContent)
        : ImageCodec.EncodeBgra(bgra, w, h, stride, format, quality);

    /// <summary>Logs a change of the encoder in use (level or format).</summary>
    public static void LogChange(ref string last, string now)
    {
        if (now == last) return;
        Log.Info($"keyframe encoder: {now}");
        last = now;
    }
}
