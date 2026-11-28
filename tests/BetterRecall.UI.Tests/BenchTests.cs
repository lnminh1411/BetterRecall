// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;

namespace BetterRecall.UI.Tests;

public class BenchTests
{
    private static BenchViewModel Vm(MockHarness h) =>
        new(h.Api, () => new AgentClient(h.Server.PipeName, MockHarness.OwnProcessTrust()));

    [Fact]
    public async Task BenchLast_ReturnsDevPcReport()
    {
        await using var h = new MockHarness();
        var r = await h.Api.BenchLastAsync();
        Assert.True(r.Ok, r.Error);
        var rep = r.Value!;
        Assert.Equal(3, rep.Hardware.Gpus.Length);
        Assert.Single(rep.Hardware.Gpus, g => g.Integrated);
        Assert.Equal(3, rep.Hardware.Monitors.Length);
        Assert.NotNull(rep.Hardware.Npu);
        Assert.Contains(rep.Encoders, e => !e.Ok && e.Error is not null);
        Assert.Contains(rep.Encoders, e => e.Ok && e.Codec == "av1" && e.Api == "vpl");
        Assert.Equal(9, rep.Inference!.Tokens!.Length);
        Assert.Contains(rep.Recommendations, x => x.Key == "replay.encoder" && x.Value == "\"av1\"" && x.Current == "\"auto\"");
    }

    [Fact]
    public async Task BenchLast_FirstRun_IsNotFound_AndViewModelShowsNone()
    {
        await using var h = new MockHarness(new MockOptions(FirstRun: true));
        Assert.Equal(AgentErrors.NotFound, (await h.Api.BenchLastAsync()).Error);
        var vm = Vm(h);
        await vm.LoadLastAsync();
        Assert.Null(vm.Report);
        Assert.Equal("BenchPage_None", vm.Message.Key);
    }

    [Fact]
    public async Task Run_StoresReport_AndBuildsRows()
    {
        await using var h = new MockHarness(new MockOptions(FirstRun: true));
        var vm = Vm(h);
        Assert.True(await vm.RunAsync(default));
        Assert.False(vm.IsRunning);
        Assert.NotEmpty(vm.Recommendations);
        Assert.Equal(3, vm.Inference.Count);
        var npu = vm.Inference.Single(i => i.Chosen);
        Assert.Equal("NPU", npu.Device);
        Assert.NotNull(npu.Ms280);
        Assert.True((await h.Api.BenchLastAsync()).Ok);
    }

    [Fact]
    public async Task Run_Cancel_ClosesConnectionQuickly()
    {
        await using var h = new MockHarness();
        h.Agent.SlowVerbDelay = TimeSpan.FromMilliseconds(400);   // bench full = 6 s
        var vm = Vm(h);
        using var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(150));
        var sw = System.Diagnostics.Stopwatch.StartNew();
        Assert.False(await vm.RunAsync(cts.Token));
        Assert.True(sw.ElapsedMilliseconds < 2000);
        Assert.Equal("BenchPage_Cancelled", vm.Message.Key);
        Assert.False(vm.IsRunning);
        Assert.True((await h.Api.StatusAsync()).Ok);   // the main connection is untouched
    }

    [Fact]
    public async Task Apply_SendsOnlySelectedKeys_InOneRequest()
    {
        await using var h = new MockHarness();
        var recorder = ScriptedClient.Recording(h.Client);
        var vm = new BenchViewModel(new AgentApi(recorder), () => h.Client);
        await vm.LoadLastAsync();
        var keys = vm.Recommendations.Select(r => r.Key).ToList();
        Assert.Contains("replay.fps", keys);
        foreach (var row in vm.Recommendations) row.Selected = row.Key is "replay.fps" or "replay.encoder";

        var r = await vm.ApplyAsync();
        Assert.True(r!.Value.Ok, r.Value.Error);
        var line = Assert.Single(recorder.Lines, l => l.StartsWith("settings set", StringComparison.Ordinal));
        Assert.Equal("settings set {\"replay\":{\"encoder\":\"av1\",\"fps\":60}}", line);
        Assert.DoesNotContain(vm.Recommendations, x => x.Key is "replay.fps" or "replay.encoder");
        var s = (await h.Api.SettingsGetAsync()).Value!;
        Assert.Equal(60, SettingsSchema.Get(s, "replay.fps")!.GetValue<int>());
        Assert.Equal("BenchPage_Applied", vm.Message.Key);
    }

    [Fact]
    public async Task Apply_InvalidKey_IsShownOnTheRow()
    {
        await using var server = new FakeLineServer(l => l.StartsWith("settings set") ? Envelope.Error("Invalid replay.fps.") : Envelope.Ok("null"));
        await using var client = new AgentClient(server.Name, MockHarness.OwnProcessTrust());
        var vm = new BenchViewModel(new AgentApi(client), () => client);
        vm.SetReport(MockBench.Build(TestResources.DefaultSettings(), DateTimeOffset.UtcNow, 10));
        await vm.ApplyAsync();
        Assert.Equal("Invalid replay.fps.", vm.Recommendations.Single(x => x.Key == "replay.fps").Error);
        Assert.Null(vm.Recommendations.First(x => x.Key != "replay.fps").Error);
        Assert.NotEmpty(vm.Recommendations);
    }

    [Fact]
    public async Task Apply_NothingSelected_SendsNothing()
    {
        var client = new ScriptedClient(_ => Task.FromResult(AgentResult.Fail(AgentErrorKind.Server, "should not send")));
        var vm = new BenchViewModel(new AgentApi(client), () => client);
        vm.SetReport(MockBench.Build(TestResources.DefaultSettings(), DateTimeOffset.UtcNow, 10));
        foreach (var r in vm.Recommendations) r.Selected = false;
        Assert.Empty(vm.BuildPatch());

        var res = await vm.ApplyAsync();
        Assert.Null(res);
        Assert.Equal("BenchPage_ApplyNone", vm.Message.Key);
        Assert.Empty(client.Lines);
    }

    [Theory]
    [InlineData("{0} on {1}: {2} ms", new[] { "AV1", "Arc" }, "AV1 on Arc: ? ms")]
    [InlineData("{0} fps", new[] { "60" }, "60 fps")]
    [InlineData("No args.", new string[0], "No args.")]
    public void Fill_NeverThrows(string template, string[] args, string expected) =>
        Assert.Equal(expected, TextFormat.Fill(template, args, System.Globalization.CultureInfo.InvariantCulture));

    /// <summary>D21: R_layout maps the layout id argument to its localized label key.</summary>
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
}
