// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Reflection;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Privacy;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Tests;

/// <summary>Sweep 3 live run: the agent grew 195 -> 725 MB over 15 min of a dense, blinking page while CPU OCR took
/// 35 s against the 30 s keyframe OCR timeout, and kept it when idle. Cause: grabs the full keyframe queue dropped
/// (BoundedChannelFullMode.DropWrite, whose TryWrite still returns true) were never disposed: 3.5 MB of native pixels each.</summary>
public class KeyframeLeakSweep3Tests
{
    private static unsafe KeyframeGrab Grab(int w, int h, int seed)
    {
        var req = new KeyframeRequest(DateTimeOffset.UtcNow, "msedge.exe", @"C:\edge.exe", "dense code", "ctx", true, w, 0, "");
        var g = KeyframeGrab.Alloc(0, req, w, h, 0);
        var span = new Span<byte>(g.Pixels, w * h * 4);
        span.Fill((byte)(seed % 2 == 0 ? 255 : 200));   // the blinking page: every grab differs
        for (int i = seed % 97; i < span.Length; i += 4099) span[i] = (byte)seed;
        return g;
    }

    private static unsafe int Live(IEnumerable<KeyframeGrab> grabs) => grabs.Count(g => g.Pixels != null);

    /// <summary>A reader that blocks until released, like OCR on a busy serial worker.</summary>
    private sealed class BlockedReader : IKeyframeReader
    {
        public readonly ManualResetEventSlim Release = new(false);
        public int Calls;
        public Task<OcrResult> ReadAsync(ReadOnlyMemory<byte> bgra, int width, int height, int stride, bool entities, CancellationToken ct)
        {
            Interlocked.Increment(ref Calls);
            return Task.Run(() => { Release.Wait(ct); return new OcrResult([], "fake", 1); }, ct);
        }
    }

    [Fact]
    public async Task Grabs_the_full_queue_refuses_are_freed_at_once()
    {
        var reader = new BlockedReader();
        var s = new Settings();
        await using var pipe = new KeyframePipeline(() => new FakeStore(), () => s, _ => { }, reader, new RedactionMasks(), TimeSpan.FromMinutes(5));
        var grabs = new List<KeyframeGrab> { Grab(64, 32, 0) };
        pipe.Post(grabs[0]);
        for (int i = 0; i < 200 && Volatile.Read(ref reader.Calls) == 0; i++) await Task.Delay(10);
        Assert.Equal(1, reader.Calls);   // the keyframe thread is stuck in OCR
        for (int i = 1; i <= 20; i++) { var g = Grab(64, 32, i); grabs.Add(g); pipe.Post(g); }
        // 1 in OCR + 6 queued keep their pixels; the 14 refused ones are zeroed and freed (they used to leak).
        Assert.Equal(14, Interlocked.Read(ref pipe.Dropped));
        Assert.Equal(7, Live(grabs));
        reader.Release.Set();
    }

    /// <summary>The live run in small: OCR slower than the caller's timeout on the real pipe and supervisor, a new grab
    /// every 20 ms. Native grabs and worker requests owed stay bounded (abandoned requests do not pile up: one keyframe
    /// thread, one write at a time).</summary>
    [Fact]
    public async Task Slow_ocr_with_grabs_arriving_keeps_memory_bounded()
    {
        int ops = 0;
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) =>
            {
                if (req.Op != "ocr") return null;
                Interlocked.Increment(ref ops);
                Thread.Sleep(300);
                return (new WorkerResponse(req.Id, true, Ocr: new OcrResult([], "fake", 300)), []);
            },
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 600);
        var client = new WorkerClient(sup);
        var conn = await sup.EnsureAsync(CancellationToken.None);
        var abandoned = (System.Collections.IDictionary)typeof(WorkerConnection).GetField("_abandoned", BindingFlags.NonPublic | BindingFlags.Instance)!.GetValue(conn)!;
        var s = new Settings();
        var pipe = new KeyframePipeline(() => new FakeStore(), () => s, _ => { }, new WorkerKeyframeReader(client), new RedactionMasks(), TimeSpan.FromMilliseconds(100));
        var grabs = new List<KeyframeGrab>();
        int maxLive = 0, maxOwed = 0;
        for (int i = 0; i < 150; i++)
        {
            var g = Grab(320, 200, i);
            grabs.Add(g);
            pipe.Post(g);
            await Task.Delay(20);
            maxLive = Math.Max(maxLive, Live(grabs));
            int owed;
            lock (abandoned) owed = abandoned.Count;
            maxOwed = Math.Max(maxOwed, owed + conn.Pending);
        }
        await pipe.DisposeAsync();
        Assert.True(maxLive <= 7, $"{maxLive} grabs held at once");
        Assert.True(maxOwed <= 2, $"{maxOwed} requests owed at once");
        Assert.Equal(0, Live(grabs));
        Assert.True(Interlocked.Read(ref pipe.Dropped) > 100);
        Assert.True(ops > 2);
    }

    /// <summary>OCR that always takes longer than the base timeout: the wait grows after the first timeout, so the
    /// next keyframe is read and kept instead of being dropped and re-read forever.</summary>
    [Fact]
    public async Task Ocr_slower_than_the_timeout_gets_a_longer_wait_and_keyframes_are_kept()
    {
        var reader = new DelayReader(TimeSpan.FromMilliseconds(280));
        var s = new Settings();
        s.Keyframes.Format = "webp";
        var store = new FakeStore();
        await using var pipe = new KeyframePipeline(() => store, () => s, _ => { }, reader, new RedactionMasks(), TimeSpan.FromMilliseconds(200));
        pipe.Post(Grab(64, 32, 1));
        for (int i = 0; i < 300 && pipe.RedactDropped == 0; i++) await Task.Delay(10);
        Assert.Equal(1, pipe.RedactDropped);
        Assert.Equal(TimeSpan.FromMilliseconds(400), pipe.OcrWait);
        pipe.Post(Grab(64, 32, 2));
        for (int i = 0; i < 300 && pipe.Kept == 0; i++) await Task.Delay(10);
        Assert.Equal(1, pipe.Kept);
        Assert.Equal(1, pipe.OcrRead);
        Assert.True(pipe.OcrWait >= TimeSpan.FromMilliseconds(500));
    }

    private sealed class DelayReader(TimeSpan delay) : IKeyframeReader
    {
        public async Task<OcrResult> ReadAsync(ReadOnlyMemory<byte> bgra, int width, int height, int stride, bool entities, CancellationToken ct)
        {
            await Task.Delay(delay, ct);
            return new OcrResult([], "fake", delay.TotalMilliseconds);
        }
    }

    [Theory]
    [InlineData(30, 30, 0, true, 60)]     // first timeout doubles
    [InlineData(30, 60, 0, true, 120)]    // up to 4x the baseline
    [InlineData(30, 120, 0, true, 120)]
    [InlineData(30, 120, 35, false, 70)]  // a 35 s read: twice its time
    [InlineData(30, 70, 2, false, 30)]    // fast again: back to the baseline
    [InlineData(30, 30, 90, false, 120)]
    public void Ocr_wait_follows_the_read_time(int baseS, int currentS, int tookS, bool timedOut, int expectS)
        => Assert.Equal(TimeSpan.FromSeconds(expectS),
            KeyframePipeline.NextOcrWait(TimeSpan.FromSeconds(baseS), TimeSpan.FromSeconds(currentS), TimeSpan.FromSeconds(tookS), timedOut));
}
