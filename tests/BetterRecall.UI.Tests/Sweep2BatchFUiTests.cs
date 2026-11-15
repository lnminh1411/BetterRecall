// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;
using Xunit;

namespace BetterRecall.UI.Tests;

/// <summary>Bug sweep 2, batch F (UI): nothing decrypted comes back after a lock or hide (C6, D4), no counts while
/// locked (D37), late chat events after a reset are ignored (D4/D6).</summary>
public sealed class Sweep2BatchFUiTests
{
    /// <summary>Holds every request until the gate opens, then forwards it: an answer that arrives "late".</summary>
    private sealed class GateClient(IAgentClient inner) : IAgentClient
    {
        public TaskCompletionSource Gate { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public int ServerPid => inner.ServerPid;

        public async Task<AgentResult> SendAsync(string line, CancellationToken ct = default)
        {
            await Gate.Task;
            return await inner.SendAsync(line, ct);
        }
    }

    private static AgentStatus Status(bool locked) => new(true, false, null, null, locked, null, new AudioState(false, false), new QueueState(218), "NPU",
        new StoreStats(934, 49_400_000, 0, null, null, 4, 10), 30);

    [Fact]
    public void Status_line_shows_no_counts_while_locked()
    {
        var open = StatusText.Describe(Status(false), DateTimeOffset.UtcNow);
        Assert.Contains(open, p => p.Key == "Status_Store");
        Assert.Contains(open, p => p.Key == "Status_Queue");

        var locked = StatusText.Describe(Status(true), DateTimeOffset.UtcNow);
        Assert.Contains(locked, p => p.Key == "Status_Locked");
        Assert.DoesNotContain(locked, p => p.Key is "Status_Store" or "Status_Queue");

        // The UI saw "Store is locked." before the next status arrived.
        var uiLocked = StatusText.Describe(Status(false), DateTimeOffset.UtcNow, locked: true);
        Assert.Contains(uiLocked, p => p.Key == "Status_Locked");
        Assert.DoesNotContain(uiLocked, p => p.Key is "Status_Store" or "Status_Queue" or "Status_Unlocked");
    }

    [Fact]
    public async Task Search_history_and_facets_answering_after_DropAll_are_discarded()
    {
        await using var h = new MockHarness();
        var gate = new GateClient(h.Client);
        var vm = new SearchViewModel(new AgentApi(gate)) { Query = "the" };
        var search = vm.SearchNowAsync();
        var history = vm.LoadHistoryAsync();
        var facets = vm.LoadFacetsAsync();
        Assert.True(vm.IsBusy);

        vm.DropAll();   // launcher hidden or locked mid-request
        gate.Gate.SetResult();
        await Task.WhenAll(search, history, facets);

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
    public async Task History_commit_answering_after_DropAll_does_not_refill_the_list()
    {
        await using var h = new MockHarness();
        var gate = new GateClient(h.Client);
        var vm = new SearchViewModel(new AgentApi(gate)) { Query = "secret query" };
        var commit = vm.CommitHistoryAsync();
        vm.DropAll();
        gate.Gate.SetResult();
        await commit;
        Assert.Empty(vm.History);
    }

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

    [Fact]
    public async Task Thumbnail_load_finishing_after_Clear_does_not_refill_the_cache()
    {
        var cache = new ThumbnailCache(maxBytes: 1000);
        var slow = new TaskCompletionSource<byte[]?>(TaskCreationOptions.RunContinuationsAsynchronously);
        var load = cache.GetOrLoadAsync("a", 320, _ => slow.Task, default);
        cache.Clear();   // lock or hide while the request is out
        slow.SetResult(new byte[10]);
        Assert.NotNull(await load);   // the caller still gets its bytes (and drops them)
        Assert.Equal(0, cache.Count);
        Assert.Null(cache.TryGet("a", 320));

        await cache.GetOrLoadAsync("b", 320, _ => Task.FromResult<byte[]?>(new byte[10]), default);
        Assert.NotNull(cache.TryGet("b", 320));   // loads after the clear are cached as before
    }

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
}
