// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;

namespace BetterRecall.UI.Tests;

public class ViewModelTests
{
    private sealed class FixedClock(DateTimeOffset now) : TimeProvider
    {
        public override DateTimeOffset GetUtcNow() => now;
    }

    private sealed class ScriptedClient(Func<string, Task<AgentResult>> answer) : IAgentClient
    {
        public List<string> Lines { get; } = [];
        public int ServerPid => 1;
        public Task<AgentResult> SendAsync(string line, CancellationToken ct = default)
        {
            lock (Lines) Lines.Add(line);
            return answer(line);
        }
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

    [Fact]
    public void RangeBounds_Today_StartsAtLocalMidnight()
    {
        var now = DateTimeOffset.Now;
        var (from, _) = SearchViewModel.RangeBounds(TimeRange.Today, now);
        Assert.Equal(now.ToLocalTime().Date, from!.Value.ToLocalTime().DateTime);
        Assert.Equal((null, null), SearchViewModel.RangeBounds(TimeRange.Any, now));
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
            await Task.Delay(5);
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

    [Theory]
    [InlineData(-1, 10, 1, 0)]
    [InlineData(-1, 10, -1, -1)]
    [InlineData(0, 10, -4, 0)]
    [InlineData(2, 10, 4, 6)]
    [InlineData(8, 10, 4, 9)]
    [InlineData(0, 0, 1, -1)]
    public void Move_ClampsSelection(int current, int count, int delta, int expected) =>
        Assert.Equal(expected, SearchViewModel.Move(current, count, delta));

    [Fact]
    public void ServerErrors_MapToResources()
    {
        Assert.Equal("Err_NotRunning", SearchViewModel.ErrorText(AgentErrorKind.NotRunning, null).Key);
        Assert.Equal("Err_NotTrusted", SearchViewModel.ErrorText(AgentErrorKind.NotTrusted, null).Key);
        Assert.Equal("Err_NotFound", SearchViewModel.ServerError("Not found.").Key);
        var inv = SearchViewModel.ServerError("Invalid retention.days.");
        Assert.Equal("Err_InvalidKey", inv.Key);
        Assert.Equal("retention.days", inv.Args[0]);
        Assert.Equal("Err_Raw", SearchViewModel.ServerError("Rate limit exceeded.").Key);
    }

    [Fact]
    public async Task Detail_LoadsTextAndKeyframe()
    {
        await using var h = new MockHarness();
        var hit = (await h.Api.SearchAsync(new SearchQuery("invoice", 20))).Value!.Results.First(x => x.HasThumb && x.Kind == CaptureKind.Screen);
        var vm = new DetailViewModel(h.Api);
        await vm.LoadAsync(CaptureItem.From(hit));
        Assert.Equal(hit.Title, vm.Text);
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

    [Theory]
    [InlineData(" delete everything ", "Delete everything", true)]
    [InlineData("delete", "Delete everything", false)]
    [InlineData("", "Delete everything", false)]
    public void PurgePhrase(string typed, string phrase, bool ok) => Assert.Equal(ok, PurgeFlow.PhraseMatches(typed, phrase));

    [Fact]
    public void StatusText_DescribesPausedAndLocked()
    {
        var s = new AgentStatus(false, true, null, "fullscreen", true, null, new AudioState(true, false), new QueueState(4), "NPU",
            new StoreStats(10, 2048, 0, null, null, 4, 10), 30);
        var parts = StatusText.Describe(s, DateTimeOffset.UtcNow);
        Assert.Equal("Status_Paused", parts[0].Key);
        Assert.Equal("Reason_fullscreen", ((LocalizedText)parts[0].Args[0]).Key);
        Assert.Contains(parts, p => p.Key == "Status_Locked");
        Assert.Contains(parts, p => p.Key == "Status_AudioSystem");
        Assert.Contains(parts, p => p.Key == "Status_Queue");
        Assert.Equal("2 KB", StatusText.FormatBytes(2048));
        Assert.Equal("2:00", StatusText.FormatDuration(120_000));
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
    }
}
