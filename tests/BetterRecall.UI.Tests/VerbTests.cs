// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;

namespace BetterRecall.UI.Tests;

/// <summary>Every UI verb through the real client, against the mock agent on a real pipe.</summary>
public class VerbTests
{
    [Fact]
    public async Task Version_Status_Ping()
    {
        await using var h = new MockHarness();
        var v = await h.Api.VersionAsync();
        Assert.True(v.Ok);
        Assert.Equal(1, v.Value!.Protocol);
        var s = await h.Api.StatusAsync();
        Assert.True(s.Ok);
        Assert.False(s.Value!.Locked);
        Assert.True(s.Value.Store.Rows > 100);
        Assert.Equal("NPU", s.Value.Device);
    }

    [Fact]
    public async Task Search_Get_Thumb_Keyframe()
    {
        await using var h = new MockHarness();
        var r = await h.Api.SearchAsync(new SearchQuery("invoice", 5));
        Assert.True(r.Ok, r.Error);
        Assert.InRange(r.Value!.Results.Count, 1, 5);
        var hit = r.Value.Results.First(x => x.Kind == CaptureKind.Screen && x.HasThumb);
        Assert.Contains("invoice", (hit.Title + hit.Snippet).ToLowerInvariant());

        var d = await h.Api.GetAsync(hit.Id);
        Assert.True(d.Ok);
        Assert.Equal(hit.Id, d.Value!.Id);

        var t = await h.Api.ThumbAsync(hit.Id, 320);
        Assert.True(t.Ok);
        Assert.Equal(320, t.Value!.Width);
        Assert.Equal(0x89, t.Value.Bytes[0]);

        var k = await h.Api.KeyframeAsync(hit.Id);
        Assert.True(k.Ok);
        Assert.Equal(1280, k.Value!.Width);

        Assert.Equal(AgentErrors.NotFound, (await h.Api.GetAsync("nope")).Error);
    }

    [Fact]
    public async Task Search_Filters_KindAndApp()
    {
        await using var h = new MockHarness();
        var audio = await h.Api.SearchAsync(new SearchQuery("the", 50, Kind: CaptureKind.Audio));
        Assert.True(audio.Ok);
        Assert.NotEmpty(audio.Value!.Results);
        Assert.All(audio.Value.Results, x => Assert.Equal(CaptureKind.Audio, x.Kind));
        Assert.All(audio.Value.Results, x => Assert.NotNull(x.DurationS));

        var app = await h.Api.SearchAsync(new SearchQuery("code", 50, App: "Code.exe"));
        Assert.All(app.Value!.Results, x => Assert.Equal("Code.exe", x.App));
    }

    [Fact]
    public async Task Timeline_NewestFirst_WithinRange()
    {
        await using var h = new MockHarness();
        var to = DateTimeOffset.UtcNow;
        var from = to.AddDays(-1);
        var r = await h.Api.TimelineAsync(from, to, 100);
        Assert.True(r.Ok, r.Error);
        Assert.InRange(r.Value!.Count, 1, 100);
        Assert.All(r.Value, m => Assert.InRange(m.Ts, from, to));
        for (int i = 1; i < r.Value.Count; i++) Assert.True(r.Value[i - 1].Ts >= r.Value[i].Ts);
    }

    [Fact]
    public async Task Locked_DataVerbsFail_UntilUnlock()
    {
        await using var h = new MockHarness(new MockOptions(StartLocked: true));
        Assert.True((await h.Api.SearchAsync(new SearchQuery("invoice"))).IsLocked);
        Assert.True((await h.Api.TimelineAsync(DateTimeOffset.UtcNow.AddDays(-1), DateTimeOffset.UtcNow)).IsLocked);
        Assert.True((await h.Api.AuditAsync()).IsLocked);
        Assert.True((await h.Api.ReplayListAsync()).IsLocked);
        Assert.True((await h.Api.ThumbAsync("f_000001")).IsLocked);
        Assert.True((await h.Api.StatusAsync()).Ok);          // not a data verb
        Assert.True((await h.Api.SettingsGetAsync()).Ok);     // not a data verb

        var u = await h.Api.UnlockAsync();
        Assert.True(u.Ok);
        Assert.True(u.Value!.Unlocked);
        Assert.NotNull(u.Value.Until);
        Assert.True((await h.Api.SearchAsync(new SearchQuery("invoice"))).Ok);

        Assert.True((await h.Api.LockAsync()).Ok);
        Assert.True((await h.Api.SearchAsync(new SearchQuery("invoice"))).IsLocked);
    }

    [Fact]
    public async Task PassphraseMode_WrongPassphraseIsCancelled()
    {
        await using var h = new MockHarness(new MockOptions(StartLocked: true, Passphrase: true));
        var s = await h.Api.SettingsGetAsync();
        Assert.True(UnlockLogic.UsesPassphrase(SettingsSchema.Get(s.Value!, "security.keyProtection")!.GetValue<string>()));
        var bad = await h.Api.UnlockAsync(MockAgent.WrongPassphrase);
        Assert.Equal(AgentErrors.WrongPassphrase, bad.Error);
        var good = await h.Api.UnlockAsync("correct horse battery staple");
        Assert.True(good.Ok);
    }

    [Fact]
    public async Task Consent_SetsConsented()
    {
        await using var h = new MockHarness(new MockOptions(FirstRun: true));
        var before = await h.Api.SettingsGetAsync();
        Assert.False(SettingsSchema.Get(before.Value!, "capture.consented")!.GetValue<bool>());
        Assert.Equal("consent", (await h.Api.StatusAsync()).Value!.PauseReason);
        Assert.True((await h.Api.ConsentAsync()).Ok);
        var after = await h.Api.SettingsGetAsync();
        Assert.True(SettingsSchema.Get(after.Value!, "capture.consented")!.GetValue<bool>());
    }

    [Fact]
    public async Task SettingsSet_MergesAndValidates()
    {
        await using var h = new MockHarness();
        var ok = await h.Api.SettingsSetAsync(new JsonObject { ["capture"] = new JsonObject { ["intervalMs"] = 3000 } });
        Assert.True(ok.Ok, ok.Error);
        Assert.Equal(3000, SettingsSchema.Get(ok.Value!, "capture.intervalMs")!.GetValue<int>());
        Assert.Equal(1280, SettingsSchema.Get(ok.Value!, "capture.maxWidth")!.GetValue<int>());

        var bad = await h.Api.SettingsSetAsync(new JsonObject { ["capture"] = new JsonObject { ["intervalMs"] = 10 } });
        Assert.Equal("Invalid capture.intervalMs.", bad.Error);
        Assert.Equal("capture.intervalMs", AgentErrors.InvalidKey(bad.Error));
        var unknown = await h.Api.SettingsSetAsync(new JsonObject { ["capture"] = new JsonObject { ["nope"] = 1 } });
        Assert.Equal("Invalid capture.nope.", unknown.Error);

        var now = await h.Api.SettingsGetAsync();
        Assert.Equal(3000, SettingsSchema.Get(now.Value!, "capture.intervalMs")!.GetValue<int>());
    }

    [Fact]
    public async Task Replays_SaveListExportDelete()
    {
        await using var h = new MockHarness();
        var list = await h.Api.ReplayListAsync();
        Assert.True(list.Ok);
        int n = list.Value!.Count;
        var saved = await h.Api.ReplaySaveAsync();
        Assert.True(saved.Ok);
        Assert.Equal(120_000, saved.Value!.DurationMs);
        Assert.Equal(n + 1, (await h.Api.ReplayListAsync()).Value!.Count);
        Assert.True((await h.Api.ReplayExportAsync(saved.Value.Id, @"C:\Users\you\Videos\my clip.mp4")).Ok);
        Assert.Equal(AgentErrors.NotFound, (await h.Api.ReplayExportAsync("r_missing", @"C:\x.mp4")).Error);
        Assert.True((await h.Api.ReplayDeleteAsync(saved.Value.Id)).Ok);
        Assert.Equal(n, (await h.Api.ReplayListAsync()).Value!.Count);
    }

    [Fact]
    public async Task Purge_RangeAppAndAll()
    {
        await using var h = new MockHarness();
        var to = DateTimeOffset.UtcNow;
        var p = await h.Api.PurgeRangeAsync(to.AddHours(-6), to);
        Assert.True(p.Ok, p.Error);
        Assert.NotNull(p.Value!.Rows);
        var done = await h.Api.PurgeConfirmAsync(p.Value.Confirm);
        Assert.Equal(p.Value.Rows, done.Value);
        Assert.Equal(AgentErrors.InvalidToken, (await h.Api.PurgeConfirmAsync(p.Value.Confirm)).Error);

        var a = await h.Api.PurgeAppAsync("Spotify.exe");
        Assert.True(a.Ok);

        // purge all needs a recent unlock; the flow does it
        Assert.Equal(AgentErrors.UnlockRequired, (await h.Api.PurgeAllAsync()).Error);
        var all = await PurgeFlow.PurgeAllAsync(h.Api, null);
        Assert.True(all.Ok, all.Error);
        Assert.Equal(0, (await h.Api.StatusAsync()).Value!.Store.Rows);
    }

    [Fact]
    public async Task Panic_Pause_Resume_Audit_Bench()
    {
        await using var h = new MockHarness();
        Assert.True((await h.Api.PanicAsync()).Ok);
        Assert.True((await h.Api.PauseAsync(60)).Ok);
        var s = (await h.Api.StatusAsync()).Value!;
        Assert.True(s.Paused);
        Assert.NotNull(s.PausedUntil);
        Assert.True((await h.Api.ResumeAsync()).Ok);
        Assert.False((await h.Api.StatusAsync()).Value!.Paused);

        var audit = await h.Api.AuditAsync(10);
        Assert.True(audit.Ok);
        Assert.Equal(10, audit.Value!.Count);

        var bench = await h.Api.BenchAsync();
        Assert.True(bench.Ok);
        Assert.Equal("NPU", bench.Value!.Chosen);
        Assert.Equal(3, bench.Value.Devices.Length);
    }

    [Fact]
    public async Task UnknownVerb_IsServerError()
    {
        await using var h = new MockHarness();
        var r = await h.Client.SendAsync("frobnicate");
        Assert.Equal(AgentErrorKind.Server, r.Kind);
        Assert.Equal("Unknown command 'frobnicate'.", r.Error);
    }

    [Fact]
    public async Task Subscribe_StreamsStatusChanges()
    {
        await using var h = new MockHarness();
        await using var sub = new StatusSubscription(h.Server.PipeName, MockHarness.OwnProcessTrust());
        var got = new TaskCompletionSource<AgentStatus>(TaskCreationOptions.RunContinuationsAsynchronously);
        sub.StatusReceived += s => { if (s.Paused) got.TrySetResult(s); };
        sub.Start();
        await Task.Delay(300);
        await h.Api.PauseAsync();
        var status = await got.Task.WaitAsync(TimeSpan.FromSeconds(5));
        Assert.Equal("user", status.PauseReason);
    }
}
