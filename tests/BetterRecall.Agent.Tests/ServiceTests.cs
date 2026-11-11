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
    private Task? _serve;
    private CancellationTokenSource? _cts;
    public bool CrashOnNext;

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
            if (req.Op == "shutdown") return;
            var custom = Handler?.Invoke(req, frame.Value.Payload);
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

    public void Kill(int pid) => _cts?.Cancel();
    public bool HasExited(int pid) => _serve is null || _serve.IsCompleted;
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
        var ex = await Assert.ThrowsAsync<WorkerException>(() => new WorkerClient(sup).BenchAsync(CancellationToken.None));
        Assert.Equal("Model not installed: embeddinggemma2.", ex.Message);
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
