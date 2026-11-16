// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Reflection;
using System.Runtime.InteropServices;
using System.Threading.Channels;
using BetterRecall.Agent.Audio;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Guardrails;
using BetterRecall.Agent.Host;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Privacy;
using BetterRecall.Agent.Replay;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Tests;

/// <summary>Bug sweep 3, batch J (privacy and purge reach), agent side.</summary>
public class Sweep3JAgentTests
{
    private static object? Get(object o, string f) => o.GetType().GetField(f, BindingFlags.NonPublic | BindingFlags.Instance)!.GetValue(o);
    private static void Set(object o, string f, object? v) => o.GetType().GetField(f, BindingFlags.NonPublic | BindingFlags.Instance)!.SetValue(o, v);
    private static object? Call(object o, string m) => o.GetType().GetMethod(m, BindingFlags.NonPublic | BindingFlags.Instance)!.Invoke(o, []);

    private static GuardrailService Guard() => new(() => new Settings(), () => false, () => [], @"C:\nowhere\");

    /// <summary>An allow snapshot for the current foreground, as the guard thread publishes it.</summary>
    private static GuardSnapshot Snap(Dictionary<int, MonitorTop> per, long epoch = 0) =>
        new(Environment.TickCount64, WindowInspector.Describe(WindowInspector.ForegroundRoot()), GuardDecision.Allowed, BadgeResult.NotBrowser, false,
            [], per, "", null, epoch);

    private sealed class FakeCapture : ICaptureStream { public bool Ended => false; public void Dispose() { } }

    // ---- A1: replay masks while the snapshot is older than the foreground window

    [Fact]
    public void A1_stale_snapshot_masks_the_new_foreground_window_not_the_canvas()
    {
        var fgWindow = new WindowInfo(42, 1, @"C:\a\app.exe", "App", "Cls");
        var snap = new GuardSnapshot(Environment.TickCount64, fgWindow, GuardDecision.Allowed, BadgeResult.NotBrowser, false, [], new Dictionary<int, MonitorTop>(), FgEpoch: 3);
        var bounds = new RECT(100, 100, 900, 700);

        Assert.Null(GuardrailService.UnscannedForeground(snap, 3, 42, bounds));                 // fresh: nothing extra
        Assert.Equal(bounds, GuardrailService.UnscannedForeground(snap, 4, 42, bounds));        // foreground event since the scan
        Assert.Equal(bounds, GuardrailService.UnscannedForeground(snap, 3, 77, bounds));        // another window came up
        Assert.Equal(CaptureService.MaskEverything, GuardrailService.UnscannedForeground(snap, 4, 77, null));   // bounds unknown
        Assert.Null(GuardrailService.UnscannedForeground(snap, 4, 0, null));                    // no foreground window at all
        Assert.Null(GuardrailService.UnscannedForeground(GuardSnapshot.Closed, 4, 77, bounds)); // Closed masks everything already

        var masks = CaptureService.MergeMasks(snap, passwords: false, [], GuardrailService.UnscannedForeground(snap, 4, 77, bounds));
        Assert.Equal([bounds], masks);
        Assert.DoesNotContain(CaptureService.MaskEverything, masks);
    }

    [Fact]
    public void A1_replay_masks_are_not_empty_while_keyframes_refuse_the_snapshot()
    {
        var guard = Guard();
        Set(guard, "_snapshot", Snap(new() { [0] = new MonitorTop(0, null, false) }));
        guard.OnForegroundEvent();   // alt-tab to a denied window; the scan has not landed yet
        var cs = new CaptureService(() => new Settings(), () => false, () => false, guard, null!, null!, false, null);
        var kf = guard.CurrentForKeyframe();
        var masks = (RECT[])Call(cs, "Masks")!;
        Assert.False(kf.Decision.Allow);
        if (WindowInspector.ForegroundRoot() != 0) Assert.NotEmpty(masks);   // a desktop without a foreground window has nothing to mask
    }

    // ---- A7: guard snapshot keys follow the output layout

    [Fact]
    public void A7_snapshot_from_before_a_rebuild_is_not_applied_to_renumbered_monitors()
    {
        var guard = Guard();
        var cs = new CaptureService(() => new Settings(), () => false, () => false, guard, null!, null!, false, null);
        var a = new OutputCapture(new OutputInfo(0, @"\\.\DISPLAY1", 1, 0x8086, "gpu", 0, 0, 1920, 1080, 0), cs.Devices, _ => { });
        var b = new OutputCapture(new OutputInfo(1, @"\\.\DISPLAY2", 1, 0x8086, "gpu", 1920, 0, 1920, 1080, 0), cs.Devices, _ => { });
        var b0 = new OutputCapture(new OutputInfo(0, @"\\.\DISPLAY2", 1, 0x8086, "gpu", 0, 0, 1920, 1080, 0), cs.Devices, _ => { });
        try
        {
            var gate = Get(cs, "_gate")!;
            void Layout(params OutputCapture[] outs) { lock (gate) { Set(cs, "_outputs", outs.ToList()); Set(cs, "_layoutGen", (int)Get(cs, "_layoutGen")! + 1); } }

            Layout(a, b);
            var keys = cs.MonitorBounds().Select(m => m.Item1).ToArray();
            var aTop = new WindowInfo(1, 1, @"C:\a\notepad.exe", "A doc", "Notepad");
            Set(guard, "_snapshot", Snap(new() { [keys[0]] = new MonitorTop(keys[0], aTop, false), [keys[1]] = new MonitorTop(keys[1], null, true) }));
            cs.RequestKeyframes(force: false);
            Assert.Equal("A doc", a.PendingKeyframe?.Title);   // control: the snapshot of this layout applies
            Assert.Null(b.PendingKeyframe);                     // denied window visible on B

            Layout(b0);   // A unplugged: DXGI renumbers B to index 0; the old snapshot is still current
            cs.RequestKeyframes(force: false);
            Assert.Null(b0.PendingKeyframe);   // unknown to the snapshot: skipped, not stored with A's title
        }
        finally { a.Dispose(); b.Dispose(); b0.Dispose(); }
    }

    // ---- A8: private-window tag in the browser root name

    [Theory]
    [InlineData("New Tab - Google Chrome (Incognito)", true)]
    [InlineData("Bing and 2 more pages - Personal - Microsoft Edge (InPrivate)", true)]
    [InlineData("Start - Brave [Private]", true)]
    [InlineData("Neuer Tab - Google Chrome (Inkognito)", true)]
    [InlineData("Docs - Google Chrome", false)]
    [InlineData("Budget (draft) - Google Chrome", false)]
    [InlineData("", false)]
    [InlineData(null, false)]
    public void A8_root_name_private_tag(string? name, bool expected) =>
        Assert.Equal(expected, BrowserBadgeDetector.RootNameIsPrivate(name));

    /// <summary>Review S3: windows without a toolbar (installed web apps, DevTools, picture-in-picture, F11) are not
    /// hidden when their root name is readable and carries no private tag; an unreadable name still fails closed.</summary>
    [Theory]
    [InlineData("Mail - Outlook", "NoBadge")]
    [InlineData("DevTools - example.com/", "NoBadge")]
    [InlineData("Picture in picture", "NoBadge")]
    [InlineData("YouTube - Google Chrome (Incognito)", "Private")]
    [InlineData("", "CheckFailed")]
    [InlineData(null, "CheckFailed")]
    public void A8_no_toolbar_result(string? rootName, string expected) =>
        Assert.Equal(Enum.Parse<BadgeResult>(expected), BrowserBadgeDetector.NoToolbarResult(rootName));

    [Fact]
    public void A8_edge_badge_name_is_matched()
    {
        Assert.Contains("InPrivate browsing", BrowserBadgeDetector.BadgeNames);   // CA-S3: UIA name conditions match the whole name
    }

    // ---- A9: panic keeps the replay redaction masks

    [Fact]
    public void A9_panic_keeps_replay_redaction_masks()
    {
        var guard = Guard();
        Set(guard, "_snapshot", Snap(new() { [0] = new MonitorTop(0, null, false) }));
        var masks = new RedactionMasks();
        var kp = new KeyframePipeline(() => null, () => new Settings(), _ => { }, null, masks);
        var cs = new CaptureService(() => new Settings(), () => false, () => false, guard, kp, null!, false, masks);
        var box = new RECT(100, 100, 400, 130);   // OCR box over a card number still on screen
        masks.Set(0, [box]);
        cs.Panic();
        Assert.Contains(box, masks.All());
        Assert.Contains(box, (RECT[])Call(cs, "Masks")!);
    }

    // ---- A10: a packet still in the encoder after a panic clear is dropped

    private static byte[] Av1SeqHeaderObu()
    {
        var bits = new List<int>();
        void W(long v, int n) { for (int i = n - 1; i >= 0; i--) bits.Add((int)((v >> i) & 1)); }
        W(0, 3); W(0, 1); W(0, 1); W(0, 1); W(0, 1); W(0, 5); W(0, 12); W(8, 5); W(0, 1);
        W(15, 4); W(15, 4); W(1279, 16); W(719, 16); W(0, 1); W(0, 1); W(1, 1); W(1, 1);
        W(0, 1); W(0, 1); W(0, 1); W(0, 1); W(1, 1); W(0, 1); W(0, 1); W(1, 1); W(1, 1); W(6, 3);
        W(0, 1); W(1, 1); W(1, 1); W(0, 1); W(0, 1); W(0, 1); W(0, 1); W(0, 2); W(0, 1); W(1, 1);
        while (bits.Count % 8 != 0) bits.Add(0);
        var payload = new byte[bits.Count / 8];
        for (int i = 0; i < bits.Count; i++) if (bits[i] == 1) payload[i / 8] |= (byte)(0x80 >> (i % 8));
        var obu = new List<byte> { (byte)((1 << 3) | 0x02), (byte)payload.Length };
        obu.AddRange(payload);
        return obu.ToArray();
    }

    private static byte[] Frame(byte[] seq, bool key, int i)
    {
        var f = new List<byte> { (2 << 3) | 0x02, 0 };
        if (key) f.AddRange(seq);
        f.AddRange([(byte)((6 << 3) | 0x02), 4, 1, 2, 3, (byte)i]);
        return f.ToArray();
    }

    [Fact]
    public void A10_late_packet_of_the_old_session_is_dropped_after_clear()
    {
        var seq = Av1SeqHeaderObu();
        using var s = new VideoStream("canvas", 16 << 20, TimeSpan.FromSeconds(60), lockMemory: false);
        int sess = s.BeginSession(VideoCodec.Av1, 1280, 720, 30);
        for (int i = 0; i < 30; i++) s.OnPacket(sess, Frame(seq, i == 0, i), (i + 1) * 333_333L, i == 0);
        s.Clear();                                                       // ReplayEngine.Panic, on the caller's thread
        s.OnPacket(sess, Frame(seq, true, 99), 31 * 333_333L, true);    // the forced 1 s keyframe still in the encoder
        using (var snap = s.Ring.Snapshot(long.MinValue)) Assert.Empty(snap.Packets);

        int next = s.BeginSession(VideoCodec.Av1, 1280, 720, 30);       // the rebuild after the panic records again
        s.OnPacket(next, Frame(seq, true, 100), 40 * 333_333L, true);
        using (var snap = s.Ring.Snapshot(long.MinValue)) Assert.Single(snap.Packets);
    }

    // ---- A11: locked replay memory covers every ring and the save snapshot

    [Fact]
    public void A11_locked_bytes_cover_rings_and_snapshot()
    {
        long ring = ReplayEngine.RingCapacity(8000, 120);
        Assert.True(ReplayEngine.LockedBytes(8000, 120, 4) >= ring * 4 * 2);   // perMonitor with 4 screens, plus the save copy
        Assert.True(ReplayEngine.LockedBytes(8000, 120, 1) > ReplayEngine.LockedBytes(8000, 30, 1));   // follows replay.seconds
    }

    // ---- A14: session lock state at start

    [Theory]
    [InlineData(0, 0, true)]     // active, locked
    [InlineData(0, 1, false)]    // active, unlocked
    [InlineData(4, 1, true)]     // disconnected
    [InlineData(0, -1, null)]    // unknown
    public void A14_session_lock_state(int state, int flags, bool? locked) =>
        Assert.Equal(locked, Win32.SessionLocked(state, flags));

    [Fact]
    public void A14_query_answers_on_this_session()
    {
        Assert.NotNull(Win32.SessionLockedNow());   // the test runs in an interactive or service session WTS knows
    }

    // ---- A15: a failed password-field search is distinguishable from "none found"

    [Fact]
    public void A15_failed_password_search_returns_null()
    {
        RECT[]? failed = [];
        var t = new Thread(() =>
        {
            Ole.CoInitializeEx(0, Ole.COINIT_MULTITHREADED);
            using (var p = new PasswordFieldProbe()) failed = p.Find(unchecked((nint)0x7FFF0F01));   // no such window
            Ole.CoUninitialize();
        });
        t.Start();
        Assert.True(t.Join(20000));
        Assert.Null(failed);
    }

    /// <summary>Review S2: the first failure warns (long.MinValue never reaches the subtraction), then once per 10 min.</summary>
    [Fact]
    public void A15_password_warning_is_rate_limited_and_fires_first()
    {
        Assert.True(GuardrailService.WarnDue(long.MinValue, 5));
        Assert.True(GuardrailService.WarnDue(long.MinValue, long.MinValue + 1));
        Assert.False(GuardrailService.WarnDue(1_000, 1_000 + 599_999));
        Assert.True(GuardrailService.WarnDue(1_000, 1_000 + 600_000));
    }

    // ---- A16 + CA-S2: raw PCM is zeroed, not just emptied

    private static float[] Items(List<float> l) => (float[])typeof(List<float>).GetField("_items", BindingFlags.NonPublic | BindingFlags.Instance)!.GetValue(l)!;
    private static float[] Sine(int n) => Enumerable.Range(0, n).Select(i => 0.5f * MathF.Sin(i * 0.2f) + 0.01f).ToArray();

    [Fact]
    public void A16_segments_evicted_by_drop_oldest_are_zeroed()
    {
        var hub = new LoopbackHub(_ => new FakeCapture());
        using var src = new TranscriptSource("system", hub, () => new Settings(), () => false);
        var q = (Channel<SpeechSegment>)Get(src, "_queue")!;
        var segs = Enumerable.Range(0, 9).Select(i => new SpeechSegment(Enumerable.Repeat(0.25f, 16000).ToArray(), i, 1000)).ToList();
        foreach (var s in segs) q.Writer.TryWrite(s);   // worker busy: the 9th evicts the 1st
        Assert.All(segs[0].Samples, x => Assert.Equal(0f, x));
        Assert.Contains(segs[8].Samples, x => x != 0);  // queued ones keep their audio until transcribed
    }

    [Fact]
    public void CA_S2_pcm_list_helpers_zero_the_backing_array()
    {
        var l = new List<float>(8) { 1, 2, 3, 4 };
        Pcm.RemoveHead(l, 2);
        Assert.Equal([3f, 4f], l);
        Assert.Equal([3f, 4f, 0f, 0f], Items(l)[..4]);
        var old = Items(l);
        var grown = Pcm.Append(l, Enumerable.Repeat(9f, 20).ToArray());
        Assert.NotSame(l, grown);
        Assert.All(old, x => Assert.Equal(0f, x));      // the outgrown array is wiped
        Pcm.Wipe(grown);
        Assert.Empty(grown);
        Assert.All(Items(grown), x => Assert.Equal(0f, x));
    }

    [Fact]
    public void CA_S2_emitted_and_cleared_segments_leave_no_pcm()
    {
        var seg = new SpeechSegmenter(() => 30);
        var outp = new List<SpeechSegment>();
        seg.Push(Sine(16000), 0, outp);
        var open = (List<float>)Get(seg, "_seg")!;
        seg.Push(new float[16000], 10_000_000, outp);   // silence ends the segment
        Assert.Single(outp);
        Assert.All(Items(open), x => Assert.Equal(0f, x));

        seg.Push(Sine(16000), 20_000_000, outp);
        open = (List<float>)Get(seg, "_seg")!;
        var pending = (List<float>)Get(seg, "_pending")!;
        seg.Clear();   // panic
        Assert.All(Items(open), x => Assert.Equal(0f, x));
        Assert.All(Items(pending), x => Assert.Equal(0f, x));
    }

    // ---- A17: pause and read gate deadlines ignore wall-clock steps

    [Fact]
    public void A17_pause_survives_a_wall_clock_step()
    {
        var wall = DateTimeOffset.UtcNow;
        long mono = 1_000_000;
        var pc = new PauseController(() => wall, () => mono);
        string? expired = null;
        pc.Expired += r => expired = r;
        pc.Set("user", wall + TimeSpan.FromHours(1));
        wall += TimeSpan.FromMinutes(70);   // NTP step, no real time elapsed
        Assert.True(pc.Has("user"));
        Assert.Equal("user", pc.Current().Reason);
        mono += 3_600_000;                  // an hour really passed
        Assert.False(pc.Has("user"));
        Assert.Equal("user", expired);
    }

    [Fact]
    public void A17_read_gate_timeout_ignores_a_backwards_step()
    {
        var wall = DateTimeOffset.UtcNow;
        long mono = 5_000;
        var gate = new ReadGate(() => wall, () => mono);
        gate.Open(5);
        wall -= TimeSpan.FromHours(2);   // clock stepped back
        mono += 5 * 60_000;              // five minutes really passed
        Assert.False(gate.IsOpen(true));
    }

    // ---- D4: Lock closes the gate even with security.requireUnlock off

    [Fact]
    public void D4_user_lock_closes_the_gate_without_require_unlock()
    {
        var gate = new ReadGate();
        Assert.True(gate.IsOpen(requireUnlock: false));
        gate.LockByUser();
        Assert.False(gate.IsOpen(requireUnlock: false));
        Assert.Null(gate.Until);
        gate.ReleaseUserLock();
        Assert.True(gate.IsOpen(requireUnlock: false));

        gate.LockByUser();
        gate.Open(5);   // a verified unlock lifts it too
        Assert.True(gate.IsOpen(requireUnlock: true));
        gate.Close();   // session lock or timeout: no user lock
        Assert.True(gate.IsOpen(requireUnlock: false));
    }
}
