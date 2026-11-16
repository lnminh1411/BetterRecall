// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.IO.Pipes;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Inference;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker.Tests;

/// <summary>K review S1: while a request runs long (inline device re-bench, NPU OCR compile), the worker loop sends id-0
/// keepalive frames, whole and never inside the reply, and stops them after their cap.</summary>
public class Sweep3KKeepaliveTests
{
    private sealed class SlowEngine(int ms) : IWorkerEngine
    {
        private readonly FakeEngine _inner = new();
        public WorkerHello Hello(int dim) => _inner.Hello(dim);
        public ImageEmbedding EmbedImage(ImageInput image, double? t, int grid = 1, double? detector = null) => _inner.EmbedImage(image, t, grid, detector);
        public float[] EmbedText(string text, TextKind kind) { Thread.Sleep(ms); return _inner.EmbedText(text, kind); }
        public TranscribeResult Transcribe(float[] pcm, string source, string language) => _inner.Transcribe(pcm, source, language);
        public BenchResult Bench() => _inner.Bench();
        public OcrResult Ocr(ImageInput image, bool entities) => _inner.Ocr(image, entities);
        public void Dispose() => _inner.Dispose();
    }

    /// <summary>Sends one embed_text and returns every frame id up to and including the reply.</summary>
    private static async Task<List<long>> RunAsync(int handlerMs, TimeSpan every, TimeSpan max)
    {
        var home = TestEnv.NewDir("keepalive");
        var s = TestEnv.Settings(home, x => x.Inference.Fake = true);
        var name = PipeNames.NewWorker();
        // 0-byte buffers like the agent's worker pipe: every write waits for the reader.
        await using var server = new NamedPipeServerStream(name, PipeDirection.InOut, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous, 0, 0);
        var wait = server.WaitForConnectionAsync();
        await using var client = await AgentPipe.ConnectAsync(name, TimeSpan.FromSeconds(5), CancellationToken.None);
        await wait;
        var handler = new RequestHandler(new SlowEngine(handlerMs), () => s);
        var loop = Task.Run(() => WorkerLoop.RunAsync(client, handler, TimeSpan.FromSeconds(30), CancellationToken.None, null, every, max));

        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(20));
        var req = JsonSerializer.SerializeToUtf8Bytes(new WorkerRequest(7, "embed_text", Text: "abc", Kind: "query"), ContractsJson.Default.WorkerRequest);
        await BinaryFrame.WriteAsync(server, req, ReadOnlyMemory<byte>.Empty, cts.Token);
        var ids = new List<long>();
        while (true)
        {
            var f = await BinaryFrame.ReadAsync(server, cts.Token) ?? throw new EndOfStreamException();
            var h = JsonSerializer.Deserialize(f.Header, ContractsJson.Default.WorkerResponse)!;   // a torn frame would not parse
            ids.Add(h.Id);
            if (h.Id == 0) { Assert.True(h.Ok); Assert.Empty(f.Payload); continue; }
            Assert.True(h.Ok, h.Error);
            Assert.NotEmpty(f.Payload);
            Assert.Equal(0, f.Payload.Length % 4);   // whole floats: no keepalive bytes inside the reply
            break;
        }
        await server.DisposeAsync();
        await loop.WaitAsync(TimeSpan.FromSeconds(10));
        return ids;
    }

    [Fact]
    public async Task Long_request_sends_keepalives_before_its_reply()
    {
        var ids = await RunAsync(700, TimeSpan.FromMilliseconds(100), TimeSpan.FromMinutes(1));
        Assert.Equal(7, ids[^1]);
        Assert.True(ids.Count(i => i == 0) >= 4, $"keepalives: {ids.Count - 1}");
        Assert.All(ids[..^1], i => Assert.Equal(0, i));
    }

    [Fact]
    public async Task Fast_request_sends_no_keepalive()
    {
        var ids = await RunAsync(0, TimeSpan.FromMilliseconds(300), TimeSpan.FromMinutes(1));
        Assert.Equal([7L], ids);
    }

    [Fact]
    public async Task Keepalives_stop_after_their_cap()
    {
        var ids = await RunAsync(900, TimeSpan.FromMilliseconds(100), TimeSpan.FromMilliseconds(250));
        Assert.Equal(7, ids[^1]);
        Assert.InRange(ids.Count(i => i == 0), 1, 3);   // beats at about 100 and 200 ms, none after 250 ms
    }
}
