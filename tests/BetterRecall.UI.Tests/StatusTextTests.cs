// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public sealed class StatusTextTests
{
    private static readonly DateTimeOffset T = new(2026, 11, 16, 9, 30, 0, TimeSpan.Zero);

    private static AgentStatus Status(
        bool paused = false,
        string? reason = null,
        bool locked = false,
        bool userPaused = false,
        int pending = 0,
        long rows = 10,
        string device = "NPU",
        bool audio = false) =>
        new(Capturing: !paused, Paused: paused, PausedUntil: null, PauseReason: reason, Locked: locked, UnlockedUntil: null,
            Audio: new AudioState(audio, false), Queue: new QueueState(pending), Device: device,
            Store: new StoreStats(rows, rows * 1000, 0, null, null, 4, 10), RetentionDays: 30, UserPaused: userPaused);

    private static string Flat(IEnumerable<LocalizedText> parts) =>
        string.Join("|", parts.Select(t => t.Key + ":" + string.Join(",", t.Args.Select(x => x is LocalizedText l ? l.Key : x))));

    [Fact]
    public void StatusText_describes_paused_storage_locked_and_formats_units()
    {
        var fullscreen = Status(paused: true, reason: "fullscreen", locked: true, pending: 4, rows: 10, audio: true);
        var parts = StatusText.Describe(fullscreen, DateTimeOffset.UtcNow);
        Assert.Equal("Status_Paused", parts[0].Key);
        Assert.Equal("Reason_fullscreen", ((LocalizedText)parts[0].Args[0]).Key);
        Assert.Contains(parts, p => p.Key == "Status_Locked");
        Assert.Contains(parts, p => p.Key == "Status_AudioSystem");
        Assert.DoesNotContain(parts, p => p.Key is "Status_Queue" or "Status_Store");

        var storage = Status(paused: true, reason: "storage");
        var storageParts = StatusText.Describe(storage, DateTimeOffset.UtcNow);
        Assert.Equal("Status_Paused", storageParts[0].Key);
        Assert.Equal("Reason_storage", ((LocalizedText)storageParts[0].Args[0]).Key);

        Assert.Equal("2 KB", StatusText.FormatBytes(2048));
        Assert.Equal("2:00", StatusText.FormatDuration(120_000));
    }

    /// <summary>D37 + D8: no store or queue counts are shown while locked (by agent or UI), including in split parts.</summary>
    [Fact]
    public void Status_hides_counts_while_locked()
    {
        var now = DateTimeOffset.UtcNow;
        var open = StatusText.Describe(Status(locked: false, pending: 218, rows: 934), now);
        Assert.Contains(open, p => p.Key == "Status_Store");
        Assert.Contains(open, p => p.Key == "Status_Queue");

        var agentLocked = StatusText.Describe(Status(locked: true, pending: 218, rows: 934), now);
        Assert.Contains(agentLocked, p => p.Key == "Status_Locked");
        Assert.DoesNotContain(agentLocked, p => p.Key is "Status_Store" or "Status_Queue");

        var uiLocked = StatusText.Describe(Status(locked: false, pending: 218, rows: 934), now, locked: true);
        Assert.Contains(uiLocked, p => p.Key == "Status_Locked");
        Assert.DoesNotContain(uiLocked, p => p.Key is "Status_Store" or "Status_Queue" or "Status_Unlocked");

        var detail = PauseControl.DetailParts(Status(pending: 3), now, locked: true);
        Assert.DoesNotContain(detail, t => t.Key is "Status_Queue" or "Status_Store");
        Assert.Contains(PauseControl.StateParts(Status(), now, locked: true), t => t.Key == "Status_Locked");
    }

    /// <summary>D16: pause button is usable under automatic pause and requires both connection and status.</summary>
    [Fact]
    public void Pause_button_enabled_and_label_follow_status_and_connection()
    {
        var fullscreen = Status(paused: true, reason: "fullscreen");
        Assert.True(PauseControl.Enabled(fullscreen, connected: true));
        Assert.Equal("Main_Pause", PauseControl.LabelKey(fullscreen));
        Assert.False(PauseControl.UserPaused(fullscreen));

        // A user pause under a higher-priority storage pause still shows Resume.
        var storage = Status(paused: true, reason: "storage", userPaused: true);
        Assert.True(PauseControl.UserPaused(storage));
        Assert.Equal("Main_Resume", PauseControl.LabelKey(storage));
        Assert.True(PauseControl.Enabled(storage, connected: true));

        // Fallback when userPaused is false/absent: top reason "ipc" still counts.
        Assert.True(PauseControl.UserPaused(Status(paused: true, reason: "ipc")));

        Assert.False(PauseControl.Enabled(Status(), connected: false));
        Assert.False(PauseControl.Enabled(null, connected: true));
        Assert.Equal("Main_Pause", PauseControl.LabelKey(null));
    }

    /// <summary>D8: only state is announced; counts live in detail parts.</summary>
    [Fact]
    public void Status_live_part_ignores_counts()
    {
        var now = DateTimeOffset.UtcNow;
        var a = PauseControl.StateParts(Status(pending: 1, rows: 10), now);
        var b = PauseControl.StateParts(Status(pending: 50, rows: 900), now);
        Assert.Equal(2, a.Count);
        Assert.Equal(Flat(a), Flat(b));
        Assert.NotEqual(Flat(PauseControl.DetailParts(Status(pending: 1, rows: 10), now)), Flat(PauseControl.DetailParts(Status(pending: 50, rows: 900), now)));
        Assert.Contains(PauseControl.DetailParts(Status(pending: 3), now), t => t.Key == "Status_Queue");
    }

    /// <summary>D17: raw "none" or empty device reads as not loaded.</summary>
    [Fact]
    public void Device_none_reads_not_loaded()
    {
        Assert.Equal(new LocalizedText("Status_DeviceNotLoaded"), StatusText.Device("none"));
        Assert.Equal(new LocalizedText("Status_DeviceNotLoaded"), StatusText.Device(""));
        Assert.Equal("NPU", StatusText.Device("NPU"));
        var device = StatusText.Describe(Status(device: "none", pending: 3, audio: true), T).Single(p => p.Key == "Status_Device");
        Assert.Equal(new LocalizedText("Status_DeviceNotLoaded"), device.Args[0]);
        Assert.True(TestResources.Strings().ContainsKey("Status_DeviceNotLoaded"));
    }

    /// <summary>D16: narrow status bar drops store, then queue, then audio.</summary>
    [Fact]
    public void Narrow_status_bar_drops_store_then_queue_then_audio()
    {
        var detail = PauseControl.DetailParts(Status(pending: 3, rows: 808, audio: true), T);
        Assert.Equal(["Status_AudioSystem", "Status_Device", "Status_Queue", "Status_Store"], detail.Select(p => p.Key));
        Assert.Equal(detail, StatusText.Fit(detail, 1200));
        Assert.Equal(["Status_AudioSystem", "Status_Device", "Status_Queue"], StatusText.Fit(detail, 900).Select(p => p.Key));
        Assert.Equal(["Status_AudioSystem", "Status_Device"], StatusText.Fit(detail, 760).Select(p => p.Key));
        Assert.Equal(["Status_Device"], StatusText.Fit(detail, 640).Select(p => p.Key));
    }

    /// <summary>B3 + R: warning codes deduplicate to one banner each, including replay-text-unchecked.</summary>
    [Fact]
    public void Warnings_map_to_one_banner_each()
    {
        var b = WarningBanners.For(["settings-unreadable", "ner-unavailable", "ocr-failing", "vc-runtime-missing", "ner-unavailable", "replay-text-unchecked", "new-code"]);
        Assert.Equal(["settings-unreadable", "ner-unavailable", "ocr-failing", "vc-runtime-missing", "replay-text-unchecked", "new-code"], b.Select(x => x.Code));
        Assert.True(b[0].CanReset && b[0].Error);
        Assert.True(b[1].Error);   // names stored unredacted
        Assert.Equal("Warn_ReplayTextTitle", b[4].TitleKey);
        Assert.True(b[4].Error);
        Assert.False(b[4].CanReset);
        Assert.Equal("Warn_OtherTitle", b[5].TitleKey);
        Assert.Empty(WarningBanners.For(null));
        Assert.True(WarningBanners.Same(null, []));
        Assert.False(WarningBanners.Same(["ocr-failing"], null));
    }
}
