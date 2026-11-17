// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Collections.Concurrent;
using System.Reflection;
using BetterRecall.Agent.Audio;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Host;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Privacy;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;
using BetterRecall.Core.Ai;
using SkiaSharp;
using PBox = BetterRecall.Agent.Privacy.Box;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 4, batch R: capture, audio and replay privacy (A-H2, A-M5, A-M7, C2, A-M6, CA-S1, TH1, TH2).</summary>
public class Sweep4RAgentTests
{
    private static async Task Until(Func<bool> done, int ms = 10_000)
    {
        long end = Environment.TickCount64 + ms;
        while (!done() && Environment.TickCount64 < end) await Task.Delay(20);
    }

    // ---------------- audio: a fake loopback stream feeding the real TranscriptSource and Transcriber

    private sealed class FakeCapture : ICaptureStream { public bool Ended => false; public void Dispose() { } }

    /// <summary>The loopback hub's dispatch, fed 10 ms packets of 48 kHz stereo 16-bit with QPC stamps.</summary>
    private sealed class Speaker
    {
        private PcmHandler? _feed;
        public long Qpc = OutputCapture.Qpc100ns(System.Diagnostics.Stopwatch.GetTimestamp()) - 600_000_000;   // a minute ago
        public LoopbackHub Hub { get; }

        public Speaker() => Hub = new LoopbackHub(h => { _feed = h; return new FakeCapture(); });

        /// <summary>Tone (speech to the segmenter) or digital silence for ms milliseconds.</summary>
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

    private static (WorkerSupervisor Sup, FakeWorkerLauncher Launcher) Worker(Func<WorkerRequest, string?>? text = null)
    {
        var launcher = new FakeWorkerLauncher();
        if (text is not null)
            launcher.Handler = (req, _) => req.Op == "transcribe" && text(req) is { } t
                ? (new WorkerResponse(req.Id, true, Transcript: new TranscribeResult(true, t, "en", [], 3)), [])
                : null;
        return (new WorkerSupervisor(launcher, "x", () => 120), launcher);
    }

    private static Transcriber NewTranscriber(WorkerSupervisor sup, FakeStore store, Settings s) =>
        new(new WorkerClient(sup), () => store, () => s, () => { })
        {
            RetryDelay = _ => TimeSpan.FromMilliseconds(400),
            GuardWait = TimeSpan.FromMilliseconds(300),
            HoldFor = 300,
        };

    [Fact]
    public void AH2_audio_verdict_follows_the_guard_snapshot()
    {
        var fg = new WindowInfo(42, 1, @"C:\Windows\System32\mstsc.exe", "Remote Desktop", "Cls");
        GuardSnapshot Snap(GuardDecision d, long epoch = 3) =>
            new(Environment.TickCount64, fg, d, BadgeResult.NotBrowser, false, [], new Dictionary<int, MonitorTop>(), FgEpoch: epoch);

        Assert.Equal(new AudioVerdict(AudioGate.Blocked, "mstsc"), GuardrailService.AudioNow(Snap(GuardDecision.Deny(GuardReason.DenyProcess)), 3));
        Assert.Equal(AudioGate.Blocked, GuardrailService.AudioNow(Snap(GuardDecision.Deny(GuardReason.PrivateBadge)), 3).Gate);
        Assert.Equal(AudioGate.Blocked, GuardrailService.AudioNow(Snap(GuardDecision.Deny(GuardReason.DisplayAffinity)), 3).Gate);
        Assert.Equal(AudioGate.Blocked, GuardrailService.AudioNow(GuardSnapshot.Closed, 3).Gate);          // stale guard
        Assert.Equal(new AudioVerdict(AudioGate.Allowed, "mstsc"), GuardrailService.AudioNow(Snap(GuardDecision.Allowed), 3));
        Assert.Equal(AudioGate.Allowed, GuardrailService.AudioNow(Snap(GuardDecision.Deny(GuardReason.NoForeground)), 3).Gate);
        Assert.Equal(AudioGate.Unknown, GuardrailService.AudioNow(Snap(GuardDecision.Allowed), 4).Gate); // alt-tab not scanned yet
    }

    [Fact]
    public void AH2_timeline_judges_a_segment_by_when_it_was_spoken()
    {
        var t = new AudioGuardTimeline();
        var ps = new AudioVerdict(AudioGate.Allowed, "powershell");
        t.Note(1_000, ps);
        t.Note(5_000, new AudioVerdict(AudioGate.Unknown, "powershell"));   // alt-tab
        t.Note(5_500, new AudioVerdict(AudioGate.Blocked, "mstsc"));        // scan: a denied window
        t.Note(9_000, new AudioVerdict(AudioGate.Unknown, "mstsc"));
        t.Note(9_200, new AudioVerdict(AudioGate.Allowed, "notepad"));

        Assert.Equal(ps, t.Check(1_000, 4_900));                                        // before the switch
        Assert.Equal(AudioGate.Blocked, t.Check(4_000, 6_000).Gate);                   // runs into the denied window
        Assert.Equal(AudioGate.Blocked, t.Check(5_100, 5_400).Gate);                   // unscanned part resolves to the denied scan
        Assert.Equal(new AudioVerdict(AudioGate.Allowed, "notepad"), t.Check(9_100, 12_000));   // resolves to the next scan's app
        Assert.Equal(AudioGate.Blocked, t.Check(-10_000_000, 2_000).Gate);             // older than anything noted

        var open = new AudioGuardTimeline();
        open.Note(0, ps);
        open.Note(100, new AudioVerdict(AudioGate.Unknown, "powershell"));
        Assert.Equal(AudioGate.Unknown, open.Check(50, 200).Gate);                     // the scan has not landed yet
    }

    [Fact]
    public async Task AH2_speech_while_a_denied_window_is_in_front_never_reaches_the_worker_or_the_store()
    {
        // Seam: guard verdict per packet -> TranscriptSource -> Transcriber -> real WorkerSupervisor and pipe -> store.
        var store = new FakeStore();
        var s = new Settings();
        var (sup, launcher) = Worker();
        int transcribes = 0;
        launcher.Handler = (req, _) => { if (req.Op == "transcribe") Interlocked.Increment(ref transcribes); return null; };
        await using var _sup = sup;
        var spk = new Speaker();
        var verdict = new AudioVerdict(AudioGate.Blocked, "powershell");
        using var src = new TranscriptSource("system", spk.Hub, () => s, () => false, () => verdict);
        var tr = NewTranscriber(sup, store, s);
        using var cts = new CancellationTokenSource();
        var run = tr.RunAsync(src, cts.Token);

        spk.Play(tone: true, 1000);                               // spoken in the denied window
        verdict = new AudioVerdict(AudioGate.Allowed, "notepad");  // switched away before the segment ended
        spk.Play(tone: false, 800);
        await Until(() => tr.Blocked == 1);
        Assert.Equal(1, tr.Blocked);

        // Wrong-label half: spoken in powershell (allowed now), alt-tab to notepad before the worker gets to it.
        verdict = new AudioVerdict(AudioGate.Allowed, "powershell");
        spk.Play(tone: true, 1000);
        verdict = new AudioVerdict(AudioGate.Allowed, "notepad");
        spk.Play(tone: false, 800);
        await Until(() => !store.Audio.IsEmpty);
        cts.Cancel();
        await run;

        var row = Assert.Single(store.Audio.Values);
        Assert.Equal("powershell", row.App);
        Assert.Equal(1, transcribes);                             // the denied segment was never sent
    }

    [Fact]
    public void AH2_replay_audio_is_silence_while_muted()
    {
        var spk = new Speaker();
        bool muted = true;
        AudioReplayEncoder enc;
        try { enc = new AudioReplayEncoder(spk.Hub, 15, false, () => muted); }
        catch (Exception ex) when (ex is not Xunit.Sdk.XunitException) { return; }   // no AAC encoder on this machine
        using (enc)
        {
            spk.Play(tone: true, 200);
            Assert.Equal(20, enc.MutedPackets);
            Assert.Equal(20, enc.SilentPackets);   // encoded as zeros (or no run at all), never the sound
            muted = false;
            spk.Play(tone: true, 200);
            Assert.Equal(20, enc.MutedPackets);
            Assert.Equal(20, enc.SilentPackets);
        }
    }

    // ---------------- A-M5: passing worker failures keep the segment

    [Fact]
    public async Task AM5_segment_survives_a_worker_crash_and_the_restart_backoff()
    {
        var store = new FakeStore();
        var s = new Settings();
        var (sup, launcher) = Worker();
        await using var _sup = sup;
        await new WorkerClient(sup).PrewarmAsync(CancellationToken.None);
        launcher.CrashOnNext = true;                               // the worker dies with the transcribe in flight
        var spk = new Speaker();
        using var src = new TranscriptSource("system", spk.Hub, () => s, () => false);
        var tr = NewTranscriber(sup, store, s);
        using var cts = new CancellationTokenSource();
        var run = tr.RunAsync(src, cts.Token);
        spk.Play(true, 1000);
        spk.Play(false, 800);
        await Until(() => !store.Audio.IsEmpty, 20_000);
        cts.Cancel();
        await run;
        Assert.Single(store.Audio.Values);
        Assert.True(tr.Retried >= 1);
        Assert.Equal(0, tr.GaveUp);
        Assert.True(launcher.Starts >= 2);
    }

    [Fact]
    public async Task AM5_retries_end_on_panic_and_error_replies_are_not_retried()
    {
        var store = new FakeStore();
        var s = new Settings();
        var (sup, launcher) = Worker();
        launcher.CrashWhen = req => req.Op == "transcribe";       // every try fails
        await using var _sup = sup;
        var spk = new Speaker();
        using var src = new TranscriptSource("system", spk.Hub, () => s, () => false);
        var tr = NewTranscriber(sup, store, s);
        using var cts = new CancellationTokenSource();
        var run = tr.RunAsync(src, cts.Token);
        spk.Play(true, 1000);
        spk.Play(false, 800);
        await Until(() => tr.Retried >= 1);
        src.Clear();                                               // panic: the held segment goes too
        launcher.CrashWhen = null;
        await Task.Delay(1500);
        Assert.Empty(store.Audio);
        Assert.Equal(0, tr.GaveUp);

        // "Model not installed" is an answer, not a passing failure: no retry.
        long retried = tr.Retried;
        launcher.Handler = (req, _) => req.Op == "transcribe" ? (new WorkerResponse(req.Id, false, Error: "Model not installed: whisper."), []) : null;
        spk.Play(true, 1000);
        spk.Play(false, 800);
        await Task.Delay(1500);
        cts.Cancel();
        await run;
        Assert.Equal(retried, tr.Retried);
        Assert.Empty(store.Audio);
    }

    // ---------------- A-M7: transcripts are redacted

    [Theory]
    [InlineData("my card number is 4111 1111 1111 1111 thanks", "my card number is thanks")]
    [InlineData("card 4111-1111-1111-1111.", "card .")]
    [InlineData("the IBAN is DE89 3704 0044 0532 0130 00 ok", "the IBAN is ok")]
    [InlineData("my password is hunter2. Then we left", "my password Then we left")]
    [InlineData("social security 123-45-6789", "social security")]
    [InlineData("nothing to hide here", "nothing to hide here")]
    public void AM7_transcript_text_is_redacted(string raw, string kept)
        => Assert.Equal(kept, Transcriber.Redact(raw, RedactionOptions.From(new PrivacySettings())));

    [Fact]
    public void AM7_card_split_across_two_segments_is_redacted_in_both()
    {
        var o = RedactionOptions.From(new PrivacySettings());
        const string a = "my card is 4111 1111", b = "1111 1111 and that's it";
        Assert.Equal(a, Transcriber.Redact(a, o));   // alone, neither half is a card number
        var (sa, sb) = Transcriber.BoundarySpans(a, b, o);
        Assert.Equal("my card is", Transcriber.Redact(a, o, sa));
        Assert.Equal("and that's it", Transcriber.Redact(b, o, sb));
    }

    [Fact]
    public async Task AM7_split_card_number_is_stored_redacted()
    {
        var store = new FakeStore();
        var s = new Settings();
        int n = 0;
        var (sup, _) = Worker(req => Interlocked.Increment(ref n) == 1 ? "my card is 4111 1111" : "1111 1111 and that's it");
        await using var _sup = sup;
        var spk = new Speaker();
        using var src = new TranscriptSource("system", spk.Hub, () => s, () => false);
        var tr = NewTranscriber(sup, store, s);
        using var cts = new CancellationTokenSource();
        var run = tr.RunAsync(src, cts.Token);
        spk.Play(true, 1000);
        spk.Play(false, 700);
        spk.Play(true, 1000);
        spk.Play(false, 800);
        await Until(() => store.Audio.Count == 2);
        cts.Cancel();
        await run;
        var texts = store.Audio.Values.Select(a => a.Text).ToList();
        Assert.Equal(2, texts.Count);
        Assert.All(texts, t => Assert.DoesNotContain("1111", t));
        Assert.Contains("my card is", texts);
        Assert.Contains("and that's it", texts);
    }

    // ---------------- C2, TH1, TH2: keyframe OCR and the replay text masks

    private sealed class SwitchReader : IKeyframeReader
    {
        public volatile Func<OcrResult> Answer = () => new OcrResult([], "fake", 1);
        public Func<CancellationToken, Task<int>>? Ready;
        public TimeSpan Delay;
        public int Calls, LastW, LastH;

        public async Task<OcrResult> ReadAsync(ReadOnlyMemory<byte> bgra, int width, int height, int stride, bool entities, CancellationToken ct)
        {
            Interlocked.Increment(ref Calls);
            LastW = width;
            LastH = height;
            if (Delay > TimeSpan.Zero) await Task.Delay(Delay, ct);
            return Answer();
        }

        public Task<int> ReadyAsync(CancellationToken ct) => Ready?.Invoke(ct) ?? Task.FromResult(0);
    }

    /// <summary>White frame; fill paints the top half (another screen).</summary>
    private static unsafe KeyframeGrab Grab(int w, int h, byte fill = 255, bool keyframe = true, bool sample = false)
    {
        var req = new KeyframeRequest(DateTimeOffset.UtcNow, "app", @"C:\app.exe", "Title", "ctx", false, w, 0, "",
            Keyframe: keyframe, ReplaySample: sample, Bounds: new RECT(0, 0, w, h));
        var g = KeyframeGrab.Alloc(0, req, w, h, 0);
        var px = new Span<byte>(g.Pixels, w * h * 4);
        px.Fill(255);
        px[..(px.Length / 2)].Fill(fill);
        return g;
    }

    [Fact]
    public async Task C2_TH1_masks_are_unchecked_until_ocr_reads_and_again_while_it_fails()
    {
        var s = new Settings();
        s.Keyframes.Format = "webp";
        var store = new FakeStore();
        var reader = new SwitchReader();
        var dropped = new ConcurrentQueue<int>();
        var pipe = new KeyframePipeline(() => store, () => s, _ => { }, reader, new RedactionMasks()) { OcrDropped = dropped.Enqueue };
        try
        {
            Assert.True(pipe.MasksUnchecked);                      // nothing read since start
            pipe.Post(Grab(64, 32));
            await Until(() => pipe.Kept == 1);
            Assert.False(pipe.MasksUnchecked);

            reader.Answer = () => throw new WorkerException("Worker pipe closed.");
            pipe.Post(Grab(64, 32, fill: 0));                      // new text while the worker is down
            await Until(() => pipe.RedactDropped == 1);
            Assert.True(pipe.MasksUnchecked);
            Assert.Equal([0], dropped);                            // TH1: grab that monitor again

            // The retry grab shows the same (still) screen as the stored keyframe: de-dup skips it, yet its OCR runs to
            // check the masks again, and nothing is stored twice.
            reader.Answer = () => new OcrResult([], "fake", 1);
            int calls = reader.Calls;
            pipe.Post(Grab(64, 32));
            await Until(() => !pipe.MasksUnchecked);
            Assert.False(pipe.MasksUnchecked);
            Assert.Equal(calls + 1, reader.Calls);
            Assert.Equal(1, pipe.Kept);
        }
        finally { await pipe.DisposeAsync(); }
    }

    [Fact]
    public async Task C2_ocr_not_installed_turns_the_text_mask_check_off()
    {
        var s = new Settings();
        var reader = new SwitchReader { Answer = () => throw new WorkerReplyException("Model not installed: ocr.") };
        var pipe = new KeyframePipeline(() => new FakeStore(), () => s, _ => { }, reader, new RedactionMasks());
        try
        {
            pipe.Post(Grab(64, 32));
            await Until(() => !pipe.MasksUnchecked);
            Assert.False(pipe.MasksUnchecked);
        }
        finally { await pipe.DisposeAsync(); }
    }

    [Fact]
    public async Task C2_sample_grab_with_searchable_off_refreshes_the_masks_only()
    {
        var s = new Settings();
        s.Replay.Searchable = false;
        var store = new FakeStore();
        var masks = new RedactionMasks();
        var reader = new SwitchReader { Answer = () => new OcrResult([Ocr.Line("Card: 4111 1111 1111 1111", 0, 0)], "fake", 1) };
        var sampler = new Replay.ReplaySampler();
        var pipe = new KeyframePipeline(() => store, () => s, _ => { }, reader, masks) { Sampler = sampler };
        try
        {
            pipe.Post(Grab(400, 40, keyframe: false, sample: true));
            await Until(() => masks.All().Length > 0);
            Assert.NotEmpty(masks.All());                          // the card's box reaches the replay
            Assert.False(pipe.MasksUnchecked);
            Assert.Empty(store.Screens);
            Assert.Equal(0, sampler.Count);                        // no sample encoded or kept
        }
        finally { await pipe.DisposeAsync(); }
    }

    [Fact]
    public void C2_samples_keep_running_through_storage_battery_and_fullscreen_pauses()
    {
        var r = new ReplaySettings { Searchable = false };
        Assert.False(CaptureService.SampleDue(r, replayPaused: false, samplesPaused: false, pipelines: 1, sinceLastMs: 5000));
        Assert.True(CaptureService.SampleDue(r, replayPaused: false, samplesPaused: false, pipelines: 1, sinceLastMs: 5000, maskRefresh: true));
        Assert.False(CaptureService.SampleDue(r, replayPaused: true, samplesPaused: false, pipelines: 1, sinceLastMs: 5000, maskRefresh: true));

        // AgentHost: SamplesPaused = ContinuousPaused(duringFullscreen: true).
        var p = new PauseController();
        foreach (var why in new[] { "storage", "battery", "fullscreen" })
        {
            p.Set(why);
            Assert.True(p.KeyframesPaused);
            Assert.False(p.ContinuousPaused(duringFullscreen: true));
        }
        p.Set("user");
        Assert.True(p.ContinuousPaused(duringFullscreen: true));
    }

    private static object? Call(object o, string m) => o.GetType().GetMethod(m, BindingFlags.NonPublic | BindingFlags.Instance)!.Invoke(o, []);
    private static void Set(object o, string f, object? v) => o.GetType().GetField(f, BindingFlags.NonPublic | BindingFlags.Instance)!.SetValue(o, v);
    private static object? Get(object o, string f) => o.GetType().GetField(f, BindingFlags.NonPublic | BindingFlags.Instance)!.GetValue(o);

    [Fact]
    public async Task C2_replay_masks_the_foreground_while_text_masks_are_unchecked_and_warns()
    {
        // Seam: KeyframePipeline OCR state -> CaptureService.Masks (what the replay compositor blacks out) and the
        // replay-text-unchecked warning.
        var s = new Settings();
        var guard = new GuardrailService(() => s, () => false, () => [], @"C:\nowhere\");
        var fgNow = WindowInspector.ForegroundRoot();
        Set(guard, "_snapshot", new GuardSnapshot(Environment.TickCount64, WindowInspector.Describe(fgNow), GuardDecision.Allowed,
            BadgeResult.NotBrowser, false, [], new Dictionary<int, MonitorTop>(), "", null, 0));
        var reader = new SwitchReader();
        var pipe = new KeyframePipeline(() => new FakeStore(), () => s, _ => { }, reader, new RedactionMasks());
        try
        {
            var cs = new CaptureService(() => s, () => false, () => false, guard, pipe, null!, false, new RedactionMasks());
            var unchecked_ = (RECT[])Call(cs, "Masks")!;
            Assert.True(cs.ReplayTextUnchecked());

            pipe.Post(Grab(64, 32));
            await Until(() => !pipe.MasksUnchecked);
            var checked_ = (RECT[])Call(cs, "Masks")!;
            Assert.False(cs.ReplayTextUnchecked());
            Assert.Equal(checked_.Length + 1, unchecked_.Length);   // the foreground window (or everything) on top
            var extra = unchecked_[^1];
            if (fgNow == 0) Assert.Equal(CaptureService.MaskEverything, extra);
            else Assert.True(extra.Right > extra.Left || extra.Equals(CaptureService.MaskEverything));

            // OCR or every text detector off: nothing to check, nothing extra.
            reader.Answer = () => throw new WorkerException("Worker pipe closed.");
            pipe.Post(Grab(64, 32, fill: 0));
            await Until(() => pipe.MasksUnchecked);
            Assert.True(cs.ReplayTextUnchecked());
            s.Ocr.Enabled = false;
            Assert.False(cs.ReplayTextUnchecked());
            Assert.Equal(checked_.Length, ((RECT[])Call(cs, "Masks")!).Length);
        }
        finally { await pipe.DisposeAsync(); }
    }

    [Fact]
    public void TH1_retry_reaches_the_output_of_that_monitor()
    {
        var guard = new GuardrailService(() => new Settings(), () => false, () => [], @"C:\nowhere\");
        var pipe = new KeyframePipeline(() => null, () => new Settings(), _ => { });
        var cs = new CaptureService(() => new Settings(), () => false, () => false, guard, pipe, null!, false, null);
        var a = new OutputCapture(new OutputInfo(0, @"\\.\DISPLAY1", 1, 0x8086, "gpu", 0, 0, 1920, 1080, 0), cs.Devices, _ => { });
        var b = new OutputCapture(new OutputInfo(1, @"\\.\DISPLAY2", 1, 0x8086, "gpu", 1920, 0, 1920, 1080, 0), cs.Devices, _ => { });
        try
        {
            lock (Get(cs, "_gate")!) Set(cs, "_outputs", new List<OutputCapture> { a, b });
            pipe.OcrDropped!(1);
            Assert.False((bool)Get(a, "_retryKeyframe")!);
            Assert.True((bool)Get(b, "_retryKeyframe")!);
        }
        finally { a.Dispose(); b.Dispose(); _ = pipe.DisposeAsync(); }
    }

    [Fact]
    public async Task TH2_a_cold_worker_start_is_not_counted_as_slow_ocr()
    {
        var s = new Settings();
        s.Keyframes.Format = "webp";
        var store = new FakeStore();
        var reader = new SwitchReader { Ready = async ct => { await Task.Delay(600, ct); return 0; } };   // device bench, NPU compile
        var pipe = new KeyframePipeline(() => store, () => s, _ => { }, reader, new RedactionMasks(), ocrTimeout: TimeSpan.FromMilliseconds(200));
        try
        {
            pipe.Post(Grab(64, 32));
            await Until(() => pipe.Kept + pipe.RedactDropped > 0);
            Assert.Equal(1, pipe.Kept);
            Assert.Equal(0, pipe.RedactDropped);
            Assert.Equal(TimeSpan.FromMilliseconds(200), pipe.OcrWait);   // not raised
        }
        finally { await pipe.DisposeAsync(); }
    }

    // ---------------- A-M6, CA-S1: describe and chat image redaction

    private static OutgoingImage WhiteWebp(int w, int h)
    {
        using var bmp = new SKBitmap(new SKImageInfo(w, h, SKColorType.Bgra8888, SKAlphaType.Premul));
        bmp.Erase(SKColors.White);
        using var data = bmp.Encode(SKEncodedImageFormat.Webp, 100);
        return new OutgoingImage(data.ToArray(), "image/webp", w, h, false);
    }

    /// <summary>"Card: 4111 1111 1111 1111" at (x, y), 10 px per character, 20 px high.</summary>
    private static OcrResult Card(int x, int y) => new([Ocr.Line("Card: 4111 1111 1111 1111", x, y)], "fake", 1);

    [Fact]
    public async Task AM6_describe_waits_as_long_as_keyframe_ocr_and_not_for_the_worker_start()
    {
        var reader = new SwitchReader { Answer = () => Card(10, 10), Delay = TimeSpan.FromMilliseconds(400), Ready = async ct => { await Task.Delay(400, ct); return 0; } };
        var adaptive = new DescribeRedactor(reader, () => new Settings(), null, () => TimeSpan.FromSeconds(2)) { MinWait = TimeSpan.FromMilliseconds(250) };
        var outp = await adaptive.RedactAsync("s_1", WhiteWebp(300, 60), CancellationToken.None);
        Assert.True(outp.Redacted);

        var fixedWait = new DescribeRedactor(reader, () => new Settings()) { MinWait = TimeSpan.FromMilliseconds(250) };
        var ex = await Assert.ThrowsAsync<DescribeException>(() => fixedWait.RedactAsync("s_1", WhiteWebp(300, 60), CancellationToken.None));
        Assert.Equal(DescribeRedactor.TimedOut, ex.Message);

        // Behind other worker requests: "busy", not a plain timeout.
        reader.Ready = _ => Task.FromResult(2);
        ex = await Assert.ThrowsAsync<DescribeException>(() => fixedWait.RedactAsync("s_1", WhiteWebp(300, 60), CancellationToken.None));
        Assert.Equal(DescribeRedactor.Busy, ex.Message);
    }

    [Fact]
    public async Task CAS1_downscaled_image_is_checked_against_the_full_size_keyframe()
    {
        // Seam: the store's keyframe (capture size) -> DescribeRedactor OCR -> boxes on the smaller outgoing image.
        var store = new FakeStore();
        var full = WhiteWebp(800, 120);
        string id = store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 800, 120, 0, full.Bytes, "image/webp", true));
        var reader = new SwitchReader { Answer = () => Card(40, 40) };   // found at capture size only
        var r = new DescribeRedactor(reader, () => new Settings(), () => store);
        var outp = await r.RedactAsync(id, WhiteWebp(400, 60), CancellationToken.None);

        Assert.Equal((800, 120), (reader.LastW, reader.LastH));         // OCR read the keyframe, not the thumbnail
        Assert.True(outp.Redacted);
        Assert.Equal((400, 60), (outp.Width, outp.Height));             // the smaller image is what goes out
        using var bmp = SKBitmap.Decode(outp.Bytes);
        // Digits at x 100..290, y 40..60 on the keyframe: x 50..145, y 20..30 on the half-size image.
        Assert.True(bmp.GetPixel(100, 25).Red < 40);
        Assert.True(bmp.GetPixel(25, 25).Red > 200);                    // "Card:" stays
    }

    [Fact]
    public async Task CAS1_unreadable_keyframe_leaves_the_image_not_checked()
    {
        var store = new FakeStore();
        string id = store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 800, 120, 0, [1, 2, 3, 4], "image/webp", true));
        var reader = new SwitchReader { Answer = () => new OcrResult([], "fake", 1) };
        var outp = await new DescribeRedactor(reader, () => new Settings(), () => store).RedactAsync(id, WhiteWebp(400, 60), CancellationToken.None);
        Assert.False(outp.Redacted);
        Assert.Equal((400, 60), (reader.LastW, reader.LastH));
    }

    [Fact]
    public void CAS1_boxes_scale_outwards()
    {
        var b = DescribeRedactor.ScaleBoxes([new PBox(101, 41, 191, 21)], 800, 120, 400, 60);
        Assert.Equal(new PBox(50, 20, 96, 11), b[0]);
    }
}
