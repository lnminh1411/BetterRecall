// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Models.Ocr;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Tests;

/// <summary>Bug sweep 3, batch I: C1 (OCR reply over the frame header limit) and C17 (OCR box cap).</summary>
public class Sweep3ITests
{
    /// <summary>OCR answers with whatever Lines returns; everything else is unused.</summary>
    private sealed class OcrEngine : IWorkerEngine
    {
        public Func<OcrLine[]> Lines = () => [];
        public Exception? Throw;
        public WorkerHello Hello(int dim) => new(1, "CPU", "fake", "fake-model", "r1", dim, true, true, []);
        public ImageEmbedding EmbedImage(ImageInput image, double? t, int grid = 1, double? detector = null) => throw new NotSupportedException();
        public float[] EmbedText(string text, TextKind kind) => throw new NotSupportedException();
        public TranscribeResult Transcribe(float[] pcm, string source, string language) => throw new NotSupportedException();
        public BenchResult Bench() => throw new NotSupportedException();
        public OcrResult Ocr(ImageInput image, bool entities) => Throw is { } e ? throw e : new OcrResult(Lines(), "onnx", 1);
        public void Dispose() { }
    }

    private static OcrLine[] Screen(int lines, int chars, char c)
    {
        var text = new string(c, chars);
        var cuts = Enumerable.Range(0, chars + 1).Select(i => i * 8).ToArray();
        return Enumerable.Range(0, lines).Select(i => new OcrLine(text, 0, i * 16, chars * 8, 16, 0.98f, cuts)).ToArray();
    }

    private static readonly WorkerRequest Ocr = new(0, "ocr", Format: "bgra8", Width: 2, Height: 2, Stride: 8);

    private static async Task<PipeHarness> Start(OcrEngine engine)
    {
        var home = TestEnv.NewDir("s3i");
        return await PipeHarness.StartAsync(engine, TestEnv.Settings(home, x => x.Inference.Fake = true));
    }

    [Fact]
    public async Task C1_dense_screen_ocr_reply_reaches_the_agent()
    {
        // 100 lines x 120 Vietnamese characters: about 90 KB of header, over the old 64 KiB limit.
        var engine = new OcrEngine { Lines = () => Screen(100, 120, 'ệ') };
        await using var h = await Start(engine);
        var (r, _) = await h.CallAsync(Ocr, new byte[16]);
        Assert.True(r.Ok, r.Error);
        Assert.Equal(100, r.Ocr!.Lines.Length);
        Assert.False(h.Loop.IsCompleted);
    }

    [Fact]
    public async Task C1_reply_over_the_header_limit_is_an_error_and_the_pipe_stays_in_step()
    {
        // About 5 MB of header: past even the raised limit. The worker must answer, not exit.
        var engine = new OcrEngine { Lines = () => Screen(400, 2000, 'x') };
        await using var h = await Start(engine);
        var (r, p) = await h.CallAsync(Ocr, new byte[16]);
        Assert.False(r.Ok);
        Assert.Equal(WorkerErrors.Internal, r.Error);
        Assert.Empty(p);
        Assert.False(h.Loop.IsCompleted);
        engine.Lines = () => Screen(2, 10, 'y');
        var (ok, _) = await h.CallAsync(Ocr, new byte[16]);
        Assert.True(ok.Ok);
        Assert.Equal(2, ok.Ocr!.Lines.Length);
        var (hello, _) = await h.CallAsync(new WorkerRequest(0, "hello"));
        Assert.True(hello.Ok);
    }

    private static float[] Blobs(int pw, int ph, int count)
    {
        // count separate 4x4 blobs on a grid with 2-pixel gaps.
        var prob = new float[pw * ph];
        int perRow = pw / 6;
        for (int n = 0; n < count; n++)
        {
            int x0 = n % perRow * 6, y0 = n / perRow * 6;
            for (int y = y0; y < y0 + 4; y++) for (int x = x0; x < x0 + 4; x++) prob[y * pw + x] = 0.9f;
        }
        return prob;
    }

    [Fact]
    public void C17_box_cap_fails_the_read_instead_of_stopping_part_way()
    {
        int pw = 60, ph = 60;
        Assert.Equal(5, OcrMath.Boxes(Blobs(pw, ph, 5), pw, ph, pw, ph, 0.2f, 0.45f, 1.4f, maxBoxes: 5).Count);   // exactly the cap: fine
        var ex = Assert.Throws<OcrOverflowException>(() => OcrMath.Boxes(Blobs(pw, ph, 6), pw, ph, pw, ph, 0.2f, 0.45f, 1.4f, maxBoxes: 5));
        Assert.StartsWith("Device failed: ", ex.Message);
    }

    [Fact]
    public async Task C17_overflow_reaches_the_agent_as_device_failed()
    {
        var engine = new OcrEngine { Throw = new OcrOverflowException("ocr found more than 3000 text boxes") };
        await using var h = await Start(engine);
        var (r, _) = await h.CallAsync(Ocr, new byte[16]);
        Assert.False(r.Ok);
        Assert.StartsWith("Device failed: ", r.Error);
        Assert.False(h.Loop.IsCompleted);
    }
}
