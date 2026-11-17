// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;

namespace BetterRecall.UI.Tests;

/// <summary>Bug sweep 4, batch U (control plane): UI side. The tests against the real agent processor (describe, unlock,
/// reset, removed events, the pipe cap) are in Core.Tests (Sweep4USeamTests), so this MIT project never links the backend.</summary>
public sealed class Sweep4UUiTests
{
    // ---- B2: describe preview and send on one connection

    [Fact]
    public async Task B2_mock_keeps_describe_tokens_per_connection_like_the_agent()
    {
        await using var h = new MockHarness();
        var id = (await h.Api.TimelineAsync(DateTimeOffset.UtcNow.AddDays(-30), DateTimeOffset.UtcNow, 50)).Value!
            .First(m => m.HasThumb && m.Kind == CaptureKind.Screen).Id;
        var p = await h.Api.DescribePreviewAsync(id);
        Assert.True(p.Ok, p.Error);
        await using (var other = new AgentClient(h.Server.PipeName, MockHarness.OwnProcessTrust()))
            Assert.Equal(AgentErrors.InvalidToken, (await new AgentApi(other).DescribeSendAsync(p.Value!.Token)).Error);

        await using var session = new DescribeSession(new AgentClient(h.Server.PipeName, MockHarness.OwnProcessTrust()));
        Assert.True((await session.PreviewAsync(id)).Ok);
        var sent = await session.SendAsync();
        Assert.True(sent.Ok, sent.Error);
        Assert.Contains("Mock description", sent.Value!.Text);
    }

    // ---- AB-S1: a slow request does not stall the shared connections

    [Fact]
    public async Task AB_S1_thumbnails_load_while_a_describe_preview_waits()
    {
        using var hold = new ManualResetEventSlim(false);
        await using var server = new FakeLineServer(line =>
        {
            if (line.StartsWith("describe preview", StringComparison.Ordinal)) hold.Wait(TimeSpan.FromSeconds(20));   // agent OCR redaction
            return line.StartsWith("thumb", StringComparison.Ordinal)
                ? Envelope.Ok("{\"id\":\"s_1\",\"mime\":\"image/webp\",\"width\":2,\"height\":2,\"bytes\":\"AAAA\"}")
                : Envelope.Error(AgentErrors.NoImage);
        });
        await using var media = new AgentClient(server.Name, new AcceptAll());
        await using var session = new DescribeSession(new AgentClient(server.Name, new AcceptAll()));
        var preview = session.PreviewAsync("s_1");
        await Task.Delay(200);
        var thumb = await new AgentApi(media).ThumbAsync("s_1").WaitAsync(TimeSpan.FromSeconds(5));
        Assert.True(thumb.Ok, thumb.Error);
        Assert.False(preview.IsCompleted);
        hold.Set();
        await preview.WaitAsync(TimeSpan.FromSeconds(5));
    }

    [Fact]
    public async Task AB_S1_one_shot_search_does_not_hold_the_main_connection()
    {
        using var hold = new ManualResetEventSlim(false);
        await using var server = new FakeLineServer(line =>
        {
            if (line.StartsWith("search", StringComparison.Ordinal)) { hold.Wait(TimeSpan.FromSeconds(20)); return Envelope.Ok("{\"count\":0,\"truncated\":false,\"tookMs\":1,\"results\":[]}"); }
            return Envelope.Ok("{\"pong\":true}");
        });
        await using var main = new AgentClient(server.Name, new AcceptAll());
        var oneShot = new OneShotClient(() => new AgentClient(server.Name, new AcceptAll()));
        AgentErrorKind? classified = null;
        oneShot.ResponseClassified += k => classified = k;
        var search = new AgentApi(oneShot).SearchAsync(new SearchQuery("acme"));
        await Task.Delay(200);
        Assert.True((await main.SendAsync("ping").WaitAsync(TimeSpan.FromSeconds(5))).Ok);   // settings, timeline, unlock go on
        Assert.False(search.IsCompleted);
        hold.Set();
        Assert.True((await search.WaitAsync(TimeSpan.FromSeconds(5))).Ok);
        Assert.Equal(AgentErrorKind.None, classified);
    }

    private sealed class AcceptAll : IServerTrust
    {
        public TrustCheck Verify(Microsoft.Win32.SafeHandles.SafePipeHandle pipe) => new(true, Environment.ProcessId);
    }

    // ---- B1 + B6 + B8 + B13: the unlock screen follows the agent

    [Theory]
    [InlineData("passphrase", "auto", "hello", UnlockMode.Passphrase)]
    [InlineData("password", "auto", "hello", UnlockMode.Password)]      // B13: no Hello, the password stands in
    [InlineData("hello", "auto", "password", UnlockMode.Hello)]         // B8/B15: no password file, Hello stands in
    [InlineData("hello", "passphrase", "hello", UnlockMode.Hello)]
    [InlineData(null, "passphrase", "hello", UnlockMode.Passphrase)]    // an agent without the field: settings decide
    [InlineData("none", "auto", "password", UnlockMode.Password)]
    public void B1_unlock_mode_comes_from_the_status_field(string? needs, string keyProtection, string method, UnlockMode expected) =>
        Assert.Equal(expected, UnlockLogic.Mode(needs, keyProtection, method, keyNeedsPassphrase: false));

    [Fact]
    public void B13_hello_missing_answer_asks_for_the_password()
    {
        Assert.True(UnlockLogic.AsksForPassword(AgentErrors.HelloMissing));
        Assert.False(UnlockLogic.AsksForPassword(AgentErrors.UnlockCancelled));
        Assert.Equal("Err_HelloMissing", SearchViewModel.ServerError(AgentErrors.HelloMissing).Key);   // translated, not raw
        Assert.Equal("Err_SettingsUnreadable", SearchViewModel.ServerError(AgentErrors.SettingsUnreadable).Key);
    }

    [Theory]
    [InlineData(1, false, true)]    // UI started before the agent: InitAsync got nothing (B8)
    [InlineData(1, true, false)]    // InitAsync already loaded them
    [InlineData(2, true, true)]     // reconnect: the agent may have restarted (D2)
    public void B8_settings_reload_on_connect(int connects, bool loaded, bool expected) =>
        Assert.Equal(expected, SettingsReload.OnConnect(connects, loaded));

    [Fact]
    public async Task B8_mock_status_carries_unlock_needs()
    {
        await using var h = new MockHarness(new MockOptions(StartLocked: true, PasswordUnlock: true));
        var st = (await h.Api.StatusAsync()).Value!;
        Assert.True(st.Locked);
        Assert.Equal("password", st.UnlockNeeds);
        await using var pass = new MockHarness(new MockOptions(StartLocked: true, Passphrase: true));
        st = (await pass.Api.StatusAsync()).Value!;
        Assert.True(st.Locked);
        Assert.Equal("passphrase", st.UnlockNeeds);
        Assert.True((await pass.Api.UnlockAsync("any passphrase")).Ok);
        Assert.Equal("none", (await pass.Api.StatusAsync()).Value!.UnlockNeeds);
    }

    // ---- B3: warnings reach the UI

    [Fact]
    public void B3_warnings_map_to_one_banner_each()
    {
        var b = WarningBanners.For(["settings-unreadable", "ner-unavailable", "ocr-failing", "vc-runtime-missing", "ner-unavailable", "new-code"]);
        Assert.Equal(["settings-unreadable", "ner-unavailable", "ocr-failing", "vc-runtime-missing", "new-code"], b.Select(x => x.Code));
        Assert.True(b[0].CanReset && b[0].Error);
        Assert.True(b[1].Error);   // names stored unredacted
        Assert.Equal("Warn_OtherTitle", b[4].TitleKey);
        Assert.Empty(WarningBanners.For(null));
        Assert.True(WarningBanners.Same(null, []));
        Assert.False(WarningBanners.Same(["ocr-failing"], null));
    }

    // ---- B10 + A-L5: rows gone reach the UI

    [Fact]
    public async Task B10_timeline_and_thumbnail_cache_drop_removed_rows()
    {
        var now = DateTimeOffset.UtcNow;
        var rows = new List<CaptureMeta>
        {
            new("s_b", now.AddMinutes(-1), CaptureKind.Screen, "bank.exe", "b", 0, 0, true, true),
            new("s_a", now.AddMinutes(-2), CaptureKind.Screen, "notepad.exe", "a", 0, 0, true, true),
            new("s_w", now.AddMinutes(-30), CaptureKind.Screen, "chrome.exe", "w", 0, 0, true, true),
        };
        var json = JsonSerializer.Serialize(new ItemsReply<CaptureMeta>(rows), ContractsJson.Default.ItemsReplyCaptureMeta);
        var vm = new TimelineViewModel(new AgentApi(new FixedLine(json)));
        vm.Day = DateOnly.FromDateTime(DateTime.Now);
        await vm.LoadAsync();
        Assert.Equal(3, vm.Items.Count);
        Assert.Equal(1, vm.RemoveWhere(RemovedMatch.For(new RemovedEvent("removed", "purge", ["s_b"]))));
        Assert.DoesNotContain(vm.Items, i => i.Id == "s_b");
        Assert.Equal(1, vm.RemoveWhere(RemovedMatch.For(new RemovedEvent("removed", "purge", [], now.AddMinutes(-40), now.AddMinutes(-20)))));
        Assert.Equal(["s_a"], vm.Items.Select(i => i.Id));
        Assert.Equal(1, vm.HourCounts.Sum());

        var cache = new ThumbnailCache(1 << 20);
        cache.Put("s_a", 320, [1, 2]);
        cache.Put("s_a", 1280, [1, 2, 3]);
        cache.Put("s_b", 320, [4]);
        cache.Remove(["s_a"]);
        Assert.Null(cache.TryGet("s_a", 320));
        Assert.Null(cache.TryGet("s_a", 1280));
        Assert.NotNull(cache.TryGet("s_b", 320));
        Assert.Equal(1, cache.Bytes);
    }

    // ---- review follow-ups

    [Fact]
    public async Task S9_thumbnail_fetch_started_before_a_removal_does_not_put_it_back()
    {
        var cache = new ThumbnailCache(1 << 20);
        var release = new TaskCompletionSource<byte[]?>(TaskCreationOptions.RunContinuationsAsynchronously);
        var load = cache.GetOrLoadAsync("s_a", 320, _ => release.Task, CancellationToken.None);   // the fetch began before the row went away
        await Task.Delay(50);
        cache.Remove(["s_a"]);
        release.SetResult([1, 2]);
        Assert.NotNull(await load);
        Assert.Null(cache.TryGet("s_a", 320));
    }

    [Fact]
    public void R_replay_text_unchecked_has_its_own_banner()
    {
        var b = Assert.Single(WarningBanners.For(["replay-text-unchecked"]));
        Assert.Equal("Warn_ReplayTextTitle", b.TitleKey);
        Assert.True(b.Error);
        Assert.False(b.CanReset);
    }

    [Fact]
    public async Task P_purge_all_reports_replay_files_left_for_the_next_start()
    {
        var r = Responses.PurgeResult(JsonDocument.Parse("{\"purged\":12,\"undeleted\":2}").RootElement);
        Assert.Equal(new PurgeResult(12, 2), r);
        Assert.Equal(["Danger_AllDone", "Danger_AllUndeleted"], PurgeFlow.AllDoneText(r).Select(t => t.Key));
        Assert.Equal(["Danger_AllDone"], PurgeFlow.AllDoneText(Responses.PurgeResult(JsonDocument.Parse("{\"purged\":3}").RootElement)).Select(t => t.Key));
        Assert.Equal(["Danger_AllDone"], PurgeFlow.AllDoneText(new PurgeResult(3, 0)).Select(t => t.Key));
        // And over the mock, end to end.
        await using var h = new MockHarness();
        var all = await PurgeFlow.PurgeAllAsync(h.Api, null);
        Assert.True(all.Ok, all.Error);
    }

    [Fact]
    public async Task M1_one_shot_falls_back_to_the_main_connection_when_the_pipe_is_busy()
    {
        await using var server = new FakeLineServer(_ => Envelope.Ok("{\"count\":0,\"truncated\":false,\"tookMs\":1,\"results\":[]}"));
        await using var main = new AgentClient(server.Name, new AcceptAll());
        var busy = new OneShotClient(() => new AgentClient("BetterRecall.nobody." + Guid.NewGuid().ToString("N"), new AcceptAll(), TimeSpan.FromMilliseconds(100)), main);
        AgentErrorKind? classified = null;
        busy.ResponseClassified += k => classified = k;
        Assert.True((await new AgentApi(busy).SearchAsync(new SearchQuery("acme"))).Ok);   // read-only: answered on main
        Assert.Null(classified);   // no "agent not running" from the one-shot side
        var write = await busy.SendAsync("pause");
        Assert.Equal(AgentErrorKind.NotRunning, write.Kind);   // a change is never sent twice
    }

    private sealed class FixedLine(string data) : IAgentClient
    {
        public int ServerPid => 1;
        public Task<AgentResult> SendAsync(string line, CancellationToken ct = default) =>
            Task.FromResult(AgentClient.Parse(Envelope.Ok(data)));
    }
}
