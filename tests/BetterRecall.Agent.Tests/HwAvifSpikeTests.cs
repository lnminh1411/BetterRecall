// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Globalization;
using System.Runtime.InteropServices;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;
using SkiaSharp;
using Xunit.Abstractions;

namespace BetterRecall.Agent.Tests;

public unsafe class HwAvifSpikeTests(ITestOutputHelper output)
{
    [Fact]
    public void Hw_vs_libaom_spike()
    {
        var dir = Environment.GetEnvironmentVariable("BR_HWAVIF_SPIKE");
        if (string.IsNullOrEmpty(dir) || !Directory.Exists(dir)) return;
        if (!Avif.TryLoad()) return;

        var repoRoot = FindRepoRoot();
        var syntheticDir = Path.Combine(dir, "synthetic");
        Directory.CreateDirectory(syntheticDir);

        var images = new List<SpikeImage>();
        try
        {
            // 1. Top-level PNGs in dir
            foreach (var file in Directory.GetFiles(dir, "*.png", SearchOption.TopDirectoryOnly))
            {
                using var src0 = SKBitmap.Decode(file);
                if (src0 is null || src0.Width <= 0 || src0.Height <= 0) continue;
                int w = Math.Min(1280, src0.Width);
                int h = (int)((long)src0.Height * w / src0.Width);
                if ((h & 1) != 0) h--;
                if ((w & 1) != 0) w--;
                if (w <= 0 || h <= 0) continue;
                using var src = src0.Resize(new SKImageInfo(w, h, SKColorType.Bgra8888, SKAlphaType.Opaque), new SKSamplingOptions(SKFilterMode.Linear, SKMipmapMode.Linear));
                if (src is null) continue;
                var px = new byte[w * h * 4];
                Marshal.Copy(src.GetPixels(), px, 0, px.Length);
                images.Add(new SpikeImage(Path.GetFileName(file), w, h, px));
            }

            // 2. Synthetic images
            images.Add(RenderCode(repoRoot, syntheticDir));
            images.Add(RenderPage(repoRoot, syntheticDir));
            images.Add(RenderColourUi(syntheticDir));

            // Encoders and settings
            var libaomQs = (Environment.GetEnvironmentVariable("BR_HWAVIF_SPIKE_Q") ?? "60,75,90")
                .Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
                .Select(int.Parse)
                .ToArray();

            var hwQps = (Environment.GetEnvironmentVariable("BR_HWAVIF_SPIKE_QP") ?? "60,80,100,120,140,160,180")
                .Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
                .Select(int.Parse)
                .ToArray();

            using var devices = new GpuDeviceCache();
            using var hw = HwAvifEncoder.Create(devices, out var why);
            if (hw is null)
            {
                output.WriteLine($"hardware unavailable: {why}");
            }

            var rows = new List<SpikeRow>();

            foreach (var img in images)
            {
                // Precompute source metrics
                int totalPixels = img.Width * img.Height;
                var srcY = new double[totalPixels];
                var srcCb = new double[totalPixels];
                var srcCr = new double[totalPixels];
                var isTextPixel = new bool[totalPixels];
                var isChromaPixel = new bool[totalPixels];
                int chromaCount = 0;
                int textCount = 0;

                for (int i = 0; i < totalPixels; i++)
                {
                    int o = i * 4;
                    byte b = img.Bgra[o];
                    byte g = img.Bgra[o + 1];
                    byte r = img.Bgra[o + 2];

                    srcY[i] = 0.2126 * r + 0.7152 * g + 0.0722 * b;

                    int maxRgb = Math.Max(r, Math.Max(g, b));
                    int minRgb = Math.Min(r, Math.Min(g, b));
                    if (maxRgb - minRgb >= 60)
                    {
                        isChromaPixel[i] = true;
                        srcCb[i] = -0.1146 * r - 0.3854 * g + 0.5 * b;
                        srcCr[i] = 0.5 * r - 0.4542 * g - 0.0458 * b;
                        chromaCount++;
                    }
                }

                // 16x16 blocks for text detection
                for (int by = 0; by < img.Height; by += 16)
                {
                    int blockH = Math.Min(16, img.Height - by);
                    for (int bx = 0; bx < img.Width; bx += 16)
                    {
                        int blockW = Math.Min(16, img.Width - bx);
                        double minY = double.MaxValue;
                        double maxY = double.MinValue;

                        for (int dy = 0; dy < blockH; dy++)
                        {
                            int y = by + dy;
                            for (int dx = 0; dx < blockW; dx++)
                            {
                                int x = bx + dx;
                                double yVal = srcY[y * img.Width + x];
                                if (yVal < minY) minY = yVal;
                                if (yVal > maxY) maxY = yVal;
                            }
                        }

                        if (maxY - minY >= 64.0)
                        {
                            for (int dy = 0; dy < blockH; dy++)
                            {
                                int y = by + dy;
                                for (int dx = 0; dx < blockW; dx++)
                                {
                                    int idx = y * img.Width + (bx + dx);
                                    isTextPixel[idx] = true;
                                    textCount++;
                                }
                            }
                        }
                    }
                }

                // libaom runs
                foreach (int q in libaomQs)
                {
                    string setting = $"q{q}";
                    var proc = Process.GetCurrentProcess();
                    proc.Refresh();
                    var cpuStart = proc.TotalProcessorTime;
                    var sw = Stopwatch.StartNew();

                    byte[]? enc = null;
                    for (int run = 0; run < 3; run++)
                    {
                        fixed (byte* p = img.Bgra)
                        {
                            enc = ImageCodec.EncodeBgra(p, img.Width, img.Height, img.Width * 4, "avif", q);
                        }
                    }

                    sw.Stop();
                    proc.Refresh();
                    var cpuEnd = proc.TotalProcessorTime;
                    double encodeMs = sw.Elapsed.TotalMilliseconds / 3.0;
                    double cpuMs = (cpuEnd - cpuStart).TotalMilliseconds / 3.0;

                    if (enc is null) continue;

                    var (dec, dw, dh) = ImageCodec.DecodeBgra(enc, "image/avif");
                    if (dw != img.Width || dh != img.Height)
                    {
                        output.WriteLine($"Decoded size mismatch for {img.Name} {setting}: expected {img.Width}x{img.Height}, got {dw}x{dh}");
                        Array.Clear(dec);
                        continue;
                    }

                    try
                    {
                        ComputeMetrics(img.Width, img.Height, srcY, srcCb, srcCr, isTextPixel, isChromaPixel, textCount, chromaCount, dec,
                            out double psnrY, out double psnrText, out double chromaMad);

                        var row = new SpikeRow(img.Name, img.Width, img.Height, "libaom", setting, 0, enc.Length, encodeMs, cpuMs, psnrY, psnrText, chromaMad);
                        rows.Add(row);
                        output.WriteLine(FormatRow(row));
                    }
                    finally
                    {
                        Array.Clear(dec);
                    }
                }

                // hardware runs
                if (hw is not null && hwQps.Length > 0)
                {
                    foreach (int qp in hwQps)
                    {
                        string setting = $"qp{qp}";
                        string? hwErr = null;
                        byte[]? enc = null;

                        double oldQpAt75 = HwAvifEncoder.QpAt75;
                        double oldQpPerQuality = HwAvifEncoder.QpPerQuality;
                        var proc = Process.GetCurrentProcess();
                        var cpuStart = TimeSpan.Zero;
                        var sw = new Stopwatch();
                        try
                        {
                            HwAvifEncoder.QpAt75 = qp;
                            HwAvifEncoder.QpPerQuality = 0;
                            // Untimed warm-up: every QP opens its own encoder session.
                            fixed (byte* p = img.Bgra) hw.Encode(p, img.Width, img.Height, img.Width * 4, 75, out _);
                            proc.Refresh();
                            cpuStart = proc.TotalProcessorTime;
                            sw.Start();
                            for (int run = 0; run < 3; run++)
                            {
                                fixed (byte* p = img.Bgra)
                                {
                                    enc = hw.Encode(p, img.Width, img.Height, img.Width * 4, 75, out hwErr);
                                }
                                if (enc is null) break;
                            }
                        }
                        finally
                        {
                            HwAvifEncoder.QpAt75 = oldQpAt75;
                            HwAvifEncoder.QpPerQuality = oldQpPerQuality;
                        }

                        sw.Stop();
                        proc.Refresh();
                        var cpuEnd = proc.TotalProcessorTime;

                        if (enc is null)
                        {
                            output.WriteLine($"hw error on {img.Name} {setting}: {hwErr}");
                            continue;
                        }

                        double encodeMs = sw.Elapsed.TotalMilliseconds / 3.0;
                        double cpuMs = (cpuEnd - cpuStart).TotalMilliseconds / 3.0;

                        var (dec, dw, dh) = ImageCodec.DecodeBgra(enc, "image/avif");
                        if (dw != img.Width || dh != img.Height)
                        {
                            output.WriteLine($"Decoded size mismatch for {img.Name} {setting}: expected {img.Width}x{img.Height}, got {dw}x{dh}");
                            Array.Clear(dec);
                            continue;
                        }

                        try
                        {
                            ComputeMetrics(img.Width, img.Height, srcY, srcCb, srcCr, isTextPixel, isChromaPixel, textCount, chromaCount, dec,
                                out double psnrY, out double psnrText, out double chromaMad);

                            var row = new SpikeRow(img.Name, img.Width, img.Height, "hw", setting, qp, enc.Length, encodeMs, cpuMs, psnrY, psnrText, chromaMad);
                            rows.Add(row);
                            output.WriteLine(FormatRow(row));
                        }
                        finally
                        {
                            Array.Clear(dec);
                        }
                    }
                }
            }

            // Write CSV
            var csvLines = new List<string>
            {
                "image,width,height,encoder,setting,bytes,encodeMs,cpuMs,psnrY,psnrText,chromaMad"
            };
            foreach (var r in rows)
            {
                csvLines.Add(FormattableString.Invariant(
                    $"{r.Image},{r.Width},{r.Height},{r.Encoder},{r.Setting},{r.Bytes},{r.EncodeMs:F2},{r.CpuMs:F2},{r.PsnrY:F2},{r.PsnrText:F2},{r.ChromaMad:F2}"));
            }
            File.WriteAllLines(Path.Combine(dir, "spike.csv"), csvLines);

            // Summary
            var summaryLines = new List<string>();
            foreach (var img in images)
            {
                var libaomRows = rows.Where(r => r.Image == img.Name && r.Encoder == "libaom");
                var hwRows = rows.Where(r => r.Image == img.Name && r.Encoder == "hw").ToList();

                foreach (var libaom in libaomRows)
                {
                    var match = hwRows
                        .Where(h => h.PsnrText >= libaom.PsnrText)
                        .OrderByDescending(h => h.Qp)
                        .FirstOrDefault();

                    string line;
                    if (match is not null)
                    {
                        double ratio = (double)match.Bytes / libaom.Bytes;
                        line = FormattableString.Invariant(
                            $"MATCH {img.Name} libaom {libaom.Setting}: {libaom.Bytes / 1024.0:F1} KiB text {libaom.PsnrText:F2} chroma {libaom.ChromaMad:F2} | hw {match.Setting}: {match.Bytes / 1024.0:F1} KiB ({ratio:P0} of libaom) text {match.PsnrText:F2} chroma {match.ChromaMad:F2} | enc {libaom.EncodeMs:F0} vs {match.EncodeMs:F0} ms, cpu {libaom.CpuMs:F0} vs {match.CpuMs:F0} ms");
                    }
                    else
                    {
                        line = FormattableString.Invariant(
                            $"MATCH {img.Name} libaom {libaom.Setting}: {libaom.Bytes / 1024.0:F1} KiB text {libaom.PsnrText:F2} chroma {libaom.ChromaMad:F2} | no hw setting reaches libaom text PSNR");
                    }
                    output.WriteLine(line);
                    summaryLines.Add(line);
                }
            }

            if (summaryLines.Count > 0)
            {
                File.AppendAllLines(Path.Combine(dir, "spike-summary.txt"), summaryLines);
            }
        }
        finally
        {
            foreach (var img in images)
            {
                img.Dispose();
            }
        }
    }

    private static string FormatRow(SpikeRow r) =>
        FormattableString.Invariant(
            $"{r.Image} {r.Width}x{r.Height} {r.Encoder} {r.Setting}: {r.Bytes / 1024.0:F1} KiB, enc {r.EncodeMs:F1} ms, cpu {r.CpuMs:F1} ms, PSNR-Y {r.PsnrY:F2}, text {r.PsnrText:F2}, chroma {r.ChromaMad:F2}");

    private static void ComputeMetrics(int w, int h, double[] srcY, double[] srcCb, double[] srcCr,
        bool[] isTextPixel, bool[] isChromaPixel, int textCount, int chromaCount, byte[] dec,
        out double psnrY, out double psnrText, out double chromaMad)
    {
        int totalPixels = w * h;
        double sumSqY = 0;
        double sumSqText = 0;
        double sumChromaDiff = 0;

        for (int i = 0; i < totalPixels; i++)
        {
            int o = i * 4;
            byte b = dec[o];
            byte g = dec[o + 1];
            byte r = dec[o + 2];

            double decY = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            double diffY = srcY[i] - decY;
            sumSqY += diffY * diffY;

            if (isTextPixel[i])
            {
                sumSqText += diffY * diffY;
            }

            if (isChromaPixel[i])
            {
                double decCb = -0.1146 * r - 0.3854 * g + 0.5 * b;
                double decCr = 0.5 * r - 0.4542 * g - 0.0458 * b;
                sumChromaDiff += (Math.Abs(srcCb[i] - decCb) + Math.Abs(srcCr[i] - decCr)) / 2.0;
            }
        }

        double mseY = sumSqY / totalPixels;
        psnrY = mseY < 1e-12 ? 99.0 : 10.0 * Math.Log10((255.0 * 255.0) / mseY);

        if (textCount == 0)
        {
            psnrText = 99.0;
        }
        else
        {
            double mseText = sumSqText / textCount;
            psnrText = mseText < 1e-12 ? 99.0 : 10.0 * Math.Log10((255.0 * 255.0) / mseText);
        }

        chromaMad = chromaCount > 0 ? (sumChromaDiff / chromaCount) : 0.0;
    }

    private static SpikeImage RenderCode(string repoRoot, string syntheticDir)
    {
        const int w = 1280, h = 720;
        var info = new SKImageInfo(w, h, SKColorType.Bgra8888, SKAlphaType.Opaque);
        using var bmp = new SKBitmap(info);
        using var canvas = new SKCanvas(bmp);
        canvas.Clear(new SKColor(0x1E, 0x1E, 0x1E));

        var tf = SKTypeface.FromFamilyName("Cascadia Mono");
        if (tf is null || !string.Equals(tf.FamilyName, "Cascadia Mono", StringComparison.OrdinalIgnoreCase))
        {
            var fallback = SKTypeface.FromFamilyName("Consolas");
            if (fallback is not null)
            {
                tf?.Dispose();
                tf = fallback;
            }
        }
        using var typeface = tf;
        using var font = new SKFont(typeface, 13);
        using var paint = new SKPaint { IsAntialias = true };

        var commentColor = new SKColor(0x6A, 0x99, 0x55);
        var stringColor = new SKColor(0xCE, 0x91, 0x78);
        var keywordColor = new SKColor(0x56, 0x9C, 0xD6);
        var otherColor = new SKColor(0xD4, 0xD4, 0xD4);

        var keywords = new HashSet<string>(StringComparer.Ordinal)
        {
            "public", "private", "internal", "static", "readonly", "int", "var",
            "return", "if", "else", "for", "foreach", "new", "class", "struct",
            "double", "bool", "const", "using", "namespace"
        };

        var changeMapPath = Path.Combine(repoRoot, "src", "BetterRecall.Agent", "Capture", "ChangeMap.cs");
        var lines = File.Exists(changeMapPath) ? File.ReadAllLines(changeMapPath) : [];

        const float lineHeight = 18f;
        const float leftMargin = 12f;
        font.GetFontMetrics(out var fm);
        float baselineOffset = -fm.Ascent;
        float yTop = 0f;

        foreach (var rawLine in lines)
        {
            if (yTop + lineHeight > h) break;
            float y = yTop + baselineOffset;
            float x = leftMargin;
            var line = rawLine.Replace("\t", "    ");

            int i = 0;
            while (i < line.Length)
            {
                if (line[i..].StartsWith("//"))
                {
                    var text = line[i..];
                    paint.Color = commentColor;
                    canvas.DrawText(text, x, y, font, paint);
                    x += font.MeasureText(text);
                    break;
                }
                if (line[i] == '"')
                {
                    int start = i;
                    i++;
                    while (i < line.Length)
                    {
                        if (line[i] == '\\' && i + 1 < line.Length) { i += 2; continue; }
                        if (line[i] == '"') { i++; break; }
                        i++;
                    }
                    var text = line[start..i];
                    paint.Color = stringColor;
                    canvas.DrawText(text, x, y, font, paint);
                    x += font.MeasureText(text);
                    continue;
                }
                if (char.IsLetter(line[i]) || line[i] == '_')
                {
                    int start = i;
                    while (i < line.Length && (char.IsLetterOrDigit(line[i]) || line[i] == '_')) i++;
                    var word = line[start..i];
                    paint.Color = keywords.Contains(word) ? keywordColor : otherColor;
                    canvas.DrawText(word, x, y, font, paint);
                    x += font.MeasureText(word);
                    continue;
                }
                int otherStart = i;
                while (i < line.Length && line[i] != '"' && !line[i..].StartsWith("//") && !char.IsLetter(line[i]) && line[i] != '_')
                {
                    i++;
                }
                var otherText = line[otherStart..i];
                paint.Color = otherColor;
                canvas.DrawText(otherText, x, y, font, paint);
                x += font.MeasureText(otherText);
            }

            yTop += lineHeight;
        }

        var pngPath = Path.Combine(syntheticDir, "code.png");
        SavePng(bmp, pngPath);

        var px = new byte[w * h * 4];
        Marshal.Copy(bmp.GetPixels(), px, 0, px.Length);
        return new SpikeImage("code", w, h, px);
    }

    private static SpikeImage RenderPage(string repoRoot, string syntheticDir)
    {
        const int w = 1280, h = 720;
        var info = new SKImageInfo(w, h, SKColorType.Bgra8888, SKAlphaType.Opaque);
        using var bmp = new SKBitmap(info);
        using var canvas = new SKCanvas(bmp);
        canvas.Clear(SKColors.White);

        using var tfNormal = SKTypeface.FromFamilyName("Segoe UI");
        using var tfBold = SKTypeface.FromFamilyName("Segoe UI", SKFontStyle.Bold);
        using var fontNormal = new SKFont(tfNormal, 14);
        using var fontBold = new SKFont(tfBold, 20);
        using var paint = new SKPaint { IsAntialias = true };

        var linkColor = new SKColor(0x05, 0x63, 0xC1);
        var textColor = new SKColor(0x1B, 0x1B, 0x1B);

        fontNormal.GetFontMetrics(out var fmNormal);
        fontBold.GetFontMetrics(out var fmBold);
        float baselineNormal = -fmNormal.Ascent;
        float baselineBold = -fmBold.Ascent;

        var docPath = Path.Combine(repoRoot, "docs", "v1-design.md");
        var rawLines = File.Exists(docPath) ? File.ReadAllLines(docPath) : [];

        float topY = 40f;
        const float leftMargin = 40f;
        const float wrapX = 1200f;
        int wordCount = 0;
        bool pageFull = false;

        foreach (var rawLine in rawLines)
        {
            if (pageFull) break;
            if (string.IsNullOrWhiteSpace(rawLine) || rawLine.StartsWith('|')) continue;

            bool isHeading = rawLine.StartsWith('#');
            var font = isHeading ? fontBold : fontNormal;
            float lineHeight = isHeading ? 28f : 20f;
            float baselineOffset = isHeading ? baselineBold : baselineNormal;

            if (topY + lineHeight > h - 40f) break;

            var words = rawLine.Split(' ', StringSplitOptions.RemoveEmptyEntries);
            float currentX = leftMargin;
            float spaceW = font.MeasureText(" ");

            for (int wi = 0; wi < words.Length; wi++)
            {
                var word = words[wi];
                float wordW = font.MeasureText(word);
                if (currentX + wordW > wrapX && currentX > leftMargin)
                {
                    topY += lineHeight;
                    if (topY + lineHeight > h - 40f)
                    {
                        pageFull = true;
                        break;
                    }
                    currentX = leftMargin;
                }

                wordCount++;
                paint.Color = (wordCount % 7 == 0) ? linkColor : textColor;
                canvas.DrawText(word, currentX, topY + baselineOffset, font, paint);
                currentX += wordW + spaceW;
            }

            topY += lineHeight;
        }

        var pngPath = Path.Combine(syntheticDir, "page.png");
        SavePng(bmp, pngPath);

        var px = new byte[w * h * 4];
        Marshal.Copy(bmp.GetPixels(), px, 0, px.Length);
        return new SpikeImage("page", w, h, px);
    }

    private static SpikeImage RenderColourUi(string syntheticDir)
    {
        const int w = 1280, h = 720;
        var info = new SKImageInfo(w, h, SKColorType.Bgra8888, SKAlphaType.Opaque);
        using var bmp = new SKBitmap(info);
        using var canvas = new SKCanvas(bmp);
        canvas.Clear(SKColors.White);

        using var tf = SKTypeface.FromFamilyName("Segoe UI");
        using var font12 = new SKFont(tf, 12);
        using var paint = new SKPaint { IsAntialias = true };

        var cardBg = new SKColor(0xF3, 0xF3, 0xF3);
        var colors = new[]
        {
            new SKColor(0xD1, 0x34, 0x38),
            new SKColor(0x10, 0x7C, 0x10),
            new SKColor(0x00, 0x78, 0xD4),
            new SKColor(0x87, 0x64, 0xB8)
        };

        font12.GetFontMetrics(out var fm);
        float baseline12 = -fm.Ascent;

        int cardIndex = 0;
        for (int row = 0; row < 6; row++)
        {
            for (int col = 0; col < 4; col++, cardIndex++)
            {
                float cardX = 16f + col * (300f + 16f);
                float cardY = 16f + row * (105f + 16f);

                // Card background rounded rect 300x105, radius 6
                paint.Color = cardBg;
                paint.Style = SKPaintStyle.Fill;
                canvas.DrawRoundRect(new SKRect(cardX, cardY, cardX + 300f, cardY + 105f), 6f, 6f, paint);

                // Three lines of text, rotating color per line and per card
                int itemNum = cardIndex + 1;
                string[] cardLines =
                [
                    $"Item {itemNum} \u00B7 updated 3 min ago \u00B7 4.2 MB",
                    $"Item {itemNum} \u00B7 updated 5 min ago \u00B7 1.8 MB",
                    $"Item {itemNum} \u00B7 updated 12 min ago \u00B7 9.1 MB"
                ];

                for (int lineIdx = 0; lineIdx < 3; lineIdx++)
                {
                    var color = colors[(cardIndex + lineIdx) % 4];
                    paint.Color = color;
                    float tx = cardX + 12f;
                    float ty = cardY + 14f + lineIdx * 18f + baseline12;
                    canvas.DrawText(cardLines[lineIdx], tx, ty, font12, paint);
                }

                // Filled rounded button 80x24, radius 4
                float btnX = cardX + 300f - 80f - 12f;
                float btnY = cardY + 105f - 24f - 12f;
                var btnColor = colors[(cardIndex + 3) % 4];
                paint.Color = btnColor;
                paint.Style = SKPaintStyle.Fill;
                canvas.DrawRoundRect(new SKRect(btnX, btnY, btnX + 80f, btnY + 24f), 4f, 4f, paint);

                // Label "Open" in white size 12
                float textW = font12.MeasureText("Open");
                float openX = btnX + (80f - textW) / 2f;
                float openY = btnY + 16f;
                paint.Color = SKColors.White;
                canvas.DrawText("Open", openX, openY, font12, paint);
            }
        }

        var pngPath = Path.Combine(syntheticDir, "colour-ui.png");
        SavePng(bmp, pngPath);

        var px = new byte[w * h * 4];
        Marshal.Copy(bmp.GetPixels(), px, 0, px.Length);
        return new SpikeImage("colour-ui", w, h, px);
    }

    private static void SavePng(SKBitmap bmp, string path)
    {
        using var img = SKImage.FromBitmap(bmp);
        using var data = img.Encode(SKEncodedImageFormat.Png, 100);
        if (data is not null)
        {
            File.WriteAllBytes(path, data.ToArray());
        }
    }

    private static string FindRepoRoot()
    {
        var current = AppContext.BaseDirectory;
        while (!string.IsNullOrEmpty(current))
        {
            if (File.Exists(Path.Combine(current, "BetterRecall.slnx"))) return current;
            var parent = Directory.GetParent(current);
            if (parent is null) break;
            current = parent.FullName;
        }
        throw new DirectoryNotFoundException("Could not find repo root with BetterRecall.slnx");
    }

    private sealed class SpikeImage(string name, int width, int height, byte[] bgra) : IDisposable
    {
        public string Name { get; } = name;
        public int Width { get; } = width;
        public int Height { get; } = height;
        public byte[] Bgra { get; } = bgra;

        public void Dispose() => Array.Clear(Bgra);
    }

    private sealed record SpikeRow(
        string Image,
        int Width,
        int Height,
        string Encoder,
        string Setting,
        int Qp,
        int Bytes,
        double EncodeMs,
        double CpuMs,
        double PsnrY,
        double PsnrText,
        double ChromaMad
    );
}
