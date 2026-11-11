// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Worker.Models.Ocr;

/// <summary>Axis-aligned text box in image pixels with the detector's score.</summary>
public readonly record struct TextBox(int X, int Y, int W, int H, float Score);

/// <summary>One CTC decode: text, mean probability of its characters and, per character, the first and last time
/// step it covered.</summary>
public sealed record CtcResult(string Text, float Confidence, int[] StepStart, int[] StepEnd);

/// <summary>Pre- and post-processing of the PaddleOCR detector and recognisers. Pure: no model, testable with synthetic
/// tensors.</summary>
public static class OcrMath
{
    /// <summary>Detector input size: the image scaled down so its longer side is at most maxSide, each side rounded to
    /// a multiple of 32 (at least 32).</summary>
    public static (int W, int H) DetSize(int w, int h, int maxSide)
    {
        double s = Math.Min(1.0, (double)maxSide / Math.Max(w, h));
        int dw = Math.Max(32, (int)Math.Round(w * s / 32, MidpointRounding.AwayFromZero) * 32);
        int dh = Math.Max(32, (int)Math.Round(h * s / 32, MidpointRounding.AwayFromZero) * 32);
        return (dw, dh);
    }

    /// <summary>[1,3,H,W] BGR tensor, (x/255 - mean[c]) / std[c], mean and std in BGR order (as the upstream yml lists
    /// them for its BGR input).</summary>
    public static float[] BgrTensor(RgbImage img, float[] mean, float[] std)
    {
        int plane = img.Width * img.Height;
        var t = new float[3 * plane];
        for (int c = 0; c < 3; c++)
        {
            int src = 2 - c;   // channel 0 = B
            float scale = 1f / 255f / std[c], bias = -mean[c] / std[c];
            for (int i = 0; i < plane; i++) t[c * plane + i] = img.Rgb[i * 3 + src] * scale + bias;
        }
        return t;
    }

    /// <summary>DB post-processing with axis-aligned boxes: probability above thresh, 4-connected components, box score
    /// = mean probability inside the component's bounding box (PaddleOCR "fast" score), kept at boxThresh or more,
    /// expanded on every side by area * unclip / perimeter, scaled from the map (pw x ph) to the image (imgW x imgH).
    /// Boxes thinner than 3 map pixels are dropped. Sorted top to bottom, then left to right.</summary>
    public static List<TextBox> Boxes(ReadOnlySpan<float> prob, int pw, int ph, int imgW, int imgH, float thresh, float boxThresh, float unclip, int maxBoxes = 3000)
    {
        if (prob.Length < pw * ph) throw new ArgumentException("probability map too short", nameof(prob));
        var label = new int[pw * ph];
        var stack = new Stack<int>();
        var boxes = new List<TextBox>();
        double sx = (double)imgW / pw, sy = (double)imgH / ph;
        int next = 0;
        for (int start = 0; start < pw * ph; start++)
        {
            if (label[start] != 0 || !(prob[start] > thresh)) continue;
            next++;
            int x0 = int.MaxValue, y0 = int.MaxValue, x1 = -1, y1 = -1;
            label[start] = next;
            stack.Push(start);
            while (stack.Count > 0)
            {
                int p = stack.Pop();
                int x = p % pw, y = p / pw;
                if (x < x0) x0 = x; if (x > x1) x1 = x;
                if (y < y0) y0 = y; if (y > y1) y1 = y;
                if (x > 0) Visit(p - 1, prob, label, next, thresh, stack);
                if (x < pw - 1) Visit(p + 1, prob, label, next, thresh, stack);
                if (y > 0) Visit(p - pw, prob, label, next, thresh, stack);
                if (y < ph - 1) Visit(p + pw, prob, label, next, thresh, stack);
            }
            int bw = x1 - x0 + 1, bh = y1 - y0 + 1;
            if (Math.Min(bw, bh) < 3) continue;
            double sum = 0;
            for (int y = y0; y <= y1; y++)
                for (int x = x0; x <= x1; x++) sum += prob[y * pw + x];
            float score = (float)(sum / (bw * bh));
            if (score < boxThresh) continue;
            double d = (double)bw * bh * unclip / (2.0 * (bw + bh));
            double l = Math.Max(0, (x0 - d) * sx), t = Math.Max(0, (y0 - d) * sy);
            double r = Math.Min(imgW, (x1 + 1 + d) * sx), b = Math.Min(imgH, (y1 + 1 + d) * sy);
            int il = (int)Math.Floor(l), it = (int)Math.Floor(t), ir = (int)Math.Ceiling(r), ib = (int)Math.Ceiling(b);
            if (ir - il < 3 || ib - it < 3) continue;
            boxes.Add(new TextBox(il, it, ir - il, ib - it, score));
            if (boxes.Count >= maxBoxes) break;
        }
        boxes.Sort((a, b) => a.Y != b.Y ? a.Y.CompareTo(b.Y) : a.X.CompareTo(b.X));
        return boxes;
    }

    private static void Visit(int q, ReadOnlySpan<float> prob, int[] label, int id, float thresh, Stack<int> stack)
    {
        if (label[q] != 0 || !(prob[q] > thresh)) return;
        label[q] = id;
        stack.Push(q);
    }

    /// <summary>Recogniser input width for a crop: height fixed, width keeping the aspect ratio, at least 8 and at most
    /// maxWidth.</summary>
    public static int RecWidth(int w, int h, int height, int maxWidth = 3200)
        => Math.Clamp((int)Math.Ceiling((double)height * w / Math.Max(1, h)), 8, maxWidth);

    /// <summary>Recogniser widths on accelerators: a crop's width is rounded up to the first bucket, so each recogniser
    /// sees at most this many shapes (times one batch size).</summary>
    public static readonly int[] RecBuckets = [256, 512, 768, 1024, 1536, 2048, 3200];

    public static int RecBucket(int width)
    {
        foreach (var b in RecBuckets) if (width <= b) return b;
        return RecBuckets[^1];
    }

    /// <summary>Batch tensor [batch,3,height,width] (batch defaults to the crop count; extra rows stay zero, mid grey):
    /// each crop resized to (widths[i], height), BGR, (x/255 - 0.5) / 0.5, zero to the right of it.</summary>
    public static float[] RecTensor(IReadOnlyList<RgbImage> resized, int height, int width, int batch = 0)
    {
        int plane = height * width;
        if (batch < resized.Count) batch = resized.Count;
        var t = new float[batch * 3 * plane];
        for (int n = 0; n < resized.Count; n++)
        {
            var img = resized[n];
            if (img.Height != height || img.Width > width) throw new ArgumentException("crop not resized to the batch shape");
            int baseN = n * 3 * plane;
            for (int c = 0; c < 3; c++)
            {
                int src = 2 - c;
                for (int y = 0; y < height; y++)
                    for (int x = 0; x < img.Width; x++)
                        t[baseN + c * plane + y * width + x] = img.Rgb[(y * img.Width + x) * 3 + src] / 127.5f - 1f;
            }
        }
        return t;
    }

    /// <summary>Greedy CTC over [steps, classes] probabilities: argmax per step, repeats collapsed, blank (class 0)
    /// dropped. table[k] is the text of class k (table[0] unused). Confidence: mean of the chosen probabilities of
    /// the kept characters (0 when none).</summary>
    public static CtcResult Ctc(ReadOnlySpan<float> probs, int steps, int classes, string[] table)
    {
        var sb = new System.Text.StringBuilder();
        var starts = new List<int>();
        var ends = new List<int>();
        double sum = 0;
        int kept = 0, prev = 0, lastUnits = 0;
        for (int t = 0; t < steps; t++)
        {
            var row = probs.Slice(t * classes, classes);
            int best = 0;
            float bp = row[0];
            for (int k = 1; k < classes; k++) if (row[k] > bp) { bp = row[k]; best = k; }
            if (best != 0 && best == prev && lastUnits > 0)
            {
                for (int u = 1; u <= lastUnits; u++) ends[^u] = t;
            }
            else if (best != 0 && best < table.Length)
            {
                string s = table[best];
                // A class can map to several UTF-16 units (surrogate pairs, combined glyphs): each unit gets the step.
                foreach (char _ in s) { starts.Add(t); ends.Add(t); }
                lastUnits = s.Length;
                sb.Append(s);
                sum += bp;
                kept++;
            }
            prev = best;
        }
        return new CtcResult(sb.ToString(), kept == 0 ? 0f : (float)(sum / kept), starts.ToArray(), ends.ToArray());
    }

    /// <summary>Character boundaries in image pixels (Text.Length + 1 values) from CTC steps: step t of the padded
    /// input covers [t, t+1) * inputWidth / steps; contentWidth of those pixels is the crop, scaled back to cropW and
    /// moved by cropX. Inner boundaries sit between one character's last step and the next one's first.</summary>
    public static int[] Cuts(CtcResult r, int steps, int inputWidth, int contentWidth, int cropX, int cropW)
    {
        int n = r.Text.Length;
        var cuts = new int[n + 1];
        if (n == 0) { cuts[0] = cropX; return cuts; }
        double step = (double)inputWidth / steps;
        double toCrop = (double)cropW / Math.Max(1, contentWidth);
        int Px(double inputX) => cropX + (int)Math.Round(Math.Clamp(inputX, 0, contentWidth) * toCrop);
        cuts[0] = Px(r.StepStart[0] * step);
        for (int i = 1; i < n; i++)
            cuts[i] = Px((r.StepEnd[i - 1] + 1 + r.StepStart[i]) * step / 2);
        cuts[n] = Px((r.StepEnd[n - 1] + 1) * step);
        for (int i = 1; i <= n; i++) if (cuts[i] < cuts[i - 1]) cuts[i] = cuts[i - 1];
        return cuts;
    }

    /// <summary>Class table: "" (blank), the dictionary entries, then a space.</summary>
    public static string[] Table(IReadOnlyList<string> dict)
    {
        var t = new string[dict.Count + 2];
        t[0] = "";
        for (int i = 0; i < dict.Count; i++) t[i + 1] = dict[i];
        t[^1] = " ";
        return t;
    }

    /// <summary>Recognisers to run for ocr.languages (codes like "en", "ko", "zh-Hant"): the first ("main") always,
    /// plus each other recogniser that lists one of the codes. Region suffixes are ignored ("ko-KR" = "ko").</summary>
    public static List<OcrRecSpec> PickRecognisers(IReadOnlyList<OcrRecSpec> all, IEnumerable<string> languages)
    {
        var picked = new List<OcrRecSpec>();
        if (all.Count == 0) return picked;
        picked.Add(all[0]);
        var codes = languages.Select(l => l.Trim().ToLowerInvariant().Split('-')[0]).Where(l => l.Length > 0).ToHashSet();
        foreach (var r in all.Skip(1))
            if (r.Languages.Any(codes.Contains)) picked.Add(r);
        return picked;
    }
}
