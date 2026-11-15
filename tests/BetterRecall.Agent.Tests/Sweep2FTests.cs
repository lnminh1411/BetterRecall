// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Host;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Privacy;
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Scheduling;
using BetterRecall.Agent.Thumbs;
using BetterRecall.Agent.Worker;
using BetterRecall.Contracts;
using BetterRecall.Core.Ai;
using SkiaSharp;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 2, batch F: purge reach into agent RAM (N5: A10/B3, A11, N4), failed badge probes (A5), samples
/// during pauses (A9), OCR failure reporting (A21), NER failures (C9) and the describe redactor (N1).</summary>
public class PurgeReachAgentTests
{
    private static readonly DateTimeOffset Now = new(2026, 11, 15, 12, 0, 0, TimeSpan.Zero);

    [Fact]
    public void User_purge_reaching_the_replay_window_clears_the_rings()
    {
        // "Delete the last 15 minutes": its window ends now.
        Assert.True(AgentHost.ReachesRing(new PurgeNotice("purge", [], Now.AddMinutes(-15), Now), Now, 120));
        // purge app / id whose newest deleted row is from a minute ago.
        Assert.True(AgentHost.ReachesRing(new PurgeNotice("purge", ["s_1"], Newest: Now.AddMinutes(-1)), Now, 120));
        // Purge all always.
        Assert.True(AgentHost.ReachesRing(new PurgeNotice("all", [], DateTimeOffset.MinValue, Now), Now, 120));
    }

    [Fact]
    public void Old_purges_and_non_user_deletes_leave_the_rings()
    {
        Assert.False(AgentHost.ReachesRing(new PurgeNotice("purge", ["s_1"], Now.AddDays(-2), Now.AddDays(-1), Now.AddDays(-1)), Now, 120));
        Assert.False(AgentHost.ReachesRing(new PurgeNotice("purge", ["s_1"], Newest: Now.AddHours(-1)), Now, 120));
        Assert.False(AgentHost.ReachesRing(new PurgeNotice("retention", ["s_1"], Newest: Now.AddSeconds(-1)), Now, 120));
        Assert.False(AgentHost.ReachesRing(new PurgeNotice("drop", ["s_1"], Newest: Now.AddSeconds(-1)), Now, 120));
        Assert.False(AgentHost.ReachesRing(new PurgeNotice("replay", ["r_1"], Newest: Now.AddSeconds(-1)), Now, 120));
    }

    private static byte[] Webp()
    {
        var px = new byte[320 * 180 * 4];
        for (int i = 0; i < px.Length; i += 4) { px[i] = (byte)(i % 251); px[i + 1] = 80; px[i + 2] = 160; px[i + 3] = 255; }
        unsafe { fixed (byte* p = px) return ImageCodec.EncodeBgra(p, 320, 180, 320 * 4, "webp", 75); }
    }

    [Fact]
    public async Task Purged_capture_is_not_served_from_the_thumbnail_cache()
    {
        var store = new FakeStore();
        var id = store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 320, 180, 0, Webp(), "image/webp", true));
        var thumbs = new Thumbnailer(() => store, () => new Settings());
        var t = await thumbs.GetThumbAsync(id, 160, CancellationToken.None);
        Assert.NotNull(t);
        // The purge deletes the row and ICoreHost.Purged clears the cache: the next request finds nothing (A10/B3).
        store.Screens.Clear();
        thumbs.ClearCache();
        Assert.All(t!.Bytes, b => Assert.Equal(0, b));   // zeroed
        Assert.Null(await thumbs.GetThumbAsync(id, 160, CancellationToken.None));
    }

    [Fact]
    public async Task Thumbnail_read_while_a_purge_runs_is_not_cached()
    {
        var store = new FakeStore();
        var id = store.AddScreenCapture(new NewScreenCapture(DateTimeOffset.UtcNow, "app", "", "t", 0, 320, 180, 0, Webp(), "image/webp", true));
        Thumbnailer? thumbs = null;
        bool purgeOnce = true;
        thumbs = new Thumbnailer(() =>
        {
            if (purgeOnce) { purgeOnce = false; thumbs!.ClearCache(); }   // the purge lands between lookup and decode
            return store;
        }, () => new Settings());
        Assert.NotNull(await thumbs.GetThumbAsync(id, 160, CancellationToken.None));
        Assert.Equal(0, thumbs.Cache.Count);
    }

    [Fact]
    public async Task Purged_ids_leave_the_embedding_queue_with_their_images_zeroed()
    {
        var launcher = new FakeWorkerLauncher();
        await using var sup = new WorkerSupervisor(launcher, "x", () => 120);
        await using var sched = new EmbeddingScheduler(() => new FakeStore(), () => new Settings(), new WorkerClient(sup),
            () => new GovernanceInputs(1, 0, false, UserNotificationState.AcceptsNotifications, 0), () => false);
        var gone = new PendingJob("s_gone", CaptureKind.Screen, [7, 7, 7], "image/webp", null);
        var kept = new PendingJob("s_kept", CaptureKind.Screen, [5, 5], "image/webp", null);
        sched.EnqueueForTest([gone, kept]);
        Assert.Equal(1, sched.Remove(new HashSet<string>(["s_gone"])));
        Assert.Equal(1, sched.Queued);
        Assert.Equal(new byte[] { 0, 0, 0 }, gone.Image);
        Assert.Equal(new byte[] { 5, 5 }, kept.Image);
    }

    [Fact]
    public void Carry_in_sample_keeps_its_grab_time_for_the_fence()
    {
        // N4: the newest sample before the clip start is re-stamped to the start; the frame carries the real grab time.
        var now = Now;
        var sampler = new ReplaySampler(() => now);
        var grabbed = Now.AddSeconds(-30);
        sampler.Add(new ReplaySampler.Sample(grabbed, 0, "app", "t", "", 64, 32, [1, 2, 3], "image/webp"), 130);
        var start = Now.AddSeconds(-10);
        var taken = Assert.Single(sampler.Take(start, Now, 5));
        Assert.Equal(start, taken.Ts);
        Assert.Equal(grabbed, taken.Grabbed);
        var frame = Assert.Single(ReplaySampler.ToFrames("rp", start, 10_000, [taken]));
        Assert.Equal(start, frame.Ts);
        Assert.Equal(grabbed, frame.CapturedAt);
        // A sample inside the clip is not re-stamped and carries no separate grab time.
        sampler.Add(new ReplaySampler.Sample(Now.AddSeconds(-8), 0, "app", "t", "", 64, 32, [4], "image/webp"), 130);
        Assert.All(sampler.Take(start, Now, 5), s => Assert.Null(s.Grabbed));
    }
}

public class BadgeFailClosedTests
{
    private static WindowInfo Win(string exe, string title = "Some page") => new(42, 1, @"C:\Apps\" + exe, title, "Chrome_WidgetWin_1");

    [Theory]
    [InlineData((int)BadgeResult.Private, true)]
    [InlineData((int)BadgeResult.CheckFailed, true)]
    [InlineData((int)BadgeResult.NotChecked, true)]
    [InlineData((int)BadgeResult.NoBadge, false)]
    [InlineData((int)BadgeResult.NotBrowser, false)]
    public void Background_browser_window_is_hidden_unless_its_probe_says_normal(int raw, bool hidden)
    {
        var result = (BadgeResult)raw;
        // A5: a failed probe used to leave an Incognito window on another monitor in keyframes and in the replay.
        Assert.Equal(hidden, GuardrailPolicy.HidesBrowserWindow(Win("chrome.exe"), _ => result));
        Assert.Equal(hidden, GuardrailPolicy.BadgeHides(result));
    }

    [Fact]
    public void No_ui_automation_hides_every_titled_browser_window_only()
    {
        Assert.True(GuardrailPolicy.HidesBrowserWindow(Win("msedge.exe"), null));
        Assert.False(GuardrailPolicy.HidesBrowserWindow(Win("notepad.exe"), null));
        Assert.False(GuardrailPolicy.HidesBrowserWindow(Win("chrome.exe", ""), null));   // untitled helper windows
        Assert.False(GuardrailPolicy.HidesBrowserWindow(Win("notepad.exe"), _ => BadgeResult.CheckFailed));
    }
}

public class SamplePauseTests
{
    private static ReplaySettings R() => new() { Enabled = true, Searchable = true, SampleSeconds = 5 };

    [Fact]
    public void Samples_stop_while_a_pause_stops_keyframes_and_inference()
    {
        // A9: fullscreen and storage pauses keep the replay running but must not run sample OCR and encodes.
        Assert.True(CaptureService.SampleDue(R(), replayPaused: false, samplesPaused: false, pipelines: 1, sinceLastMs: 5000));
        Assert.False(CaptureService.SampleDue(R(), replayPaused: false, samplesPaused: true, pipelines: 1, sinceLastMs: 5000));
        Assert.False(CaptureService.SampleDue(R(), replayPaused: true, samplesPaused: false, pipelines: 1, sinceLastMs: 5000));
        Assert.False(CaptureService.SampleDue(R(), replayPaused: false, samplesPaused: false, pipelines: 0, sinceLastMs: 5000));
        Assert.False(CaptureService.SampleDue(R(), replayPaused: false, samplesPaused: false, pipelines: 1, sinceLastMs: 4000));
    }
}

public class OcrHealthTests
{
    [Fact]
    public void Repeated_failures_mark_ocr_failing_and_warn_at_most_every_ten_minutes()
    {
        long now = 0;
        var h = new OcrHealth(() => now);
        for (int i = 1; i < OcrHealth.FailingAfter; i++) Assert.False(h.Failed());
        Assert.False(h.Failing);
        Assert.True(h.Failed());    // the FailingAfter-th failure warns
        Assert.True(h.Failing);
        Assert.False(h.Failed());   // throttled
        now += 600_000;
        Assert.True(h.Failed());
        Assert.False(h.Read(entitiesAsked: false, entitiesMissing: false));
        Assert.False(h.Failing);    // one good read clears it
    }

    [Fact]
    public void Missing_entities_are_reported_only_when_names_or_addresses_were_asked()
    {
        long now = 0;
        var h = new OcrHealth(() => now);
        Assert.False(h.Read(entitiesAsked: false, entitiesMissing: true));
        Assert.False(h.EntitiesMissing);
        Assert.True(h.Read(entitiesAsked: true, entitiesMissing: true));
        Assert.True(h.EntitiesMissing);
        Assert.False(h.Read(entitiesAsked: true, entitiesMissing: true));   // throttled
        h.Read(entitiesAsked: true, entitiesMissing: false);
        Assert.False(h.EntitiesMissing);
    }
}

public class KeyframeOcrFailureTests
{
    private sealed class Reader(Func<OcrResult> answer) : IKeyframeReader
    {
        public Task<OcrResult> ReadAsync(ReadOnlyMemory<byte> bgra, int width, int height, int stride, bool entities, CancellationToken ct)
            => Task.FromResult(answer());
    }

    private static unsafe KeyframeGrab WhiteGrab(int w, int h, int seed)
    {
        var req = new KeyframeRequest(DateTimeOffset.UtcNow, "app", @"C:\app.exe", "Title", "ctx" + seed, true, w, 0, "", Bounds: new RECT(0, 0, w * 2, h * 2));
        var g = KeyframeGrab.Alloc(0, req, w, h, 0);
        new Span<byte>(g.Pixels, w * h * 4).Fill(255);
        return g;
    }

    [Fact]
    public async Task Persistent_ocr_error_is_reported_instead_of_silently_dropping_every_keyframe()
    {
        // A21: "Device failed" on every read with the default text detectors on: frames are dropped (fail closed) and
        // the pipeline reports OCR as failing so the status can say so.
        var s = new Settings();
        s.Keyframes.Format = "webp";
        var store = new FakeStore();
        var pipe = new KeyframePipeline(() => store, () => s, _ => { }, new Reader(() => throw new WorkerReplyException("Device failed: npu.")), new RedactionMasks());
        for (int i = 0; i < OcrHealth.FailingAfter; i++)
        {
            long before = pipe.RedactDropped;
            pipe.Post(WhiteGrab(64, 32, i));
            for (int k = 0; k < 200 && pipe.RedactDropped == before; k++) await Task.Delay(20);
        }
        await pipe.DisposeAsync();
        Assert.Equal(OcrHealth.FailingAfter, pipe.RedactDropped);
        Assert.Empty(store.Screens);
        Assert.True(pipe.OcrHealth.Failing);
    }

    [Fact]
    public async Task Broken_name_detector_keeps_storing_with_pattern_redaction()
    {
        // C9: the worker returns the lines with EntitiesMissing; the card number is still blacked out and the frame kept.
        var s = new Settings();
        s.Keyframes.Format = "webp";
        s.Privacy.RedactNames = true;
        var store = new FakeStore();
        var pipe = new KeyframePipeline(() => store, () => s, _ => { },
            new Reader(() => new OcrResult([Ocr.Line("Card: 4111 1111 1111 1111", 10, 10)], "onnx", 5, EntitiesMissing: true)), new RedactionMasks());
        pipe.Post(WhiteGrab(400, 200, 1));
        for (int k = 0; k < 200 && pipe.Kept + pipe.RedactDropped + pipe.Failed == 0; k++) await Task.Delay(20);
        await pipe.DisposeAsync();
        var cap = Assert.Single(store.Screens.Values);
        Assert.Equal("Card:", cap.Text);
        Assert.True(pipe.OcrHealth.EntitiesMissing);
        Assert.False(pipe.OcrHealth.Failing);
    }
}

public class DescribeNerMissingTests
{
    private sealed class Reader(OcrResult answer) : IKeyframeReader
    {
        public Task<OcrResult> ReadAsync(ReadOnlyMemory<byte> bgra, int width, int height, int stride, bool entities, CancellationToken ct)
            => Task.FromResult(answer);
    }

    private static OutgoingImage WhiteWebp()
    {
        using var bmp = new SKBitmap(new SKImageInfo(200, 60, SKColorType.Bgra8888, SKAlphaType.Premul));
        bmp.Erase(SKColors.White);
        using var data = bmp.Encode(SKEncodedImageFormat.Webp, 100);
        return new OutgoingImage(data.ToArray(), "image/webp", 200, 60, false);
    }

    private static OcrResult Card(bool entitiesMissing)
    {
        const string text = "Card: 4111 1111 1111 1111";
        var cuts = Enumerable.Range(0, text.Length + 1).Select(i => 10 + i * 6).ToArray();
        return new OcrResult([new OcrLine(text, 10, 10, text.Length * 6, 20, 0.99f, cuts)], "fake", 1, entitiesMissing);
    }

    [Theory]
    [InlineData(true, false)]
    [InlineData(false, true)]
    public async Task Names_on_with_the_detector_missing_is_not_redacted(bool names, bool expectRedacted)
    {
        // N1: chat sends images only when Redacted; names were not checked, so it must be false. Patterns still painted.
        var s = new Settings();
        s.Privacy.RedactNames = names;
        var r = new DescribeRedactor(new Reader(Card(entitiesMissing: true)), () => s);
        var outp = await r.RedactAsync("s_1", WhiteWebp(), CancellationToken.None);
        Assert.Equal(expectRedacted, outp.Redacted);
        using var bmp = SKBitmap.Decode(outp.Bytes);
        Assert.True(bmp.GetPixel(100, 20).Red < 40);   // card digits blacked out either way
    }

    [Fact]
    public async Task Names_on_with_the_detector_present_is_redacted()
    {
        var s = new Settings();
        s.Privacy.RedactNames = true;
        var r = new DescribeRedactor(new Reader(Card(entitiesMissing: false)), () => s);
        Assert.True((await r.RedactAsync("s_1", WhiteWebp(), CancellationToken.None)).Redacted);
    }
}
