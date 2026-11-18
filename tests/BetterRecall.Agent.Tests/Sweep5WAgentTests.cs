// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using System.Threading.Channels;
using BetterRecall.Agent.Scheduling;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 5, batch W: worker, scheduler and worker connection (C5 + A6, A3).</summary>
public class Sweep5WAgentTests
{
    private static GovernanceInputs Idle() => new(1, 0, false, UserNotificationState.AcceptsNotifications, 0);
    private static GovernanceInputs Busy() => new(95, 0, false, UserNotificationState.AcceptsNotifications, 0);

    private static async Task Until(Func<bool> condition, int ms = 10_000)
    {
        long until = Environment.TickCount64 + ms;
        while (!condition())
        {
            Assert.True(Environment.TickCount64 < until, "condition not reached");
            await Task.Delay(10);
        }
    }

    // ---------------- C5 + A6: bounded/coalesced _wake and _space == null guard ----------------

    [Fact]
    public async Task C5_Signal_burst_does_not_bypass_model_missing_backoff()
    {
        var store = new FakeStore();
        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [1, 2, 3], "image/webp", true));
        int embedCalls = 0;
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) =>
            {
                if (req.Op != "embed_image") return null;
                Interlocked.Increment(ref embedCalls);
                return (new WorkerResponse(req.Id, false, Error: "Model not installed: embeddinggemma-2."), []);
            },
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        await using var sched = new EmbeddingScheduler(() => store, () => new Settings(), new WorkerClient(sup), Idle, () => false);

        for (int i = 0; i < 20; i++) sched.Signal();
        sched.Start();
        await Until(() => Volatile.Read(ref embedCalls) >= 1 && sched.LastDecision == "model-missing");

        for (int i = 0; i < 20; i++) sched.Signal();
        await Task.Delay(300);
        Assert.Equal(1, Volatile.Read(ref embedCalls));
        Assert.Equal("model-missing", sched.LastDecision);
    }

    [Fact]
    public async Task C5_Signal_burst_does_not_bypass_governance_defer()
    {
        var store = new FakeStore();
        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [1, 2, 3], "image/webp", true));
        int probeCalls = 0;
        await using var sup = new WorkerSupervisor(new FakeWorkerLauncher(), "x", () => 120);
        await using var sched = new EmbeddingScheduler(() => store, () => new Settings(), new WorkerClient(sup), () =>
        {
            Interlocked.Increment(ref probeCalls);
            return Busy();
        }, () => false);

        for (int i = 0; i < 20; i++) sched.Signal();
        sched.Start();
        await Until(() => Volatile.Read(ref probeCalls) >= 1 && sched.LastDecision == "cpu");

        for (int i = 0; i < 20; i++) sched.Signal();
        await Task.Delay(300);
        Assert.Equal(1, Volatile.Read(ref probeCalls));
        Assert.Equal("cpu", sched.LastDecision);
    }

    [Fact]
    public async Task A6_StepAsync_with_null_space_never_calls_worker_embed_ops()
    {
        var store = new FakeStore();
        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [1, 2, 3], "image/webp", true));
        int embedCalls = 0;
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) =>
            {
                if (req.Op == "hello")
                    return (new WorkerResponse(req.Id, true, Hello: new WorkerHello(1, "CPU", "fake", "fake-model", "r1", 4, true, true, [])), []);
                if (req.Op.StartsWith("embed", StringComparison.Ordinal))
                    Interlocked.Increment(ref embedCalls);
                return null;
            },
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        await using var sched = new EmbeddingScheduler(() => store, () => new Settings(), new WorkerClient(sup), Idle, () => false, indexDim: () => 768);

        var wait = await sched.StepAsync(CancellationToken.None);
        Assert.Equal(TimeSpan.FromSeconds(60), wait);
        Assert.Equal("space-refused", sched.LastDecision);
        Assert.Equal(0, Volatile.Read(ref embedCalls));
        Assert.True(sup.RestartPending);
    }

    [Fact]
    public async Task C5_A6_Signal_burst_does_not_bypass_space_refused_and_accepted_hello_wakes_it()
    {
        var store = new FakeStore();
        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [1, 2, 3], "image/webp", true));
        int embedCalls = 0, probeCalls = 0, helloDim = 8;
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) =>
            {
                if (req.Op == "hello")
                    return (new WorkerResponse(req.Id, true, Hello: new WorkerHello(1, "CPU", "fake", "fake-model", "r1", Volatile.Read(ref helloDim), true, true, [])), []);
                if (req.Op.StartsWith("embed", StringComparison.Ordinal))
                    Interlocked.Increment(ref embedCalls);
                return null;
            },
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var client = new WorkerClient(sup);
        await using var sched = new EmbeddingScheduler(() => store, () => new Settings(), client, () =>
        {
            Interlocked.Increment(ref probeCalls);
            return Idle();
        }, () => false, indexDim: () => 4);

        for (int i = 0; i < 20; i++) sched.Signal();
        sched.Start();
        await Until(() => Volatile.Read(ref probeCalls) >= 1 && sched.LastDecision == "space-refused");

        for (int i = 0; i < 20; i++) sched.Signal();
        await Task.Delay(300);
        Assert.Equal(1, Volatile.Read(ref probeCalls));
        Assert.Equal(0, Volatile.Read(ref embedCalls));
        Assert.Equal("space-refused", sched.LastDecision);

        // An accepted hello wakes the 60 s sleep immediately and finishes the queued job.
        Volatile.Write(ref helloDim, 4);
        await sup.ShutdownAsync();
        await client.PrewarmAsync(CancellationToken.None);
        await Until(() => store.Vectors.Count == 1);
        Assert.Equal(1, Volatile.Read(ref embedCalls));
    }

    [Fact]
    public async Task C5_Signal_still_wakes_empty_scheduler_once()
    {
        var store = new FakeStore();
        await using var sup = new WorkerSupervisor(new FakeWorkerLauncher(), "x", () => 120);
        await using var sched = new EmbeddingScheduler(() => store, () => new Settings(), new WorkerClient(sup), Idle, () => false);
        sched.Start();
        await Until(() => sched.LastDecision == "empty");

        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [1, 2, 3], "image/webp", true));
        for (int i = 0; i < 20; i++) sched.Signal();
        await Until(() => store.Vectors.Count == 1);
    }

    // ---------------- A3: same-tick urgent preemption blames wire order, not arrival id ----------------

    [Fact]
    public async Task A3_Same_tick_urgent_preemption_blames_first_on_wire_in_WorkerConnection()
    {
        using var stream = new GatedDuplexStream();
        await using var conn = new WorkerConnection(stream, clock: () => 1000L);

        var r1 = conn.SendAsync(new WorkerRequest(0, "embed_text", Text: "one", Kind: "document"), ReadOnlyMemory<byte>.Empty, CancellationToken.None);
        Assert.True(await stream.FirstWriteEntered.WaitAsync(TimeSpan.FromSeconds(10)));

        var r2 = conn.SendAsync(new WorkerRequest(0, "embed_text", Text: "two", Kind: "document"), ReadOnlyMemory<byte>.Empty, CancellationToken.None, urgent: false);
        await Until(() => conn.WriteGateState.Normal == 1);

        var r3 = conn.SendAsync(new WorkerRequest(0, "embed_text", Text: "three", Kind: "query"), ReadOnlyMemory<byte>.Empty, CancellationToken.None, urgent: true);
        await Until(() => conn.WriteGateState.Urgent == 1);

        stream.ReleaseFirstWrite();
        await Until(() => stream.Writes == 3 && !conn.WriteGateState.Held);
        Assert.Equal([1L, 3L, 2L], stream.WrittenIds);

        // Request 1 finishes cleanly; worker now owes 3 (first on wire) and 2 (behind), both stamped at tick 1000.
        await stream.PushReplyAsync(new WorkerResponse(1, true, Dim: 4, Ms: 1), BinaryFrame.FloatsToBytes([1, 0, 0, 0]));
        await r1.WaitAsync(TimeSpan.FromSeconds(10));

        stream.CloseInbound();
        var e3 = await Assert.ThrowsAsync<WorkerException>(() => r3.WaitAsync(TimeSpan.FromSeconds(10)));
        var e2 = await Assert.ThrowsAsync<WorkerException>(() => r2.WaitAsync(TimeSpan.FromSeconds(10)));
        Assert.Equal("Worker pipe closed.", e3.Message);
        Assert.Equal(WorkerConnection.CrashedElsewhereMessage, e2.Message);
    }

    [Fact]
    public async Task A3_Seam_EmbeddingScheduler_behind_urgent_query_does_not_take_crash_strike()
    {
        var store = new FakeStore();
        var captureId = store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [1, 2, 3], "image/webp", true));
        using var running = new SemaphoreSlim(0);
        using var gate = new SemaphoreSlim(0);
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) =>
            {
                if (req.Op == "transcribe") { running.Release(); gate.Wait(TimeSpan.FromSeconds(10)); }
                return null;
            },
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120) { LivenessClock = () => 1000L };
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        var conn = await sup.EnsureAsync(CancellationToken.None);
        await using var sched = new EmbeddingScheduler(() => store, () => new Settings(), client, Idle, () => false);

        var t = client.TranscribeAsync(new float[1600], "mic", "auto", CancellationToken.None);
        Assert.True(await running.WaitAsync(TimeSpan.FromSeconds(10)));
        static byte[] Big(byte mark) { var b = new byte[2 << 20]; b[0] = mark; return b; }
        var i1 = client.EmbedImageAsync(Big(1), "image/webp", null, 1, null, CancellationToken.None);
        await Until(() => conn.WriteGateState is { Held: true, Normal: 0, Urgent: 0 });

        var step = sched.StepAsync(CancellationToken.None);
        await Until(() => conn.WriteGateState.Normal == 1);

        var q = client.EmbedTextAsync("where is invoice", "query", CancellationToken.None);
        await Until(() => conn.WriteGateState.Urgent == 1);

        launcher.NoReply = r => r.Op == "embed_text";
        launcher.CrashWhen = r => r.Op == "embed_image" && r.Nsfw is not null;
        gate.Release();

        await t;
        await i1;
        var eq = await Assert.ThrowsAsync<WorkerException>(() => q);
        await step;

        Assert.Equal("Worker pipe closed.", eq.Message);
        Assert.True(EmbeddingScheduler.IsCrash(eq));
        Assert.Equal("worker-unavailable", sched.LastDecision);
        Assert.Equal(1, sched.Queued);
        Assert.Empty(store.Failed);
        Assert.Equal(0, sched.Attempts(captureId));
    }

    private sealed class GatedDuplexStream : Stream
    {
        private readonly Channel<byte[]> _inbound = Channel.CreateUnbounded<byte[]>();
        private readonly SemaphoreSlim _firstGate = new(0);
        private readonly List<long> _writtenIds = [];
        private byte[] _cur = [];
        private int _pos;
        private int _writes;

        public SemaphoreSlim FirstWriteEntered { get; } = new(0);
        public int Writes => Volatile.Read(ref _writes);
        public long[] WrittenIds { get { lock (_writtenIds) return [.. _writtenIds]; } }

        public void ReleaseFirstWrite() => _firstGate.Release();
        public void CloseInbound() => _inbound.Writer.TryComplete();

        public async Task PushReplyAsync(WorkerResponse resp, byte[] payload)
        {
            using var ms = new MemoryStream();
            await BinaryFrame.WriteAsync(ms, JsonSerializer.SerializeToUtf8Bytes(resp, ContractsJson.Default.WorkerResponse), payload, CancellationToken.None);
            _inbound.Writer.TryWrite(ms.ToArray());
        }

        public override bool CanRead => true;
        public override bool CanSeek => false;
        public override bool CanWrite => true;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => throw new NotSupportedException(); set => throw new NotSupportedException(); }
        public override void Flush() { }
        public override Task FlushAsync(CancellationToken cancellationToken) => Task.CompletedTask;
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override int Read(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();

        public override async ValueTask WriteAsync(ReadOnlyMemory<byte> buffer, CancellationToken cancellationToken = default)
        {
            int n = Interlocked.Increment(ref _writes);
            if (n == 1)
            {
                FirstWriteEntered.Release();
                await _firstGate.WaitAsync(cancellationToken).ConfigureAwait(false);
            }
            if (buffer.Length >= 8)
            {
                int headerLen = System.Buffers.Binary.BinaryPrimitives.ReadInt32LittleEndian(buffer.Span[..4]);
                if (headerLen > 0 && 4 + headerLen <= buffer.Length)
                {
                    using var doc = JsonDocument.Parse(buffer.Slice(4, headerLen));
                    long id = doc.RootElement.GetProperty("id").GetInt64();
                    lock (_writtenIds) _writtenIds.Add(id);
                }
            }
        }

        public override Task WriteAsync(byte[] buffer, int offset, int count, CancellationToken cancellationToken)
            => WriteAsync(buffer.AsMemory(offset, count), cancellationToken).AsTask();

        public override async ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken cancellationToken = default)
        {
            while (_pos >= _cur.Length)
            {
                if (!await _inbound.Reader.WaitToReadAsync(cancellationToken).ConfigureAwait(false)) return 0;
                if (_inbound.Reader.TryRead(out var next)) { _cur = next; _pos = 0; }
            }
            int take = Math.Min(buffer.Length, _cur.Length - _pos);
            _cur.AsSpan(_pos, take).CopyTo(buffer.Span);
            _pos += take;
            return take;
        }

        public override Task<int> ReadAsync(byte[] buffer, int offset, int count, CancellationToken cancellationToken)
            => ReadAsync(buffer.AsMemory(offset, count), cancellationToken).AsTask();

        protected override void Dispose(bool disposing)
        {
            if (disposing) _inbound.Writer.TryComplete();
            base.Dispose(disposing);
        }
    }
}
