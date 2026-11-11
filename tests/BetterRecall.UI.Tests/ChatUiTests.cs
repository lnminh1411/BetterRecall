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
    [InlineData("server", "Chat_ErrGeneric")]
    [InlineData(null, "Chat_ErrGeneric")]
    public void Error_titles(string? code, string key) => Assert.Equal(key, ChatViewModel.ErrorTitleKey(code));

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
    public async Task Quota_warning_comes_before_the_answer()
    {
        await using var h = await Harness();
        var ap = (await Stream(h, AgentApi.ChatSendLine(null, "quota warning")))[^1];
        var ev = await Stream(h, AgentApi.ChatApproveLine(ap.Token!, []));
        var warn = ev.FindIndex(e => e is { Type: "warning", Code: "ratelimit" });
        Assert.True(warn >= 0);
        Assert.True(warn < ev.FindIndex(e => e.Type == "text"));
        Assert.Equal("done", ev[^1].Type);
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
    private static Dictionary<string, string> Strings()
    {
        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        while (dir != null && !File.Exists(Path.Combine(dir.FullName, "BetterRecall.slnx"))) dir = dir.Parent;
        var resw = System.Xml.Linq.XDocument.Load(Path.Combine(dir!.FullName, "src", "BetterRecall.UI", "Strings", "en-US", "Resources.resw"));
        return resw.Root!.Elements("data").ToDictionary(e => (string)e.Attribute("name")!, e => (string)e.Element("value")!);
    }

    [Theory]
    [InlineData("ai.chatMaxHits", 1, 20)]
    [InlineData("ai.chatMaxImages", 0, 8)]
    [InlineData("ai.dailyRequests", 0, 100000)]
    [InlineData("ai.dailyTokens", 0, 1000000000)]
    public void Ranges_match_the_agent(string key, double min, double max)
    {
        var d = SettingsSchema.Find(key)!;
        Assert.Equal(min, d.Min);
        Assert.Equal(max, d.Max);
        Assert.Equal("ai.enabled", d.EnabledWhenKey);
    }

    [Fact]
    public void Share_and_history_settings_have_strings()
    {
        Assert.Equal(AiShare.All, SettingsSchema.Find("ai.share")!.Choices!);
        Assert.Equal("ai.enabled", SettingsSchema.Find("ai.chatHistory")!.EnabledWhenKey);
        var s = Strings();
        foreach (var k in new[] { "share", "chatMaxHits", "chatMaxImages", "dailyRequests", "dailyTokens", "chatHistory" })
        {
            Assert.True(s.ContainsKey("S_ai_" + k), k);
            Assert.True(s.ContainsKey("S_ai_" + k + "_D"), k);
        }
        foreach (var c in AiShare.All) Assert.True(s.ContainsKey("S_ai_share_" + c), c);
        foreach (var k in new[] { "Nav_Chat.Content", "Chat_Usage", "Chat_NoLimit", "Chat_Searching", "Chat_ApprovalTitle", "Chat_AllowChat", "Chat_DontShare", "Chat_ErrGeneric" })
            Assert.True(s.ContainsKey(k), k);
        Assert.Contains("on-screen text reading", s["S_ai_share_D"]);
    }
}
