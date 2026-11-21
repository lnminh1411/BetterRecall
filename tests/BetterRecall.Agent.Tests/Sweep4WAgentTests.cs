// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Collections.Concurrent;
using System.Security.Cryptography;
using System.Text.Json;
using BetterRecall.Agent.Scheduling;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;
using BetterRecall.Core.Store;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 4, batch W: worker, scheduler and embedding space. Seam tests run the real scheduler and
/// supervisor over the real private pipe to the in-process fake worker, and a real EncryptedStore where the store side
/// matters.</summary>
public class Sweep4WAgentTests
{
    private static GovernanceInputs Idle() => new(1, 0, false, UserNotificationState.AcceptsNotifications, 0);
    private static GovernanceInputs Busy() => new(95, 0, false, UserNotificationState.AcceptsNotifications, 0);

    private static string TempDir()
    {
        var root = Environment.GetEnvironmentVariable("BR_TEST_TMP") is { Length: > 0 } t ? t : Path.GetTempPath();
        var d = Path.Combine(root, "betterrecall-agent-s4w", Guid.NewGuid().ToString("N")[..12]);
        Directory.CreateDirectory(d);
        return d;
    }

    private sealed class RealStore : IDisposable
    {
        private readonly byte[] _key = RandomNumberGenerator.GetBytes(32);
        public readonly string Home = TempDir();
        public readonly EncryptedStore Store;
        public RealStore() => Store = new EncryptedStore(Home, () => (byte[])_key.Clone());
        public string Add(int i) => Store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow.AddMinutes(-10).AddSeconds(i), "app", "", $"t{i}",
            0, 10, 10, 0, [1, 2, (byte)i], "image/webp", true));
        public int Pending => Store.Stats().PendingEmbeddings;
        public void Dispose()
        {
            Store.Dispose();
            try { Directory.Delete(Home, recursive: true); } catch (IOException) { } catch (UnauthorizedAccessException) { }
        }
    }

    private static (WorkerResponse, byte[]) Hello(long id, string rev, int dim = 4, string device = "CPU")
        => (new WorkerResponse(id, true, Hello: new WorkerHello(1, device, "fake", "fake-model", rev, dim, true, true, rev == "" ? ["embeddinggemma-2"] : [])), []);

    // ---------------- A-M4 + C5: hello checks before the store's embedding space ----------------

    /// <summary>A-M4 seam: a worker whose models are missing says hello with revision "". The agent used to pass it
    /// to EnsureEmbeddingSpace, which queued the whole history (and again when the models came back).</summary>
    [Fact]
    public async Task Hello_without_an_embedder_does_not_requeue_the_history()
    {
        using var rs = new RealStore();
        for (int i = 0; i < 3; i++) rs.Add(i);
        var launcher = new FakeWorkerLauncher();
        string rev = "r1";
        launcher.Handler = (req, _) => req.Op == "hello" ? Hello(req.Id, rev)
            : rev == "" && req.Op.StartsWith("embed", StringComparison.Ordinal) ? (new WorkerResponse(req.Id, false, Error: "Model not installed: embeddinggemma-2."), [])
            : null;
        await using var sup = new WorkerSupervisor(launcher, rs.Home, () => 120);
        var settings = new Settings();
        await using var sched = new EmbeddingScheduler(() => rs.Store, () => settings, new WorkerClient(sup), Idle, () => false, indexDim: () => 4);

        await sched.StepAsync(CancellationToken.None);
        Assert.Equal(0, rs.Pending);

        // Models gone (modelDir on an unmounted drive): the next worker says hello without an embedder.
        await sup.ShutdownAsync();
        rev = "";
        rs.Add(3);
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal("model-missing", sched.LastDecision);
        Assert.Equal(1, rs.Pending);   // only the new row; it was 4 (whole history queued)

        // Models back: nothing to re-embed but the new row.
        await sup.ShutdownAsync();
        rev = "r1";
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal(0, rs.Pending);
        Assert.Equal(4, rs.Store.Stats().Vectors);
    }

    /// <summary>C5: a hello whose dim is not the agent's index.dim (a worker that read a hand-edited settings.json) is
    /// refused: the space is kept (no truncation), its vectors are not stored, and a restart is asked for.</summary>
    [Fact]
    public async Task Hello_with_another_dim_is_refused()
    {
        var store = new FakeStore();
        store.AddAudioCapture(new NewAudioCapture(DateTimeOffset.UtcNow, "mic", 1000, "hello world", "en", ""));
        var launcher = new FakeWorkerLauncher();
        launcher.Handler = (req, _) => req.Op == "hello" ? Hello(req.Id, "r1", dim: 4) : null;
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var settings = new Settings();
        await using var sched = new EmbeddingScheduler(() => store, () => settings, new WorkerClient(sup), Idle, () => false, indexDim: () => 768);

        await sched.StepAsync(CancellationToken.None);
        Assert.Null(store.Space);
        Assert.Empty(store.Vectors);
        Assert.Empty(store.Failed);              // kept for a worker on the agent's settings
        Assert.Equal(1, sched.Queued);
        Assert.Equal("space-refused", sched.LastDecision);   // SF2: waits for an accepted hello, not 15 s re-embeds
        Assert.True(sup.RestartPending);
    }

    /// <summary>C5 agent half: the worker gets the agent's worker keys on its command line, so a hand edit of
    /// settings.json the agent never loaded cannot reach it.</summary>
    [Fact]
    public void Worker_settings_argument_carries_the_agents_worker_keys_only()
    {
        var s = new Settings();
        s.Index.Dim = 512;
        s.Inference.Device = "cpu";
        s.Ocr.Languages = "en,vi";
        var json = WorkerSettingsDiff.Json(s);
        using var doc = JsonDocument.Parse(json);
        var root = doc.RootElement;
        Assert.Equal(512, root.GetProperty("index").GetProperty("dim").GetInt32());
        Assert.Equal("cpu", root.GetProperty("inference").GetProperty("device").GetString());
        Assert.Equal("en,vi", root.GetProperty("ocr").GetProperty("languages").GetString());
        Assert.True(root.TryGetProperty("governance", out var gov) && gov.TryGetProperty("ecoQos", out _));
        Assert.False(root.TryGetProperty("ai", out _));              // nothing the worker does not read
        Assert.False(root.TryGetProperty("capture", out _));
        Assert.False(root.GetProperty("privacy").TryGetProperty("deny", out _));
        // Round trip through the contract: what the worker parses is what the agent holds.
        var back = JsonSerializer.Deserialize(json, ContractsJson.Default.Settings)!;
        Assert.Null(WorkerSettingsDiff.Changed(s, back));
    }

    // ---------------- A-M3 + CA-S2: replies name the embedder ----------------

    /// <summary>A-M3 + CA-S2 seam: the running worker switches model and device (bench or replan) with no new hello.
    /// The reply names the new embedder: Hello and status.device follow, and the store's space moves to the new model
    /// (rows embedded under the old one are queued again) instead of filing new vectors under the old one.</summary>
    [Fact]
    public async Task Embed_reply_naming_a_new_model_moves_the_space_and_the_device()
    {
        using var rs = new RealStore();
        rs.Add(0);
        rs.Add(1);
        var launcher = new FakeWorkerLauncher();
        string? model = null;
        launcher.Handler = (req, payload) => req.Op switch
        {
            "embed_image" when model is not null => (new WorkerResponse(req.Id, true, Dim: 4, Ms: 1, Model: model, Revision: "r2", Device: "GPU"),
                BinaryFrame.FloatsToBytes([payload.Length, 1, 0, 0])),
            "bench" => (new WorkerResponse(req.Id, true, Bench: new BenchResult("GPU", []), Model: "fake-model", Revision: "r2", Device: "GPU"), []),
            _ => null,
        };
        await using var sup = new WorkerSupervisor(launcher, rs.Home, () => 120);
        var client = new WorkerClient(sup);
        var settings = new Settings();
        await using var sched = new EmbeddingScheduler(() => rs.Store, () => settings, client, Idle, () => false, indexDim: () => 4);
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal(0, rs.Pending);
        Assert.Equal("CPU", sup.Hello!.Device);

        // CA-S2: a bench that re-selected the device; status.device reads Hello.
        await client.BenchAsync(CancellationToken.None);
        Assert.Equal("GPU", sup.Hello!.Device);
        Assert.Equal("r2", sup.Hello.Revision);
        Assert.Equal(0, rs.Pending);   // SF4: the store hears of the switch on the scheduler loop, not on the bench call

        // A-M3: the loop moves the store to r2 (both rows embedded by r1 are queued again); a new row's vector comes
        // from r2 and is filed under it.
        model = "fake-model";
        rs.Add(2);
        int before = sched.Processed;
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal(3, sched.Processed - before);   // 2 re-embedded for r2 + the new row
        Assert.Equal(0, rs.Pending);
        Assert.Equal(3, rs.Store.Stats().Vectors);
    }

    /// <summary>SF3 (review): a reply naming an embedder other than the accepted space (here one with no revision,
    /// which raises no hello) is never filed under the accepted space; the job waits.</summary>
    [Fact]
    public async Task Vector_from_another_embedder_is_not_filed_under_the_accepted_space()
    {
        var store = new FakeStore();
        store.AddAudioCapture(new NewAudioCapture(DateTimeOffset.UtcNow, "mic", 1000, "hello world", "en", ""));
        var launcher = new FakeWorkerLauncher();
        launcher.Handler = (req, _) => req.Op == "embed_text"
            ? (new WorkerResponse(req.Id, true, Dim: 4, Ms: 1, Model: "other-model", Revision: "", Device: "CPU"), BinaryFrame.FloatsToBytes([1, 0, 0, 0]))
            : null;
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var settings = new Settings();
        await using var sched = new EmbeddingScheduler(() => store, () => settings, new WorkerClient(sup), Idle, () => false);
        Assert.Equal(TimeSpan.FromSeconds(60), await sched.StepAsync(CancellationToken.None));
        Assert.Equal("space-refused", sched.LastDecision);
        Assert.Empty(store.Vectors);
        Assert.Equal(1, sched.Queued);
        Assert.Equal("fake-model", sup.Hello!.Model);   // N7: an empty revision names no embedder
    }

    // ---------------- A-M1: panic and the store's job leases ----------------

    /// <summary>A-M1 seam: panic clears the scheduler's RAM queue without telling the store. The store's leases on
    /// those jobs now expire, so the rows are handed out again and pending drains, without an agent restart.</summary>
    [Fact]
    public async Task Jobs_cleared_by_panic_are_handed_out_again_after_the_lease()
    {
        using var rs = new RealStore();
        Assert.Equal(TimeSpan.FromMinutes(2), rs.Store.EmbeddingLeaseTimeout);
        rs.Store.EmbeddingLeaseTimeout = TimeSpan.FromMilliseconds(400);
        for (int i = 0; i < 3; i++) rs.Add(i);
        var launcher = new FakeWorkerLauncher();
        await using var sup = new WorkerSupervisor(launcher, rs.Home, () => 120);
        var load = Busy();
        var settings = new Settings();
        await using var sched = new EmbeddingScheduler(() => rs.Store, () => settings, new WorkerClient(sup), () => load, () => false);

        await sched.StepAsync(CancellationToken.None);   // CPU busy: jobs taken into RAM, not sent
        Assert.Equal("cpu", sched.LastDecision);
        Assert.Equal(3, sched.Queued);
        sched.Clear();                                     // panic
        load = Idle();
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal("empty", sched.LastDecision);         // still leased
        Assert.Equal(3, rs.Pending);

        await Task.Delay(600);
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal(0, rs.Pending);
        Assert.Equal(3, rs.Store.Stats().Vectors);
    }

    /// <summary>A-L3 seam: jobs the scheduler holds in RAM are still rows of the store's jobs table, so
    /// status.queue.pending is the store's count alone (it used to add Queued and count them twice).</summary>
    [Fact]
    public async Task Jobs_held_by_the_scheduler_are_already_in_the_store_count()
    {
        using var rs = new RealStore();
        for (int i = 0; i < 3; i++) rs.Add(i);
        await using var sup = new WorkerSupervisor(new FakeWorkerLauncher(), rs.Home, () => 120);
        var settings = new Settings();
        await using var sched = new EmbeddingScheduler(() => rs.Store, () => settings, new WorkerClient(sup), Busy, () => false);
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal(3, sched.Queued);
        Assert.Equal(3, rs.Pending);   // the old status showed 6
    }

    // ---------------- A-L2: purge while the job is in flight ----------------

    /// <summary>A-L2 seam: a purge (Remove) while the row's job is with the worker. A retryable reply used to put the
    /// job (image intact) back at the head of the queue and send the purged image to the worker again.</summary>
    [Fact]
    public async Task Purge_of_the_job_in_flight_does_not_requeue_it()
    {
        var store = new FakeStore();
        var id = store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [1, 2, 3], "image/webp", true));
        var launcher = new FakeWorkerLauncher();
        using var entered = new SemaphoreSlim(0);
        using var gate = new SemaphoreSlim(0);
        int sends = 0;
        launcher.Handler = (req, _) =>
        {
            if (req.Op != "embed_image") return null;
            Interlocked.Increment(ref sends);
            entered.Release();
            gate.Wait(TimeSpan.FromSeconds(10));
            return (new WorkerResponse(req.Id, false, Error: "Internal error."), []);   // transient: would be requeued
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var settings = new Settings();
        await using var sched = new EmbeddingScheduler(() => store, () => settings, new WorkerClient(sup), Idle, () => false);

        var step = sched.StepAsync(CancellationToken.None);
        Assert.True(await entered.WaitAsync(TimeSpan.FromSeconds(10)));
        sched.Remove(new HashSet<string>(StringComparer.Ordinal) { id });   // purge notice for the row in flight
        gate.Release();
        await step;
        Assert.Equal(0, sched.Queued);
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal(1, sends);
        Assert.Empty(store.Failed);

        // N4: panic (Clear) while a job is in flight drops it the same way; the store's lease brings the row back later.
        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [4, 5, 6], "image/webp", true));
        step = sched.StepAsync(CancellationToken.None);
        Assert.True(await entered.WaitAsync(TimeSpan.FromSeconds(10)));
        sched.Clear();
        gate.Release();
        await step;
        Assert.Equal(0, sched.Queued);
        Assert.Equal(2, sends);
    }

    // ---------------- A-L1 + C8: what counts as a crash ----------------

    /// <summary>A-L1 seam: an embed queued behind a bench the user cancels (UI page closed) three times. Each cancel
    /// stops the worker; the embed used to be counted as a crash each time (or failed outright when its write was cut),
    /// and the row was set aside for good.</summary>
    [Fact]
    public async Task Three_cancelled_benches_do_not_set_the_queued_job_aside()
    {
        var store = new FakeStore();
        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, new byte[64 * 1024], "image/webp", true));
        var launcher = new FakeWorkerLauncher();
        using var benchIn = new SemaphoreSlim(0);
        using var benchGate = new SemaphoreSlim(0);
        launcher.Handler = (req, _) =>
        {
            if (req.Op != "bench") return null;
            benchIn.Release();
            benchGate.Wait(TimeSpan.FromSeconds(10));   // the serial worker is busy benching
            return (new WorkerResponse(req.Id, true, Bench: new BenchResult("CPU", [])), []);
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var client = new WorkerClient(sup);
        var settings = new Settings();
        await using var sched = new EmbeddingScheduler(() => store, () => settings, client, Idle, () => false);

        for (int round = 0; round < EmbeddingScheduler.MaxCrashAttempts; round++)
        {
            using var cts = new CancellationTokenSource();
            var bench = client.BenchAsync(cts.Token);
            Assert.True(await benchIn.WaitAsync(TimeSpan.FromSeconds(10)));
            var step = sched.StepAsync(CancellationToken.None);   // the embed waits behind the bench
            await Task.Delay(300);
            cts.Cancel();                                          // Benchmark page closed: the worker is stopped
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => bench);
            await step;
            benchGate.Release();
            Assert.Equal("worker-unavailable", sched.LastDecision);
        }
        Assert.Empty(store.Failed);
        Assert.Equal(1, sched.Queued);
        await sched.StepAsync(CancellationToken.None);
        Assert.Single(store.Vectors);
    }

    /// <summary>C8 seam: liveness runs on a clock that skips sleep. Real time passing while that clock stands still (a
    /// sleep) is no silence; when the clock does move past the allowance the worker is recycled, and only the request it
    /// was serving is blamed (A-L1): the one queued behind is failed as stopped, not as a crash.</summary>
    [Fact]
    public async Task Liveness_runs_on_the_injected_clock_and_blames_only_the_served_request()
    {
        long now = 1_000;
        var launcher = new FakeWorkerLauncher { NoReply = r => r.Op == "embed_text" };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120)
        {
            UnresponsiveAfter = TimeSpan.FromMilliseconds(300),
            LivenessClock = () => Interlocked.Read(ref now),
        };
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        var served = client.EmbedTextAsync("served", "document", CancellationToken.None);
        var behind = client.EmbedTextAsync("behind", "document", CancellationToken.None);
        await Task.Delay(700);       // TickCount64 would call this silence: the PC slept
        sup.CheckNow();
        Assert.Equal(0, sup.Recycled);
        Assert.False(served.IsCompleted);

        Interlocked.Add(ref now, 1_000);   // awake, and the worker really is silent
        sup.CheckNow();
        Assert.Equal(1, sup.Recycled);
        var e1 = await Assert.ThrowsAnyAsync<WorkerException>(() => served);
        var e2 = await Assert.ThrowsAnyAsync<WorkerException>(() => behind);
        Assert.True(EmbeddingScheduler.IsCrash(e1));
        Assert.False(EmbeddingScheduler.IsCrash(e2));
        Assert.Equal(WorkerConnection.StoppedMessage, e2.Message);
    }

    [Fact]
    public async Task Default_liveness_clock_follows_the_tick_count_while_awake()
    {
        long t0 = LivenessClock.NowMs(), k0 = Environment.TickCount64;
        await Task.Delay(300);
        long dt = LivenessClock.NowMs() - t0, dk = Environment.TickCount64 - k0;
        Assert.InRange(dt, 250, 5_000);
        Assert.InRange(dt - dk, -100, 100);
    }

    // ---------------- A-L4: interactive query embeds first ----------------

    /// <summary>A-L4 seam: a search's query embed jumps the background requests waiting for the serial worker. It
    /// still waits for the running op and the frame already being written (neither can be preempted).</summary>
    [Fact]
    public async Task Query_embed_is_written_before_waiting_background_requests()
    {
        var launcher = new FakeWorkerLauncher();
        var order = new ConcurrentQueue<string>();
        using var running = new SemaphoreSlim(0);
        using var gate = new SemaphoreSlim(0);
        launcher.Handler = (req, payload) =>
        {
            if (req.Op == "hello") return null;
            order.Enqueue(req.Op == "embed_image" ? "image" + payload[0] : req.Op + ":" + req.Kind);
            if (req.Op == "transcribe") { running.Release(); gate.Wait(TimeSpan.FromSeconds(10)); }
            return null;
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);

        var conn = await sup.EnsureAsync(CancellationToken.None);

        var t = client.TranscribeAsync(new float[1600], "mic", "auto", CancellationToken.None);
        Assert.True(await running.WaitAsync(TimeSpan.FromSeconds(10)));
        static byte[] Big(byte mark) { var b = new byte[2 << 20]; b[0] = mark; return b; }   // larger than any pipe buffer
        // Each call reaches the write gate before it returns its task; the gate state is the signal, no timed sleeps.
        var i1 = client.EmbedImageAsync(Big(1), "image/webp", null, 1, null, CancellationToken.None);
        await Until(() => conn.WriteGateState is { Held: true, Normal: 0, Urgent: 0 });   // i1's frame is going out
        var i2 = client.EmbedImageAsync(Big(2), "image/webp", null, 1, null, CancellationToken.None);
        await Until(() => conn.WriteGateState.Normal == 1);
        var q = client.EmbedTextAsync("where is the invoice", "query", CancellationToken.None);
        await Until(() => conn.WriteGateState.Urgent == 1);
        gate.Release();
        await Task.WhenAll(t, i1, i2, q);
        Assert.Equal(["transcribe:", "image1", "embed_text:query", "image2"], order.ToArray());
    }

    private static async Task Until(Func<bool> condition)
    {
        var until = Environment.TickCount64 + 20_000;
        while (!condition())
        {
            Assert.True(Environment.TickCount64 < until, "condition not reached");
            await Task.Delay(5);
        }
    }

    /// <summary>MF1 (review): Modern Standby and a frozen process (T's SuspendProbe) stop the agent's timer while the
    /// unbiased clock keeps running. A gap between two checks over 30 s gives a 45 s grace before any "not answering"
    /// verdict, so the thawed worker can answer; a worker still silent after the grace is recycled.</summary>
    [Fact]
    public async Task Liveness_gives_a_grace_after_the_agent_itself_was_frozen()
    {
        long live = 1_000, check = 1_000;
        void Jump(long ms) { Interlocked.Add(ref live, ms); Interlocked.Add(ref check, ms); }
        var launcher = new FakeWorkerLauncher { NoReply = r => r.Op == "embed_text" };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 3600)
        {
            UnresponsiveAfter = TimeSpan.FromSeconds(90),
            LivenessClock = () => Interlocked.Read(ref live),
            CheckClock = () => Interlocked.Read(ref check),
        };
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        var owed = client.EmbedTextAsync("frozen", "document", CancellationToken.None);
        sup.CheckNow();
        Jump(120_000);             // agent and worker frozen 120 s; the unbiased clock ran on
        sup.CheckNow();
        Assert.Equal(0, sup.Recycled);
        Jump(20_000);
        sup.CheckNow();
        Jump(20_000);              // 40 s into the 45 s grace
        sup.CheckNow();
        Assert.Equal(0, sup.Recycled);
        Assert.False(owed.IsCompleted);
        Jump(10_000);              // grace over, still silent: a real hang
        sup.CheckNow();
        Assert.Equal(1, sup.Recycled);
        await Assert.ThrowsAnyAsync<WorkerException>(() => owed);
    }

    /// <summary>SF7 (review): a worker that dies by itself can only have died on the request it was serving; the one
    /// queued behind is not counted as a crash (an innocent audio row used to be set aside after three such crashes).</summary>
    [Fact]
    public async Task Self_crash_blames_only_the_served_request()
    {
        var launcher = new FakeWorkerLauncher { NoReply = r => r.Text == "served", CrashWhen = r => r.Text == "behind" };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        var served = client.EmbedTextAsync("served", "document", CancellationToken.None);
        var behind = client.EmbedTextAsync("behind", "document", CancellationToken.None);
        var e1 = await Assert.ThrowsAnyAsync<WorkerException>(() => served);
        var e2 = await Assert.ThrowsAnyAsync<WorkerException>(() => behind);
        Assert.True(EmbeddingScheduler.IsCrash(e1));
        Assert.False(EmbeddingScheduler.IsCrash(e2));
        Assert.Equal(WorkerConnection.CrashedElsewhereMessage, e2.Message);
    }

    /// <summary>The worker gets exactly these keys (on stdin). Adding one is a review point: nothing secret may go there.</summary>
    [Fact]
    public void Worker_settings_keys_are_pinned()
    {
        Assert.Equal(
        [
            "inference.device", "inference.gpuAdapter", "inference.modelDir", "inference.imageTokens", "inference.idleExitS",
            "inference.fake", "index.dim", "audio.language", "governance.ecoQos", "security.memoryIsolation", "log.level",
            "privacy.nsfwFilter", "privacy.nsfwDetector", "privacy.nsfwDevice", "privacy.nsfwGpuAdapter",
            "ocr.enabled", "ocr.engine", "ocr.languages", "ocr.device",
        ], WorkerSettingsDiff.KeyNames);
    }

    /// <summary>C5 + SF9 seam: the agent's real launcher starts the real worker exe and hands it the settings on stdin,
    /// with a modelDir holding spaces, a quote, non-ASCII and a trailing backslash. The worker logs what it parsed
    /// before it tries the pipe (it then waits for a pipe nobody serves and is killed). Nothing of the settings is on the
    /// command line.</summary>
    [Fact]
    public async Task Agent_launcher_hands_its_settings_to_the_real_worker()
    {
        var root = TempDir();
        var workerExe = Environment.GetEnvironmentVariable("BR_WORKER_DIR") is { } workerDir && !string.IsNullOrWhiteSpace(workerDir)
            ? Path.Combine(workerDir, "BetterRecall.Worker.exe")
            : ResolveWorkerExe();
        static string ResolveWorkerExe()
        {
            var pivot = new DirectoryInfo(AppContext.BaseDirectory.TrimEnd('\\')).Name;
            var artifacts = AppContext.BaseDirectory;
            while (!Directory.Exists(Path.Combine(artifacts, "bin", "BetterRecall.Worker"))) artifacts = Path.GetDirectoryName(artifacts.TrimEnd('\\'))!;
            return Path.Combine(artifacts, "bin", "BetterRecall.Worker", pivot, "BetterRecall.Worker.exe");
        }
        Assert.True(File.Exists(workerExe), workerExe);

        var s = new Settings();
        s.Inference.Fake = true;
        s.Index.Dim = 256;
        s.Inference.ModelDir = Path.Combine(root, "Models dir", "with \"quote\" \u00e9t\u00e9") + "\\";
        var home = Path.Combine(root, "home dir");
        var launcher = new ProcessWorkerLauncher(workerExe, () => false, () => WorkerSettingsDiff.Json(s));
        int pid = launcher.Start(PipeNames.NewWorker(), home);
        string log = "";
        try
        {
            var until = Environment.TickCount64 + 30_000;
            while (Environment.TickCount64 < until)
            {
                var f = Directory.Exists(Path.Combine(home, "logs")) ? Directory.GetFiles(Path.Combine(home, "logs"), "worker-*.log").FirstOrDefault() : null;
                if (f is not null)
                {
                    try { log = File.ReadAllText(f); } catch (IOException) { }
                    if (log.Contains("start settings=")) break;
                }
                await Task.Delay(50);
            }
        }
        finally { launcher.Kill(pid); }
        Assert.Contains("start settings=agent ", log);
        Assert.Contains($"modelDir={Path.GetFullPath(s.Inference.ModelDir)} ", log);
        Assert.Contains(" dim=256 ", log);
        Assert.Contains(" fake=True ", log);
        var args = launcher.LastStartInfo!.ArgumentList;
        Assert.Contains("--settings-stdin", args);
        Assert.DoesNotContain(args, a => a.Contains("Models dir", StringComparison.Ordinal) || a.Contains('{'));
    }

    // ---------------- C10: idle exit disposes the connection ----------------

    /// <summary>C10 seam: the worker exits by itself (idle) with nothing pending. The supervisor used to only forget
    /// the connection; its pipe handle, CTS and lock waited for the finalizer.</summary>
    [Fact]
    public async Task Clean_close_disposes_the_connection()
    {
        var launcher = new FakeWorkerLauncher();
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var conn = await sup.EnsureAsync(CancellationToken.None);
        launcher.Kill(0);   // the fake worker ends its pipe like an idle exit (the supervisor did not ask)
        var until = Environment.TickCount64 + 10_000;
        while (!conn.IsDisposed && Environment.TickCount64 < until) await Task.Delay(20);
        Assert.True(conn.IsClosed);
        Assert.True(conn.IsDisposed);
        Assert.Equal(0, sup.Failures);
        // The next request starts a fresh worker as before.
        var v = await new WorkerClient(sup).EmbedTextAsync("again", "query", CancellationToken.None);
        Assert.Equal([5f, 0, 0, 1], v);
        Assert.Equal(2, launcher.Starts);
    }
}
