// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;
using SkiaSharp;

namespace BetterRecall.Agent.Capture;

/// <summary>
/// Keyframe image codecs. WebP and JPEG through SkiaSharp; AVIF (libavif: aom encode, dav1d decode) and JPEG XL
/// (libjxl) through the native DLLs next to the exe. All P/Invoke, Native AOT safe. keyframes.format avif or jxl falls
/// back to WebP when its DLL is missing. The UI and the worker only decode WebP, JPEG and PNG: Transcode and
/// DecodeBgra bridge the other two.
/// </summary>
internal static unsafe class ImageCodec
{
    /// <summary>libavif speed (0 slowest .. 10 fastest) and libjxl effort (1 fastest .. 9) for keyframes. Spike 2026-11-12 on
    /// six 1280 px UI screenshots: AVIF speed 7 at quality 50 was 30 % smaller than WebP quality 75 with a lower mean
    /// error, 70-120 ms per frame; JPEG XL effort 3 gave no saving over WebP.</summary>
    internal static int AvifSpeed = 7, JxlEffort = 3;

    /// <summary>keyframes.quality is tuned for WebP; AVIF reaches the same look 25 points lower.</summary>
    public const int AvifQualityOffset = 25;

    private static int _warnedAvif, _warnedJxl;

    public static string Mime(string format) => format switch
    {
        "jpeg" => "image/jpeg",
        "avif" => "image/avif",
        "jxl" => "image/jxl",
        "png" => "image/png",
        _ => "image/webp",
    };

    /// <summary>Mime types the UI (WinUI BitmapImage without store extensions) and the worker (Skia) decode.</summary>
    public static bool CommonlyDecodable(string mime) => mime is "image/webp" or "image/jpeg" or "image/png";

    /// <summary>Loads the AVIF and JPEG XL DLLs before Arbitrary Code Guard is applied (keeps the first capture fast).</summary>
    public static void Preload()
    {
        if (!Avif.TryLoad()) Log.Warn($"AVIF codec not loaded: {Avif.LoadError}");
        if (!Jxl.TryLoad()) Log.Warn($"JPEG XL codec not loaded: {Jxl.LoadError}");
    }

    /// <summary>keyframes.format to the format actually written: avif and jxl fall back to webp without their DLL.</summary>
    public static string Resolve(string format)
    {
        switch (format)
        {
            case "jpeg": return "jpeg";
            case "avif":
                if (Avif.TryLoad()) return "avif";
                if (Interlocked.Exchange(ref _warnedAvif, 1) == 0) Log.Warn($"AVIF unavailable ({Avif.LoadError}); keyframes are WebP");
                return "webp";
            case "jxl":
                if (Jxl.TryLoad()) return "jxl";
                if (Interlocked.Exchange(ref _warnedJxl, 1) == 0) Log.Warn($"JPEG XL unavailable ({Jxl.LoadError}); keyframes are WebP");
                return "webp";
            default: return "webp";
        }
    }

    public static byte[] EncodeBgra(byte* bgra, int width, int height, int stride, string format, int quality)
    {
        switch (format)
        {
            case "avif": return Avif.Encode(bgra, width, height, stride, Math.Clamp(quality - AvifQualityOffset, 0, 100), AvifSpeed);
            case "jxl": return Jxl.Encode(bgra, width, height, stride, quality, JxlEffort);
        }
        var info = new SKImageInfo(width, height, SKColorType.Bgra8888, SKAlphaType.Opaque);
        using var pixmap = new SKPixmap(info, (nint)bgra, stride);
        using SKData? data = format == "jpeg"
            ? pixmap.Encode(new SKJpegEncoderOptions(quality, SKJpegEncoderDownsample.Downsample420, SKJpegEncoderAlphaOption.Ignore))
            : pixmap.Encode(new SKWebpEncoderOptions(SKWebpEncoderCompression.Lossy, quality));
        if (data is null) throw new InvalidOperationException("Image encode failed.");
        return data.ToArray();
    }

    /// <summary>Any stored keyframe to BGRA, stride width * 4. The caller clears Bgra after use.</summary>
    public static (byte[] Bgra, int Width, int Height) DecodeBgra(byte[] encoded, string mime)
    {
        switch (mime)
        {
            case "image/avif": return Avif.Decode(encoded);
            case "image/jxl": return Jxl.Decode(encoded);
        }
        using var src = SKBitmap.Decode(encoded) ?? throw new InvalidDataException("Keyframe decode failed.");
        using var bgra = src.ColorType == SKColorType.Bgra8888 ? null : src.Copy(SKColorType.Bgra8888);
        var b = bgra ?? src;
        var pixels = new byte[b.Width * b.Height * 4];
        try
        {
            for (int y = 0; y < b.Height; y++)
                Marshal.Copy(b.GetPixels() + y * b.RowBytes, pixels, y * b.Width * 4, b.Width * 4);
            return (pixels, b.Width, b.Height);
        }
        finally
        {
            new Span<byte>((void*)src.GetPixels(), src.ByteCount).Clear();
            if (bgra is not null) new Span<byte>((void*)bgra.GetPixels(), bgra.ByteCount).Clear();
        }
    }

    /// <summary>Decodes, scales to width (never up) and encodes WebP.</summary>
    public static (byte[] Bytes, int Width, int Height) ResizeToWebp(byte[] encoded, string mime, int width, int quality)
    {
        var (px, w0, h0) = DecodeBgra(encoded, mime);
        var info = new SKImageInfo(w0, h0, SKColorType.Bgra8888, SKAlphaType.Opaque);
        using var src = new SKBitmap(info);
        SKBitmap? scaled = null;
        try
        {
            Marshal.Copy(px, 0, src.GetPixels(), px.Length);
            Array.Clear(px);
            int w = Math.Min(width, w0);
            int h = Math.Max(1, (int)Math.Round(h0 * (double)w / w0));
            var target = src;
            if (w != w0)
            {
                scaled = src.Resize(new SKImageInfo(w, h, SKColorType.Bgra8888, SKAlphaType.Opaque), new SKSamplingOptions(SKFilterMode.Linear, SKMipmapMode.Linear))
                         ?? throw new InvalidOperationException("Resize failed.");
                target = scaled;
            }
            using var pixmap = target.PeekPixels();
            using var data = pixmap.Encode(new SKWebpEncoderOptions(SKWebpEncoderCompression.Lossy, quality))
                             ?? throw new InvalidOperationException("Thumbnail encode failed.");
            return (data.ToArray(), target.Width, target.Height);
        }
        finally
        {
            Array.Clear(px);
            if (scaled is not null)
            {
                new Span<byte>((void*)scaled.GetPixels(), scaled.ByteCount).Clear();
                scaled.Dispose();
            }
            new Span<byte>((void*)src.GetPixels(), src.ByteCount).Clear();
        }
    }

    /// <summary>Re-encodes a keyframe as WebP or JPEG at its size. Returns the new bytes and mime.</summary>
    public static (byte[] Bytes, string Mime, int Width, int Height) Transcode(byte[] encoded, string mime, string toFormat, int quality)
    {
        var (px, w, h) = DecodeBgra(encoded, mime);
        try
        {
            fixed (byte* p = px)
                return (EncodeBgra(p, w, h, w * 4, toFormat, quality), Mime(toFormat), w, h);
        }
        finally { Array.Clear(px); }
    }
}
