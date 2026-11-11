// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.Text;
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Protocol;
using BetterRecall.Worker.Vectors;

namespace BetterRecall.Worker.Tests;

/// <summary>Round trips over a real named pipe in fake mode: every op and every error string of v1-design section 4.</summary>
public class ProtocolTests
{
    private static async Task<PipeHarness> Fake(int dim = 768, TimeSpan? idle = null)
    {
        var home = TestEnv.NewDir("proto");
        var s = TestEnv.Settings(home, x => { x.Inference.Fake = true; x.Index.Dim = dim; });
        return await PipeHarness.StartAsync(new FakeEngine(), s, idle);
    }

    [Fact]
    public async Task Server_pid_is_this_process()
    {
        await using var h = await Fake();
        Assert.Equal(Environment.ProcessId, AgentPipe.ServerProcessId(h.Client.SafePipeHandle));
        var self = AgentPipe.ImagePath(Environment.ProcessId)!;
        Assert.True(AgentPipe.VerifyServer(h.Client.SafePipeHandle, self, out _, out _));
        Assert.False(AgentPipe.VerifyServer(h.Client.SafePipeHandle, @"C:\nowhere\BetterRecall.Agent.exe", out _, out var actual));
        Assert.Equal(self, actual);
    }

    [Fact]
    public async Task Hello_reports_fake_and_dim()
    {
        await using var h = await Fake(dim: 512);
        var (r, p) = await h.CallAsync(new WorkerRequest(7, "hello"));
        Assert.True(r.Ok);
        Assert.Equal(7, r.Id);
        Assert.Empty(p);
        Assert.Equal("fake", r.Hello!.Ep);
        Assert.Equal(512, r.Hello.Dim);
        Assert.Equal(1, r.Hello.Protocol);
        Assert.Empty(r.Hello.Missing);
    }

    [Theory]
    [InlineData(768)]
    [InlineData(512)]
    [InlineData(256)]
    public async Task Embed_text_returns_normalised_vector_at_index_dim(int dim)
    {
        await using var h = await Fake(dim);
        var (r, p) = await h.CallAsync(new WorkerRequest(0, "embed_text", Text: "quarterly sales", Kind: "query"));
        Assert.True(r.Ok);
        Assert.Equal(dim, r.Dim);
        Assert.NotNull(r.Ms);
        var v = BinaryFrame.BytesToFloats(p);
        Assert.Equal(dim, v.Length);
        Assert.Equal(1.0, Math.Sqrt(v.Sum(x => (double)x * x)), 4);
    }

    [Fact]
    public async Task Fake_text_is_deterministic_and_query_document_close()
    {
        await using var h = await Fake();
        async Task<float[]> E(string t, string k) => BinaryFrame.BytesToFloats((await h.CallAsync(new WorkerRequest(0, "embed_text", Text: t, Kind: k))).Payload);
        var q1 = await E("Budget meeting notes", "query");
        var q2 = await E("Budget meeting notes", "query");
        var d = await E("budget meeting notes", "document");
        var other = await E("holiday photos at the beach", "document");
        var partial = await E("notes from the budget review", "document");
        Assert.Equal(q1, q2);
        Assert.True(VectorMath.Cosine(q1, d) > 0.97, $"query/doc {VectorMath.Cosine(q1, d)}");
        Assert.True(VectorMath.Cosine(q1, other) < 0.3);
        Assert.True(VectorMath.Cosine(q1, partial) > VectorMath.Cosine(q1, other));
    }

    [Theory]
    [InlineData("webp")]
    [InlineData("jpeg")]
    [InlineData("png")]
    public async Task Embed_image_encoded_formats(string format)
    {
        await using var h = await Fake(256);
        var bytes = Encoding.UTF8.GetBytes("not really an image, the fake engine only hashes it");
        var (r, p) = await h.CallAsync(new WorkerRequest(0, "embed_image", Format: format, Nsfw: 0.8), bytes);
        Assert.True(r.Ok);
        Assert.Null(r.Dropped);
        Assert.Equal(256, r.Dim);
        Assert.Equal(256 * 4, p.Length);
        var (r2, p2) = await h.CallAsync(new WorkerRequest(0, "embed_image", Format: format), bytes);
        Assert.Equal(p, p2);
    }

    [Fact]
    public async Task Embed_image_bgra8_and_errors()
    {
        await using var h = await Fake();
        var px = TestEnv.Bgra(8, 4, 10, 20, 30);
        var (ok, p) = await h.CallAsync(new WorkerRequest(0, "embed_image", Format: "bgra8", Width: 8, Height: 4, Stride: 32), px);
        Assert.True(ok.Ok);
        Assert.Equal(768 * 4, p.Length);

        await AssertBad(h, new WorkerRequest(0, "embed_image", Format: "bgra8", Width: 8, Height: 4, Stride: 31), px);      // stride too small
        await AssertBad(h, new WorkerRequest(0, "embed_image", Format: "bgra8", Width: 8, Height: 5, Stride: 32), px);      // payload short
        await AssertBad(h, new WorkerRequest(0, "embed_image", Format: "bgra8", Width: 8, Stride: 32), px);                 // no height
        await AssertBad(h, new WorkerRequest(0, "embed_image", Format: "gif"), px);
        await AssertBad(h, new WorkerRequest(0, "embed_image"), px);
        await AssertBad(h, new WorkerRequest(0, "embed_image", Format: "png"), []);
        await AssertBad(h, new WorkerRequest(0, "embed_image", Format: "png", Nsfw: 1.5), px);
        await AssertBad(h, new WorkerRequest(0, "embed_image", Format: "png", Nsfw: 0), px);
        await AssertBad(h, new WorkerRequest(0, "embed_image", Format: "png", Nsfw: 0.8, NsfwDetector: 0), px);
        await AssertBad(h, new WorkerRequest(0, "embed_image", Format: "png", Nsfw: 0.8, NsfwDetector: -0.1), px);
        await AssertBad(h, new WorkerRequest(0, "embed_image", Format: "png", Nsfw: 0.8, NsfwDetector: 1.01), px);
        var (okDet, pDet) = await h.CallAsync(new WorkerRequest(0, "embed_image", Format: "bgra8", Width: 8, Height: 4, Stride: 32, Nsfw: 0.8, NsfwDetector: 1.0), px);
        Assert.True(okDet.Ok, okDet.Error);
        Assert.Equal(768 * 4, pDet.Length);
    }

    [Fact]
    public async Task Embed_text_errors()
    {
        await using var h = await Fake();
        await AssertBad(h, new WorkerRequest(0, "embed_text", Kind: "query"), []);
        await AssertBad(h, new WorkerRequest(0, "embed_text", Text: "x", Kind: "passage"), []);
        await AssertBad(h, new WorkerRequest(0, "embed_text", Text: "x"), []);
    }

    [Fact]
    public async Task Transcribe_speech_silence_and_errors()
    {
        await using var h = await Fake();
        var speech = new WorkerRequest(0, "transcribe", Format: "f32", SampleRate: 16000, Channels: 1, Source: "system", Language: "auto");
        var (r, p) = await h.CallAsync(speech, TestEnv.Pcm(0.2f));
        Assert.True(r.Ok);
        Assert.Empty(p);
        Assert.True(r.Transcript!.Speech);
        Assert.Contains("system", r.Transcript.Text);
        Assert.Single(r.Transcript.Segments);
        Assert.Equal(1000, r.Transcript.Segments[0].EndMs);

        var (s, _) = await h.CallAsync(speech with { Language = "vi" }, TestEnv.Pcm(0f));
        Assert.True(s.Ok);
        Assert.False(s.Transcript!.Speech);
        Assert.Equal("vi", s.Transcript.Language);

        var pcm = TestEnv.Pcm(0.2f);
        await AssertBad(h, speech with { SampleRate = 44100 }, pcm);
        await AssertBad(h, speech with { Channels = 2 }, pcm);
        await AssertBad(h, speech with { Format = "s16" }, pcm);
        await AssertBad(h, speech with { Source = "camera" }, pcm);
        await AssertBad(h, speech with { Language = "english" }, pcm);
        await AssertBad(h, speech, pcm[..^1]);
        await AssertBad(h, speech, []);
        var nan = BinaryFrame.FloatsToBytes([0.1f, float.NaN]);
        await AssertBad(h, speech, nan);
    }

    [Fact]
    public async Task Bench_and_shutdown()
    {
        await using var h = await Fake();
        var (b, _) = await h.CallAsync(new WorkerRequest(0, "bench"));
        Assert.True(b.Ok);
        Assert.Equal("CPU", b.Bench!.Chosen);
        var (s, _) = await h.CallAsync(new WorkerRequest(99, "shutdown"));
        Assert.True(s.Ok);
        Assert.Equal(99, s.Id);
        Assert.Equal(LoopExit.Shutdown, await h.Loop.WaitAsync(TimeSpan.FromSeconds(5)));
    }

    [Fact]
    public async Task Unknown_op_and_bad_json()
    {
        await using var h = await Fake();
        await AssertBad(h, new WorkerRequest(0, "explode"), []);
        var (r, _) = await h.CallRawAsync(Encoding.UTF8.GetBytes("{\"id\":41,\"op\":"), []);
        Assert.False(r.Ok);
        Assert.Equal("Bad request.", r.Error);
        var (r2, _) = await h.CallRawAsync(Encoding.UTF8.GetBytes("{\"id\":42,\"op\":\"hello\",\"width\":\"wide\"}"), []);
        Assert.Equal(42, r2.Id);
        Assert.Equal("Bad request.", r2.Error);
        var (r3, _) = await h.CallRawAsync(Encoding.UTF8.GetBytes("[]"), []);
        Assert.Equal(0, r3.Id);
        Assert.Equal("Bad request.", r3.Error);
        // Still serving after errors.
        Assert.True((await h.CallAsync(new WorkerRequest(0, "hello"))).Header.Ok);
    }

    [Fact]
    public async Task Requests_answered_in_arrival_order()
    {
        await using var h = await Fake();
        var reqs = Enumerable.Range(1, 20).Select(i => new WorkerRequest(i, "embed_text", Text: $"t{i}", Kind: "document")).ToList();
        // Write and read concurrently: the pipe buffers are small, so writing all requests first could deadlock.
        var writer = Task.Run(async () =>
        {
            foreach (var r in reqs)
                await BinaryFrame.WriteAsync(h.Server, System.Text.Json.JsonSerializer.SerializeToUtf8Bytes(r, ContractsJson.Default.WorkerRequest), Array.Empty<byte>(), CancellationToken.None);
        });
        for (int i = 1; i <= 20; i++)
        {
            var f = (await BinaryFrame.ReadAsync(h.Server, CancellationToken.None))!.Value;
            var resp = System.Text.Json.JsonSerializer.Deserialize(f.Header, ContractsJson.Default.WorkerResponse)!;
            Assert.Equal(i, resp.Id);
        }
        await writer;
    }

    [Fact]
    public async Task Exits_when_pipe_closes()
    {
        var h = await Fake();
        await h.Server.DisposeAsync();
        Assert.Equal(LoopExit.PipeClosed, await h.Loop.WaitAsync(TimeSpan.FromSeconds(5)));
        await h.DisposeAsync();
    }

    [Fact]
    public async Task Exits_when_idle()
    {
        await using var h = await Fake(idle: TimeSpan.FromMilliseconds(300));
        Assert.True((await h.CallAsync(new WorkerRequest(0, "hello"))).Header.Ok);
        Assert.Equal(LoopExit.Idle, await h.Loop.WaitAsync(TimeSpan.FromSeconds(5)));
    }

    [Fact]
    public async Task Oversized_header_is_a_protocol_error()
    {
        await using var h = await Fake();
        var len = new byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(len, BinaryFrame.MaxHeaderBytes + 1);
        await h.Server.WriteAsync(len);
        await h.Server.FlushAsync();
        Assert.Equal(LoopExit.ProtocolError, await h.Loop.WaitAsync(TimeSpan.FromSeconds(5)));
    }

    [Fact]
    public async Task Engine_exceptions_map_to_wire_errors()
    {
        var home = TestEnv.NewDir("errs");
        await using var h = await PipeHarness.StartAsync(new ThrowingEngine(), TestEnv.Settings(home));
        Assert.Equal("Internal error.", (await h.CallAsync(new WorkerRequest(0, "embed_text", Text: "x", Kind: "query"))).Header.Error);
        Assert.Equal("Model not installed: whisper-large-v3-turbo.",
            (await h.CallAsync(new WorkerRequest(0, "transcribe", Format: "f32", SampleRate: 16000, Channels: 1, Source: "mic"), TestEnv.Pcm(0.1f))).Header.Error);
        Assert.Equal("Device failed: NPU compile failed.", (await h.CallAsync(new WorkerRequest(0, "bench"))).Header.Error);
        Assert.Equal("Device failed: model output contains NaN.",
            (await h.CallAsync(new WorkerRequest(0, "embed_image", Format: "png"), [1, 2, 3])).Header.Error);
    }

    private static async Task AssertBad(PipeHarness h, WorkerRequest r, byte[] payload)
    {
        var (resp, p) = await h.CallAsync(r, payload);
        Assert.False(resp.Ok);
        Assert.Equal("Bad request.", resp.Error);
        Assert.Empty(p);
    }

    private sealed class ThrowingEngine : IWorkerEngine
    {
        public WorkerHello Hello(int dim) => throw new NotImplementedException();
        public ImageEmbedding EmbedImage(ImageInput image, double? t, int grid = 1, double? detector = null) => new([float.NaN, .. new float[767]], null);
        public float[] EmbedText(string text, TextKind kind) => throw new InvalidOperationException("boom");
        public TranscribeResult Transcribe(float[] pcm, string source, string language) => throw new ModelNotInstalledException("whisper-large-v3-turbo");
        public BenchResult Bench() => throw new DeviceFailedException("NPU compile failed.");
        public OcrResult Ocr(ImageInput image, bool entities) => throw new ModelNotInstalledException("ocr");
        public void Dispose() { }
    }
}
