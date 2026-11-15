// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Models;
using BetterRecall.Worker.Models.Ner;
using BetterRecall.Worker.Models.Ocr;
using BetterRecall.Worker.Protocol;
using Xunit.Abstractions;

namespace BetterRecall.Worker.Tests;

/// <summary>Bug sweep 2, batch F: OCR outer cuts (C1) and NER failures that must not drop the OCR lines (C9).</summary>
public class Sweep2FTests(ITestOutputHelper output)
{
    [Fact]
    public void Outer_cuts_are_the_box_edges_even_when_the_first_and_last_steps_sit_inside_the_glyphs()
    {
        // CTC fires "a" at step 3 and "b" at step 6 of 10: the first glyph's left part lies before step 3 (C1).
        var r = new CtcResult("ab", 0.9f, [3, 6], [3, 6]);
        var cuts = OcrMath.Cuts(r, 10, 100, 100, 200, 50);
        Assert.Equal(3, cuts.Length);
        Assert.Equal(200, cuts[0]);        // box left, not the step-3 position (215)
        Assert.Equal(250, cuts[2]);        // box right, not the end of step 6 (235)
        Assert.InRange(cuts[1], 220, 232); // between the two characters
    }

    [Fact]
    public void Outer_cuts_with_one_character_span_the_box()
    {
        var r = new CtcResult("x", 0.9f, [5], [5]);
        Assert.Equal(new[] { 40, 90 }, OcrMath.Cuts(r, 10, 100, 80, 40, 50));
    }

    private static OcrLine[] Lines(params string[] texts) =>
        texts.Select((t, i) => new OcrLine(t, 0, i * 20, 100, 18, 0.9f, Enumerable.Range(0, t.Length + 1).ToArray())).ToArray();

    [Fact]
    public void Ner_success_attaches_entities()
    {
        var gate = new NerGate(() => 0);
        var (lines, missing) = gate.Apply(Lines("Anna Schmidt", "hello"), () => l => [[new OcrEntity(0, 4, "person", 0.9f)], null]);
        Assert.False(missing);
        Assert.Single(lines[0].Entities!);
        Assert.Empty(lines[1].Entities!);
    }

    [Fact]
    public void Missing_ner_model_keeps_the_lines_and_is_checked_again_next_frame()
    {
        int loads = 0;
        var gate = new NerGate(() => 0);
        Func<Func<IReadOnlyList<string>, OcrEntity[]?[]>> ensure = () => { loads++; throw new ModelNotInstalledException(ModelCatalog.Ner); };
        var (lines, missing) = gate.Apply(Lines("Card 4111 1111 1111 1111"), ensure);
        Assert.True(missing);
        Assert.Equal("Card 4111 1111 1111 1111", Assert.Single(lines).Text);
        gate.Apply(Lines("x"), ensure);
        Assert.Equal(2, loads);
    }

    public static TheoryData<string> Failures() => new() { "json", "device", "keynotfound", "count" };

    [Theory]
    [MemberData(nameof(Failures))]
    public void Broken_ner_model_keeps_the_lines_and_backs_off(string kind)
    {
        // A damaged tokenizer.json, an ONNX load error, a label mismatch or a wrong result count must not fail the ocr op:
        // pattern redaction (cards, IBANs) still needs the lines (C9).
        long now = 1_000;
        int loads = 0;
        var gate = new NerGate(() => now);
        Func<Func<IReadOnlyList<string>, OcrEntity[]?[]>> ensure = () =>
        {
            loads++;
            return kind switch
            {
                "json" => throw new System.Text.Json.JsonException("bad tokenizer"),
                "device" => throw new DeviceFailedException("ner load failed"),
                "keynotfound" => throw new KeyNotFoundException(),
                _ => _ => [],   // wrong count
            };
        };
        var (lines, missing) = gate.Apply(Lines("IBAN DE89 3704 0044 0532 0130 00", "Anna"), ensure);
        Assert.True(missing);
        Assert.Equal(2, lines.Length);
        Assert.All(lines, l => Assert.Null(l.Entities));
        Assert.NotNull(gate.LastError);

        // Backed off: no reload on the next frames, still missing.
        now += 60_000;
        Assert.True(gate.Apply(Lines("x"), ensure).Missing);
        Assert.Equal(1, loads);
        // After the back-off it tries again.
        now += (long)NerGate.Backoff.TotalMilliseconds;
        gate.Apply(Lines("x"), ensure);
        Assert.Equal(2, loads);
    }

    [Fact]
    public void No_lines_skip_the_ner()
    {
        var gate = new NerGate(() => 0);
        var (lines, missing) = gate.Apply([], () => throw new InvalidOperationException("not called"));
        Assert.Empty(lines);
        Assert.False(missing);
    }

    private static (int L, int R) Ink(RgbImage img, int y0, int y1)
    {
        int l = int.MaxValue, r = -1;
        for (int y = Math.Max(0, y0); y < Math.Min(img.Height, y1); y++)
            for (int x = 0; x < img.Width; x++)
                if (img.Rgb[(y * img.Width + x) * 3] < 128) { l = Math.Min(l, x); r = Math.Max(r, x); }
        return (l, r);
    }

    [Theory]
    [InlineData("Anna Schmidt", 22)]
    [InlineData("John Carter", 16)]
    [InlineData("4111 1111 1111 1111", 14)]
    [InlineData("Wolfgang Müller", 30)]
    public void Redaction_box_of_a_line_starting_span_covers_the_first_glyph(string text, float size)
    {
        // C's probe: the ink of a rendered line must sit inside [cuts[0] - 2, cuts[n] + 2] (the agent pads 2 px).
        var m = Environment.GetEnvironmentVariable("BR_OCR_MODELS");
        if (string.IsNullOrWhiteSpace(m) || !Directory.Exists(Path.Combine(m, ModelCatalog.Ocr))) { output.WriteLine("skipped: BR_OCR_MODELS not set"); return; }
        var home = TestEnv.NewDir("ocr-cuts");
        var s = TestEnv.Settings(home, s => { s.Inference.ModelDir = m; s.Ocr.Engine = "onnx"; s.Ocr.Languages = "en"; s.Ocr.Device = "cpu"; });
        using var e = new RealEngine(s, home);
        var img = TextImages.Render(800, 120, (text, 40, 40, size));
        var r = e.Ocr(TextImages.AsInput(img), entities: false);
        var line = Assert.Single(r.Lines);
        var (inkL, inkR) = Ink(img, line.Y, line.Y + line.H);
        output.WriteLine($"'{line.Text}' box x={line.X} w={line.W} cuts0={line.Cuts[0]} cutsN={line.Cuts[^1]} ink {inkL}..{inkR}");
        Assert.True(line.Cuts[0] - 2 <= inkL, $"left ink at {inkL} outside the box from {line.Cuts[0] - 2}");
        Assert.True(line.Cuts[^1] + 2 >= inkR + 1, $"right ink at {inkR} outside the box to {line.Cuts[^1] + 2}");
    }
}
