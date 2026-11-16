// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.IO.Pipes;
using System.Text.Json;
using BetterRecall.Agent.Audio;
using BetterRecall.Agent.Scheduling;
using BetterRecall.Agent.Thumbs;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Tests;

public class GovernanceTests
{
    private static GovernanceInputs In(double cpu = 10, double? gpu = 5, bool battery = false, UserNotificationState n = UserNotificationState.AcceptsNotifications, long idleMs = 0)
        => new(cpu, gpu, battery, n, idleMs);

    [Fact]
    public void Defers_on_battery_fullscreen_and_load()
    {
        var g = new GovernanceSettings();
        var c = new CaptureSettings();
        Assert.Equal("battery", Governance.Decide(In(battery: true), g, c).Reason);
        Assert.False(Governance.Decide(In(battery: true), g, c).Run);
        Assert.True(Governance.Decide(In(battery: true), new GovernanceSettings { PauseOnBattery = false }, c).Run);
        Assert.Equal("fullscreen", Governance.Decide(In(n: UserNotificationState.FullscreenD3D), g, c).Reason);
        Assert.Equal("fullscreen", Governance.Decide(In(n: UserNotificationState.Presentation), g, c).Reason);
        Assert.True(Governance.Decide(In(n: UserNotificationState.FullscreenD3D), g, new CaptureSettings { PauseOnFullscreen = false }).Run);
        Assert.Equal("cpu", Governance.Decide(In(cpu: 71), g, c).Reason);
        Assert.True(Governance.Decide(In(cpu: 70), g, c).Run);
        Assert.Equal("gpu", Governance.Decide(In(gpu: 61), g, c).Reason);
        Assert.True(Governance.Decide(In(gpu: null), g, c).Run);
    }

    [Fact]
    public void Drains_faster_when_user_idle()
    {
        var busy = Governance.Decide(In(idleMs: 1000), new GovernanceSettings(), new CaptureSettings());
        var idle = Governance.Decide(In(idleMs: 120_000), new GovernanceSettings(), new CaptureSettings());
        Assert.True(idle.BatchSize > busy.BatchSize);
        Assert.True(idle.NextCheck < busy.NextCheck);
        Assert.Equal("idle-drain", idle.Reason);
    }

    [Fact]
    public void Keyframes_pause_in_fullscreen_only_when_enabled()
    {
        Assert.True(Governance.PauseKeyframes(UserNotificationState.FullscreenD3D, new CaptureSettings()));
        Assert.False(Governance.PauseKeyframes(UserNotificationState.Busy, new CaptureSettings()));
        Assert.False(Governance.PauseKeyframes(UserNotificationState.FullscreenD3D, new CaptureSettings { PauseOnFullscreen = false }));
    }
}

/// <summary>Fake worker served in-process over the real private pipe created by the supervisor.</summary>
internal sealed class FakeWorkerLauncher : IWorkerLauncher
{
    public int Starts;
    public string? LastPipe;
    public Func<WorkerRequest, byte[], (WorkerResponse, byte[])?>? Handler;
    /// <summary>Requests the fake reads but never answers (a hung worker).</summary>
    public Func<WorkerRequest, bool>? NoReply;
    /// <summary>Send keepalive frames while Handler runs (the real worker's WorkerLoop does, every 20 s).</summary>
    public TimeSpan? KeepaliveEvery;
    public TimeSpan KeepaliveMax = TimeSpan.FromMinutes(15);
    private Task? _serve;
    private CancellationTokenSource? _cts;
    public bool CrashOnNext;
    public Func<WorkerRequest, bool>? CrashWhen;
    public int Kills;

    public int Start(string pipeName, string home)
    {
        Starts++;
        LastPipe = pipeName;
        _cts = new CancellationTokenSource();
        _serve = Task.Run(() => ServeAsync(pipeName, _cts.Token));
        return Environment.ProcessId;
    }

    private async Task ServeAsync(string pipe, CancellationToken ct)
    {
        using var client = new NamedPipeClientStream(".", pipe, PipeDirection.InOut, PipeOptions.Asynchronous);
        await client.ConnectAsync(5000, ct);
        while (!ct.IsCancellationRequested)
        {
            var frame = await BinaryFrame.ReadAsync(client, ct);
            if (frame is null) return;
            var req = JsonSerializer.Deserialize(frame.Value.Header, ContractsJson.Default.WorkerRequest)!;
            if (CrashOnNext) { CrashOnNext = false; return; } // drop the pipe mid-request
            if (CrashWhen?.Invoke(req) == true) return;        // crash on every such request (poison job)
            if (req.Op == "shutdown") return;
            if (NoReply?.Invoke(req) == true) continue;
            (WorkerResponse, byte[])? custom;
            if (KeepaliveEvery is { } every)
            {
                // Like WorkerLoop: id-0 frames while the handler runs, for at most KeepaliveMax.
                var work = Task.Run(() => Handler?.Invoke(req, frame.Value.Payload));
                var until = Environment.TickCount64 + (long)KeepaliveMax.TotalMilliseconds;
                while (await Task.WhenAny(work, Task.Delay(every, ct)) != work)
                    if (Environment.TickCount64 < until)
                        await BinaryFrame.WriteAsync(client, JsonSerializer.SerializeToUtf8Bytes(new WorkerResponse(0, true), ContractsJson.Default.WorkerResponse), ReadOnlyMemory<byte>.Empty, ct);
                custom = await work;
            }
            else custom = Handler?.Invoke(req, frame.Value.Payload);
            var (resp, payload) = custom ?? Default(req, frame.Value.Payload);
            await BinaryFrame.WriteAsync(client, JsonSerializer.SerializeToUtf8Bytes(resp, ContractsJson.Default.WorkerResponse), payload, ct);
        }
    }

    private static (WorkerResponse, byte[]) Default(WorkerRequest req, byte[] payload) => req.Op switch
    {
        "hello" => (new WorkerResponse(req.Id, true, Hello: new WorkerHello(1, "CPU", "fake", "fake-model", "r1", 4, true, true, [])), []),
        "embed_text" => (new WorkerResponse(req.Id, true, Dim: 4, Ms: 1), BinaryFrame.FloatsToBytes([req.Text!.Length, 0, 0, 1])),
        "embed_image" when payload.Length > 0 && payload[0] == 0xEE && req.Nsfw is not null => (new WorkerResponse(req.Id, true, Dropped: "nsfw"), []),
        "embed_image" => (new WorkerResponse(req.Id, true, Dim: 4, Ms: 2), BinaryFrame.FloatsToBytes([payload.Length, 1, 0, 0])),
        "transcribe" => (new WorkerResponse(req.Id, true, Transcript: new TranscribeResult(true, $"{payload.Length / 4} samples", "en", [], 3)), []),
        _ => (new WorkerResponse(req.Id, false, Error: "Bad request."), []),
    };

    public void Kill(int pid) { Kills++; _cts?.Cancel(); }
    public bool HasExited(int pid) => _serve is null || _serve.IsCompleted;
}

/// <summary>Fake worker with a pid per start that lingers after shutdown, so a late kill can hit the wrong one (A4).</summary>
internal sealed class PidWorkerLauncher : IWorkerLauncher
{
    private int _nextPid = 1000;
    public readonly System.Collections.Concurrent.ConcurrentDictionary<int, CancellationTokenSource> Alive = new();
    public readonly System.Collections.Concurrent.ConcurrentQueue<int> Killed = new();

    public int Start(string pipeName, string home)
    {
        int pid = Interlocked.Increment(ref _nextPid);
        var cts = new CancellationTokenSource();
        Alive[pid] = cts;
        _ = Task.Run(() => Serve(pid, pipeName, cts.Token));
        return pid;
    }

    private async Task Serve(int pid, string pipe, CancellationToken ct)
    {
        try
        {
            using var client = new NamedPipeClientStream(".", pipe, PipeDirection.InOut, PipeOptions.Asynchronous);
            await client.ConnectAsync(5000, ct);
            while (!ct.IsCancellationRequested)
            {
                var frame = await BinaryFrame.ReadAsync(client, ct);
                if (frame is null) return;
                var req = JsonSerializer.Deserialize(frame.Value.Header, ContractsJson.Default.WorkerRequest)!;
                if (req.Op == "shutdown") { await Task.Delay(600, CancellationToken.None); return; }
                var resp = req.Op == "hello"
                    ? new WorkerResponse(req.Id, true, Hello: new WorkerHello(1, "CPU", "fake", "m", "r", 4, true, true, []))
                    : new WorkerResponse(req.Id, true, Dim: 4, Ms: 1);
                byte[] payload = req.Op == "embed_text" ? BinaryFrame.FloatsToBytes([1, 0, 0, 0]) : [];
                await BinaryFrame.WriteAsync(client, JsonSerializer.SerializeToUtf8Bytes(resp, ContractsJson.Default.WorkerResponse), payload, ct);
            }
        }
        catch { }
        finally { Alive.TryRemove(pid, out _); }
    }

    public void Kill(int pid) { Killed.Enqueue(pid); if (Alive.TryRemove(pid, out var c)) c.Cancel(); }
    public bool HasExited(int pid) => !Alive.ContainsKey(pid);
}

public class WorkerProtocolTests
{
    [Fact]
    public async Task Round_trip_over_real_pipe_with_pid_check()
    {
        var launcher = new FakeWorkerLauncher();
        await using var sup = new WorkerSupervisor(launcher, @"Z:\no-such-dir", () => 120);
        var client = new WorkerClient(sup);
        var v = await client.EmbedTextAsync("hello", "query", CancellationToken.None);
        Assert.Equal([5f, 0, 0, 1], v);
        Assert.Equal("fake-model", sup.Hello!.Model);
        Assert.StartsWith("BetterRecall.worker.", launcher.LastPipe);
        Assert.Equal(32, launcher.LastPipe!.Split('.').Last().Length);

        var img = await client.EmbedImageAsync(new byte[1000], "image/webp", 0.8, 3, 0.5, CancellationToken.None);
        Assert.False(img.DroppedNsfw);
        Assert.Equal(1000f, img.Vector![0]);
        var nsfw = await client.EmbedImageAsync([0xEE, 1, 2], "image/webp", 0.8, 3, 0.5, CancellationToken.None);
        Assert.True(nsfw.DroppedNsfw);
        Assert.Null(nsfw.Vector);

        var t = await client.TranscribeAsync(new float[1600], "mic", "auto", CancellationToken.None);
        Assert.Equal("1600 samples", t.Text);
        Assert.Equal(1, launcher.Starts);
    }

    [Fact]
    public async Task Concurrent_requests_match_by_id()
    {
        var launcher = new FakeWorkerLauncher();
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var client = new WorkerClient(sup);
        var tasks = Enumerable.Range(1, 50).Select(i => client.EmbedTextAsync(new string('a', i), "document", CancellationToken.None)).ToArray();
        var results = await Task.WhenAll(tasks);
        for (int i = 0; i < 50; i++) Assert.Equal(i + 1, results[i][0]);
    }

    [Fact]
    public async Task Crash_fails_pending_then_restarts_after_backoff()
    {
        var launcher = new FakeWorkerLauncher();
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        launcher.CrashOnNext = true;
        await Assert.ThrowsAsync<WorkerException>(() => client.EmbedTextAsync("x", "query", CancellationToken.None));
        Assert.Equal(1, sup.Failures);
        // Inside the backoff window the supervisor refuses to start.
        await Assert.ThrowsAsync<WorkerException>(() => client.EmbedTextAsync("x", "query", CancellationToken.None));
        await Task.Delay(1100);
        var v = await client.EmbedTextAsync("abc", "query", CancellationToken.None);
        Assert.Equal(3f, v[0]);
        Assert.Equal(2, launcher.Starts);
        Assert.Equal(0, sup.Failures);
    }

    [Fact]
    public async Task Worker_error_surfaces_message()
    {
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) => req.Op == "bench" ? (new WorkerResponse(req.Id, false, Error: "Model not installed: embeddinggemma2."), []) : null,
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var ex = await Assert.ThrowsAsync<WorkerReplyException>(() => new WorkerClient(sup).BenchAsync(CancellationToken.None));
        Assert.Equal("Model not installed: embeddinggemma2.", ex.Message);
    }

    /// <summary>A2/D28: a caller's short timeout (keyframe OCR) ends only its own wait. The slow hello (first-run device
    /// benchmark) goes on, and the worker it started serves the next request: no kill, no second start, no failure.</summary>
    [Fact]
    public async Task Caller_timeout_does_not_cancel_a_slow_worker_start()
    {
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) => { if (req.Op == "hello") Thread.Sleep(1500); return null; },
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var client = new WorkerClient(sup);
        using (var cts = new CancellationTokenSource(200))
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => client.EmbedTextAsync("abc", "query", cts.Token));
        using (var cts = new CancellationTokenSource(200))
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => client.EmbedTextAsync("abc", "query", cts.Token));
        var v = await client.EmbedTextAsync("abcd", "query", CancellationToken.None);   // joins the start already running
        Assert.Equal(4f, v[0]);
        Assert.Equal(1, launcher.Starts);
        Assert.Equal(0, launcher.Kills);
        Assert.Equal(0, sup.Failures);
    }

    /// <summary>A5 + C-checks-A S1: a worker that reads a request but never answers is recycled once it has served that
    /// request for UnresponsiveAfter with no frame, even when the caller has no timeout (scheduler, transcriber): the old
    /// rule needed 3 caller timeouts, which a serial worker never lets happen. The restart goes through the backoff.</summary>
    [Fact]
    public async Task Hung_worker_is_recycled_with_untimed_callers()
    {
        bool hung = true;
        var launcher = new FakeWorkerLauncher { NoReply = req => hung && req.Op == "embed_text" };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120) { UnresponsiveAfter = TimeSpan.FromMilliseconds(300) };
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        var job = client.EmbedTextAsync("x", "document", CancellationToken.None);
        await Task.Delay(100);
        sup.CheckNow();
        Assert.True(sup.IsRunning);   // 100 ms on it: not yet
        await Task.Delay(300);
        sup.CheckNow();
        await Assert.ThrowsAsync<WorkerException>(() => job.WaitAsync(TimeSpan.FromSeconds(5)));
        Assert.Equal(1, sup.Recycled);
        Assert.Equal(1, launcher.Kills);
        Assert.False(sup.IsRunning);
        Assert.Equal(1, sup.Failures);
        await Assert.ThrowsAsync<WorkerException>(() => client.EmbedTextAsync("x", "query", CancellationToken.None));   // backoff
        hung = false;
        await Task.Delay(1100);
        Assert.Equal(3f, (await client.EmbedTextAsync("abc", "query", CancellationToken.None))[0]);
        Assert.Equal(2, launcher.Starts);
        Assert.Equal(0, sup.Failures);
    }

    /// <summary>A request the caller gave up on is still being served by the worker: it keeps counting.</summary>
    [Fact]
    public async Task Hung_worker_is_recycled_after_caller_timeouts()
    {
        var launcher = new FakeWorkerLauncher { NoReply = req => req.Op == "embed_text" };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120) { UnresponsiveAfter = TimeSpan.FromMilliseconds(300) };
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        using (var cts = new CancellationTokenSource(50))
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => client.EmbedTextAsync("x", "query", cts.Token));
        await Task.Delay(350);
        sup.CheckNow();
        await Task.Delay(100);
        Assert.Equal(1, sup.Recycled);
        Assert.False(sup.IsRunning);
    }

    /// <summary>A4: ShutdownAsync takes the pid with the connection. The worker started while it waits for the old one to
    /// exit is not killed.</summary>
    [Fact]
    public async Task Shutdown_does_not_kill_the_worker_started_after_it()
    {
        var l = new PidWorkerLauncher();
        await using var sup = new WorkerSupervisor(l, "x", () => 120) { VerifyClientPid = false };
        var client = new WorkerClient(sup);
        await client.EmbedTextAsync("a", "query", CancellationToken.None);
        int first = sup.Pid;
        var shut = sup.ShutdownAsync();
        await Task.Delay(100);
        await client.EmbedTextAsync("b", "query", CancellationToken.None);
        int second = sup.Pid;
        await shut;
        Assert.NotEqual(first, second);
        Assert.DoesNotContain(second, l.Killed);
        Assert.True(l.Alive.ContainsKey(second));
        Assert.True(sup.IsRunning);
    }

    /// <summary>Cancelled requests whose replies still arrive (a healthy worker, impatient callers) are not a hang.</summary>
    [Fact]
    public async Task Late_replies_keep_a_slow_worker_alive()
    {
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) => { if (req.Op == "embed_text") Thread.Sleep(150); return null; },
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120) { UnresponsiveAfter = TimeSpan.FromMilliseconds(100) };
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        for (int i = 0; i < 3; i++)
        {
            using var cts = new CancellationTokenSource(30);
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => client.EmbedTextAsync("x", "query", cts.Token));
            await Task.Delay(300);   // the reply comes back late
            sup.CheckNow();
            Assert.True(sup.IsRunning);
        }
        Assert.Equal(0, sup.Recycled);
        Assert.Equal(1, launcher.Starts);
    }

    [Theory]
    [InlineData("embed_image", 90_000, true)]
    [InlineData("embed_text", 89_000, false)]   // not long enough
    [InlineData("transcribe", 90_000, true)]    // long ones send keepalives, which restart the silence
    [InlineData("bench", 3_600_000, false)]     // bench and hello are never judged
    [InlineData("hello", 3_600_000, false)]
    public void Unresponsive_rule(string op, long servingMs, bool expected)
        => Assert.Equal(expected, WorkerSupervisor.IsUnresponsive(op, servingMs, 90_000));

    /// <summary>A long transcribe (cold Whisper compile) sends keepalives: it and an embed queued behind it wait longer
    /// than UnresponsiveAfter without the worker being recycled.</summary>
    [Fact]
    public async Task Slow_transcribe_with_requests_queued_behind_it_is_not_a_hang()
    {
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) => { if (req.Op == "transcribe") Thread.Sleep(1500); return null; },
            KeepaliveEvery = TimeSpan.FromMilliseconds(100),
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120) { UnresponsiveAfter = TimeSpan.FromMilliseconds(300) };
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        var t = client.TranscribeAsync(new float[1600], "mic", "auto", CancellationToken.None);
        await Task.Delay(100);
        var e = client.EmbedTextAsync("abc", "document", CancellationToken.None);
        for (int i = 0; i < 10; i++) { await Task.Delay(100); sup.CheckNow(); }
        Assert.True(sup.IsRunning);
        Assert.Equal("1600 samples", (await t).Text);
        Assert.Equal(3f, (await e)[0]);
        Assert.Equal(0, sup.Recycled);
    }

    /// <summary>K review S1: an embed that re-benches the device inline (after 3 device failures) or compiles an NPU OCR
    /// graph runs past UnresponsiveAfter. Its keepalives keep it alive: no kill and restart loop.</summary>
    [Fact]
    public async Task Long_embed_with_keepalives_is_not_recycled()
    {
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) => { if (req.Op == "embed_image") Thread.Sleep(1200); return null; },
            KeepaliveEvery = TimeSpan.FromMilliseconds(100),
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120) { UnresponsiveAfter = TimeSpan.FromMilliseconds(300) };
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        var job = client.EmbedImageAsync([1, 2, 3], "image/webp", null, 1, null, CancellationToken.None);
        for (int i = 0; i < 10; i++) { await Task.Delay(100); sup.CheckNow(); }
        Assert.NotNull(await job);
        Assert.Equal(0, sup.Recycled);
        Assert.Equal(1, launcher.Starts);
    }

    /// <summary>Keepalives end after their cap (a handler stuck in native code keeps its timer thread alive). A request
    /// the caller gave up on stays owed through the keepalives, so the worker is recycled once they stop.</summary>
    [Fact]
    public async Task Abandoned_request_is_recycled_once_keepalives_stop()
    {
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) => { if (req.Op == "embed_text") Thread.Sleep(5000); return null; },
            KeepaliveEvery = TimeSpan.FromMilliseconds(100),
            KeepaliveMax = TimeSpan.FromMilliseconds(400),
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120) { UnresponsiveAfter = TimeSpan.FromMilliseconds(300) };
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        using (var cts = new CancellationTokenSource(50))
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => client.EmbedTextAsync("x", "query", cts.Token));
        await Task.Delay(250);
        sup.CheckNow();
        Assert.True(sup.IsRunning);   // keepalives still coming
        await Task.Delay(900);        // they stopped at 400 ms; 300 ms of silence after that
        sup.CheckNow();
        await Task.Delay(100);
        Assert.Equal(1, sup.Recycled);
    }

    /// <summary>K review N1: a caller that gives up while its frame is still being written (the worker is busy, the pipe
    /// has no buffer) leaves the request owed: the worker reads and serves it later. A hang on it is still caught.</summary>
    [Fact]
    public async Task Request_cancelled_mid_write_stays_owed()
    {
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) => { if (req.Op == "embed_text" && req.Text == "busy") Thread.Sleep(400); return null; },
            NoReply = req => req.Op == "transcribe",
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120) { UnresponsiveAfter = TimeSpan.FromMilliseconds(700) };
        var client = new WorkerClient(sup);
        await client.PrewarmAsync(CancellationToken.None);
        var busy = client.EmbedTextAsync("busy", "document", CancellationToken.None);
        await Task.Delay(50);
        using (var cts = new CancellationTokenSource(100))   // still in the write when this fires
            await Assert.ThrowsAnyAsync<OperationCanceledException>(() => client.TranscribeAsync(new float[16], "mic", "auto", cts.Token));
        Assert.Equal(4f, (await busy)[0]);
        await Task.Delay(300);
        sup.CheckNow();
        Assert.True(sup.IsRunning);   // the transcribe is owed, but the worker answered 300 ms ago
        await Task.Delay(700);
        sup.CheckNow();
        await Task.Delay(100);
        Assert.Equal(1, sup.Recycled);
    }

    /// <summary>T2 (sweep 3 verification): the never-checked sentinel must not overflow into "fresh", or the probe never runs.</summary>
    [Fact]
    public void Vc_runtime_cache_is_stale_before_the_first_check()
    {
        long now = Environment.TickCount64;
        Assert.False(VcRuntime.Fresh(long.MinValue, now));
        Assert.True(VcRuntime.Fresh(now - 1_000, now));
        Assert.False(VcRuntime.Fresh(now - 61_000, now));
    }

    /// <summary>T2 + K review N2: the runtime probe finds DLLs in the worker folder or System32 only, never on PATH.</summary>
    [Fact]
    public void Vc_runtime_probe_reports_missing_dlls()
    {
        var root = Environment.GetEnvironmentVariable("BR_TEST_TMP") is { Length: > 0 } t ? t : Path.GetTempPath();
        var worker = Directory.CreateDirectory(Path.Combine(root, "betterrecall-agent", Guid.NewGuid().ToString("N"), "worker")).FullName;
        var onPath = Directory.CreateDirectory(Path.Combine(worker, "..", "tools")).FullName;
        File.WriteAllBytes(Path.Combine(worker, "br-app-local-140.dll"), [0]);
        File.WriteAllBytes(Path.Combine(onPath, "br-on-path-140.dll"), [0]);
        var sys = Environment.SystemDirectory;
        try
        {
            Assert.True(VcRuntime.Probe(worker, sys, ["kernel32.dll"]));                // System32
            Assert.True(VcRuntime.Probe(worker, sys, ["br-app-local-140.dll"]));        // app-local copy
            Assert.False(VcRuntime.Probe(worker, sys, ["br-on-path-140.dll"]));         // another folder (PATH) does not count
            Assert.False(VcRuntime.Probe(worker, sys, ["kernel32.dll", "br-no-such-runtime-140.dll"]));
            Assert.False(VcRuntime.Probe(null, sys, ["br-app-local-140.dll"]));
        }
        finally
        {
            Directory.Delete(Path.GetDirectoryName(worker)!, true);
        }
    }

    [Fact]
    public void Pipe_security_denies_network_and_is_protected()
    {
        var sec = WorkerSupervisor.CreatePipeSecurity();
        var sddl = sec.GetSecurityDescriptorSddlForm(System.Security.AccessControl.AccessControlSections.Access);
        Assert.Contains("D:P", sddl);
        Assert.Contains("(D;", sddl);
        Assert.Contains(";NU)", sddl);
    }
}

public class SchedulerTests
{
    [Fact]
    public async Task Embeds_drops_nsfw_and_text_under_governance()
    {
        var store = new FakeStore();
        var settings = new Settings();
        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [1, 2, 3], "image/webp", true));
        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [0xEE, 2, 3], "image/webp", true));
        store.AddAudioCapture(new NewAudioCapture(DateTimeOffset.UtcNow, "mic", 1000, "hello world", "en", ""));
        var launcher = new FakeWorkerLauncher();
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var load = new GovernanceInputs(90, 0, false, UserNotificationState.AcceptsNotifications, 0);
        await using var sched = new EmbeddingScheduler(() => store, () => settings, new WorkerClient(sup), () => load, () => false);

        // CPU above the threshold: jobs are taken into RAM but nothing is sent.
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal("cpu", sched.LastDecision);
        Assert.Equal(0, launcher.Starts);
        Assert.Equal(3, sched.Queued);

        load = load with { CpuPercent = 5 };
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal(2, store.Vectors.Count);
        Assert.Equal("nsfw", store.Dropped.Single().Value);
        Assert.Equal(new EmbeddingInfo("fake-model", "r1", 4), store.Space);
        Assert.Equal(11f, store.Vectors.Single(kv => kv.Key.StartsWith('a')).Value[0]); // text length
    }

    [Fact]
    public async Task Rejected_jobs_settle_and_missing_models_wait_with_the_image_intact()
    {
        var store = new FakeStore();
        var settings = new Settings();
        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [1, 2, 3], "image/webp", true));
        var launcher = new FakeWorkerLauncher();
        string? error = "Model not installed: nsfw-detector.";
        byte[]? sent = null;
        launcher.Handler = (req, payload) =>
        {
            if (req.Op != "embed_image") return null;
            sent = payload.ToArray();
            return error is null ? null : (new WorkerResponse(req.Id, false, Error: error), []);
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        await using var sched = new EmbeddingScheduler(() => store, () => settings, new WorkerClient(sup),
            () => new GovernanceInputs(1, 0, false, UserNotificationState.AcceptsNotifications, 0), () => false);

        // Missing model: the job waits (10 min) and keeps its image; it used to be zeroed and retried every 15 s.
        Assert.Equal(TimeSpan.FromMinutes(10), await sched.StepAsync(CancellationToken.None));
        Assert.Equal("model-missing", sched.LastDecision);
        Assert.Equal(1, sched.Queued);
        Assert.Empty(store.Failed);

        // Next attempt sends the original bytes, the worker rejects them: the job is settled, not retried forever.
        error = "Bad image.";
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal([1, 2, 3], sent);
        Assert.Equal("Bad image.", store.Failed.Single().Value);
        Assert.Equal(0, sched.Queued);
    }

    [Fact]
    public async Task Nsfw_threshold_not_sent_when_filter_off()
    {
        var store = new FakeStore();
        var settings = new Settings();
        settings.Privacy.NsfwFilter = false;
        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [0xEE], "image/webp", true));
        var launcher = new FakeWorkerLauncher();
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        await using var sched = new EmbeddingScheduler(() => store, () => settings, new WorkerClient(sup),
            () => new GovernanceInputs(1, 0, false, UserNotificationState.AcceptsNotifications, 0), () => false);
        await sched.StepAsync(CancellationToken.None);
        Assert.Empty(store.Dropped);
        Assert.Single(store.Vectors);
    }

    [Theory]
    [InlineData(true, true, 0.8, 0.6)]
    [InlineData(true, false, 0.8, null)]
    [InlineData(false, true, null, null)]
    public async Task Nsfw_detector_threshold_sent_only_when_filter_and_detector_on(bool filter, bool detector, double? nsfw, double? expected)
    {
        var store = new FakeStore();
        var settings = new Settings();
        settings.Privacy.NsfwFilter = filter;
        settings.Privacy.NsfwDetector = detector;
        settings.Privacy.NsfwDetectorThreshold = 0.6;
        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [1, 2], "image/webp", true));
        var launcher = new FakeWorkerLauncher();
        WorkerRequest? seen = null;
        launcher.Handler = (req, _) => { if (req.Op == "embed_image") seen = req; return null; };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        await using var sched = new EmbeddingScheduler(() => store, () => settings, new WorkerClient(sup),
            () => new GovernanceInputs(1, 0, false, UserNotificationState.AcceptsNotifications, 0), () => false);
        await sched.StepAsync(CancellationToken.None);
        Assert.NotNull(seen);
        Assert.Equal(nsfw, seen!.Nsfw);
        Assert.Equal(expected, seen.NsfwDetector);
        Assert.Equal(1, seen.NsfwGrid);
    }
}

public class ThumbnailCacheTests
{
    private static ThumbData T(string id, int size) => new(id, "image/webp", 1, 1, new byte[size]);

    [Fact]
    public void Lru_is_bounded_by_bytes_and_evicts_least_recent()
    {
        var c = new ByteLruCache<string>(1000);
        c.Put("a", T("a", 400));
        c.Put("b", T("b", 400));
        Assert.True(c.TryGet("a", out _)); // a is now most recent
        c.Put("c", T("c", 400));           // evicts b
        Assert.False(c.TryGet("b", out _));
        Assert.True(c.TryGet("a", out _));
        Assert.True(c.TryGet("c", out _));
        Assert.True(c.Bytes <= 1000);
        c.Put("huge", T("huge", 5000));    // larger than the cache: not stored
        Assert.False(c.TryGet("huge", out _));
        c.Resize(500);
        Assert.Equal(1, c.Count);
        Assert.True(c.Bytes <= 500);
    }

    [Fact]
    public void Clear_zeroes_cached_bytes()
    {
        var c = new ByteLruCache<string>(1000);
        var t = new ThumbData("a", "image/webp", 1, 1, [1, 2, 3]);
        c.Put("a", t);
        c.Clear();
        Assert.Equal(0, c.Count);
        Assert.All(t.Bytes, b => Assert.Equal(0, b));
    }

    [Fact]
    public async Task Thumbnailer_resizes_to_webp_and_caches_in_ram()
    {
        var store = new FakeStore();
        var settings = new Settings();
        settings.Keyframes.ThumbCacheMb = 8;
        byte[] webp;
        unsafe
        {
            var px = new byte[640 * 360 * 4];
            for (int i = 0; i < px.Length; i += 4) { px[i] = (byte)(i % 251); px[i + 1] = 80; px[i + 2] = 160; px[i + 3] = 255; }
            fixed (byte* p = px) webp = Capture.ImageCodec.EncodeBgra(p, 640, 360, 640 * 4, "webp", 75);
        }
        var id = store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 640, 360, 0, webp, "image/webp", true));
        var thumbs = new Thumbnailer(() => store, () => settings);
        var t = await thumbs.GetThumbAsync(id, 320, CancellationToken.None);
        Assert.NotNull(t);
        Assert.Equal(320, t!.Width);
        Assert.Equal(180, t.Height);
        Assert.Equal("RIFF", System.Text.Encoding.ASCII.GetString(t.Bytes, 0, 4));
        Assert.Same(t, await thumbs.GetThumbAsync(id, 320, CancellationToken.None));
        Assert.Equal(1, thumbs.Cache.Count);
        Assert.Null(await thumbs.GetThumbAsync("missing", 320, CancellationToken.None));
        thumbs.ClearCache();
        Assert.Equal(0, thumbs.Cache.Count);
    }
}

public class AudioLogicTests
{
    private static float[] Tone(int samples, float amp) =>
        Enumerable.Range(0, samples).Select(i => amp * MathF.Sin(i * 2 * MathF.PI * 440 / 16000)).ToArray();

    [Fact]
    public void Segmenter_emits_speech_and_ignores_silence()
    {
        var seg = new SpeechSegmenter(() => 30);
        var outp = new List<SpeechSegment>();
        long t = 0;
        void Feed(float[] s) { seg.Push(s, t, outp); t += s.Length * 10_000_000L / 16000; }
        Feed(new float[16000 * 2]);           // 2 s silence
        Feed(Tone(16000 * 2, 0.3f));          // 2 s "speech"
        Feed(new float[16000]);               // 1 s silence ends it
        Assert.Single(outp);
        var s0 = outp[0];
        Assert.InRange(s0.DurationMs, 2000, 2700);
        Assert.InRange(s0.StartQpc100ns, 18_000_000, 20_000_000); // ~1.8 s with pre-roll
        Feed(new float[16000 * 5]);
        Assert.Single(outp);
    }

    [Fact]
    public void Segmenter_cuts_at_max_length()
    {
        var seg = new SpeechSegmenter(() => 5);
        var outp = new List<SpeechSegment>();
        seg.Push(Tone(16000 * 12, 0.3f), 0, outp);
        Assert.Equal(2, outp.Count);
        Assert.All(outp, s => Assert.True(s.DurationMs <= 5000));
        seg.Flush(outp);
        Assert.Equal(3, outp.Count);
    }

    [Fact]
    public void Resampler_48k_stereo_int16_to_16k_mono()
    {
        var r = new Resampler(new PcmFormat(48000, 2, false, 16));
        var pcm = new byte[4800 * 4];
        for (int i = 0; i < 4800; i++)
        {
            short v = (short)(10000 * Math.Sin(i * 2 * Math.PI * 440 / 48000));
            BitConverter.TryWriteBytes(pcm.AsSpan(i * 4), v);
            BitConverter.TryWriteBytes(pcm.AsSpan(i * 4 + 2), v);
        }
        var output = new List<float>();
        r.Process(pcm.AsSpan(0, 2401 * 4), 2401, false, output); // odd split exercises the carry-over
        r.Process(pcm.AsSpan(2401 * 4), 4800 - 2401, false, output);
        Assert.Equal(1600, output.Count);
        Assert.InRange(output.Max(), 0.25f, 0.32f);
        var silent = new List<float>();
        r.Process(pcm, 4800, true, silent);
        Assert.All(silent, v => Assert.Equal(0f, v));
    }

    [Fact]
    public void Replay_audio_gate_skips_only_near_silence()
    {
        var quiet = new short[] { 0, 12, -40, 64, -64, 3 };
        var loud = new short[] { 0, 12, -40, 65, 3 };
        Assert.True(AudioReplayEncoder.IsNearSilent16(System.Runtime.InteropServices.MemoryMarshal.AsBytes(quiet.AsSpan())));
        Assert.False(AudioReplayEncoder.IsNearSilent16(System.Runtime.InteropServices.MemoryMarshal.AsBytes(loud.AsSpan())));
        Assert.True(AudioReplayEncoder.IsNearSilent16(ReadOnlySpan<byte>.Empty));
    }
}
