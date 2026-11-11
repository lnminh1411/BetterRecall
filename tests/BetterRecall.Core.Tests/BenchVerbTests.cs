// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;

namespace BetterRecall.Core.Tests;

public sealed class BenchVerbTests : IDisposable
{
    private readonly TempStore _t = new();
    private readonly TempHome _home = new();
    private readonly SettingsStore _settings;
    private readonly KeyVault _vault;
    private readonly FakeAgent _agent;
    private readonly CommandProcessor _p;
    private bool _storeOpen = true;

    private static readonly ClientSession Pub = new(PipeKind.Public, 100, @"C:\tools\client.exe");
    private readonly ClientSession Ui = new(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");
    private static readonly ClientSession Mcp = new(PipeKind.Mcp, 102, @"C:\app\betterrecall-mcp.exe");

    public BenchVerbTests()
    {
        var id = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-5), "chrome.exe", "Acme invoice"));
        _t.Store.SetEmbedding(id, Vec.Seeded(1), Vec.Info);

        _settings = SettingsStore.Load(_home.Path);
        _vault = new KeyVault(_home.Path, () => _settings.Current, probeHello: false);
        Assert.True(_vault.TryLoadSilent());

        _agent = new FakeAgent { FullReport = Profiles.ThisPc() };

        _p = new CommandProcessor(new CommandDeps
        {
            Settings = _settings,
            Store = () => _storeOpen ? _t.Store : null,
            Keys = _vault,
            Agent = _agent,
            Embedder = new FakeEmbedder(),
            Thumbnailer = new FakeThumbs(),
            PurgeAll = _ => Task.FromResult(0),
            Blobs = BlobCipher.FromVault(_vault),
            Home = _home.Path
        });
    }

    public void Dispose()
    {
        _vault.Dispose();
        _t.Dispose();
        _home.Dispose();
    }

    private async Task<(bool Ok, JsonElement Data, string? Error)> Call(ClientSession s, string line)
    {
        var r = await _p.HandleAsync(s, line, CancellationToken.None);
        Assert.DoesNotContain('\n', r.Line);
        return Envelope.Parse(r.Line);
    }

    private async Task<string?> Err(ClientSession s, string line) => (await Call(s, line)).Error;

    [Fact]
    public async Task Bench_full_returns_report_with_search_and_recommendations()
    {
        var reply = await _p.HandleAsync(Ui, "bench full", CancellationToken.None);
        Assert.DoesNotContain('\n', reply.Line);
        var (ok, data, error) = Envelope.Parse(reply.Line);
        Assert.True(ok);
        Assert.Null(error);

        var report = JsonSerializer.Deserialize(data.GetRawText(), BetterRecall.Core.CoreJson.Default.BenchReport);
        Assert.NotNull(report);
        Assert.NotEmpty(report.Recommendations);
        Assert.Contains(report.Recommendations, r => r.Key == "replay.encoder" && r.Value == "\"av1\"");
        Assert.NotNull(report.Search);
        Assert.Equal(1, report.Search.Rows);
        Assert.True(report.Search.VectorMs >= 0);
        Assert.True(report.Search.HybridMs >= 0);
        Assert.Equal(Profiles.ThisPc().Hardware.Cpu, report.Hardware.Cpu);
        Assert.Equal(TimeSpan.Zero, report.Ts.Offset);

        Assert.Contains("\"ts\":\"", reply.Line);
        var tsIdx = reply.Line.IndexOf("\"ts\":\"", StringComparison.Ordinal);
        var endQuote = reply.Line.IndexOf('"', tsIdx + 6);
        var tsStr = reply.Line[(tsIdx + 6)..endQuote];
        Assert.EndsWith("Z", tsStr);
    }

    [Fact]
    public async Task Bench_full_skips_search_when_store_locked_or_closed()
    {
        _storeOpen = false;
        var (ok, data, error) = await Call(Ui, "bench full");
        Assert.True(ok);
        Assert.Null(error);

        var report = JsonSerializer.Deserialize(data.GetRawText(), BetterRecall.Core.CoreJson.Default.BenchReport);
        Assert.NotNull(report);
        Assert.Null(report.Search);
    }

    [Fact]
    public async Task Bench_last_before_any_run_is_not_found()
    {
        var err = await Err(Ui, "bench last");
        Assert.Equal(CommandProcessor.ErrNotFound, err);
    }

    [Fact]
    public async Task Bench_last_returns_saved_report()
    {
        var full = await Call(Ui, "bench full");
        Assert.True(full.Ok);
        var last = await Call(Ui, "bench last");
        Assert.True(last.Ok);
        Assert.Equal(full.Data.GetRawText(), last.Data.GetRawText());
    }

    [Fact]
    public async Task Saved_report_is_encrypted()
    {
        var full = await Call(Ui, "bench full");
        Assert.True(full.Ok);
        var path = DataPaths.BenchReport(_home.Path);
        Assert.True(File.Exists(path));
        var text = File.ReadAllText(path, Encoding.UTF8);
        Assert.DoesNotContain("replay.encoder", text);
        Assert.DoesNotContain("Intel(R) Graphics", text);
        Assert.DoesNotContain("R_encoder_realtime", text);
    }

    [Fact]
    public async Task Bench_last_survives_a_new_processor()
    {
        var full = await Call(Ui, "bench full");
        Assert.True(full.Ok);

        var p2 = new CommandProcessor(new CommandDeps
        {
            Settings = _settings,
            Store = () => _storeOpen ? _t.Store : null,
            Keys = _vault,
            Agent = _agent,
            Embedder = new FakeEmbedder(),
            Thumbnailer = new FakeThumbs(),
            PurgeAll = _ => Task.FromResult(0),
            Blobs = BlobCipher.FromVault(_vault),
            Home = _home.Path
        });

        var r = await p2.HandleAsync(Ui, "bench last", CancellationToken.None);
        var (ok, data, error) = Envelope.Parse(r.Line);
        Assert.True(ok);
        Assert.Null(error);
        Assert.Equal(full.Data.GetRawText(), data.GetRawText());
    }

    [Fact]
    public async Task Bench_bad_argument_gives_usage()
    {
        Assert.Equal(CommandProcessor.UsageBench, await Err(Ui, "bench nope"));
        Assert.Equal(CommandProcessor.UsageBench, await Err(Ui, "bench full extra"));
    }

    [Fact]
    public async Task Bench_full_is_ui_only()
    {
        var pub = await Call(Pub, "bench full");
        Assert.False(pub.Ok);
        Assert.False(_agent.FullBenchStarted.Task.IsCompleted);

        var mcp = await Call(Mcp, "bench full");
        Assert.False(mcp.Ok);
        Assert.False(_agent.FullBenchStarted.Task.IsCompleted);
    }

    [Fact]
    public async Task Help_lists_bench_full_and_last_for_ui()
    {
        var reply = await _p.HandleAsync(Ui, "help", CancellationToken.None);
        Assert.Contains("\"bench full\"", reply.Line);
        Assert.Contains("\"bench last\"", reply.Line);
    }

    [Fact]
    public void Long_running_detection()
    {
        Assert.True(CommandProcessor.IsLongRunning(PipeKind.Ui, "bench full"));
        Assert.True(CommandProcessor.IsLongRunning(PipeKind.Ui, "BENCH  FULL"));
        Assert.False(CommandProcessor.IsLongRunning(PipeKind.Ui, "bench"));
        Assert.False(CommandProcessor.IsLongRunning(PipeKind.Ui, "bench last"));
        Assert.False(CommandProcessor.IsLongRunning(PipeKind.Public, "bench full"));
        Assert.False(CommandProcessor.IsLongRunning(PipeKind.Mcp, "bench full"));
    }

    [Fact]
    public async Task Closing_the_pipe_cancels_bench_full()
    {
        _agent.BlockFullBench = true;
        var name = "BRtest." + Guid.NewGuid().ToString("N")[..8] + ".benchui";
        await using var server = new PipeServer(name, PipeKind.Ui, _ => true, _p.ServeAsync);
        server.Start();

        var (r, c) = await PipeClient.ConnectAsync(name, Environment.ProcessPath!, 2000);
        Assert.Equal(PipeConnectResult.Ok, r);

        var pending = c!.RequestAsync("bench full");
        await _agent.FullBenchStarted.Task.WaitAsync(TimeSpan.FromSeconds(10));

        await c.DisposeAsync();
        await _agent.FullBenchCancelled.Task.WaitAsync(TimeSpan.FromSeconds(10));

        try { await pending; } catch { }
    }

    [Fact]
    public async Task Bench_full_is_not_cut_by_the_request_timeout()
    {
        _agent.BlockFullBench = true;
        var name = "BRtest." + Guid.NewGuid().ToString("N")[..8] + ".benchui";
        await using var server = new PipeServer(name, PipeKind.Ui, _ => true, _p.ServeAsync);
        server.Start();

        var (r, c) = await PipeClient.ConnectAsync(name, Environment.ProcessPath!, 2000);
        Assert.Equal(PipeConnectResult.Ok, r);

        var pending = c!.RequestAsync("bench full");
        await _agent.FullBenchStarted.Task.WaitAsync(TimeSpan.FromSeconds(10));

        await Task.Delay(1500);
        Assert.False(_agent.FullBenchCancelled.Task.IsCompleted);

        await c.DisposeAsync();
        await _agent.FullBenchCancelled.Task.WaitAsync(TimeSpan.FromSeconds(10));

        try { await pending; } catch { }
    }
}
