// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Text;
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Inference.Onnx;
using BetterRecall.Worker.Inference.OpenVino;
using BetterRecall.Worker.Models;
using BetterRecall.Worker.Models.Ner;
using BetterRecall.Worker.Models.Ocr;
using SkiaSharp;
using Xunit.Abstractions;

namespace BetterRecall.Worker.Tests;

/// <summary>Synthetic screens: dark text rendered on white with SkiaSharp (no real screenshots).</summary>
internal static class TextImages
{
    public static RgbImage Render(int width, int height, params (string Text, int X, int Y, float Size)[] items)
    {
        using var bmp = new SKBitmap(new SKImageInfo(width, height, SKColorType.Rgba8888, SKAlphaType.Premul));
        using (var canvas = new SKCanvas(bmp))
        {
            canvas.Clear(SKColors.White);
            using var paint = new SKPaint { Color = new SKColor(20, 20, 20), IsAntialias = true };
            foreach (var (text, x, y, size) in items)
            {
                using var font = new SKFont(SKFontManager.Default.MatchCharacter(text.FirstOrDefault(char.IsLetter)) ?? SKTypeface.Default, size);
                canvas.DrawText(text, x, y + size, font, paint);
            }
        }
        var rgb = new byte[width * height * 3];
        var px = bmp.Bytes;
        for (int i = 0; i < width * height; i++) { rgb[i * 3] = px[i * 4]; rgb[i * 3 + 1] = px[i * 4 + 1]; rgb[i * 3 + 2] = px[i * 4 + 2]; }
        return new RgbImage(width, height, rgb);
    }

    public static ImageInput AsInput(RgbImage img)
    {
        var bgra = new byte[img.Width * img.Height * 4];
        for (int i = 0; i < img.Width * img.Height; i++)
        {
            bgra[i * 4] = img.Rgb[i * 3 + 2]; bgra[i * 4 + 1] = img.Rgb[i * 3 + 1]; bgra[i * 4 + 2] = img.Rgb[i * 3]; bgra[i * 4 + 3] = 255;
        }
        return new ImageInput("bgra8", img.Width, img.Height, img.Width * 4, bgra);
    }

    public static string Squash(string s) => new(s.Where(c => !char.IsWhiteSpace(c)).ToArray());
}

public class OcrMathTests
{
    [Theory]
    [InlineData(8, 256)]
    [InlineData(256, 256)]
    [InlineData(257, 512)]
    [InlineData(1100, 1536)]
    [InlineData(3200, 3200)]
    [InlineData(5000, 3200)]
    public void Recogniser_widths_round_up_to_few_buckets(int width, int bucket) => Assert.Equal(bucket, OcrMath.RecBucket(width));

    [Fact]
    public void Recogniser_batch_is_padded_with_mid_grey_rows()
    {
        var crop = new RgbImage(4, 2, Enumerable.Repeat((byte)255, 4 * 2 * 3).ToArray());
        var t = OcrMath.RecTensor([crop], 2, 8, batch: 3);
        Assert.Equal(3 * 3 * 2 * 8, t.Length);
        Assert.Equal(1f, t[0]);                                  // white pixel of the real crop
        Assert.Equal(0f, t[5]);                                  // right of the crop: padding
        Assert.All(t.AsSpan(3 * 2 * 8).ToArray(), v => Assert.Equal(0f, v));   // rows 2 and 3: padding only
    }

    [Theory]
    [InlineData(1280, 720, 1920, 1280, 736)]
    [InlineData(3840, 2160, 1920, 1920, 1088)]
    [InlineData(10, 10, 1920, 32, 32)]
    public void Detector_size_is_scaled_and_rounded_to_32(int w, int h, int max, int ew, int eh)
        => Assert.Equal((ew, eh), OcrMath.DetSize(w, h, max));

    [Fact]
    public void Boxes_find_components_score_unclip_and_scale()
    {
        int pw = 64, ph = 32;
        var prob = new float[pw * ph];
        void Fill(int x0, int y0, int x1, int y1, float v) { for (int y = y0; y < y1; y++) for (int x = x0; x < x1; x++) prob[y * pw + x] = v; }
        Fill(4, 4, 30, 10, 0.9f);     // line 1
        Fill(40, 20, 60, 26, 0.9f);   // line 2 (lower, right)
        Fill(10, 20, 12, 30, 0.9f);   // 2 px wide: dropped
        Fill(50, 2, 60, 8, 0.25f);    // above thresh but box score below boxThresh
        var boxes = OcrMath.Boxes(prob, pw, ph, 128, 64, 0.2f, 0.45f, 1.4f);
        Assert.Equal(2, boxes.Count);
        var a = boxes[0];
        // 26 x 6 component: d = 156 * 1.4 / 64 = 3.4125 map px, scale 2.
        Assert.Equal(1, a.X);
        Assert.Equal(1, a.Y);
        Assert.Equal(67, a.X + a.W);
        Assert.Equal(27, a.Y + a.H);
        Assert.True(a.Score > 0.89f);
        Assert.True(boxes[1].Y > a.Y);
    }

    private static float[] Steps(int classes, params int[] argmax)
    {
        var p = new float[argmax.Length * classes];
        for (int t = 0; t < argmax.Length; t++) { for (int k = 0; k < classes; k++) p[t * classes + k] = 0.01f; p[t * classes + argmax[t]] = 0.9f; }
        return p;
    }

    [Fact]
    public void Ctc_collapses_repeats_drops_blanks_and_tracks_steps()
    {
        var table = OcrMath.Table(["a", "b", "😀"]);   // 0 blank, 1 a, 2 b, 3 emoji, 4 space
        Assert.Equal(5, table.Length);
        var probs = Steps(5, 1, 1, 0, 1, 4, 2, 2, 0, 3, 0);
        var r = OcrMath.Ctc(probs, 10, 5, table);
        Assert.Equal("aa b😀", r.Text);
        Assert.Equal(r.Text.Length, r.StepStart.Length);
        Assert.Equal(new[] { 0, 3, 4, 5, 8, 8 }, r.StepStart);
        Assert.Equal(new[] { 1, 3, 4, 6, 8, 8 }, r.StepEnd);
        Assert.InRange(r.Confidence, 0.89f, 0.91f);

        var cuts = OcrMath.Cuts(r, 10, 100, 80, 200, 40);
        Assert.Equal(r.Text.Length + 1, cuts.Length);
        for (int i = 1; i < cuts.Length; i++) Assert.True(cuts[i] >= cuts[i - 1]);
        Assert.Equal(200, cuts[0]);
        Assert.InRange(cuts[^1], 200, 240);
    }

    [Fact]
    public void Rec_tensor_is_bgr_normalised_and_padded_with_zero()
    {
        var img = new RgbImage(2, 1, [255, 0, 0, 0, 0, 255]);   // red, blue
        var t = OcrMath.RecTensor([img], 1, 3);
        // channel 0 = B: red px -> -1, blue px -> 1, pad 0
        Assert.Equal(new float[] { -1, 1, 0, -1, -1, 0, 1, -1, 0 }, t);
    }

    [Fact]
    public void Recognisers_follow_languages()
    {
        var all = new List<OcrRecSpec>
        {
            new() { Name = "main", Languages = ["zh", "en", "ja", "latin"] },
            new() { Name = "korean", Languages = ["ko"] },
            new() { Name = "cyrillic", Languages = ["ru", "uk"] },
        };
        Assert.Equal(["main"], OcrMath.PickRecognisers(all, ["vi-VN", "en-US"]).Select(r => r.Name));
        Assert.Equal(["main", "korean", "cyrillic"], OcrMath.PickRecognisers(all, ["ko-KR", "uk"]).Select(r => r.Name));
    }

    [Fact]
    public void Windows_words_become_character_cuts()
    {
        var l = WindowsOcr.FromWords("Hello big world", [("Hello", 10, 5, 50, 12), ("big", 70, 5, 30, 12), ("world", 110, 6, 50, 12)])!;
        Assert.Equal(16, l.Cuts.Length);
        Assert.Equal(10, l.Cuts[0]);
        Assert.Equal(70, l.Cuts[6]);    // "b"
        Assert.Equal(110, l.Cuts[10]);  // "w"
        Assert.Equal(160, l.Cuts[^1]);
        Assert.Equal((10, 5, 150, 13), (l.X, l.Y, l.W, l.H));
    }
}

public class BpeTokenizerTests
{
    private static BpeTokenizer Tiny()
    {
        // vocab: specials, byte tokens for 'é' (C3 A9), single chars, merges ▁+a -> ▁a, ▁a+b -> ▁ab, \n added.
        var json = """
        {
          "added_tokens": [ {"id":0,"content":"<pad>","special":true}, {"id":1,"content":"<eos>","special":true},
                            {"id":2,"content":"<bos>","special":true}, {"id":3,"content":"<unk>","special":true},
                            {"id":20,"content":"\n","special":false} ],
          "model": { "type": "BPE", "byte_fallback": true,
            "vocab": { "<pad>":0, "<eos>":1, "<bos>":2, "<unk>":3, "<0xC3>":4, "<0xA9>":5, "▁":6, "a":7, "b":8, "c":9, "▁a":10, "▁ab":11, "bc":12 },
            "merges": [ ["▁","a"], ["▁a","b"], ["b","c"] ] }
        }
        """;
        var path = Path.Combine(TestEnv.NewDir("bpe"), "tokenizer.json");
        File.WriteAllText(path, json, new UTF8Encoding(false));
        return BpeTokenizer.Load(path);
    }

    [Fact]
    public void Merges_by_rank_with_virtual_word_marker_and_byte_fallback()
    {
        var t = Tiny();
        Assert.Equal((2, 1), (t.BosId, t.EosId));
        // "abc": ▁ a b c -> ▁a b c -> ▁ab c (rank 1 beats b+c rank 2)
        Assert.Equal(new[] { 11, 9 }, t.Encode("abc").Select(x => x.Id));
        // "a bc": words "a" and " bc"; space becomes ▁
        Assert.Equal(new[] { 10, 6, 12 }, t.Encode("a bc").Select(x => x.Id));
        // é is not in the vocabulary: UTF-8 bytes
        Assert.Equal(new[] { 6, 4, 5 }, t.Encode("é").Select(x => x.Id));
        // added token splits; the next piece gets its own marker
        var nl = t.Encode("a\nb");
        Assert.Equal(new[] { 10, 20, 6, 8 }, nl.Select(x => x.Id));
        Assert.Equal((1, 2), (nl[1].Start, nl[1].End));
        Assert.Equal((0, 1), (nl[0].Start, nl[0].End));
    }
}

public class PiiNerLogicTests
{
    private static readonly NerSpec Spec = new()
    {
        Labels = ["O", "B-GIVEN_NAME", "I-GIVEN_NAME", "B-SURNAME", "I-SURNAME", "B-CITY", "I-CITY", "B-EMAIL", "I-EMAIL"],
        Entities = new() { ["GIVEN_NAME"] = "person", ["SURNAME"] = "person", ["CITY"] = "address" },
    };

    [Fact]
    public void Pick_is_recall_first_and_ignores_unmapped_labels()
    {
        var map = PiiNer.Tags(Spec);
        Assert.Null(map[0]);
        Assert.Null(map[7]);
        // O most likely (0.55) but B-CITY at 0.35 reaches the floor.
        var l = new float[9];
        l[0] = MathF.Log(0.55f); l[5] = MathF.Log(0.35f); l[7] = MathF.Log(0.10f - 1e-6f);
        for (int k = 0; k < 9; k++) if (l[k] == 0) l[k] = -30;
        Assert.Equal(5, PiiNer.Pick(l, map).Tag);
        // An unmapped label (EMAIL) alone never counts.
        var e = Enumerable.Repeat(-30f, 9).ToArray(); e[7] = 0;
        Assert.Equal(-1, PiiNer.Pick(e, map).Tag);
    }

    [Fact]
    public void Spans_join_inside_and_split_on_new_begin_or_label()
    {
        var map = PiiNer.Tags(Spec);
        // "Anna Schmidt Berlin": tokens ▁Anna, ▁Schm, idt, ▁Berlin
        var toks = new List<BpeToken> { new(0, 0, 4), new(0, 4, 9), new(0, 9, 12), new(0, 12, 19) };
        var tags = new (int, float)[] { (1, 0.9f), (3, 0.8f), (4, 0.8f), (5, 0.7f) };
        var spans = PiiNer.Spans(toks, 0, tags, map);
        Assert.Equal(3, spans.Count);
        Assert.Equal((0, 4, "person"), (spans[0].Item1, spans[0].Item2, spans[0].Item3));
        Assert.Equal((4, 12, "person"), (spans[1].Item1, spans[1].Item2, spans[1].Item3));
        Assert.Equal((12, 19, "address"), (spans[2].Item1, spans[2].Item2, spans[2].Item3));
    }
}

/// <summary>Real models from BR_OCR_MODELS (a folder holding ocr\ and ner\ as Scripts\models\export_ocr.py and
/// export_ner.py write them); skipped when it is not set.</summary>
public class RealOcrTests(ITestOutputHelper output)
{
    private static string? Models()
    {
        var m = Environment.GetEnvironmentVariable("BR_OCR_MODELS");
        return string.IsNullOrWhiteSpace(m) || !Directory.Exists(Path.Combine(m, ModelCatalog.Ocr)) ? null : m;
    }

    private static RealEngine Engine(string models, string engine = "onnx", string languages = "en,ko,ru", string device = "cpu")
    {
        var home = TestEnv.NewDir("ocr");
        var s = TestEnv.Settings(home, s =>
        {
            s.Inference.ModelDir = models;
            s.Ocr.Engine = engine; s.Ocr.Languages = languages; s.Ocr.Device = device;
        });
        return new RealEngine(s, home);
    }

    [Fact]
    public void PaddleOcr_reads_rendered_lines_in_several_scripts()
    {
        if (Models() is not { } models) { output.WriteLine("skipped: BR_OCR_MODELS not set"); return; }
        using var e = Engine(models);
        var img = TextImages.Render(1280, 400,
            ("Invoice 4471 is due on 2026-11-30", 40, 30, 22),
            ("Password: hunter2", 40, 90, 22),
            ("비밀번호를 입력하세요", 40, 150, 22),
            ("Пароль и дата рождения", 40, 210, 22),
            ("パスワードを入力", 640, 30, 22));
        var sw = Stopwatch.StartNew();
        var r = e.Ocr(TextImages.AsInput(img), entities: false);
        output.WriteLine($"{r.Engine} {sw.ElapsedMilliseconds} ms");
        foreach (var l in r.Lines) output.WriteLine($"{l.X},{l.Y} {l.W}x{l.H} {l.Confidence:F2} '{l.Text}'");
        string all = TextImages.Squash(string.Join(" ", r.Lines.Select(l => l.Text)));
        Assert.Contains("Invoice4471isdueon2026-11-30", all);
        Assert.Contains("Password:hunter2", all);
        Assert.Contains("비밀번호를입력하세요", all);
        Assert.Contains("Парольидатарождения", all);
        // Kana: the long-vowel mark and 力/カ are easy to confuse at this size; the rest must be right.
        Assert.Contains("パスワ", all);
        Assert.Contains("ドを入", all);
        // Character cuts locate "hunter2" inside its line box.
        var pw = r.Lines.First(l => l.Text.Contains("hunter2"));
        Assert.Equal("Password: hunter2", pw.Text);
        int at = pw.Text.IndexOf("hunter2", StringComparison.Ordinal);
        Assert.Equal(pw.Text.Length + 1, pw.Cuts.Length);
        Assert.InRange(pw.Cuts[at], pw.X + pw.W / 3, pw.X + pw.W);
        Assert.InRange(pw.Cuts[at + 7], pw.Cuts[at] + 40, pw.X + pw.W + 2);
    }

    [Fact]
    public void Ner_finds_names_and_addresses()
    {
        if (Models() is not { } models || !Directory.Exists(Path.Combine(models, ModelCatalog.Ner))) { output.WriteLine("skipped: BR_OCR_MODELS not set"); return; }
        using var e = Engine(models);
        var img = TextImages.Render(1280, 200,
            ("Please send the contract to Anna Schmidt, Hauptstrasse 12, 10115 Berlin.", 30, 30, 22),
            ("Ship to John Carter, 742 Evergreen Terrace, Springfield, IL 62704.", 30, 100, 22));
        var r = e.Ocr(TextImages.AsInput(img), entities: true);
        foreach (var l in r.Lines)
            output.WriteLine($"'{l.Text}' -> {string.Join(", ", (l.Entities ?? []).Select(x => $"{l.Text.Substring(x.Start, x.Length)}:{x.Label}:{x.Score:F2}"))}");
        var ents = r.Lines.SelectMany(l => (l.Entities ?? []).Select(x => (Text: l.Text.Substring(x.Start, x.Length).Trim(), x.Label))).ToList();
        Assert.Contains(ents, x => x.Label == "person" && x.Text.Contains("Anna"));
        Assert.Contains(ents, x => x.Label == "person" && x.Text.Contains("Carter"));
        Assert.Contains(ents, x => x.Label == "address" && (x.Text.Contains("Hauptstrasse") || x.Text.Contains("Berlin")));
        Assert.Contains(ents, x => x.Label == "address" && x.Text.Contains("Evergreen"));
    }

    [Fact]
    public void Missing_ner_model_still_returns_the_ocr_lines()
    {
        // Pattern redaction (cards, IBANs) needs the lines even when name/address detection cannot run.
        if (Models() is not { } models) { output.WriteLine("skipped: BR_OCR_MODELS not set"); return; }
        var onlyOcr = TestEnv.NewDir("ocr-no-ner");
        var src = Path.Combine(models, ModelCatalog.Ocr);
        var dst = Directory.CreateDirectory(Path.Combine(onlyOcr, ModelCatalog.Ocr)).FullName;
        foreach (var f in Directory.GetFiles(src)) File.Copy(f, Path.Combine(dst, Path.GetFileName(f)));
        using var e = Engine(onlyOcr);
        var img = TextImages.Render(900, 120, ("Card 4111 1111 1111 1111 for Anna Schmidt", 30, 40, 22));
        var r = e.Ocr(TextImages.AsInput(img), entities: true);
        Assert.True(r.EntitiesMissing);
        Assert.Contains("4111", string.Join(" ", r.Lines.Select(l => l.Text)));
        Assert.All(r.Lines, l => Assert.Null(l.Entities));
    }

    [Fact]
    public void Tokenizer_matches_the_python_tokenizers_library()
    {
        if (Models() is not { } models || !Directory.Exists(Path.Combine(models, ModelCatalog.Ner))) { output.WriteLine("skipped: BR_OCR_MODELS not set"); return; }
        var tok = BpeTokenizer.Load(Path.Combine(models, ModelCatalog.Ner, "tokenizer.json"));
        // Reference ids from tokenizers 0.22 (Python), without bos/eos.
        Assert.Equal(new[] { 124960, 531, 188306, 73044, 80663, 1355, 187889, 187868, 187904, 187928, 129430, 901, 11365, 15972, 12575, 187885, 108, 24274, 577, 3318, 187868, 457, 376, 370, 347 },
            tok.Encode("Liên hệ Nguyễn Văn An, 25 Lý Thường Kiệt.\nShip to John 🙂").Select(t => t.Id));
    }
}

/// <summary>ocr.engine = windows on a rendered English line. Skipped when Windows has no OCR language installed.</summary>
public class WindowsOcrTests(ITestOutputHelper output)
{
    [Fact]
    public void Windows_ocr_reads_a_rendered_line()
    {
        var w = WindowsOcr.TryCreate(["en-US"]);
        if (w is null) { output.WriteLine("skipped: no Windows OCR language"); return; }
        var img = TextImages.Render(900, 120, ("Meeting notes for Thursday", 30, 40, 28));
        var lines = w.Read(img);
        output.WriteLine($"{w.Language}: {string.Join(" | ", lines.Select(l => l.Text))}");
        Assert.Contains("Meetingnotes", TextImages.Squash(string.Join(" ", lines.Select(l => l.Text))));
        Assert.All(lines, l => Assert.Equal(l.Text.Length + 1, l.Cuts.Length));
    }
}

/// <summary>PaddleOCR on every device this PC offers (DirectML per DXGI adapter, OpenVINO GPUs, CPU): load time,
/// latency and agreement with the CPU text. Manual: needs BR_OCR_MODELS and BR_OCR_DEVICES=1.</summary>
public class OcrDeviceTests(ITestOutputHelper output)
{
    [Fact]
    public void PaddleOcr_on_each_device_matches_the_cpu()
    {
        var models = Environment.GetEnvironmentVariable("BR_OCR_MODELS");
        if (string.IsNullOrWhiteSpace(models) || Environment.GetEnvironmentVariable("BR_OCR_DEVICES") != "1")
        { output.WriteLine("skipped: set BR_OCR_MODELS and BR_OCR_DEVICES=1"); return; }
        var m = ModelCatalog.Load(models, ModelCatalog.Ocr)!;
        var recs = OcrMath.PickRecognisers(m.Ocr!.Rec, ["en", "ko", "ru"]);
        var items = new List<(string, int, int, float)>();
        string[] sample = ["Invoice 4471 is due on 2026-11-30", "Password: hunter2", "Quarterly planning review on Friday",
            "비밀번호를 입력하세요", "Пароль и дата рождения", "Total 1,240.00 EUR incl. VAT", "Meeting notes from the design team"];
        for (int i = 0; i < 21; i++) items.Add((sample[i % sample.Length], 30 + i % 3 * 420, 20 + i / 3 * 95, 18 + i % 4 * 2));
        var img = TextImages.Render(1280, 720, [.. items]);

        var cpuOrt = new OnnxBackend(cpuOnly: true);
        var devices = new List<(IInferenceBackend, DeviceInfo, bool?)>();
        foreach (var a in DxgiAdapters.Enumerate().Where(a => !a.IsSoftware && !a.IsBasicRender))
            devices.Add((cpuOrt, GpuSelection.Dml(a), null));
        if (OpenVinoBackend.TryCreate() is { } ov)
            foreach (var d in ov.Devices.Where(d => d.Kind is DeviceKind.Gpu or DeviceKind.Npu)) devices.Add((ov, d, null));
        var cpuDev = cpuOrt.Devices.First(d => d.Kind == DeviceKind.Cpu);
        devices.Add((cpuOrt, cpuDev, null));
        devices.Add((cpuOrt, cpuDev, true));   // CPU with the accelerators' fixed shapes: isolates the padding effect
        // BR_OCR_ONLY: comma list of device ids to run (e.g. "CPU,NPU,GPU.0"); the CPU always runs as the reference.
        var only = (Environment.GetEnvironmentVariable("BR_OCR_ONLY") ?? "").Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
        if (only.Length > 0) devices = devices.Where(x => x.Item2.Kind == DeviceKind.Cpu || only.Contains(x.Item2.Id)).ToList();
        devices = [.. devices.OrderBy(x => x.Item2.Kind == DeviceKind.Cpu && x.Item3 is null ? 0 : 1)];

        // Varied screens: new line widths and counts, as real keyframes bring. Same set for every device.
        var rnd = new Random(7);
        var variedImages = new List<RgbImage>();
        for (int v = 0; v < 12; v++)
        {
            // One line per row, no overlaps: lengths, sizes and positions vary.
            var vi = new List<(string, int, int, float)>();
            int y = 10;
            while (true)
            {
                int size = rnd.Next(12, 28);
                if (y + size + 8 > 710) break;
                var t0 = sample[rnd.Next(sample.Length)];
                vi.Add((t0[..rnd.Next(Math.Min(6, t0.Length), t0.Length + 1)], rnd.Next(10, 600), y, size));
                y += size + rnd.Next(14, 40);
            }
            variedImages.Add(TextImages.Render(1280, 720, [.. vi]));
        }

        List<string>? reference = null;
        var report = new StringBuilder();
        foreach (var (backend, dev, fixedShapes) in devices)
        {
            var sw = Stopwatch.StartNew();
            var cache = TestEnv.NewDir("ocrdev");
            try
            {
                long load, firstMs, reload;
                var texts = new List<string>();
                List<double> cold = [], warm = [];
                double cpuCold, cpuWarm;
                int graphs;
                using (var ocr = new PaddleOcr(backend, dev, m, recs, cache, fixedShapes))
                {
                    ocr.WarmUp();
                    load = sw.ElapsedMilliseconds;
                    sw.Restart();
                    texts.Add(string.Join("\n", ocr.Read(img).Select(l => l.Text)));
                    firstMs = sw.ElapsedMilliseconds;
                    var cpu0 = Process.GetCurrentProcess().TotalProcessorTime;
                    foreach (var vimg in variedImages)
                    {
                        sw.Restart();
                        texts.Add(string.Join("\n", ocr.Read(vimg).Select(l => l.Text)));
                        cold.Add(sw.Elapsed.TotalMilliseconds);
                    }
                    cpuCold = (Process.GetCurrentProcess().TotalProcessorTime - cpu0).TotalMilliseconds / variedImages.Count;
                    // Second pass over the same screens: every shape is compiled now (steady state).
                    cpu0 = Process.GetCurrentProcess().TotalProcessorTime;
                    foreach (var vimg in variedImages) { sw.Restart(); ocr.Read(vimg); warm.Add(sw.Elapsed.TotalMilliseconds); }
                    cpuWarm = (Process.GetCurrentProcess().TotalProcessorTime - cpu0).TotalMilliseconds / variedImages.Count;
                    graphs = ocr.CompiledGraphs;
                }
                // A new instance on the same cache: what the next worker start pays.
                sw.Restart();
                using (var again = new PaddleOcr(backend, dev, m, recs, cache, fixedShapes)) { again.WarmUp(); again.Read(img); }
                reload = sw.ElapsedMilliseconds;

                reference ??= texts;
                int same = texts.Zip(reference).Count(p => p.First == p.Second);
                report.AppendLine($"{dev.Ep} {dev.Id}{(fixedShapes == true ? " fixed shapes" : "")} '{dev.Name}': load {load} ms, first read {firstMs} ms, graphs {graphs}, reload+read {reload} ms");
                report.AppendLine($"    varied cold: mean {cold.Average():F0} ms, max {cold.Max():F0} ms, cpu {cpuCold:F0} ms/read; " +
                    $"warm: mean {warm.Average():F0} ms, max {warm.Max():F0} ms, cpu {cpuWarm:F0} ms/read; screens same as CPU {same}/{texts.Count}");
                for (int i = 0; i < texts.Count; i++)
                    if (texts[i] != reference[i])
                    {
                        var r = reference[i].Split('\n');
                        foreach (var l in texts[i].Split('\n').Where(l => !r.Contains(l)).Take(3)) report.AppendLine($"    screen {i} differs: '{l}'");
                    }
            }
            catch (Exception e)
            {
                report.AppendLine($"{dev.Ep} {dev.Id} '{dev.Name}': FAILED after {sw.ElapsedMilliseconds} ms: {e.GetType().Name}: {e.Message}");
            }
            output.WriteLine(report.ToString());
            var file = Environment.GetEnvironmentVariable("BR_OCR_REPORT");
            if (!string.IsNullOrEmpty(file)) File.WriteAllText(file, report.ToString());
            try { Directory.Delete(cache, true); } catch (IOException) { } catch (UnauthorizedAccessException) { }
        }
        Assert.NotNull(reference);
    }
}
