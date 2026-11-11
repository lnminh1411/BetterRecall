// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using SkiaSharp;

namespace BetterRecall.Agent.Capture;

/// <summary>Image encode/resize through SkiaSharp (P/Invoke, Native AOT safe).</summary>
internal static unsafe class ImageCodec
{
    public static string Mime(string format) => format == "jpeg" ? "image/jpeg" : "image/webp";

    public static byte[] EncodeBgra(byte* bgra, int width, int height, int stride, string format, int quality)
    {
        var info = new SKImageInfo(width, height, SKColorType.Bgra8888, SKAlphaType.Opaque);
        using var pixmap = new SKPixmap(info, (nint)bgra, stride);
        using SKData? data = format == "jpeg"
            ? pixmap.Encode(new SKJpegEncoderOptions(quality, SKJpegEncoderDownsample.Downsample420, SKJpegEncoderAlphaOption.Ignore))
            : pixmap.Encode(new SKWebpEncoderOptions(SKWebpEncoderCompression.Lossy, quality));
        if (data is null) throw new InvalidOperationException("Image encode failed.");
        return data.ToArray();
    }

    /// <summary>Decodes a WebP/JPEG/PNG, scales it to width (never up) and encodes WebP.</summary>
    public static (byte[] Bytes, int Width, int Height) ResizeToWebp(byte[] encoded, int width, int quality)
    {
        using var src = SKBitmap.Decode(encoded) ?? throw new InvalidDataException("Keyframe decode failed.");
        int w = Math.Min(width, src.Width);
        int h = Math.Max(1, (int)Math.Round(src.Height * (double)w / src.Width));
        SKBitmap? scaled = null;
        try
        {
            var target = src;
            if (w != src.Width)
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
            if (scaled is not null)
            {
                // Zero decoded pixels before release.
                new Span<byte>((void*)scaled.GetPixels(), scaled.ByteCount).Clear();
                scaled.Dispose();
            }
            new Span<byte>((void*)src.GetPixels(), src.ByteCount).Clear();
        }
    }
}
