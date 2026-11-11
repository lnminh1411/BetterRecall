// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices.WindowsRuntime;
using BetterRecall.Contracts;
using Windows.Graphics.Imaging;

namespace BetterRecall.Worker.Models.Ocr;

/// <summary>ocr.engine = windows: Windows.Media.Ocr, built into Windows and offline, but it reads only languages whose
/// Windows OCR language pack is installed, one language per engine. Picks the first ocr.languages code Windows can
/// read, else the user profile languages. Confidence is not reported by Windows (1 for every line).</summary>
public sealed class WindowsOcr
{
    private readonly Windows.Media.Ocr.OcrEngine _engine;
    public string Language => _engine.RecognizerLanguage.LanguageTag;

    private WindowsOcr(Windows.Media.Ocr.OcrEngine engine) => _engine = engine;

    /// <summary>Null when no requested language and no profile language has an installed OCR pack.</summary>
    public static WindowsOcr? TryCreate(IEnumerable<string> languages)
    {
        foreach (var code in languages)
        {
            try
            {
                var lang = new Windows.Globalization.Language(code);
                if (Windows.Media.Ocr.OcrEngine.IsLanguageSupported(lang) && Windows.Media.Ocr.OcrEngine.TryCreateFromLanguage(lang) is { } e)
                    return new WindowsOcr(e);
            }
            catch (ArgumentException) { }
        }
        return Windows.Media.Ocr.OcrEngine.TryCreateFromUserProfileLanguages() is { } p ? new WindowsOcr(p) : null;
    }

    public OcrLine[] Read(RgbImage img)
    {
        int max = (int)Windows.Media.Ocr.OcrEngine.MaxImageDimension;
        if (img.Width > max || img.Height > max)
        {
            double s = (double)max / Math.Max(img.Width, img.Height);
            var small = ImagePipeline.Resize(img, Math.Max(1, (int)(img.Width * s)), Math.Max(1, (int)(img.Height * s)), "bilinear");
            return Scale(Read(small), 1 / s);
        }
        var bgra = new byte[img.Width * img.Height * 4];
        for (int i = 0, n = img.Width * img.Height; i < n; i++)
        {
            bgra[i * 4] = img.Rgb[i * 3 + 2];
            bgra[i * 4 + 1] = img.Rgb[i * 3 + 1];
            bgra[i * 4 + 2] = img.Rgb[i * 3];
            bgra[i * 4 + 3] = 255;
        }
        using var bmp = new SoftwareBitmap(BitmapPixelFormat.Bgra8, img.Width, img.Height, BitmapAlphaMode.Premultiplied);
        bmp.CopyFromBuffer(bgra.AsBuffer());
        Array.Clear(bgra);
        var result = _engine.RecognizeAsync(bmp).AsTask().GetAwaiter().GetResult();
        var lines = new List<OcrLine>();
        foreach (var line in result.Lines)
        {
            var words = line.Words.Select(w => (w.Text, X: (int)Math.Round(w.BoundingRect.X), Y: (int)Math.Round(w.BoundingRect.Y),
                W: (int)Math.Round(w.BoundingRect.Width), H: (int)Math.Round(w.BoundingRect.Height))).ToList();
            if (words.Count == 0) continue;
            if (FromWords(line.Text, words) is { } l) lines.Add(l);
        }
        return lines.ToArray();
    }

    /// <summary>Line from Windows words: the line text, each word's characters spread evenly over its box, characters
    /// between words (spaces) over the gap. Words not found in the text in order give whole-line cuts.</summary>
    internal static OcrLine? FromWords(string text, IReadOnlyList<(string Text, int X, int Y, int W, int H)> words)
    {
        if (string.IsNullOrEmpty(text) || words.Count == 0) return null;
        int x0 = words.Min(w => w.X), y0 = words.Min(w => w.Y);
        int x1 = words.Max(w => w.X + w.W), y1 = words.Max(w => w.Y + w.H);
        var cuts = new int[text.Length + 1];
        int pos = 0, prevEnd = x0;
        bool ok = true;
        foreach (var w in words)
        {
            int at = text.IndexOf(w.Text, pos, StringComparison.Ordinal);
            if (at < 0 || w.Text.Length == 0) { ok = false; break; }
            for (int i = pos; i < at; i++) cuts[i] = prevEnd + (w.X - prevEnd) * (i - pos) / Math.Max(1, at - pos);
            for (int i = 0; i < w.Text.Length; i++) cuts[at + i] = w.X + w.W * i / w.Text.Length;
            pos = at + w.Text.Length;
            prevEnd = w.X + w.W;
        }
        if (!ok)
        {
            for (int i = 0; i <= text.Length; i++) cuts[i] = x0 + (x1 - x0) * i / text.Length;
            return new OcrLine(text, x0, y0, x1 - x0, y1 - y0, 1f, cuts);
        }
        for (int i = pos; i <= text.Length; i++) cuts[i] = prevEnd;
        return new OcrLine(text, x0, y0, x1 - x0, y1 - y0, 1f, cuts);
    }

    private static OcrLine[] Scale(OcrLine[] lines, double f)
        => lines.Select(l => l with
        {
            X = (int)(l.X * f), Y = (int)(l.Y * f), W = (int)Math.Ceiling(l.W * f), H = (int)Math.Ceiling(l.H * f),
            Cuts = l.Cuts.Select(c => (int)(c * f)).ToArray(),
        }).ToArray();
}
