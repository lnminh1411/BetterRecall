// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Privacy;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;
using BetterRecall.Core.Ai;
using SkiaSharp;

namespace BetterRecall.Agent.Tests;

/// <summary>Cloud describe hook: the outgoing image is read again and redacted with the current settings.</summary>
public class DescribeRedactorTests
{
    private sealed class Reader(Func<OcrResult> answer) : IKeyframeReader
    {
        public int Calls;
        public Task<OcrResult> ReadAsync(ReadOnlyMemory<byte> bgra, int width, int height, int stride, bool entities, CancellationToken ct)
        {
            Calls++;
            return Task.FromResult(answer());
        }
    }

    private static OutgoingImage WhiteWebp(int w = 200, int h = 60)
    {
        using var bmp = new SKBitmap(new SKImageInfo(w, h, SKColorType.Bgra8888, SKAlphaType.Premul));
        bmp.Erase(SKColors.White);
        using var data = bmp.Encode(SKEncodedImageFormat.Webp, 100);
        return new OutgoingImage(data.ToArray(), "image/webp", w, h, false);
    }

    // "Card: 4111 1111 1111 1111" at (10,10), 10 px per character, 20 px high.
    private static OcrResult CardLine()
    {
        const string text = "Card: 4111 1111 1111 1111";
        var cuts = Enumerable.Range(0, text.Length + 1).Select(i => 10 + i * 6).ToArray();
        return new OcrResult([new OcrLine(text, 10, 10, text.Length * 6, 20, 0.99f, cuts)], "fake", 1);
    }

    private static Settings Defaults() => new();

    [Fact]
    public async Task Card_number_is_blacked_out_and_marked_redacted()
    {
        var reader = new Reader(CardLine);
        var r = new DescribeRedactor(reader, Defaults);
        var outp = await r.RedactAsync("s_1", WhiteWebp(), CancellationToken.None);
        Assert.True(outp.Redacted);
        Assert.Equal("image/webp", outp.Mime);
        Assert.Equal(1, reader.Calls);
        using var bmp = SKBitmap.Decode(outp.Bytes);
        // Inside the digits (x 46..160, y 10..30): black. The label "Card:" (x 10..40) stays white.
        Assert.True(bmp.GetPixel(100, 20).Red < 40);
        Assert.True(bmp.GetPixel(20, 20).Red > 200);
    }

    [Fact]
    public async Task Clean_text_goes_out_unchanged_but_checked()
    {
        var input = WhiteWebp();
        var r = new DescribeRedactor(new Reader(() => new OcrResult([new OcrLine("Meeting notes", 10, 10, 78, 20, 0.99f, [.. Enumerable.Range(0, 14).Select(i => 10 + i * 6)])], "fake", 1)), Defaults);
        var outp = await r.RedactAsync("s_1", input, CancellationToken.None);
        Assert.True(outp.Redacted);
        Assert.Same(input.Bytes, outp.Bytes);
    }

    [Fact]
    public async Task Missing_model_or_ocr_off_sends_as_stored_and_says_so()
    {
        var input = WhiteWebp();
        var missing = new DescribeRedactor(new Reader(() => throw new WorkerException("Model not installed: ocr.")), Defaults);
        var a = await missing.RedactAsync("s_1", input, CancellationToken.None);
        Assert.False(a.Redacted);
        Assert.Same(input.Bytes, a.Bytes);

        var off = new Settings();
        off.Ocr.Enabled = false;
        var reader = new Reader(CardLine);
        var b = await new DescribeRedactor(reader, () => off).RedactAsync("s_1", input, CancellationToken.None);
        Assert.False(b.Redacted);
        Assert.Equal(0, reader.Calls);
    }

    [Fact]
    public async Task Ocr_failure_blocks_the_send()
    {
        var r = new DescribeRedactor(new Reader(() => throw new WorkerException("Device failed: x.")), Defaults);
        var ex = await Assert.ThrowsAsync<DescribeException>(() => r.RedactAsync("s_1", WhiteWebp(), CancellationToken.None));
        Assert.Contains("not sent", ex.Message);
    }

    [Fact]
    public async Task No_text_detector_on_needs_no_ocr()
    {
        var s = new Settings();
        s.Privacy.RedactPasswords = s.Privacy.RedactCards = s.Privacy.RedactIds = s.Privacy.RedactBirthDates = false;
        var reader = new Reader(CardLine);
        var outp = await new DescribeRedactor(reader, () => s).RedactAsync("s_1", WhiteWebp(), CancellationToken.None);
        Assert.True(outp.Redacted);
        Assert.Equal(0, reader.Calls);
    }
}
