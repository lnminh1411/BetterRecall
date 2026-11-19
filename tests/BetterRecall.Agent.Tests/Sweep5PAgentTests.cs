// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Reflection;
using BetterRecall.Agent.Audio;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Host;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Privacy;
using BetterRecall.Agent.Thumbs;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;
using BetterRecall.Core;
using BetterRecall.Core.Ai;
using Microsoft.Data.Sqlite;
using SkiaSharp;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 5, batch P: capture/mask timestamps (A1), thumbnail cache vs describe redactor (A2),
/// audio pause/lock flush (A4), and WASAPI start timeout safety (C7).</summary>
public sealed class Sweep5PAgentTests : IDisposable
{
    private readonly string _home;

    public Sweep5PAgentTests()
    {
        var root = Environment.GetEnvironmentVariable("BR_TEST_TMP") is { Length: > 0 } t ? t : Path.GetTempPath();
        _home = Path.Combine(root, "br-s5p-" + Guid.NewGuid().ToString("N")[..12]);
        Directory.CreateDirectory(_home);
        File.WriteAllText(DataPaths.SettingsFile(_home), "{\"security\":{\"keyProtection\":\"dpapi\"}}");
        if (!BetterRecall.Core.Log.Initialized) BetterRecall.Core.Log.Init(Path.Combine(root, "br-s5p-logs"), "core");
    }

    public void Dispose()
    {
        SqliteConnection.ClearAllPools();
        for (int i = 0; i < 5; i++)
        {
            try { if (Directory.Exists(_home)) Directory.Delete(_home, true); return; }
            catch (IOException) { Thread.Sleep(100); }
            catch (UnauthorizedAccessException) { Thread.Sleep(100); }
        }
    }

    private static async Task Until(Func<bool> done, int ms = 10_000)
    {
        long end = Environment.TickCount64 + ms;
        while (!done() && Environment.TickCount64 < end) await Task.Delay(20);
    }

    private static object? GetField(object o, string name) =>
        o.GetType().GetField(name, BindingFlags.NonPublic | BindingFlags.Instance)!.GetValue(o);

    private static void SetField(object o, string name, object? value) =>
        o.GetType().GetField(name, BindingFlags.NonPublic | BindingFlags.Instance)!.SetValue(o, value);

    // ---------------- A1: worker start inside KeyframePipeline.Read must not out-stamp the triggering grab ----------------

    private sealed class SwitchReader : IKeyframeReader
    {
        public volatile Func<OcrResult> Answer = () => new OcrResult([], "fake", 1);
        public Func<CancellationToken, Task<int>>? Ready;

        public Task<OcrResult> ReadAsync(ReadOnlyMemory<byte> bgra, int width, int height, int stride, bool entities, CancellationToken ct)
            => Task.FromResult(Answer());

        public Task<int> ReadyAsync(CancellationToken ct) => Ready?.Invoke(ct) ?? Task.FromResult(0);
    }

    private static unsafe KeyframeGrab Grab(int w, int h, byte fill = 255, int monitor = 0, DateTimeOffset? ts = null)
    {
        var req = new KeyframeRequest(ts ?? DateTimeOffset.UtcNow, "app", @"C:\app.exe", "Title", "ctx", false, w, 0, "",
            Keyframe: true, ReplaySample: false, Bounds: new RECT(0, 0, w, h));
        var g = KeyframeGrab.Alloc(monitor, req, w, h, 0);
        var px = new Span<byte>(g.Pixels, w * h * 4);
        px.Fill(255);
        px[..(px.Length / 2)].Fill(fill);
        return g;
    }

    [Fact]
    public async Task A1_cold_or_restarted_worker_in_Read_clears_masks_unchecked_after_ocr_succeeds()
    {
        var s = new Settings();
        s.Keyframes.Format = "webp";
        var store = new FakeStore();
        var readyEntered = new ManualResetEventSlim();
        var readyGate = new TaskCompletionSource<int>(TaskCreationOptions.RunContinuationsAsynchronously);
        bool stallNext = false;
        var reader = new SwitchReader
        {
            Ready = ct =>
            {
                if (!stallNext) return Task.FromResult(0);
                readyEntered.Set();
                return readyGate.Task.WaitAsync(ct);
            },
        };
        using var guard = new GuardrailService(() => s, () => false, () => [], @"C:\nowhere\");
        await using var pipe = new KeyframePipeline(() => store, () => s, _ => { }, reader, new RedactionMasks());
        await using var cs = new CaptureService(() => s, () => false, () => false, guard, pipe, null!, false, new RedactionMasks());

        // Cold start where ReadyAsync is incomplete and req.Ts is strictly earlier than Read().
        pipe.TrackMonitors([0]);
        stallNext = true;
        pipe.Post(Grab(64, 32, fill: 10, ts: DateTimeOffset.UtcNow.AddSeconds(-2)));
        Assert.True(readyEntered.Wait(5000));
        await Until(() => pipe.MasksUnchecked);
        Assert.True(pipe.MasksUnchecked);
        SetField(pipe, "_anyUncheckedAtMs", Environment.TickCount64 - CaptureService.WarnAfterMs);
        Assert.True(cs.ReplayTextUnchecked());

        readyGate.SetResult(0);
        await Until(() => Interlocked.Read(ref pipe.OcrRead) == 1);
        Assert.False(pipe.MasksUnchecked);
        Assert.False(cs.ReplayTextUnchecked());

        // Restarted worker after idle exit: must also clear once OCR succeeds for that grab.
        readyEntered.Reset();
        readyGate = new TaskCompletionSource<int>(TaskCreationOptions.RunContinuationsAsynchronously);
        pipe.Post(Grab(64, 32, fill: 40, ts: DateTimeOffset.UtcNow.AddSeconds(-2)));
        Assert.True(readyEntered.Wait(5000));
        await Until(() => pipe.MasksUnchecked);
        Assert.True(pipe.MasksUnchecked);
        SetField(pipe, "_anyUncheckedAtMs", Environment.TickCount64 - CaptureService.WarnAfterMs);
        Assert.True(cs.ReplayTextUnchecked());

        readyGate.SetResult(0);
        await Until(() => Interlocked.Read(ref pipe.OcrRead) == 2);
        Assert.False(pipe.MasksUnchecked);
        Assert.False(cs.ReplayTextUnchecked());
    }

    [Fact]
    public async Task A1_static_second_monitor_stays_checked_and_queued_monitor_clears_after_worker_starts()
    {
        var s = new Settings();
        s.Keyframes.Format = "webp";
        var readyEntered = new ManualResetEventSlim();
        var readyGate = new TaskCompletionSource<int>(TaskCreationOptions.RunContinuationsAsynchronously);
        bool stallNext = false;
        var reader = new SwitchReader
        {
            Ready = ct =>
            {
                if (!stallNext) return Task.FromResult(0);
                readyEntered.Set();
                return readyGate.Task.WaitAsync(ct);
            },
        };
        await using var pipe = new KeyframePipeline(() => new FakeStore(), () => s, _ => { }, reader, new RedactionMasks());
        pipe.TrackMonitors([0, 1]);
        pipe.Post(Grab(64, 32, fill: 10, monitor: 0));
        pipe.Post(Grab(64, 32, fill: 10, monitor: 1));
        await Until(() => !pipe.MasksUnchecked);

        // Monitor 0 triggers worker start while monitor 1 stays static: monitor 1 remains checked.
        stallNext = true;
        pipe.Post(Grab(64, 32, fill: 30, monitor: 0));
        Assert.True(readyEntered.Wait(5000));
        await Until(() => pipe.MonitorUnchecked(0));
        Assert.True(pipe.MonitorUnchecked(0));
        Assert.False(pipe.MonitorUnchecked(1));

        // Monitor 1 now grabs while worker startup is still blocking: it becomes unchecked until read, then clears.
        pipe.Post(Grab(64, 32, fill: 50, monitor: 1));
        Assert.True(pipe.MonitorUnchecked(1));

        readyGate.SetResult(0);
        await Until(() => Interlocked.Read(ref pipe.OcrRead) == 4);
        Assert.False(pipe.MasksUnchecked);
    }

    // ---------------- A2: DescribeRedactor owns its outgoing bytes across Thumbnailer.ClearCache ----------------

    private static OutgoingImage WhiteWebp(int w = 320, int h = 180)
    {
        using var bmp = new SKBitmap(new SKImageInfo(w, h, SKColorType.Bgra8888, SKAlphaType.Premul));
        bmp.Erase(SKColors.White);
        using var data = bmp.Encode(SKEncodedImageFormat.Webp, 100);
        return new OutgoingImage(data.ToArray(), "image/webp", w, h, false);
    }

    [Fact]
    public async Task A2_describe_redactor_preserves_bytes_when_thumbnail_cache_clears()
    {
        var store = new FakeStore();
        var webp = WhiteWebp(640, 360);
        string id = store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 640, 360, 0, webp.Bytes, "image/webp", true));
        var thumbs = new Thumbnailer(() => store, () => new Settings());
        var t = await thumbs.GetThumbAsync(id, 320, CancellationToken.None);
        Assert.NotNull(t);
        var original = (byte[])t!.Bytes.Clone();

        var cleanReader = new SwitchReader { Answer = () => new OcrResult([new OcrLine("Hello world", 10, 10, 66, 20, 0.99f, [.. Enumerable.Range(0, 12).Select(i => 10 + i * 6)])], "fake", 1) };
        var clean = await new DescribeRedactor(cleanReader, () => new Settings(), () => store)
            .RedactAsync(id, new OutgoingImage(t.Bytes, t.Mime, t.Width, t.Height, false), CancellationToken.None);
        Assert.True(clean.Redacted);

        var noDetectors = new Settings();
        noDetectors.Privacy.RedactPasswords = noDetectors.Privacy.RedactCards = noDetectors.Privacy.RedactIds = noDetectors.Privacy.RedactBirthDates = false;
        var uncheckedOut = await new DescribeRedactor(cleanReader, () => noDetectors, () => store)
            .RedactAsync(id, new OutgoingImage(t.Bytes, t.Mime, t.Width, t.Height, false), CancellationToken.None);

        var ocrOff = new Settings();
        ocrOff.Ocr.Enabled = false;
        var offOut = await new DescribeRedactor(cleanReader, () => ocrOff, () => store)
            .RedactAsync(id, new OutgoingImage(t.Bytes, t.Mime, t.Width, t.Height, false), CancellationToken.None);

        var missingReader = new SwitchReader { Answer = () => throw new WorkerException("Model not installed: ocr.") };
        var missingOut = await new DescribeRedactor(missingReader, () => new Settings(), () => store)
            .RedactAsync(id, new OutgoingImage(t.Bytes, t.Mime, t.Width, t.Height, false), CancellationToken.None);

        // Simulates an unrelated purge / lock clearing the thumbnail cache while callers hold OutgoingImage.
        thumbs.ClearCache();
        Assert.Equal(0, thumbs.Cache.Count);
        Assert.All(t.Bytes, b => Assert.Equal(0, b));

        foreach (var outp in new[] { clean, uncheckedOut, offOut, missingOut })
        {
            Assert.NotSame(t.Bytes, outp.Bytes);
            Assert.Equal(original, outp.Bytes);
            Assert.Equal("RIFF", System.Text.Encoding.ASCII.GetString(outp.Bytes, 0, 4));
            using var decoded = SKBitmap.Decode(outp.Bytes);
            Assert.NotNull(decoded);
        }
    }

    [Fact]
    public async Task A2_seam_unrelated_purge_through_agent_keeps_describe_redactor_bytes_intact()
    {
        var core = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(_home, _home));
        await using (core)
        {
            await core.OpenStoreAsync(CancellationToken.None);
            var agent = new AgentHost(core, new AgentOptions(_home, _home, false));
            await using (agent)
            {
                core.Purged += typeof(AgentHost).GetMethod("OnPurged", BindingFlags.NonPublic | BindingFlags.Instance)!.CreateDelegate<Action<PurgeNotice>>(agent);
                var thumbs = (Thumbnailer)GetField(agent, "_thumbs")!;

                var launcher = new FakeWorkerLauncher
                {
                    Handler = (req, _) => req.Op == "ocr"
                        ? (new WorkerResponse(req.Id, true, Ocr: new OcrResult([], "fake", 1)), [])
                        : null,
                };
                await using var sup = new WorkerSupervisor(launcher, _home, () => 120);
                var worker = new WorkerClient(sup);
                var redactor = new DescribeRedactor(new WorkerKeyframeReader(worker), () => core.Settings.Current, () => core.Store);

                var imgA = WhiteWebp(640, 360);
                var imgB = WhiteWebp(640, 360);
                string idA = core.Store!.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow.AddMinutes(-2), "notepad", "", "note A", 0, 640, 360, 1, imgA.Bytes, "image/webp", true));
                string idB = core.Store!.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow.AddMinutes(-1), "calc", "", "calc B", 0, 640, 360, 2, imgB.Bytes, "image/webp", true));
                var vec = new float[4];
                vec[0] = 1;
                var embInfo = new EmbeddingInfo("fake", "r1", 4);
                core.Store!.SetEmbedding(idA, vec, embInfo);
                core.Store!.SetEmbedding(idB, vec, embInfo);

                var t = await thumbs.GetThumbAsync(idA, 320, CancellationToken.None);
                Assert.NotNull(t);
                var original = (byte[])t!.Bytes.Clone();

                var outp = await redactor.RedactAsync(idA, new OutgoingImage(t.Bytes, t.Mime, t.Width, t.Height, false), CancellationToken.None);
                Assert.True(outp.Redacted);

                // Unrelated store purge raises CoreHost.Purged -> AgentHost.OnPurged -> Thumbnailer.ClearCache().
                Assert.Equal(1, core.Store!.Purge(PurgeScope.ForId(idB)));
                Assert.Equal(0, thumbs.Cache.Count);
                Assert.All(t.Bytes, b => Assert.Equal(0, b));

                Assert.NotSame(t.Bytes, outp.Bytes);
                Assert.Equal(original, outp.Bytes);
                using var decoded = SKBitmap.Decode(outp.Bytes);
                Assert.NotNull(decoded);
            }
        }
    }

    // ---------------- A4: pause and session lock flush buffered speech and wipe raw PCM from RAM ----------------

    private sealed class FakeCapture : ICaptureStream { public bool Ended => false; public void Dispose() { } }

    private sealed class Speaker
    {
        private PcmHandler? _feed;
        public long Qpc = OutputCapture.Qpc100ns(Stopwatch.GetTimestamp()) - 9_000_000_000L; // 15 min ago
        public LoopbackHub Hub { get; }

        public Speaker() => Hub = new LoopbackHub(h => { _feed = h; return new FakeCapture(); });

        public void Play(bool tone, int ms)
        {
            var buf = new byte[480 * 4];
            for (int p = 0; p < ms / 10; p++)
            {
                if (tone)
                    for (int f = 0; f < 480; f++)
                    {
                        short v = (short)(16000 * Math.Sin((p * 480 + f) * 2 * Math.PI * 440 / 48000));
                        BitConverter.TryWriteBytes(buf.AsSpan(f * 4), v);
                        BitConverter.TryWriteBytes(buf.AsSpan(f * 4 + 2), v);
                    }
                else Array.Clear(buf);
                _feed!(buf, 480, Qpc, false);
                Qpc += 100_000;
            }
        }
    }

    private static void AssertNoBufferedPcm(TranscriptSource src)
    {
        var seg = (SpeechSegmenter)GetField(src, "_segmenter")!;
        Assert.False(seg.Active);
        Assert.Null(GetField(seg, "_seg"));
        Assert.Empty((List<float>)GetField(seg, "_pending")!);
        Assert.Empty((Queue<float[]>)GetField(seg, "_preRoll")!);
        Assert.Empty((List<float>)GetField(src, "_buf")!);
        var resampler = (Resampler)GetField(src, "_resampler")!;
        Assert.Empty((List<float>)GetField(resampler, "_mono")!);
    }

    [Fact]
    public async Task A4_pause_flushes_buffered_speech_through_guard_and_wipes_pcm_before_long_resume()
    {
        var store = new FakeStore();
        var s = new Settings();
        int n = 0;
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) => req.Op == "transcribe"
                ? (new WorkerResponse(req.Id, true, Transcript: new TranscribeResult(true, $"utterance {Interlocked.Increment(ref n)}", "en", [], 3)), [])
                : null,
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var tr = new Transcriber(new WorkerClient(sup), () => store, () => s, () => { })
        {
            RetryDelay = _ => TimeSpan.FromMilliseconds(200),
            GuardWait = TimeSpan.FromMilliseconds(300),
            HoldFor = 150,
        };
        bool paused = false;
        var verdict = new AudioVerdict(AudioGate.Allowed, "notepad", 1);
        var spk = new Speaker();
        using var src = new TranscriptSource("system", spk.Hub, () => s, () => paused, () => verdict);
        using var cts = new CancellationTokenSource();
        var run = tr.RunAsync(src, cts.Token);

        // Spoken across two allowed apps without trailing silence, leaving SpeechSegmenter.Active == true.
        spk.Play(tone: true, 600);
        verdict = new AudioVerdict(AudioGate.Allowed, "edge", 2);
        spk.Play(tone: true, 600);
        Assert.True(((SpeechSegmenter)GetField(src, "_segmenter")!).Active);

        paused = true;
        src.Flush();
        AssertNoBufferedPcm(src);

        await Until(() => store.Audio.Count == 1);
        Assert.Single(store.Audio.Values);
        Assert.Equal(0, tr.Blocked);

        // Resume > 10 minutes later onto a 3rd allowed app: pre-pause speech was already guarded, transcribed and stored.
        spk.Qpc += AudioGuardTimeline.KeepFor100ns + 600_000_000L;
        paused = false;
        verdict = new AudioVerdict(AudioGate.Allowed, "powershell", 3);
        spk.Play(tone: true, 1000);
        spk.Play(tone: false, 800);

        await Until(() => store.Audio.Count == 2);
        cts.Cancel();
        await run;
        Assert.Equal(2, store.Audio.Count);
        Assert.Equal(0, tr.Blocked);
    }

    [Fact]
    public async Task A4_on_pcm_while_paused_flushes_active_speech_and_wipes_pcm()
    {
        var store = new FakeStore();
        var s = new Settings();
        var launcher = new FakeWorkerLauncher
        {
            Handler = (req, _) => req.Op == "transcribe"
                ? (new WorkerResponse(req.Id, true, Transcript: new TranscribeResult(true, "pre pause speech", "en", [], 3)), [])
                : null,
        };
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        var tr = new Transcriber(new WorkerClient(sup), () => store, () => s, () => { })
        {
            HoldFor = 100,
            GuardWait = TimeSpan.FromMilliseconds(300),
        };
        bool paused = false;
        var verdict = new AudioVerdict(AudioGate.Allowed, "notepad", 1);
        var spk = new Speaker();
        using var src = new TranscriptSource("system", spk.Hub, () => s, () => paused, () => verdict);
        using var cts = new CancellationTokenSource();
        var run = tr.RunAsync(src, cts.Token);

        spk.Play(tone: true, 1000);
        Assert.True(((SpeechSegmenter)GetField(src, "_segmenter")!).Active);

        // No explicit Flush() call: the first OnPcm packet arriving while paused flushes and wipes.
        paused = true;
        verdict = new AudioVerdict(AudioGate.Blocked, "lockapp", 2);
        spk.Play(tone: false, 20);
        AssertNoBufferedPcm(src);

        await Until(() => store.Audio.Count == 1);
        cts.Cancel();
        await run;
        var row = Assert.Single(store.Audio.Values);
        Assert.Equal("pre pause speech", row.Text);
        Assert.Equal("notepad", row.App);
        Assert.Equal(0, tr.Blocked);
    }

    [Fact]
    public async Task A4_agent_host_pause_and_session_lock_flush_transcript_sources()
    {
        var core = new FakeCoreHost();
        core.SettingsStore.Current.Privacy.PauseOnLock = true;
        var agent = new AgentHost(core, new AgentOptions(_home, _home, false));
        await using (agent)
        {
            var pause = (PauseController)GetField(agent, "_pause")!;
            var spk = new Speaker();
            using var src = new TranscriptSource("system", spk.Hub, () => core.Settings.Current, () => pause.ContinuousPaused(true), () => new AudioVerdict(AudioGate.Allowed, "notepad", 1));
            SetField(agent, "_systemAudio", src);

            spk.Play(tone: true, 1000);
            Assert.True(((SpeechSegmenter)GetField(src, "_segmenter")!).Active);

            agent.Pause(null, "user");
            AssertNoBufferedPcm(src);
            Assert.Equal(1, src.Segments);
            Assert.True(src.Segments_.TryRead(out var seg1));
            Assert.NotNull(seg1);
            Assert.Equal(AudioGate.Allowed, src.GuardFor(seg1!).Gate);

            agent.Resume("user");
            spk.Qpc += 10_000_000;
            spk.Play(tone: true, 1000);
            Assert.True(((SpeechSegmenter)GetField(src, "_segmenter")!).Active);

            typeof(AgentHost).GetMethod("OnSessionChanged", BindingFlags.NonPublic | BindingFlags.Instance)!.Invoke(agent, [Win32.WTS_SESSION_LOCK]);
            AssertNoBufferedPcm(src);
            Assert.Equal(2, src.Segments);
            Assert.True(src.Segments_.TryRead(out var seg2));
            Assert.NotNull(seg2);
            Assert.Equal(AudioGate.Allowed, src.GuardFor(seg2!).Gate);
        }
    }

    // ---------------- C7: WasapiCapture.Start timeout must not dispose _ready before thread finishes ----------------

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void C7_wasapi_start_timeout_and_late_ready_do_not_throw_ObjectDisposedException(bool failLate)
    {
        using var unblock = new ManualResetEventSlim();
        Thread? captureThread = null;
        AppDomain.CurrentDomain.UnhandledException += OnUnhandled;
        Exception? unhandled = null;

        void OnUnhandled(object sender, UnhandledExceptionEventArgs e) => unhandled = e.ExceptionObject as Exception;

        try
        {
            Assert.Throws<TimeoutException>(() => WasapiCapture.StartForTest("loopback", (_, _, _, _) => { }, () =>
            {
                Volatile.Write(ref captureThread, Thread.CurrentThread);
                unblock.Wait();
                if (failLate) throw new IOException("late failure");
            }, readyTimeoutMs: 60, joinTimeoutMs: 40));

            Assert.True(SpinWait.SpinUntil(() => Volatile.Read(ref captureThread) is not null, 2000));
            Thread.Sleep(60); // well after Dispose()'s join timeout returned
            unblock.Set();
            Assert.True(Volatile.Read(ref captureThread)!.Join(5000));
            Assert.Null(unhandled);
        }
        finally
        {
            unblock.Set();
            AppDomain.CurrentDomain.UnhandledException -= OnUnhandled;
        }
    }

    [Fact]
    public void C7_wasapi_start_error_and_normal_dispose_clean_up()
    {
        var ex = Assert.Throws<IOException>(() => WasapiCapture.StartForTest("mic", (_, _, _, _) => { },
            () => throw new IOException("no mic"), readyTimeoutMs: 2000, joinTimeoutMs: 500));
        Assert.Equal("no mic", ex.Message);

        using var c = WasapiCapture.StartForTest("loopback", (_, _, _, _) => { }, () => { }, readyTimeoutMs: 2000, joinTimeoutMs: 500);
        c.Dispose(); // idempotent second dispose via using
        Assert.True(c.Ended);
    }
}
