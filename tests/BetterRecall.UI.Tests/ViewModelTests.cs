// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

namespace BetterRecall.UI.Tests;

public class ViewModelTests
{
    private sealed class FixedClock(DateTimeOffset now) : TimeProvider
    {
        public override DateTimeOffset GetUtcNow() => now;
    }

    [Fact]
    public void BuildQuery_AppliesFilters()
    {
        var now = new DateTimeOffset(2026, 11, 10, 12, 0, 0, TimeSpan.Zero);
        var vm = new SearchViewModel(new AgentApi(new ScriptedClient(_ => Task.FromResult(AgentResult.Fail(AgentErrorKind.NotRunning)))), new FixedClock(now))
        {
            Query = "  acme invoice ",
            Range = TimeRange.LastHour,
            Kind = KindFilter.Screen,
            App = " chrome.exe ",
        };
        var q = vm.BuildQuery()!;
        Assert.Equal("acme invoice", q.Text);
        Assert.Equal(now.AddHours(-1), q.From);
        Assert.Null(q.To);
        Assert.Equal(CaptureKind.Screen, q.Kind);
        Assert.Equal("chrome.exe", q.App);

        vm.Query = "   ";
        Assert.Null(vm.BuildQuery());
    }

    private static SearchHit Hit(string id, CaptureKind kind, bool text, string app = "chrome.exe", string site = "", string? replay = null, int? offset = null) =>
        new(id, DateTimeOffset.UtcNow, kind, app, "t " + id, 0.5f, "", kind != CaptureKind.Audio, null, 0.5f, text, false, site, replay, offset);

    [Fact]
    public void Results_split_into_text_visual_audio_sections_in_rank_order()
    {
        var items = new[]
        {
            Hit("v1", CaptureKind.Screen, false), Hit("t1", CaptureKind.Screen, true), Hit("a1", CaptureKind.Audio, true),
            Hit("t2", CaptureKind.Screen, true), Hit("a2", CaptureKind.Audio, false),
        }.Select(h => CaptureItem.From(h, "q"));
        var groups = ResultGroups.Build(items);
        Assert.Equal([ResultGroupKind.Text, ResultGroupKind.Visual, ResultGroupKind.Audio], groups.Select(g => g.Kind));
        Assert.Equal(["t1", "t2"], groups[0].Select(i => i.Id));
        Assert.Equal(["a1", "a2"], groups[2].Select(i => i.Id));
        Assert.Equal("q", groups[0][0].Query);
    }

    [Fact]
    public void Replay_frames_go_to_the_replay_section_whatever_matched()
    {
        var items = new[]
        {
            Hit("t1", CaptureKind.Screen, true, "game.exe"), Hit("p1", CaptureKind.Replay, true, "game.exe", replay: "r_001", offset: 5000),
            Hit("p2", CaptureKind.Replay, false, "game.exe", replay: "r_001", offset: 10000), Hit("a1", CaptureKind.Audio, true, "game.exe"),
        }.Select(h => CaptureItem.From(h, "q"));
        var groups = ResultGroups.Build(items);
        var replay = Assert.Single(groups, g => g.Kind == ResultGroupKind.Replay);
        Assert.Equal(["p1", "p2"], replay.Select(i => i.Id));
        Assert.True(replay[0].IsReplay);
        Assert.True(replay[0].HasImage);
        Assert.Equal("r_001", replay[0].ReplayId);
        Assert.Equal(5000, replay[0].ReplayOffsetMs);
        Assert.DoesNotContain(groups.Where(g => g.Kind != ResultGroupKind.Replay).SelectMany(g => g), i => i.IsReplay);
    }

    [Theory]
    [InlineData("r_001@83000", "r_001", 83000, "replays:r_001@83000")]
    [InlineData("20261112T100000Z@0", "20261112T100000Z", 0, "replays:20261112T100000Z@0")]
    public void ReplayLink_roundtrips_and_clamps_negative_offset(string arg, string id, int ms, string expectedPage)
    {
        Assert.Equal((id, ms), ReplayLink.Parse(arg));
        Assert.Equal(expectedPage, ReplayLink.Page(id, ms));
        Assert.Equal("replays:r@0", ReplayLink.Page("r", -10));
    }

    [Theory]
    [InlineData(null)]
    [InlineData("r_001")]
    [InlineData("r_001@")]
    [InlineData("@5")]
    [InlineData("r_001@-5")]
    [InlineData("r 1@5")]
    [InlineData("..\\x@5")]
    public void ReplayLink_rejects(string? arg) => Assert.Null(ReplayLink.Parse(arg));

    [Fact]
    public void Sections_preview_one_row_and_show_all()
    {
        var g = new ResultGroup(ResultGroupKind.Text, Enumerable.Range(0, 20).Select(i => CaptureItem.From(Hit("t" + i, CaptureKind.Screen, true))).ToList());
        Assert.Equal(ResultGroup.Preview, g.Count);
        Assert.True(g.HasMore);
        Assert.True(g.RemoveWhere(i => i.Id is "t0" or "t15"));
        Assert.Equal(18, g.Total);
        Assert.Equal(ResultGroup.Preview, g.Count);
        Assert.Equal("t1", g[0].Id);
        g.ShowAll();
        Assert.Equal(18, g.Count);
        Assert.False(g.HasMore);
    }

    [Fact]
    public async Task Search_builds_sections_and_delete_removes_from_them()
    {
        var hits = new[] { Hit("t1", CaptureKind.Screen, true, site: "github.com"), Hit("v1", CaptureKind.Screen, false, site: "github.com"), Hit("a1", CaptureKind.Audio, true, "teams.exe") };
        var json = JsonSerializer.Serialize(new SearchResponse(3, false, 5, hits), ContractsJson.Default.SearchResponse);
        var client = new ScriptedClient(_ => Task.FromResult(AgentResult.Success(JsonDocument.Parse(json).RootElement)));
        var vm = new SearchViewModel(new AgentApi(client)) { Query = "acme", Site = "github.com" };
        await vm.SearchNowAsync();
        Assert.Contains("\"site\":\"github.com\"", client.Lines[0]);
        Assert.Equal(3, vm.Groups.Count);
        Assert.Equal(["t1", "v1", "a1"], vm.Results.Select(i => i.Id));
        Assert.Equal(0, vm.SelectedIndex);
        vm.SelectedIndex = 2;
        Assert.Equal(2, vm.RemoveWhere(i => i.Site == "github.com"));
        Assert.Single(vm.Groups);
        Assert.Equal(["a1"], vm.Results.Select(i => i.Id));
        Assert.Equal(0, vm.SelectedIndex);   // the selected hit kept its selection
    }

    /// <summary>K-review S2: degraded search message distinguishes cold-start timeout from unavailable visual search.</summary>
    [Theory]
    [InlineData(null, 0, "Search_NoResults")]
    [InlineData(null, 1, "Search_Count")]
    [InlineData("vector", 0, "Search_NoResultsTextOnly")]
    [InlineData("vector", 1, "Search_CountTextOnly")]
    [InlineData("vector-unavailable", 0, "Search_NoResultsVisualUnavailable")]
    [InlineData("vector-unavailable", 1, "Search_CountVisualUnavailable")]
    public async Task Degraded_search_message_matches_the_cause(string? degraded, int hits, string key)
    {
        SearchHit[] results = hits == 0 ? [] :
            [new("a1", DateTimeOffset.UtcNow, CaptureKind.Screen, "chrome.exe", "t", 0.5f, "", true, null, 0.5f, true, false, "")];
        var data = JsonSerializer.Serialize(new SearchResponse(hits, false, 3, results, degraded), ContractsJson.Default.SearchResponse);
        var vm = new SearchViewModel(new AgentApi(ScriptedClient.FixedOk(data))) { Query = "acme" };
        await vm.SearchNowAsync();
        Assert.Equal(key, vm.Message.Key);
        var text = TestResources.Strings()[key];
        if (degraded == "vector-unavailable") Assert.DoesNotContain("starting", text);
    }

    [Fact]
    public void Relative_time_and_badge()
    {
        var now = new DateTimeOffset(2026, 11, 11, 12, 0, 0, TimeSpan.Zero);
        Assert.Equal("Rel_Now", RelativeTime.Of(now.AddSeconds(-20), now).Key);
        Assert.Equal(new object?[] { 5 }, RelativeTime.Of(now.AddMinutes(-5), now).Args);
        Assert.Equal("Rel_Hours", RelativeTime.Of(now.AddHours(-3), now).Key);
        Assert.Equal("Rel_Days", RelativeTime.Of(now.AddDays(-6), now).Key);
        Assert.Equal("Rel_Date", RelativeTime.Of(now.AddDays(-40), now).Key);
        var web = CaptureItem.From(Hit("x", CaptureKind.Screen, true, "chrome.exe", "github.com"));
        Assert.Equal("github.com", web.BadgeText);
        Assert.Equal("G", web.BadgeInitial);
        Assert.Equal("Code", CaptureItem.From(Hit("y", CaptureKind.Screen, true, "Code.exe")).BadgeText);
    }

    [Fact]
    public async Task Debounce_OnlyLastQueryIsSent()
    {
        var client = new ScriptedClient(_ => Task.FromResult(AgentClient.Parse(Envelope.Ok("{\"count\":0,\"truncated\":false,\"tookMs\":1,\"results\":[]}"))));
        var vm = new SearchViewModel(new AgentApi(client), debounce: TimeSpan.FromMilliseconds(60));
        var tasks = new List<Task<bool>>();
        foreach (var q in new[] { "i", "in", "inv", "invoice" })
        {
            vm.Query = q;
            tasks.Add(vm.ScheduleAsync());
        }
        var ran = await Task.WhenAll(tasks);
        Assert.Equal([false, false, false, true], ran);
        Assert.Single(client.Lines);
        Assert.Contains("\"q\":\"invoice\"", client.Lines[0]);
        Assert.Equal("Search_NoResults", vm.Message.Key);
    }

    [Fact]
    public async Task StaleResponse_DoesNotOverwriteNewer()
    {
        var slow = new TaskCompletionSource<AgentResult>();
        var client = new ScriptedClient(line => line.Contains("\"q\":\"old\"")
            ? slow.Task
            : Task.FromResult(AgentClient.Parse(Envelope.Ok(
                "{\"count\":1,\"truncated\":false,\"tookMs\":1,\"results\":[{\"id\":\"f_new\",\"ts\":\"2026-11-10T00:00:00Z\",\"kind\":\"screen\",\"app\":\"a.exe\",\"title\":\"t\",\"score\":0.9,\"snippet\":\"s\",\"hasThumb\":true}]}"))));
        var vm = new SearchViewModel(new AgentApi(client));
        vm.Query = "old";
        var first = vm.SearchNowAsync();
        vm.Query = "new";
        await vm.SearchNowAsync();
        slow.SetResult(AgentClient.Parse(Envelope.Ok(
            "{\"count\":1,\"truncated\":false,\"tookMs\":1,\"results\":[{\"id\":\"f_old\",\"ts\":\"2026-11-10T00:00:00Z\",\"kind\":\"screen\",\"app\":\"a.exe\",\"title\":\"t\",\"score\":0.9,\"snippet\":\"s\",\"hasThumb\":true}]}")));
        await first;
        Assert.Equal("f_new", Assert.Single(vm.Results).Id);
        Assert.Equal(0, vm.SelectedIndex);
    }

    [Fact]
    public async Task Search_Locked_RaisesFailed()
    {
        var vm = new SearchViewModel(new AgentApi(new ScriptedClient(_ => Task.FromResult(AgentResult.FromEnvelope(false, default, "Store is locked.")))));
        AgentErrorKind? failed = null;
        vm.Failed += k => failed = k;
        vm.Query = "x";
        await vm.SearchNowAsync();
        Assert.Equal(AgentErrorKind.Locked, failed);
        Assert.Equal("Err_Locked", vm.Message.Key);
        Assert.Empty(vm.Results);
    }

    /// <summary>C6/D4: in-flight search, history, history commit, and facets answering after DropAll are discarded.</summary>
    [Fact]
    public async Task Search_history_commit_and_facets_answering_after_DropAll_are_discarded()
    {
        await using var h = new MockHarness();
        var gate = new GateClient(h.Client);
        var vm = new SearchViewModel(new AgentApi(gate)) { Query = "the" };
        var search = vm.SearchNowAsync();
        var history = vm.LoadHistoryAsync();
        var facets = vm.LoadFacetsAsync();
        var commit = vm.CommitHistoryAsync();
        Assert.True(vm.IsBusy);

        vm.DropAll();   // launcher hidden or locked mid-request
        gate.Gate.SetResult();
        await Task.WhenAll(search, history, facets, commit);

        Assert.Empty(vm.Results);
        Assert.Empty(vm.Groups);
        Assert.Empty(vm.History);
        Assert.Empty(vm.Facets.Apps);
        Assert.Empty(vm.Facets.Sites);
        Assert.False(vm.IsBusy);
        Assert.Equal(-1, vm.SelectedIndex);

        // Control: the same requests fill the lists when nothing dropped them.
        await vm.SearchNowAsync();
        await vm.LoadHistoryAsync();
        await vm.LoadFacetsAsync();
        Assert.NotEmpty(vm.Results);
        Assert.NotEmpty(vm.History);
        Assert.NotEmpty(vm.Facets.Apps);
    }

    [Fact]
    public async Task Detail_LoadsTextAndKeyframe()
    {
        await using var h = new MockHarness();
        var hit = (await h.Api.SearchAsync(new SearchQuery("invoice", 20))).Value!.Results.First(x => x.HasThumb && x.Kind == CaptureKind.Screen);
        var vm = new DetailViewModel(h.Api);
        await vm.LoadAsync(CaptureItem.From(hit));
        // Demo OCR text of the invoice screen, with the redacted card number already gone.
        Assert.Contains("Amount due", vm.Text);
        Assert.EndsWith("Pay by card:\nReply Forward", vm.Text);
        Assert.NotNull(vm.Image);
        Assert.False(vm.IsBusy);

        var audio = (await h.Api.SearchAsync(new SearchQuery("the", 50, Kind: CaptureKind.Audio))).Value!.Results[0];
        await vm.LoadAsync(CaptureItem.From(audio));
        Assert.Null(vm.Image);
        Assert.False(string.IsNullOrWhiteSpace(vm.Text));
    }

    [Fact]
    public async Task Timeline_CountsPerHour_AndFiltersByHour()
    {
        await using var h = new MockHarness();
        var vm = new TimelineViewModel(h.Api) { Day = DateOnly.FromDateTime(DateTime.Now.AddDays(-1)) };
        await vm.LoadAsync();
        Assert.Equal(vm.Items.Count, vm.HourCounts.Sum());
        Assert.True(vm.Items.Count > 0);
        int busiest = Array.IndexOf(vm.HourCounts, vm.HourCounts.Max());
        vm.SelectHour(busiest);
        Assert.Equal(vm.HourCounts[busiest], vm.Items.Count);
        Assert.All(vm.Items, i => Assert.Equal(busiest, i.Ts.ToLocalTime().Hour));
        vm.SelectHour(null);
        Assert.Equal(vm.HourCounts.Sum(), vm.Items.Count);
    }

    [Fact]
    public void Timeline_DayBounds_AreLocalMidnights()
    {
        var vm = new TimelineViewModel(new AgentApi(new ScriptedClient(_ => Task.FromResult(AgentResult.Fail(AgentErrorKind.NotRunning)))),
            TimeZoneInfo.CreateCustomTimeZone("t", TimeSpan.FromHours(7), "t", "t"));
        var (from, to) = vm.DayBounds(new DateOnly(2026, 11, 10));
        Assert.Equal(new DateTimeOffset(2026, 11, 9, 17, 0, 0, TimeSpan.Zero), from.ToUniversalTime());
        Assert.Equal(TimeSpan.FromDays(1), to - from);
    }

    /// <summary>C6/D4: in-flight timeline load answering after Clear is discarded.</summary>
    [Fact]
    public async Task Timeline_load_answering_after_Clear_is_discarded()
    {
        await using var h = new MockHarness();
        var gate = new GateClient(h.Client);
        var vm = new TimelineViewModel(new AgentApi(gate)) { Day = DateOnly.FromDateTime(DateTime.Now.AddDays(-1)) };
        var load = vm.LoadAsync();
        vm.Clear();
        gate.Gate.SetResult();
        await load;
        Assert.Empty(vm.Items);
        Assert.Equal(0, vm.HourCounts.Sum());
        Assert.False(vm.IsBusy);

        await vm.LoadAsync();
        Assert.NotEmpty(vm.Items);
    }

    /// <summary>D38: empty timeline day mentions pending captures waiting for indexing.</summary>
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

    /// <summary>B10 + A-L5: removed events drop matching rows from timeline and thumbnail cache.</summary>
    [Fact]
    public async Task Timeline_and_thumbnail_cache_drop_removed_rows()
    {
        var now = DateTimeOffset.UtcNow;
        var rows = new List<CaptureMeta>
        {
            new("s_b", now.AddMinutes(-1), CaptureKind.Screen, "bank.exe", "b", 0, 0, true, true),
            new("s_a", now.AddMinutes(-2), CaptureKind.Screen, "notepad.exe", "a", 0, 0, true, true),
            new("s_w", now.AddMinutes(-30), CaptureKind.Screen, "chrome.exe", "w", 0, 0, true, true),
        };
        var json = JsonSerializer.Serialize(new ItemsReply<CaptureMeta>(rows), ContractsJson.Default.ItemsReplyCaptureMeta);
        var vm = new TimelineViewModel(new AgentApi(ScriptedClient.FixedOk(json)));
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

    [Fact]
    public async Task ThumbnailCache_DedupesAndEvicts()
    {
        var cache = new ThumbnailCache(maxBytes: 250);
        int loads = 0;
        Task<byte[]?> Load(CancellationToken _) { Interlocked.Increment(ref loads); return Task.FromResult<byte[]?>(new byte[100]); }
        var a = cache.GetOrLoadAsync("a", 320, Load, default);
        var b = cache.GetOrLoadAsync("a", 320, Load, default);
        await Task.WhenAll(a, b);
        Assert.Equal(1, loads);
        await cache.GetOrLoadAsync("b", 320, Load, default);
        await cache.GetOrLoadAsync("c", 320, Load, default);   // evicts a (least recently used)
        Assert.Null(cache.TryGet("a", 320));
        Assert.NotNull(cache.TryGet("c", 320));
        Assert.Equal(200, cache.Bytes);
        cache.Clear();
        Assert.Equal(0, cache.Count);
    }

    [Fact]
    public async Task ThumbnailCache_CancelledLoadIsNotCached()
    {
        var cache = new ThumbnailCache(1000);
        using var cts = new CancellationTokenSource();
        cts.Cancel();
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() =>
            cache.GetOrLoadAsync("a", 320, async ct => { await Task.Delay(10, ct); return new byte[1]; }, cts.Token));
        Assert.Null(cache.TryGet("a", 320));
    }

    /// <summary>C6/D4 + S9: thumbnail fetch started before Clear or Remove does not refill the cache.</summary>
    [Theory]
    [InlineData(true)]
    [InlineData(false)]
    public async Task Thumbnail_load_finishing_after_Clear_or_Remove_does_not_refill_the_cache(bool clearAll)
    {
        var cache = new ThumbnailCache(maxBytes: 1000);
        var slow = new TaskCompletionSource<byte[]?>(TaskCreationOptions.RunContinuationsAsynchronously);
        var load = cache.GetOrLoadAsync("a", 320, _ => slow.Task, CancellationToken.None);
        if (clearAll) cache.Clear();
        else cache.Remove(["a"]);
        slow.SetResult(new byte[10]);
        Assert.NotNull(await load);   // the caller still gets its bytes (and drops them)
        Assert.Equal(0, cache.Count);
        Assert.Null(cache.TryGet("a", 320));

        await cache.GetOrLoadAsync("b", 320, _ => Task.FromResult<byte[]?>(new byte[10]), CancellationToken.None);
        Assert.NotNull(cache.TryGet("b", 320));   // loads after the invalidation are cached as before
    }

    [Theory]
    [InlineData(" delete everything ", "Delete everything", true)]
    [InlineData("delete", "Delete everything", false)]
    [InlineData("", "Delete everything", false)]
    public void PurgePhrase(string typed, string phrase, bool ok) => Assert.Equal(ok, PurgeFlow.PhraseMatches(typed, phrase));

    private sealed class Dialog { public int Hidden; }

    /// <summary>C2: dialogs showing decrypted content hide on lock (all) and when their owner window hides.</summary>
    [Fact]
    public void Open_dialogs_hide_on_lock_and_with_their_window()
    {
        var set = new OpenDialogs<Dialog>();
        object main = new(), launcher = new();
        var a = new Dialog();
        var b = new Dialog();
        var ha = set.Track(a, main);
        using (set.Track(b, launcher))
        {
            set.HideAll(d => d.Hidden++, main);       // main window hides
            Assert.Equal((1, 0), (a.Hidden, b.Hidden));
            set.HideAll(d => d.Hidden++);             // lock: every window
            Assert.Equal((2, 1), (a.Hidden, b.Hidden));
        }
        Assert.Equal(1, set.Count);                   // closed dialogs untracked
        ha.Dispose();
        set.HideAll(d => d.Hidden++);
        Assert.Equal((2, 1), (a.Hidden, b.Hidden));
        set.Track(new Dialog(), null).Dispose();
        set.HideAll(_ => throw new InvalidOperationException());   // a dialog already closing does not break the lock
    }

    [Fact]
    public void Args_Parse()
    {
        var a = AppArgs.Parse(AppArgs.SplitCommandLine("\"C:\\Program Files\\BetterRecall\\ui\\BetterRecall.UI.exe\" --mock-locked --page settings:security"));
        Assert.True(a.Mock);
        Assert.True(a.MockLocked);
        Assert.Equal("settings:security", a.Page);
        Assert.Equal("replays", AppArgs.Parse(["--replays"]).Page);
        Assert.Null(AppArgs.Parse([]).Page);

        var pw = AppArgs.Parse(["--mock-password"]);
        Assert.True(pw.Mock);
        Assert.True(pw.MockLocked);
        Assert.True(pw.MockPassword);

        Assert.Equal(new AppArgs(Mock: true, MockStorage: true), AppArgs.Parse(["--mock-storage"]));
    }
}
