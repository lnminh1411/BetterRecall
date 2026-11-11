// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Protocol;
using SkiaSharp;

namespace BetterRecall.Worker.Models;

/// <summary>8-bit RGB, row-major, no padding.</summary>
public sealed record RgbImage(int Width, int Height, byte[] Rgb);

public static class ImagePipeline
{
    public const long MaxPixels = 64L * 1024 * 1024;

    /// <summary>Request image to RGB. Alpha is dropped (like PIL convert("RGB")). Bad bytes throw BadRequestException.</summary>
    public static RgbImage Decode(ImageInput img)
    {
        if (img.Format == "bgra8")
        {
            var src = img.Bytes.Span;
            var rgb = new byte[img.Width * img.Height * 3];
            for (int y = 0; y < img.Height; y++)
            {
                var row = src.Slice(y * img.Stride, img.Width * 4);
                int o = y * img.Width * 3;
                for (int x = 0; x < img.Width; x++)
                {
                    rgb[o++] = row[x * 4 + 2];
                    rgb[o++] = row[x * 4 + 1];
                    rgb[o++] = row[x * 4 + 0];
                }
            }
            return new RgbImage(img.Width, img.Height, rgb);
        }

        using var data = SKData.CreateCopy(img.Bytes.Span);
        using var codec = SKCodec.Create(data) ?? throw new BadRequestException("undecodable image");
        var expected = img.Format switch { "webp" => SKEncodedImageFormat.Webp, "jpeg" => SKEncodedImageFormat.Jpeg, _ => SKEncodedImageFormat.Png };
        if (codec.EncodedFormat != expected) throw new BadRequestException("format mismatch");
        var info = codec.Info;
        if (info.Width <= 0 || info.Height <= 0 || (long)info.Width * info.Height > MaxPixels) throw new BadRequestException("image size");
        var want = new SKImageInfo(info.Width, info.Height, SKColorType.Rgba8888, SKAlphaType.Unpremul);
        using var bmp = new SKBitmap(want);
        var r = codec.GetPixels(want, bmp.GetPixels());
        if (r is not (SKCodecResult.Success or SKCodecResult.IncompleteInput)) throw new BadRequestException("undecodable image");
        var px = bmp.GetPixelSpan();
        var outRgb = new byte[info.Width * info.Height * 3];
        int rowBytes = bmp.RowBytes;
        for (int y = 0; y < info.Height; y++)
        {
            var row = px.Slice(y * rowBytes, info.Width * 4);
            int o = y * info.Width * 3;
            for (int x = 0; x < info.Width; x++)
            {
                outRgb[o++] = row[x * 4 + 0];
                outRgb[o++] = row[x * 4 + 1];
                outRgb[o++] = row[x * 4 + 2];
            }
        }
        return new RgbImage(info.Width, info.Height, outRgb);
    }

    /// <summary>Copies a rectangle out of a packed RGB image.</summary>
    public static RgbImage Crop(RgbImage img, int x, int y, int w, int h)
    {
        if (x < 0 || y < 0 || w <= 0 || h <= 0 || x + w > img.Width || y + h > img.Height)
            throw new ArgumentOutOfRangeException(nameof(w), $"crop {x},{y} {w}x{h} outside {img.Width}x{img.Height}");
        var dst = new byte[w * h * 3];
        for (int r = 0; r < h; r++)
            Buffer.BlockCopy(img.Rgb, ((y + r) * img.Width + x) * 3, dst, r * w * 3, w * 3);
        return new RgbImage(w, h, dst);
    }

    /// <summary>Resize + rescale + normalise into a [1,3,H,W] float tensor (CHW).</summary>
    public static float[] ToTensor(RgbImage img, int height, int width, ImagePreprocessSpec spec)
    {
        RgbImage sized;
        int offX = 0, offY = 0;
        if (spec.Fit == "pad")
        {
            double s = Math.Min((double)width / img.Width, (double)height / img.Height);
            int w = Math.Max(1, (int)Math.Round(img.Width * s)), h = Math.Max(1, (int)Math.Round(img.Height * s));
            sized = Resize(img, w, h, spec.Resample);
            offX = (width - w) / 2; offY = (height - h) / 2;
        }
        else sized = Resize(img, width, height, spec.Resample);

        var t = new float[3 * height * width];
        int plane = height * width;
        for (int c = 0; c < 3; c++)
        {
            float padN = (spec.PadValue * spec.Rescale - spec.Mean[c]) / spec.Std[c];
            if (spec.Fit == "pad") Array.Fill(t, padN, c * plane, plane);
            float scale = spec.Rescale / spec.Std[c], bias = -spec.Mean[c] / spec.Std[c];
            for (int y = 0; y < sized.Height; y++)
                for (int x = 0; x < sized.Width; x++)
                    t[c * plane + (y + offY) * width + (x + offX)] = sized.Rgb[(y * sized.Width + x) * 3 + c] * scale + bias;
        }
        return t;
    }

    /// <summary>transformers get_aspect_ratio_preserving_size (Gemma 4): the largest height and width, both multiples of
    /// patch*pool, whose patch count fits maxPatches, keeping the aspect ratio. Degenerate (very thin) images get one
    /// patch*pool side, the other capped at (maxPatches / pool^2) * patch*pool.</summary>
    public static (int Height, int Width) AspectSize(int height, int width, int patch, int maxPatches, int pool)
    {
        double targetPx = (double)maxPatches * patch * patch;
        double factor = Math.Sqrt(targetPx / ((double)height * width));
        int side = pool * patch;
        int th = (int)Math.Floor(factor * height / side) * side;
        int tw = (int)Math.Floor(factor * width / side) * side;
        if (th == 0 && tw == 0) throw new BadRequestException("image size");
        int maxSide = maxPatches / (pool * pool) * side;
        if (th == 0) { th = side; tw = Math.Min(width / height * side, maxSide); }
        else if (tw == 0) { tw = side; th = Math.Min(height / width * side, maxSide); }
        return (th, tw);
    }

    /// <summary>Patch form input, same as the Gemma 4 image processor: aspect-preserving resize, rescale/normalise,
    /// patch-by-patch raster order with each patch flattened as (row, column, channel), positions (x, y), padded to the
    /// graph's patch budget with zero patches at (-1,-1), and optional 3x3 average-pooling weights [Tokens, PatchBudget].
    /// SoftTokens = real patches / pool^2.</summary>
    public static (float[] Patches, long[] Positions, int SoftTokens, int Height, int Width, float[]? PoolWeights) ToPatches(RgbImage img, VisionGraph v)
    {
        int p = v.PatchSize, m = v.PatchBudget, dimP = p * p * 3;
        var (h, w) = AspectSize(img.Height, img.Width, p, m, v.Pool);
        var sized = Resize(img, w, h, v.Preprocess.Resample);
        int nh = h / p, nw = w / p, real = nh * nw;
        var patches = new float[m * dimP];
        var pos = new long[m * 2];
        Array.Fill(pos, -1L);
        var weights = v.PoolInput is not null ? new float[v.Tokens * m] : null;
        var spec = v.Preprocess;
        Span<float> scale = stackalloc float[3], bias = stackalloc float[3];
        for (int c = 0; c < 3; c++) { scale[c] = spec.Rescale / spec.Std[c]; bias[c] = -spec.Mean[c] / spec.Std[c]; }
        var rgb = sized.Rgb;
        for (int py = 0; py < nh; py++)
            for (int px = 0; px < nw; px++)
            {
                int idx = py * nw + px;
                pos[idx * 2] = px; pos[idx * 2 + 1] = py;
                if (weights is not null)
                    weights[(px / v.Pool + (nw / v.Pool) * (py / v.Pool)) * m + idx] = 1f / (v.Pool * v.Pool);
                int o = idx * dimP;
                for (int dy = 0; dy < p; dy++)
                {
                    int src = ((py * p + dy) * w + px * p) * 3;
                    for (int k = 0; k < p * 3; k++)
                        patches[o + dy * p * 3 + k] = rgb[src + k] * scale[k % 3] + bias[k % 3];
                }
            }
        return (patches, pos, real / (v.Pool * v.Pool), h, w, weights);
    }

    /// <summary>PIL-compatible separable resample (Image.resize with BICUBIC a=-0.5 or BILINEAR): the filter support
    /// widens with the downscale factor (antialias), horizontal pass first, rounding to 8 bit between passes.</summary>
    public static RgbImage Resize(RgbImage img, int width, int height, string resample)
    {
        if (img.Width == width && img.Height == height) return img;
        Func<double, double> filter; double support;
        if (resample == "bilinear") { filter = Triangle; support = 1.0; }
        else { filter = Bicubic; support = 2.0; }

        var tmp = img.Width == width ? img : Pass(img, width, horizontal: true, filter, support);
        return tmp.Height == height ? tmp : Pass(tmp, height, horizontal: false, filter, support);
    }

    private static double Triangle(double x) { x = Math.Abs(x); return x < 1.0 ? 1.0 - x : 0.0; }

    private static double Bicubic(double x)
    {
        const double a = -0.5;
        x = Math.Abs(x);
        if (x < 1.0) return ((a + 2.0) * x - (a + 3.0)) * x * x + 1;
        if (x < 2.0) return (((x - 5) * x + 8) * x - 4) * a;
        return 0.0;
    }

    private static RgbImage Pass(RgbImage src, int outSize, bool horizontal, Func<double, double> filter, double support)
    {
        int inSize = horizontal ? src.Width : src.Height;
        double scale = (double)inSize / outSize;
        double fscale = Math.Max(scale, 1.0);
        double sup = support * fscale;
        int ksize = (int)Math.Ceiling(sup) * 2 + 1;
        var bounds = new (int Min, int Count)[outSize];
        var weights = new double[outSize * ksize];
        for (int i = 0; i < outSize; i++)
        {
            double center = (i + 0.5) * scale;
            int xmin = Math.Max((int)(center - sup + 0.5), 0);
            int xmax = Math.Min((int)(center + sup + 0.5), inSize) - xmin;
            double total = 0;
            for (int j = 0; j < xmax; j++)
            {
                double w = filter((j + xmin - center + 0.5) / fscale);
                weights[i * ksize + j] = w; total += w;
            }
            if (total != 0) for (int j = 0; j < xmax; j++) weights[i * ksize + j] /= total;
            bounds[i] = (xmin, xmax);
        }

        int ow = horizontal ? outSize : src.Width, oh = horizontal ? src.Height : outSize;
        var dst = new byte[ow * oh * 3];
        var s = src.Rgb;
        for (int y = 0; y < oh; y++)
            for (int x = 0; x < ow; x++)
            {
                int i = horizontal ? x : y;
                var (min, count) = bounds[i];
                double r = 0, g = 0, b = 0;
                for (int j = 0; j < count; j++)
                {
                    double w = weights[i * ksize + j];
                    int sx = horizontal ? min + j : x, sy = horizontal ? y : min + j;
                    int p = (sy * src.Width + sx) * 3;
                    r += s[p] * w; g += s[p + 1] * w; b += s[p + 2] * w;
                }
                int o = (y * ow + x) * 3;
                dst[o] = Clip(r); dst[o + 1] = Clip(g); dst[o + 2] = Clip(b);
            }
        return new RgbImage(ow, oh, dst);
    }

    private static byte Clip(double v) => (byte)Math.Clamp((int)Math.Round(v, MidpointRounding.AwayFromZero), 0, 255);
}
