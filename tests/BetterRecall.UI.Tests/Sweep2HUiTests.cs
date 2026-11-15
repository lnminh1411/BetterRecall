// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using Xunit;

namespace BetterRecall.UI.Tests;

/// <summary>Bug sweep 2, batch H: UI logic behind the visual and accessibility fixes.</summary>
public sealed class Sweep2HUiTests
{
    private static Dictionary<string, string> Strings()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir is not null && !File.Exists(Path.Combine(dir.FullName, "BetterRecall.slnx"))) dir = dir.Parent;
        Assert.NotNull(dir);
        var resw = System.Xml.Linq.XDocument.Load(Path.Combine(dir!.FullName, "src", "BetterRecall.UI", "Strings", "en-US", "Resources.resw"));
        return resw.Root!.Elements("data").ToDictionary(e => (string)e.Attribute("name")!, e => (string)e.Element("value")!);
    }

    private static AgentStatus Status(bool paused = false, string? reason = null, bool userPaused = false, int pending = 0, long rows = 10) =>
        new(Capturing: !paused, Paused: paused, PausedUntil: null, PauseReason: reason, Locked: false, UnlockedUntil: null,
            Audio: new AudioState(false, false), Queue: new QueueState(pending), Device: "NPU",
            Store: new StoreStats(rows, rows * 1000, 0, null, null, 0, 0), RetentionDays: 30, UserPaused: userPaused);

    // ---------------- D16: pause button

    [Fact]
    public void Pause_button_is_usable_under_an_automatic_pause()
    {
        var fullscreen = Status(paused: true, reason: "fullscreen");
        Assert.True(PauseControl.Enabled(fullscreen, connected: true));
        Assert.Equal("Main_Pause", PauseControl.LabelKey(fullscreen));
        Assert.False(PauseControl.UserPaused(fullscreen));

        // a user pause under a higher-priority storage pause still shows Resume
        var storage = Status(paused: true, reason: "storage", userPaused: true);
        Assert.True(PauseControl.UserPaused(storage));
        Assert.Equal("Main_Resume", PauseControl.LabelKey(storage));
        Assert.True(PauseControl.Enabled(storage, connected: true));

        // older agents without the field: the top reason still counts
        Assert.True(PauseControl.UserPaused(Status(paused: true, reason: "ipc")));
    }

    [Fact]
    public void Pause_button_needs_a_connection_and_a_status()
    {
        Assert.False(PauseControl.Enabled(Status(), connected: false));
        Assert.False(PauseControl.Enabled(null, connected: true));
        Assert.Equal("Main_Pause", PauseControl.LabelKey(null));
    }

    [Fact]
    public void UserPaused_round_trips_in_status_json()
    {
        var json = JsonSerializer.Serialize(Status(paused: true, reason: "battery", userPaused: true), ContractsJson.Default.AgentStatus);
        Assert.Contains("\"userPaused\":true", json);
        Assert.True(JsonSerializer.Deserialize(json, ContractsJson.Default.AgentStatus)!.UserPaused);
        // a status without the field (older agent) reads as false
        var old = json.Replace(",\"userPaused\":true", "");
        Assert.False(JsonSerializer.Deserialize(old, ContractsJson.Default.AgentStatus)!.UserPaused);
    }

    [Fact]
    public async Task Mock_reports_a_user_pause()
    {
        await using var h = new MockHarness();
        Assert.True((await h.Api.PauseAsync()).Ok);
        Assert.True((await h.Api.StatusAsync()).Value!.UserPaused);
        Assert.True((await h.Api.ResumeAsync()).Ok);
        Assert.False((await h.Api.StatusAsync()).Value!.UserPaused);
    }

    // ---------------- D8: only state is announced

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

    [Fact]
    public void Status_split_parts_hide_counts_while_the_ui_is_locked()
    {
        // F-ui D37 kept through the D8 split: no queue or store counts in the status bar while locked.
        var now = DateTimeOffset.UtcNow;
        var detail = PauseControl.DetailParts(Status(pending: 3), now, locked: true);
        Assert.DoesNotContain(detail, t => t.Key is "Status_Queue" or "Status_Store");
        Assert.Contains(PauseControl.StateParts(Status(), now, locked: true), t => t.Key == "Status_Locked");
    }

    private static string Flat(IEnumerable<LocalizedText> parts) =>
        string.Join("|", parts.Select(t => t.Key + ":" + string.Join(",", t.Args.Select(x => x is LocalizedText l ? l.Key : x))));

    // ---------------- D7: Shortcuts rows rebuild only on a real change

    [Fact]
    public void Shortcuts_signature_ignores_status_noise()
    {
        var settings = new JsonObject { ["hotkeys"] = new JsonObject { ["search"] = "Ctrl+Alt+Space", ["pause"] = "" } };
        var a = ShortcutRules.ViewSignature(settings, true, []);
        Assert.Equal(a, ShortcutRules.ViewSignature(settings, true, null));
        Assert.NotEqual(a, ShortcutRules.ViewSignature(settings, false, []));
        Assert.NotEqual(a, ShortcutRules.ViewSignature(settings, true, ["search"]));
        Assert.Equal(ShortcutRules.ViewSignature(settings, true, ["search", "pause"]), ShortcutRules.ViewSignature(settings, true, ["pause", "search"]));
        var changed = (JsonObject)settings.DeepClone();
        SettingsSchema.Set(changed, "hotkeys.pause", "Ctrl+Alt+P");
        Assert.NotEqual(a, ShortcutRules.ViewSignature(changed, true, []));
    }

    // ---------------- D24: Tab leaves the recorder

    [Fact]
    public void Tab_leaves_the_hotkey_recorder_unless_a_modifier_is_down()
    {
        Assert.True(HotkeyFormat.LeavesRecorder(9, false, false, false));
        Assert.False(HotkeyFormat.LeavesRecorder(9, true, false, false));
        Assert.False(HotkeyFormat.LeavesRecorder(9, false, true, false));
        Assert.False(HotkeyFormat.LeavesRecorder(9, false, false, true));
        Assert.False(HotkeyFormat.LeavesRecorder(0x41, false, false, false));
    }

    // ---------------- D9: purge token asked after the dialog

    [Fact]
    public async Task Purge_uses_a_token_asked_after_the_confirm_dialog()
    {
        var client = new PurgeClient();
        int asks = 0;
        var shown = new List<int>();
        var r = await PurgeFlow.ConfirmFreshAsync(new AgentApi(client),
            () => Task.FromResult(new AgentResult<PurgeConfirmReply>(true, new PurgeConfirmReply("t" + ++asks, 5), AgentErrorKind.None, null)),
            rows => { shown.Add(rows); return Task.FromResult(true); });
        Assert.True(r!.Value.Ok);
        Assert.Equal([5], shown);
        Assert.Equal("purge confirm t2", Assert.Single(client.Lines));   // not t1, which may have expired
    }

    [Fact]
    public async Task Purge_confirms_again_when_more_captures_match()
    {
        var client = new PurgeClient();
        int[] counts = [5, 7, 7];
        int asks = 0;
        var shown = new List<int>();
        var r = await PurgeFlow.ConfirmFreshAsync(new AgentApi(client),
            () => { var n = counts[asks]; asks++; return Task.FromResult(new AgentResult<PurgeConfirmReply>(true, new PurgeConfirmReply("t" + asks, n), AgentErrorKind.None, null)); },
            rows => { shown.Add(rows); return Task.FromResult(true); });
        Assert.True(r!.Value.Ok);
        Assert.Equal([5, 7], shown);
        Assert.Equal("purge confirm t3", Assert.Single(client.Lines));
    }

    [Fact]
    public async Task Purge_cancel_sends_nothing()
    {
        var client = new PurgeClient();
        var r = await PurgeFlow.ConfirmFreshAsync(new AgentApi(client),
            () => Task.FromResult(new AgentResult<PurgeConfirmReply>(true, new PurgeConfirmReply("t", 3), AgentErrorKind.None, null)),
            _ => Task.FromResult(false));
        Assert.Null(r);
        Assert.Empty(client.Lines);
    }

    [Fact]
    public async Task Purge_with_the_mock_agent_after_the_dialog()
    {
        await using var h = new MockHarness();
        var to = DateTimeOffset.UtcNow;
        int? shown = null;
        var r = await PurgeFlow.ConfirmFreshAsync(h.Api, () => h.Api.PurgeRangeAsync(to.AddHours(-6), to),
            async rows => { shown = rows; await Task.Yield(); return true; });
        Assert.True(r!.Value.Ok, r.Value.Error);
        Assert.Equal(shown, r.Value.Value);
    }

    private sealed class PurgeClient : IAgentClient
    {
        public List<string> Lines { get; } = [];
        public int ServerPid => 1;
        public Task<AgentResult> SendAsync(string line, CancellationToken ct = default)
        {
            Lines.Add(line);
            return Task.FromResult(AgentResult.Success(JsonDocument.Parse("{\"purged\":5}").RootElement.Clone()));
        }
    }

    // ---------------- C17: Today uses the offset at midnight

    [Fact]
    public void Today_starts_at_local_midnight_on_a_daylight_saving_day()
    {
        var cet = TimeZoneInfo.FindSystemTimeZoneById("Central European Standard Time");
        // 2026-03-29: clocks went forward at 02:00; at 10:00 the offset is +2, at midnight it was +1
        var now = new DateTimeOffset(2026, 3, 29, 10, 0, 0, TimeSpan.FromHours(2));
        var (from, to) = SearchViewModel.RangeBounds(TimeRange.Today, now, cet);
        Assert.Null(to);
        Assert.Equal(new DateTimeOffset(2026, 3, 29, 0, 0, 0, TimeSpan.FromHours(1)), from);
        Assert.Equal(new DateTimeOffset(2026, 3, 28, 23, 0, 0, TimeSpan.Zero), from!.Value.ToUniversalTime());
    }

    [Fact]
    public void Skipped_time_moves_by_the_size_of_the_gap()
    {
        // a zone whose clocks jump 30 minutes at 02:00 (like Lord Howe Island)
        var rule = TimeZoneInfo.AdjustmentRule.CreateAdjustmentRule(new DateTime(2000, 1, 1), new DateTime(2099, 12, 31), TimeSpan.FromMinutes(30),
            TimeZoneInfo.TransitionTime.CreateFloatingDateRule(new DateTime(1, 1, 1, 2, 0, 0), 10, 1, DayOfWeek.Sunday),
            TimeZoneInfo.TransitionTime.CreateFloatingDateRule(new DateTime(1, 1, 1, 2, 0, 0), 4, 1, DayOfWeek.Sunday));
        var zone = TimeZoneInfo.CreateCustomTimeZone("Test Half Hour", TimeSpan.FromHours(10.5), "Test", "Test", "Test DST", [rule]);
        var skipped = new DateTime(2026, 10, 4, 2, 10, 0);
        Assert.True(zone.IsInvalidTime(skipped));
        var at = LocalTime.At(skipped, zone);
        Assert.Equal(new DateTime(2026, 10, 4, 2, 40, 0), at.DateTime);
        Assert.Equal(TimeSpan.FromHours(11), at.Offset);
    }

    // ---------------- D38: timeline says when captures wait for indexing

    [Fact]
    public async Task Timeline_empty_day_mentions_pending_captures()
    {
        await using var h = new MockHarness();
        var vm = new TimelineViewModel(h.Api) { Day = new DateOnly(2001, 1, 1), Pending = 12 };
        await vm.LoadAsync();
        Assert.Empty(vm.Items);
        Assert.Equal(new LocalizedText("Timeline_EmptyPending", 12).Key, vm.Message.Key);
        vm.Pending = 0;
        await vm.LoadAsync();
        Assert.Equal("Timeline_Empty", vm.Message.Key);
    }

    // ---------------- S4: chat lock and stop events

    [Fact]
    public void Chat_lock_and_stop_errors_are_localised()
    {
        Assert.Equal("Chat_ErrLocked", ChatViewModel.ErrorTitleKey("locked"));
        Assert.Equal("Chat_ErrStopped", ChatViewModel.ErrorTitleKey("stopped"));
        Assert.Equal("Chat_ErrLockedBody", ChatViewModel.ErrorMessageKey("locked"));
        Assert.Equal("Chat_ErrStoppedBody", ChatViewModel.ErrorMessageKey("STOPPED"));
        Assert.Null(ChatViewModel.ErrorMessageKey("quota"));   // provider errors keep their detail
        Assert.True(ChatViewModel.IsLockCode("locked"));
        Assert.False(ChatViewModel.IsLockCode("stopped"));
        var s = Strings();
        foreach (var k in new[] { "Chat_ErrLocked", "Chat_ErrStopped", "Chat_ErrLockedBody", "Chat_ErrStoppedBody" }) Assert.True(s.ContainsKey(k), k);
    }

    // ---------------- D21: strings

    [Fact]
    public void Recommendation_layout_reason_uses_the_label()
    {
        var t = RecommendationRow.ReasonText("R_layout", ["composited", "1080p"]);
        var label = Assert.IsType<LocalizedText>(t.Args[0]);
        Assert.Equal("S_replay_layout_composited", label.Key);
        Assert.Equal("1080p", t.Args[1]);
        var other = RecommendationRow.ReasonText("R_fps_headroom", ["30", "12"]);
        Assert.Equal(new object[] { "30", "12" }, other.Args);
    }

    [Fact]
    public void Strings_used_by_the_fixes_exist_and_dead_ones_are_gone()
    {
        var s = Strings();
        foreach (var k in new[] { "Home_Connecting", "Home_Day", "Timeline_EmptyPending", "Chat_PageTitle.Text", "Chat_ApprovalInclude", "Audit_Empty.Text", "Key_Delete", "Nav_Settings.Content" })
            Assert.True(s.ContainsKey(k), k);
        Assert.False(s.ContainsKey("Chat_Sources"));
        Assert.False(s.ContainsKey("Launcher_App.PlaceholderText"));
        Assert.Contains("{0}", s["R_replay_unsupported"]);
        Assert.Contains("{1}", s["R_replay_unsupported"]);
        Assert.Contains("{0}", s["Chat_ApprovalInclude"]);
        Assert.Contains("{1}", s["Chat_ApprovalInclude"]);
    }
}
