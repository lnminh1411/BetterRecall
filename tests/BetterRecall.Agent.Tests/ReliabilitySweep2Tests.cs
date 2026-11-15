// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Audio;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Host;
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Scheduling;
using BetterRecall.Agent.Util;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 2, batch G (agent reliability).</summary>
public class ReliabilitySweep2Tests
{
    private static string TempDir()
    {
        var root = Environment.GetEnvironmentVariable("BR_TEST_TMP") is { Length: > 0 } t ? t : Path.GetTempPath();
        var d = Path.Combine(root, "betterrecall-agent", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(d);
        return d;
    }

    // ---------------- backoff (A7, A13, A14, A15) ----------------

    [Fact]
    public void Backoff_doubles_caps_and_resets()
    {
        var b = new RetryBackoff(1000, 4000);
        Assert.True(b.Due(0));
        Assert.Equal(1000, b.Failed(0));
        Assert.False(b.Due(999));
        Assert.True(b.Due(1000));
        Assert.Equal(2000, b.Failed(1000));
        Assert.Equal(4000, b.Failed(3000));
        Assert.Equal(4000, b.Failed(7000));   // capped
        Assert.Equal(4, b.Failures);
        b.Succeeded();
        Assert.True(b.Due(7001));
        Assert.Equal(1000, b.Failed(7001));
    }

    // ---------------- A7: encoder watchdog ----------------

    [Fact]
    public void Watchdog_reports_an_encoder_error_at_once()
    {
        var w = new EncoderWatchdog(0);
        Assert.Null(w.Check(10, 0, null));
        Assert.Equal("SyncOperation -17", w.Check(20, 0, "SyncOperation -17"));
    }

    [Fact]
    public void Watchdog_reports_a_stall_only_while_frames_go_in()
    {
        var w = new EncoderWatchdog(0, stallMs: 3000);
        long packets = 0;
        // Healthy: a packet comes out every tick.
        for (long t = 0; t < 5000; t += 100) { w.Attempt(t); Assert.Null(w.Check(t, ++packets, null)); }
        // Encoder silent (TDR, ReadyForInput never comes back) while the pipeline keeps trying.
        string? failure = null;
        for (long t = 5000; t < 9000 && failure is null; t += 100) { w.Attempt(t); failure = w.Check(t, packets, null); }
        Assert.NotNull(failure);
        Assert.StartsWith("no output for", failure);
    }

    [Fact]
    public void Watchdog_ignores_a_pause_without_attempts()
    {
        var w = new EncoderWatchdog(0, stallMs: 3000);
        w.Attempt(0);
        Assert.Null(w.Check(0, 1, null));
        // Replay paused for a minute: no ticks, no attempts. The first ticks after it are not a stall.
        w.Attempt(60_000);
        Assert.Null(w.Check(60_000, 1, null));
        w.Attempt(61_000);
        Assert.Null(w.Check(61_000, 2, null));
    }

    // ---------------- D29: pause reasons ----------------

    [Fact]
    public void Setting_the_same_pause_reason_again_does_not_raise_changed()
    {
        var p = new PauseController();
        int changed = 0;
        p.Changed += () => changed++;
        p.Set("storeerror");
        p.Set("storeerror");
        Assert.Equal(1, changed);
        var until = DateTimeOffset.UtcNow.AddMinutes(5);
        p.Set("user", until);
        p.Set("user", until);
        Assert.Equal(2, changed);
        p.Set("user", until.AddMinutes(1));   // a new end time is a change
        Assert.Equal(3, changed);
    }

    [Fact]
    public void Store_error_retry_never_leaves_a_moment_without_a_pause_reason()
    {
        var p = new PauseController();
        p.Set("keylocked");
        var seen = new List<string?>();
        p.Changed += () => seen.Add(p.Current().Reason);
        // First failure, in the order OpenStoreAsync now uses, then two 30 s retries.
        for (int i = 0; i < 3; i++) { p.Set("storeerror"); p.Clear("keylocked"); }
        Assert.DoesNotContain(null, seen);
        Assert.Equal(2, seen.Count);   // one Set and one Clear; the retries change nothing
    }

    // ---------------- A8: change gates ----------------

    [Fact]
    public void Sample_grab_does_not_reset_the_keyframe_gate()
    {
        var kf = new ChangeGate();
        var sample = new ChangeGate();
        kf.Reset(); sample.Reset();         // a keyframe and a sample were just grabbed
        kf.Frame(20); sample.Frame(20);     // the user typed
        Assert.True(sample.Changed(0.5));
        sample.Reset();                     // sample-only grab: only its own gate
        Assert.True(kf.Changed(0.5));       // the keyframe of that change is still due
        Assert.False(sample.Changed(0.5));
        kf.Reset();
        Assert.False(kf.Changed(0.5));
        kf.Invalidate();
        Assert.True(kf.Changed(100));
    }

    // ---------------- A12: monitor selection ----------------

    private static OutputInfo Out(int index, string name) => new(index, name, 1, 0x8086, "gpu", index * 1920, 0, 1920, 1080, 0);

    [Fact]
    public void Monitor_selection_follows_monitor_ids_across_a_hot_plug()
    {
        // Three monitors; the user captures 0 and 1, so the confidential one (C) stays out.
        var before = new[] { Out(0, "A"), Out(1, "B"), Out(2, "C") };
        var (sel, pin) = MonitorSelection.Resolve([0, 1], before, o => o.DeviceName, null);
        Assert.Equal(["A", "B"], sel.Select(o => o.DeviceName));
        Assert.NotNull(pin);
        Assert.Equal(["A", "B"], pin!.Ids);

        // Lid closed: A goes away, C becomes index 1. By index C would be captured now; by id it is not.
        var after = new[] { Out(0, "B"), Out(1, "C") };
        var (sel2, pin2) = MonitorSelection.Resolve([0, 1], after, o => o.DeviceName, pin);
        Assert.Equal(["B"], sel2.Select(o => o.DeviceName));
        Assert.Null(pin2);

        // The user picks again on the settings page (indices of today's enumeration): pinned anew.
        var (sel3, pin3) = MonitorSelection.Resolve([1], after, o => o.DeviceName, pin);
        Assert.Equal(["C"], sel3.Select(o => o.DeviceName));
        Assert.Equal(["C"], pin3!.Ids);
    }

    [Fact]
    public void Monitor_selection_empty_means_all_and_old_values_read_as_indices()
    {
        var outs = new[] { Out(0, "A"), Out(1, "B") };
        var (all, pin) = MonitorSelection.Resolve([], outs, o => o.DeviceName, new MonitorSelection.Pin([0], ["A"]));
        Assert.Equal(2, all.Count);
        Assert.Null(pin);
        // A value written before ids existed (no pin file): read as indices once, then pinned.
        var (sel, pin2) = MonitorSelection.Resolve([1], outs, o => o.DeviceName, null);
        Assert.Equal(["B"], sel.Select(o => o.DeviceName));
        Assert.Equal([1], pin2!.Indices);
    }

    [Fact]
    public void Monitor_pin_round_trips_through_its_file()
    {
        var path = Path.Combine(TempDir(), MonitorSelection.FileName);
        Assert.Null(MonitorSelection.Load(path));
        MonitorSelection.Save(path, new MonitorSelection.Pin([0, 2], [@"\\?\DISPLAY#DEL41A8#5&1&0&UID4352#{e6f07b5f}", @"\\.\DISPLAY3"]));
        var back = MonitorSelection.Load(path)!;
        Assert.Equal([0, 2], back.Indices);
        Assert.Equal(@"\\.\DISPLAY3", back.Ids[1]);
    }

    // ---------------- A13: loopback recovery ----------------

    private sealed class FakeStream : ICaptureStream
    {
        public bool Ended { get; set; }
        public bool Disposed;
        public PcmHandler? Handler;
        public void Dispose() => Disposed = true;
        public void Push() => Handler!(new byte[4], 1, 0, false);
    }

    [Fact]
    public void Loopback_restarts_after_its_thread_ends_and_keeps_subscribers()
    {
        var streams = new List<FakeStream>();
        bool fail = false;
        using var hub = new LoopbackHub(h =>
        {
            if (fail) throw new IOException("audio service down");
            var s = new FakeStream { Handler = h };
            streams.Add(s);
            return s;
        });
        int got = 0;
        PcmHandler sub = (_, _, _, _) => got++;
        hub.Subscribe(sub);
        Assert.True(hub.Running);
        streams[0].Push();
        Assert.Equal(1, got);

        // The audio service restarts: the capture thread ends.
        streams[0].Ended = true;
        fail = true;
        Assert.False(hub.Running);
        Assert.False(hub.CheckHealth(0));        // first restart waits one backoff step
        Assert.True(streams[0].Disposed);
        Assert.False(hub.CheckHealth(1000));     // service still down: next try later
        fail = false;
        Assert.False(hub.CheckHealth(2000));     // backoff (2 s after the failed try) not over yet
        Assert.True(hub.CheckHealth(3000));
        Assert.True(hub.Running);
        Assert.Equal(1, hub.Restarts);
        streams[1].Push();
        Assert.Equal(2, got);                    // same subscriber, new stream
    }

    [Fact]
    public void Loopback_without_subscribers_is_not_restarted()
    {
        int opens = 0;
        using var hub = new LoopbackHub(h => { opens++; return new FakeStream { Handler = h, Ended = true }; });
        Assert.False(hub.CheckHealth(10_000));
        Assert.Equal(0, opens);
    }

    // ---------------- A14: microphone retry trigger ----------------

    [Theory]
    [InlineData(false, false, true, false, false, true)]   // reopen failed: mic wanted, none open -> retry (was never retried)
    [InlineData(false, false, true, true, true, true)]     // lost
    [InlineData(false, false, true, true, false, false)]   // running
    [InlineData(true, false, false, false, false, true)]   // system audio wanted, not open
    [InlineData(false, false, false, false, false, false)] // nothing wanted
    public void Audio_sources_are_retried_while_wanted_and_missing(bool wantSystem, bool systemOpen, bool wantMic, bool micOpen, bool micLost, bool expected)
        => Assert.Equal(expected, AgentHost.AudioNeedsSync(wantSystem, systemOpen, wantMic, micOpen, micLost));

    // ---------------- A19: sample encodes obey the budget ----------------

    private sealed class Frame : IDisposable { public void Dispose() { } }

    [Fact]
    public void Sample_encodes_are_deferred_when_the_budget_is_spent()
    {
        long now = 0;
        // 4 logical processors: 10 s window limit 600 ms (target 480 ms).
        var g = new EncodeGovernor<Frame>(() => now, 4, (_, _) => 10, (_, _) => { }, [100, 60, 30, 200]);
        Assert.True(g.TryAdmitSample());
        g.ChargeSample(200);
        now += 100;
        Assert.True(g.TryAdmitSample());
        g.ChargeSample(200);
        now += 100;
        // 400 ms used, the next 200 ms sample would pass the 480 ms target: skipped.
        Assert.False(g.TryAdmitSample());
        Assert.Equal(1, g.SamplesDeferred);
        now += 10_000;                           // the window moves on
        Assert.True(g.TryAdmitSample());
    }

    [Fact]
    public void Sample_waits_while_a_keyframe_waits_for_budget()
    {
        long now = 0;
        var g = new EncodeGovernor<Frame>(() => now, 4, (_, _) => 10, (_, _) => { }, [500, 500, 500, 5]);
        g.Offer(new Frame());                    // does not fit: queued
        Assert.Equal(1, g.Waiting);
        Assert.False(g.TryAdmitSample());        // keyframes first
    }

    // ---------------- A20: replay ids and partial files ----------------

    [Fact]
    public void Replay_ids_are_unique_within_a_second_and_old_ids_still_parse()
    {
        var home = TempDir();
        var s = new Settings();
        var lib = new ReplayLibrary(home, () => s, null);
        var ts = new DateTimeOffset(2026, 11, 15, 10, 20, 30, 123, TimeSpan.Zero);
        var a = lib.UniqueId(ts);
        var b = lib.UniqueId(ts);
        Assert.Equal("20261115T102030123Z", a);
        Assert.NotEqual(a, b);
        Assert.True(ReplayLibrary.TryParseId(b, out var tb));
        Assert.Equal(ts.UtcDateTime.AddMilliseconds(1), tb);
        Assert.True(ReplayLibrary.TryParseId("20261115T102030Z", out var legacy));   // 0.2.0 clips
        Assert.Equal(new DateTime(2026, 11, 15, 10, 20, 30, DateTimeKind.Utc), legacy);

        // An id already on disk (or its partial file) is skipped too.
        s.Replay.EncryptExports = false;
        Directory.CreateDirectory(lib.PlainDir);
        var ts2 = ts.AddSeconds(5);
        File.WriteAllBytes(Path.Combine(lib.PlainDir, ReplayLibrary.NewId(ts2) + ".mp4.partial"), [1]);
        Assert.NotEqual(ReplayLibrary.NewId(ts2), lib.UniqueId(ts2));
    }

    [Fact]
    public void Partial_replay_files_are_deleted_on_start_and_clips_kept()
    {
        var home = TempDir();
        var s = new Settings();
        var lib = new ReplayLibrary(home, () => s, null);
        Directory.CreateDirectory(lib.EncryptedDir);
        File.WriteAllBytes(Path.Combine(lib.EncryptedDir, "20261115T102030Z.mp4.enc.partial"), [1]);
        File.WriteAllBytes(Path.Combine(lib.EncryptedDir, "20261115T102031123Z.mp4.partial"), [1]);
        File.WriteAllBytes(Path.Combine(lib.EncryptedDir, "20261115T102032Z.mp4"), [1]);
        Assert.Equal(2, lib.DeletePartials());
        Assert.Equal(["20261115T102032Z.mp4"], Directory.GetFiles(lib.EncryptedDir).Select(Path.GetFileName));
        Assert.Single(lib.List());
    }

    // ---------------- A22: guard disposal ----------------

    [Fact]
    public void Guard_tolerates_events_after_dispose_and_double_dispose()
    {
        var g = new GuardrailService(() => new Settings(), () => false, () => [], @"C:\nowhere");
        g.Dispose();
        g.Refresh();
        g.OnForegroundEvent();
        g.Dispose();
    }

    // ---------------- A17, S2, S3: embedding scheduler ----------------

    private static GovernanceInputs Idle() => new(1, 0, false, UserNotificationState.AcceptsNotifications, 0);

    private static void AddShot(FakeStore store) =>
        store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 10, 10, 0, [1, 2, 3], "image/webp", true));

    [Fact]
    public async Task Embedding_space_is_ensured_on_a_new_store_after_purge_all()
    {
        var first = new FakeStore();
        IStore? current = first;
        var settings = new Settings();
        var launcher = new FakeWorkerLauncher();
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        await using var sched = new EmbeddingScheduler(() => current, () => settings, new WorkerClient(sup), Idle, () => false);
        AddShot(first);
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal(new EmbeddingInfo("fake-model", "r1", 4), first.Space);

        // Purge all: Core replaces the store; the worker (and its hello) stays the same.
        var second = new FakeStore();
        current = second;
        AddShot(second);
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal(new EmbeddingInfo("fake-model", "r1", 4), second.Space);
        Assert.Single(second.Vectors);
    }

    [Fact]
    public async Task Hello_before_the_store_opens_is_applied_once_it_opens()
    {
        IStore? current = null;
        var store = new FakeStore();
        var settings = new Settings();
        var launcher = new FakeWorkerLauncher();
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var client = new WorkerClient(sup);
        await using var sched = new EmbeddingScheduler(() => current, () => settings, client, Idle, () => false);
        await client.EmbedTextAsync("prewarm", "query", CancellationToken.None);   // search hotkey: hello, no store yet
        current = store;
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal(new EmbeddingInfo("fake-model", "r1", 4), store.Space);
    }

    [Fact]
    public async Task Device_failures_are_retried_and_settled_only_after_the_attempt_limit()
    {
        var store = new FakeStore();
        var settings = new Settings();
        AddShot(store);
        var launcher = new FakeWorkerLauncher();
        int failuresLeft = 2;
        launcher.Handler = (req, _) => req.Op == "embed_image" && failuresLeft-- > 0
            ? (new WorkerResponse(req.Id, false, Error: "Device failed: nsfw gate failed on every device"), [])
            : null;
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        await using var sched = new EmbeddingScheduler(() => store, () => settings, new WorkerClient(sup), Idle, () => false);

        Assert.True(await sched.StepAsync(CancellationToken.None) > TimeSpan.Zero);
        Assert.Equal("worker-error", sched.LastDecision);
        Assert.Empty(store.Failed);              // not hidden, keyframe not deleted
        Assert.Equal(1, sched.Queued);
        await sched.StepAsync(CancellationToken.None);
        Assert.Empty(store.Failed);
        await sched.StepAsync(CancellationToken.None);   // third try works
        Assert.Single(store.Vectors);
        Assert.Empty(store.Failed);

        // A device that never recovers: settled after MaxTransientAttempts tries.
        AddShot(store);
        failuresLeft = int.MaxValue;
        for (int i = 0; i < EmbeddingScheduler.MaxTransientAttempts; i++) await sched.StepAsync(CancellationToken.None);
        Assert.Equal("Device failed: nsfw gate failed on every device", store.Failed.Single().Value);
        Assert.Equal(0, sched.Queued);
    }

    [Fact]
    public async Task Bad_requests_are_still_settled_at_once()
    {
        var store = new FakeStore();
        var settings = new Settings();
        AddShot(store);
        var launcher = new FakeWorkerLauncher();
        launcher.Handler = (req, _) => req.Op == "embed_image" ? (new WorkerResponse(req.Id, false, Error: "Bad request."), []) : null;
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        await using var sched = new EmbeddingScheduler(() => store, () => settings, new WorkerClient(sup), Idle, () => false);
        await sched.StepAsync(CancellationToken.None);
        Assert.Equal("Bad request.", store.Failed.Single().Value);
    }

    [Fact]
    public async Task A_job_that_keeps_crashing_the_worker_is_set_aside()
    {
        var store = new FakeStore();
        var settings = new Settings();
        AddShot(store);                                  // poison
        store.AddAudioCapture(new NewAudioCapture(DateTimeOffset.UtcNow, "mic", 1000, "hello", "en", ""));
        var launcher = new FakeWorkerLauncher { CrashWhen = req => req.Op == "embed_image" };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        await using var sched = new EmbeddingScheduler(() => store, () => settings, new WorkerClient(sup), Idle, () => false);

        var deadline = DateTime.UtcNow + TimeSpan.FromSeconds(30);
        while (store.Failed.IsEmpty && DateTime.UtcNow < deadline)
        {
            await sched.StepAsync(CancellationToken.None);
            await Task.Delay(200);                       // the supervisor backs off between restarts
        }
        Assert.Equal("worker crashed", store.Failed.Single().Value);
        Assert.Equal(EmbeddingScheduler.MaxCrashAttempts, launcher.Starts);
        // The job behind it moves again.
        while (store.Vectors.IsEmpty && DateTime.UtcNow < deadline)
        {
            await sched.StepAsync(CancellationToken.None);
            await Task.Delay(200);
        }
        Assert.Single(store.Vectors);
    }

    [Theory]
    [InlineData("Device failed: npu lost", true)]
    [InlineData("Internal error.", true)]
    [InlineData("Bad request.", false)]
    [InlineData("Model not installed: ocr.", false)]
    public void Transient_worker_errors(string message, bool transient)
        => Assert.Equal(transient, EmbeddingScheduler.IsTransient(new WorkerReplyException(message)));

    // ---------------- S4: worker-side settings ----------------

    [Fact]
    public void Worker_side_settings_are_detected()
    {
        var a = new Settings();
        Assert.Null(WorkerSettingsDiff.Changed(a, new Settings()));
        var b = new Settings();
        b.Ocr.Languages = "en,vi";
        Assert.Equal("ocr.languages", WorkerSettingsDiff.Changed(a, b));
        var c = new Settings();
        c.Privacy.NsfwDevice = "cpu";
        Assert.Equal("privacy.nsfwDevice", WorkerSettingsDiff.Changed(a, c));
        var d = new Settings();
        d.Index.Dim = 512;
        Assert.Equal("index.dim", WorkerSettingsDiff.Changed(a, d));
        var e = new Settings();
        e.Capture.IntervalMs = 5000;                    // agent-side only: no restart
        e.Privacy.NsfwThreshold = 0.5;                  // sent per request
        Assert.Null(WorkerSettingsDiff.Changed(a, e));
    }

    [Fact]
    public async Task Restart_request_shuts_the_idle_worker_down_and_the_next_request_starts_a_new_one()
    {
        var launcher = new FakeWorkerLauncher();
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var client = new WorkerClient(sup);
        await client.EmbedTextAsync("a", "query", CancellationToken.None);
        Assert.Equal(1, launcher.Starts);
        sup.RequestRestart("ocr.languages");
        var deadline = DateTime.UtcNow + TimeSpan.FromSeconds(10);
        while (sup.IsRunning && DateTime.UtcNow < deadline) await Task.Delay(50);
        Assert.False(sup.IsRunning);
        Assert.False(sup.RestartPending);
        await client.EmbedTextAsync("b", "query", CancellationToken.None);
        Assert.Equal(2, launcher.Starts);
        Assert.Equal(0, sup.Failures);                  // a graceful restart is not a crash
    }

    [Fact]
    public async Task Restart_waits_for_the_request_in_flight()
    {
        var launcher = new FakeWorkerLauncher();
        var release = new TaskCompletionSource();
        launcher.NoReply = req => req.Op == "embed_text" && req.Text == "slow";
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var client = new WorkerClient(sup);
        await client.EmbedTextAsync("a", "query", CancellationToken.None);
        using var cts = new CancellationTokenSource();
        var slow = client.EmbedTextAsync("slow", "query", cts.Token);
        await Task.Delay(200);
        sup.RequestRestart("inference.device");
        Assert.True(sup.RestartPending);
        Assert.True(sup.IsRunning);                     // not cut off mid-request
        cts.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => slow);
        sup.CheckNow();
        var deadline = DateTime.UtcNow + TimeSpan.FromSeconds(10);
        while (sup.IsRunning && DateTime.UtcNow < deadline) await Task.Delay(50);
        Assert.False(sup.IsRunning);
        Assert.False(sup.RestartPending);
    }
}
