// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;

namespace BetterRecall.UI.Tests;

public sealed class ChatViewModelTests
{
    private static readonly DateTimeOffset T = new(2026, 11, 12, 10, 0, 0, TimeSpan.Zero);
    private static ChatSource Src(string id) => new(id, T, CaptureKind.Screen, "chrome.exe", "Acme invoice", "acme.com");
    private static ChatApprovalItem Item(string id) => new(id, T, CaptureKind.Screen, "chrome.exe", "Acme", "acme.com", "[S1] ...", null, null);
    private static ChatDetail SavedChat(string id) =>
        new(new ChatSummary(id, "Chat " + id, T, T, 1), [new ChatMessage(1, T, "user", "hello " + id)], false);

    [Fact]
    public void Streamed_turn_builds_one_question_and_one_answer()
    {
        var vm = new ChatViewModel();
        vm.BeginSend("Which invoice?");
        Assert.True(vm.Busy);
        Assert.False(vm.CanSend);
        vm.Handle(new ChatEvent("start", ChatId: "c1"));
        vm.Handle(new ChatEvent("search", Query: "invoice"));
        Assert.Equal("invoice", vm.Messages[1].Status);
        vm.Handle(new ChatEvent("text", Text: "The "));
        vm.Handle(new ChatEvent("text", Text: "Acme "));
        vm.Handle(new ChatEvent("text", Text: "one [S1]."));
        Assert.Equal("", vm.Messages[1].Status);
        vm.Handle(new ChatEvent("sources", Sources: [Src("s1")]));
        vm.Handle(new ChatEvent("done", Usage: new AiUsage("2026-11-12", 3, 900, 200, 0)));
        Assert.Equal("c1", vm.ChatId);
        Assert.Equal(["user", "assistant"], vm.Messages.Select(m => m.Role));
        Assert.Equal("The Acme one [S1].", vm.Messages[1].Text);
        Assert.True(vm.Messages[1].HasSources);
        Assert.False(vm.Busy);
        Assert.True(vm.CanSend);
        Assert.Equal(3, vm.Usage!.Requests);
    }

    [Fact]
    public void Warning_event_sets_warning_and_next_send_clears_it()
    {
        var vm = new ChatViewModel();
        vm.BeginSend("q");
        vm.Handle(new ChatEvent("warning", Code: "ratelimit", Text: "Approaching daily quota."));
        Assert.Equal("Approaching daily quota.", vm.Warning);
        vm.Handle(new ChatEvent("done"));
        vm.BeginSend("next");
        Assert.Null(vm.Warning);
    }

    [Fact]
    public void Approval_blocks_sending_and_reports_unticked_items()
    {
        var vm = new ChatViewModel();
        vm.BeginSend("q");
        vm.Handle(new ChatEvent("approval", ChatId: "c1", Token: "t1", Items: [Item("a"), Item("b"), Item("c")], Share: AiShare.TextImages, Text: "note"));
        Assert.True(vm.HasApproval);
        Assert.False(vm.Busy);
        Assert.False(vm.CanSend);
        Assert.Equal("t1", vm.ApprovalToken);
        Assert.Equal("note", vm.ApprovalNotice);
        vm.Approval[1].Include = false;
        Assert.Equal(["b"], vm.RemovedIds());
        vm.ClearApproval();
        Assert.False(vm.HasApproval);
        Assert.True(vm.CanSend);
    }

    [Fact]
    public void Error_rolls_back_the_question_and_keeps_its_text()
    {
        var vm = new ChatViewModel();
        vm.BeginSend("first");
        vm.Handle(new ChatEvent("text", Text: "answer"));
        vm.Handle(new ChatEvent("done"));
        vm.BeginSend("second");
        vm.Handle(new ChatEvent("text", Text: "partial"));
        var retry = T.AddSeconds(30);
        vm.Handle(new ChatEvent("error", Code: "rate", Text: "Slow down.", RetryAt: retry));
        Assert.Equal(["first", "answer"], vm.Messages.Select(m => m.Text));
        Assert.Equal("second", vm.LastQuestion);
        Assert.Equal("rate", vm.ErrorCode);
        Assert.Equal(retry, vm.RetryAt);
        Assert.False(vm.Busy);

        vm.Handle(new ChatEvent("error", Code: "token", Text: "Invalid or expired token."));   // no question in flight
        Assert.Equal(2, vm.Messages.Count);
    }

    [Fact]
    public void Stop_load_and_reset()
    {
        var vm = new ChatViewModel();
        vm.BeginSend("q");
        vm.Handle(new ChatEvent("search", Query: "x"));
        vm.Stopped();
        Assert.False(vm.Busy);
        Assert.Equal("", vm.Messages[1].Status);

        vm.Load(new ChatDetail(new ChatSummary("c9", "Saved", T, T, 2),
            [new ChatMessage(0, T, "user", "hi"), new ChatMessage(1, T, "assistant", "hello", [Src("s1")])], true));
        Assert.Equal("c9", vm.ChatId);
        Assert.Equal("Saved", vm.Title);
        Assert.Single(vm.Messages[1].Sources);
        vm.Reset();
        Assert.Null(vm.ChatId);
        Assert.Empty(vm.Messages);
        Assert.Null(vm.LastQuestion);
    }

    /// <summary>D4/D6: late chat events from a stream started before Reset or Load are ignored.</summary>
    [Fact]
    public void Chat_events_of_a_stream_from_before_a_reset_are_ignored()
    {
        var vm = new ChatViewModel();
        vm.BeginSend("What did I see?");
        var gen = vm.Generation;
        Assert.True(vm.Handle(gen, new ChatEvent("text", Text: "You saw ")));
        Assert.Equal("You saw ", vm.Messages[^1].Text);

        vm.Reset();   // lock, launcher hidden, Chat page left
        Assert.False(vm.Handle(gen, new ChatEvent("text", Text: "the bank statement")));
        Assert.False(vm.Handle(gen, new ChatEvent("approval", Token: "t", Items: [])));
        Assert.Empty(vm.Messages);
        Assert.False(vm.HasApproval);

        var after = vm.Generation;
        vm.Load(new ChatDetail(new ChatSummary("c_1", "t", DateTimeOffset.UtcNow, DateTimeOffset.UtcNow, 0), [], false));
        Assert.NotEqual(after, vm.Generation);   // another chat shown: the old stream's events no longer apply either
    }

    /// <summary>C7: a late saved-chat fetch is dropped after Reset or a newer BeginOpen.</summary>
    [Fact]
    public void Open_ticket_drops_stale_or_reset_loads()
    {
        var vm = new ChatViewModel();
        var afterReset = vm.BeginOpen();
        vm.Reset();
        Assert.False(vm.LoadIfCurrent(afterReset, SavedChat("A")));
        Assert.Null(vm.ChatId);
        Assert.Empty(vm.Messages);

        var a = vm.BeginOpen();
        var b = vm.BeginOpen();
        Assert.False(vm.LoadIfCurrent(a, SavedChat("A")));
        Assert.True(vm.LoadIfCurrent(b, SavedChat("B")));
        Assert.Equal("B", vm.ChatId);
    }

    /// <summary>D6: saved-chat accessible name uses title and local date, not record id or type name.</summary>
    [Fact]
    public void Saved_chat_name_is_title_and_date_not_the_record()
    {
        var name = ChatViewModel.SummaryName(new ChatSummary("mc_sample_tokyo", "Flight to Tokyo", T, T, 2));
        Assert.StartsWith("Flight to Tokyo, ", name);
        Assert.DoesNotContain("mc_sample_tokyo", name);
        Assert.DoesNotContain("ChatSummary", name);
        Assert.Contains(T.ToLocalTime().ToString("g", System.Globalization.CultureInfo.CurrentCulture), name);
    }

    /// <summary>B6: ApplyRemoved prunes cited chats, pending approval cards, and full wipes.</summary>
    [Fact]
    public void ApplyRemoved_clears_cited_messages_truncated_all_and_approvals()
    {
        var t0 = DateTimeOffset.UtcNow;

        // 1) Unrelated removed event leaves chat intact.
        var vm = new ChatViewModel();
        vm.Load(new ChatDetail(new ChatSummary("c1", "Test", t0, t0, 2),
        [
            new ChatMessage(1, t0, "user", "hi"),
            new ChatMessage(2, t0, "assistant", "there", [new ChatSource("cap-1", t0, CaptureKind.Screen, "notepad.exe", "t", "")]),
        ], Approved: true));
        Assert.False(vm.ApplyRemoved(new RemovedEvent("removed", "purge", ["cap-other"])));
        Assert.Equal("c1", vm.ChatId);
        Assert.Equal(2, vm.Messages.Count);

        // 2) Cited capture removed resets the chat.
        Assert.True(vm.ApplyRemoved(new RemovedEvent("removed", "purge", ["cap-1"])));
        Assert.Null(vm.ChatId);
        Assert.Empty(vm.Messages);

        // 3) Pending approval mentioning a removed capture drops the card and rolls back the open turn.
        var apVm = new ChatViewModel();
        apVm.BeginSend("summarize this");
        apVm.Handle(new ChatEvent("approval", Token: "tok-a", Share: "text", Items:
        [
            new ChatApprovalItem("cap-2", t0, CaptureKind.Screen, "code.exe", "main.cs", "", "hello"),
        ]));
        Assert.True(apVm.HasApproval);
        Assert.True(apVm.ApplyRemoved(new RemovedEvent("removed", "purge", ["cap-2"])));
        Assert.False(apVm.HasApproval);
        Assert.Empty(apVm.Messages);
        Assert.False(apVm.Busy);

        // 4) "all" clears any loaded chat.
        var trVm = new ChatViewModel();
        trVm.Load(new ChatDetail(new ChatSummary("c2", "Keep?", t0, t0, 1), [new ChatMessage(1, t0, "user", "q")], Approved: true));
        Assert.True(trVm.ApplyRemoved(new RemovedEvent("removed", "all", [])));
        Assert.Empty(trVm.Messages);
    }

    [Theory]
    [InlineData("auth", "Chat_ErrAuth")]
    [InlineData("quota", "Chat_ErrQuota")]
    [InlineData("rate", "Chat_ErrRate")]
    [InlineData("overloaded", "Chat_ErrOverloaded")]
    [InlineData("context", "Chat_ErrContext")]
    [InlineData("model", "Chat_ErrModel")]
    [InlineData("offline", "Chat_ErrOffline")]
    [InlineData("timeout", "Chat_ErrTimeout")]
    [InlineData("budget", "Chat_ErrBudget")]
    [InlineData("refused", "Chat_ErrRefused")]
    [InlineData("locked", "Chat_ErrLocked")]
    [InlineData("stopped", "Chat_ErrStopped")]
    [InlineData("server", "Chat_ErrGeneric")]
    [InlineData(null, "Chat_ErrGeneric")]
    public void Error_titles(string? code, string key) => Assert.Equal(key, ChatViewModel.ErrorTitleKey(code));

    /// <summary>S4: lock and stop chat errors use localized message keys and lock classification.</summary>
    [Theory]
    [InlineData("locked", "Chat_ErrLockedBody", true)]
    [InlineData("STOPPED", "Chat_ErrStoppedBody", false)]
    [InlineData("quota", null, false)]
    public void Error_messages_and_lock_code(string code, string? messageKey, bool isLock)
    {
        Assert.Equal(messageKey, ChatViewModel.ErrorMessageKey(code));
        Assert.Equal(isLock, ChatViewModel.IsLockCode(code));
    }

    [Fact]
    public void Usage_parts()
    {
        Assert.Null(ChatViewModel.UsageParts(null));
        var p = ChatViewModel.UsageParts(new AiUsage("d", 12, 48211, 200, 0))!.Value;
        Assert.Equal(("12", "200", "48,211", "Chat_NoLimit"), p);
    }

    [Fact]
    public void Source_opens_as_a_capture()
    {
        var c = ChatViewModel.ToCaptureItem(new ChatSource("r1", T, CaptureKind.Replay, "game.exe", "t", "", "rep1", 4000));
        Assert.Equal("rep1", c.ReplayId);
        Assert.Equal(4000, c.ReplayOffsetMs);
        Assert.True(c.HasThumb);
        Assert.False(ChatViewModel.ToCaptureItem(new ChatSource("a1", T, CaptureKind.Audio, "teams.exe", "", "")).HasThumb);
    }
}

public sealed class MockChatTests
{
    private static async Task<MockHarness> Harness(MockOptions? o = null)
    {
        var h = new MockHarness(o);
        if (o?.StartLocked != true)
            Assert.True((await h.Api.SettingsSetAsync(new JsonObject { ["ai"] = new JsonObject { ["enabled"] = true } })).Ok);
        return h;
    }

    private static async Task<List<ChatEvent>> Stream(MockHarness h, string line)
    {
        await using var run = new AgentClient(h.Server.PipeName, MockHarness.OwnProcessTrust());
        var events = new List<ChatEvent>();
        var r = await AgentApi.ChatStreamAsync(run, line, events.Add);
        Assert.True(r.Ok, r.Error);
        return events;
    }

    [Fact]
    public async Task Approval_then_allow_then_same_chat_does_not_ask()
    {
        await using var h = await Harness();
        var ev = await Stream(h, AgentApi.ChatSendLine(null, "Acme invoice"));
        Assert.Equal(["start", "search", "approval"], ev.Select(e => e.Type));
        var ap = ev[^1];
        Assert.Equal(3, ap.Items!.Count);
        Assert.Contains(ap.Items, i => i.Image is { Length: > 0 });
        var chatId = ev[0].ChatId!;

        var done = await Stream(h, AgentApi.ChatApproveLine(ap.Token!, [ap.Items[0].Id]));
        Assert.Equal("done", done[^1].Type);
        var sources = Assert.Single(done, e => e.Type == "sources").Sources!;
        Assert.Equal(2, sources.Count);
        Assert.DoesNotContain(sources, s => s.Id == ap.Items[0].Id);
        Assert.True((await h.Api.ChatGetAsync(chatId)).Value!.Approved);

        var again = await Stream(h, AgentApi.ChatSendLine(chatId, "And the total?"));
        Assert.DoesNotContain(again, e => e.Type == "approval");
        Assert.Equal("done", again[^1].Type);

        var other = await Stream(h, AgentApi.ChatSendLine(null, "Something else"));
        Assert.Equal("approval", other[^1].Type);
        Assert.NotEqual(chatId, other[0].ChatId);
    }

    [Fact]
    public async Task Deny_answers_without_sources()
    {
        await using var h = await Harness();
        var ap = (await Stream(h, AgentApi.ChatSendLine(null, "Acme invoice")))[^1];
        var ev = await Stream(h, AgentApi.ChatDenyLine(ap.Token!));
        Assert.Equal("done", ev[^1].Type);
        Assert.DoesNotContain(ev, e => e.Type == "sources");
        Assert.Equal("token", (await Stream(h, AgentApi.ChatDenyLine(ap.Token!)))[^1].Code);   // single use
    }

    [Fact]
    public async Task Scripted_quota_and_rate_errors()
    {
        await using var h = await Harness();
        Assert.Equal("quota", (await Stream(h, AgentApi.ChatSendLine(null, "show the quota error")))[^1].Code);
        var rate = (await Stream(h, AgentApi.ChatSendLine(null, "rate limit please")))[^1];
        Assert.Equal("rate", rate.Code);
        Assert.NotNull(rate.RetryAt);
        Assert.Equal("budget", (await Stream(h, AgentApi.ChatSendLine(null, "budget")))[^1].Code);
    }

    [Fact]
    public async Task Saved_chats_list_rename_delete()
    {
        await using var h = await Harness();
        var list = (await h.Api.ChatListAsync()).Value!;
        var sample = Assert.Single(list);
        Assert.Equal("Flight to Tokyo", sample.Title);
        var detail = (await h.Api.ChatGetAsync(sample.Id)).Value!;
        Assert.Equal(2, detail.Messages.Count);
        Assert.NotEmpty(detail.Messages[1].Sources!);

        var ap = (await Stream(h, AgentApi.ChatSendLine(null, "Acme invoice")))[^1];
        await Stream(h, AgentApi.ChatApproveLine(ap.Token!, []));
        Assert.Equal(2, (await h.Api.ChatListAsync()).Value!.Count);

        Assert.True((await h.Api.ChatRenameAsync(sample.Id, "Trip")).Ok);
        Assert.Equal("Trip", (await h.Api.ChatGetAsync(sample.Id)).Value!.Chat.Title);
        Assert.True((await h.Api.ChatDeleteAsync(sample.Id)).Ok);
        Assert.False((await h.Api.ChatGetAsync(sample.Id)).Ok);
        Assert.True((await h.Api.ChatClearAsync()).Ok);
        Assert.Empty((await h.Api.ChatListAsync()).Value!);
    }

    [Fact]
    public async Task Off_and_locked_answer_one_error_line()
    {
        await using (var off = new MockHarness())
        {
            Assert.True((await off.Api.SettingsSetAsync(new JsonObject { ["ai"] = new JsonObject { ["enabled"] = false } })).Ok);
            await using var run = new AgentClient(off.Server.PipeName, MockHarness.OwnProcessTrust());
            var r = await AgentApi.ChatStreamAsync(run, AgentApi.ChatSendLine(null, "hi"), _ => Assert.Fail("no events"));
            Assert.False(r.Ok);
            Assert.Equal(AgentErrors.AiOff, r.Error);
        }
        await using var locked = new MockHarness(new MockOptions(StartLocked: true));
        Assert.False((await locked.Api.ChatListAsync()).Ok);
        await using var run2 = new AgentClient(locked.Server.PipeName, MockHarness.OwnProcessTrust());
        var r2 = await AgentApi.ChatStreamAsync(run2, AgentApi.ChatSendLine(null, "hi"), _ => Assert.Fail("no events"));
        Assert.False(r2.Ok);
    }

    [Fact]
    public async Task Ai_status_carries_usage()
    {
        await using var h = await Harness();
        var s = (await h.Api.AiStatusAsync()).Value!;
        Assert.NotNull(s.Usage);
        Assert.False(s.Local);
    }
}

public sealed class ChatSettingsTests
{
    /// <summary>Pins the UI SettingsSchema min/max bounds and ai.enabled gate for chat settings (cannot read Rust agent bounds directly).</summary>
    [Theory]
    [InlineData("ai.chatMaxHits", 1, 20)]
    [InlineData("ai.chatMaxImages", 0, 8)]
    [InlineData("ai.dailyRequests", 0, 100000)]
    [InlineData("ai.dailyTokens", 0, 1000000000)]
    public void Chat_settings_schema_ranges_and_gate(string key, double min, double max)
    {
        var d = SettingsSchema.Find(key)!;
        Assert.Equal(min, d.Min);
        Assert.Equal(max, d.Max);
        Assert.Equal("ai.enabled", d.EnabledWhenKey);
    }

    [Fact]
    public void Share_and_history_settings_schema_entries()
    {
        Assert.Equal(AiShare.All, SettingsSchema.Find("ai.share")!.Choices!);
        Assert.Equal("ai.enabled", SettingsSchema.Find("ai.chatHistory")!.EnabledWhenKey);
    }
}
