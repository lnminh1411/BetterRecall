// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json.Nodes;
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
        var lines = new List<string>();
        var api = new AgentApi(new Recording(h.Client, lines));
        var vm = new BenchViewModel(api, () => h.Client);
        await vm.LoadLastAsync();
        var keys = vm.Recommendations.Select(r => r.Key).ToList();
        Assert.Contains("replay.fps", keys);
        foreach (var row in vm.Recommendations) row.Selected = row.Key is "replay.fps" or "replay.encoder";

        var r = await vm.ApplyAsync();
        Assert.True(r!.Value.Ok, r.Value.Error);
        var line = Assert.Single(lines, l => l.StartsWith("settings set", StringComparison.Ordinal));
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
        vm.SetReport(MockBench.Build((JsonObject)JsonNode.Parse(System.Text.Json.JsonSerializer.Serialize(new Settings(), ContractsJson.Default.Settings))!, DateTimeOffset.UtcNow, 10));
        await vm.ApplyAsync();
        Assert.Equal("Invalid replay.fps.", vm.Recommendations.Single(x => x.Key == "replay.fps").Error);
        Assert.Null(vm.Recommendations.First(x => x.Key != "replay.fps").Error);
        Assert.NotEmpty(vm.Recommendations);
    }

    [Fact]
    public void Apply_NothingSelected_SendsNothing()
    {
        var vm = new BenchViewModel(new AgentApi(new Recording(null!, [])), () => null!);
        vm.SetReport(MockBench.Build((JsonObject)JsonNode.Parse(System.Text.Json.JsonSerializer.Serialize(new Settings(), ContractsJson.Default.Settings))!, DateTimeOffset.UtcNow, 10));
        foreach (var r in vm.Recommendations) r.Selected = false;
        Assert.Empty(vm.BuildPatch());
    }

    [Fact]
    public void Recommendations_OnlyWhenDifferent()
    {
        var s = (JsonObject)JsonNode.Parse(System.Text.Json.JsonSerializer.Serialize(new Settings(), ContractsJson.Default.Settings))!;
        SettingsSchema.Set(s, "replay.fps", 60);
        var rep = MockBench.Build(s, DateTimeOffset.UtcNow, 1);
        Assert.DoesNotContain(rep.Recommendations, r => r.Key == "replay.fps");
        Assert.All(rep.Recommendations, r => Assert.NotEqual(r.Current, r.Value));
    }

    [Theory]
    [InlineData("{0} on {1}: {2} ms", new[] { "AV1", "Arc" }, "AV1 on Arc: ? ms")]
    [InlineData("{0} fps", new[] { "60" }, "60 fps")]
    [InlineData("No args.", new string[0], "No args.")]
    public void Fill_NeverThrows(string template, string[] args, string expected) =>
        Assert.Equal(expected, TextFormat.Fill(template, args, System.Globalization.CultureInfo.InvariantCulture));

    [Fact]
    public void ScalePercent_EnabledOnlyForScale()
    {
        var s = (JsonObject)JsonNode.Parse(System.Text.Json.JsonSerializer.Serialize(new Settings(), ContractsJson.Default.Settings))!;
        var d = SettingsSchema.Find("replay.scalePercent")!;
        Assert.False(d.IsEnabled(s));
        SettingsSchema.Set(s, "replay.resolution", "scale");
        Assert.True(d.IsEnabled(s));
        Assert.True(SettingsSchema.Find("replay.layout")!.IsEnabled(s));
    }

    [Theory]
    [InlineData("replay.encoder", "av1", true)]
    [InlineData("replay.encoder", "av1_qsv", false)]
    [InlineData("replay.layout", "perMonitor", true)]
    [InlineData("replay.layout", "grid", false)]
    [InlineData("replay.resolution", "900p", true)]
    [InlineData("replay.resolution", "800p", false)]
    [InlineData("replay.encoderAdapter", "RTX 3050", true)]
    [InlineData("replay.encoderAdapter", " ", false)]
    public void ReplayKeys_Validate(string key, string value, bool ok)
    {
        var s = (JsonObject)JsonNode.Parse(System.Text.Json.JsonSerializer.Serialize(new Settings(), ContractsJson.Default.Settings))!;
        SettingsSchema.Set(s, key, value);
        Assert.Equal(ok, SettingsSchema.Validate(SettingsSchema.Find(key)!, s) is null);
    }

    [Theory]
    [InlineData(25, true)]
    [InlineData(24, false)]
    [InlineData(101, false)]
    public void ScalePercent_Range(int v, bool ok)
    {
        var s = (JsonObject)JsonNode.Parse(System.Text.Json.JsonSerializer.Serialize(new Settings(), ContractsJson.Default.Settings))!;
        SettingsSchema.Set(s, "replay.scalePercent", v);
        Assert.Equal(ok, SettingsSchema.Validate(SettingsSchema.Find("replay.scalePercent")!, s) is null);
    }

    private sealed class Recording(IAgentClient inner, List<string> lines) : IAgentClient
    {
        public int ServerPid => 1;
        public Task<AgentResult> SendAsync(string line, CancellationToken ct = default)
        {
            lock (lines) lines.Add(line);
            return inner.SendAsync(line, ct);
        }
    }
}
