// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.IO.Pipes;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;

namespace BetterRecall.Core.Tests;

public sealed class PipeTests : IAsyncLifetime
{
    private readonly TempStore _t = new();
    private readonly TempHome _home = new();   // dpapi: auto would wrap test keys with a TPM key of the user's Windows account
    private readonly string _tag = Guid.NewGuid().ToString("N")[..10];
    private SettingsStore _settings = null!;
    private KeyVault _vault = null!;
    private readonly FakeAgent _agent = new();
    private PipeManager _pipes = null!;
    private static readonly string Self = Environment.ProcessPath!;

    private string Name(PipeKind k) => k switch
    {
        PipeKind.Ui => $"BRtest.{_tag}.ui",
        PipeKind.Mcp => $"BRtest.{_tag}.mcp",
        _ => $"BRtest.{_tag}.{_settings.Current.Ipc.PipeName}",
    };

    public async Task InitializeAsync()
    {
        _settings = SettingsStore.Load(_home.Path);
        Set("{\"ipc\":{\"enabled\":true}}");
        _vault = new KeyVault(_home.Path, () => _settings.Current, probeHello: false);
        var processor = new CommandProcessor(new CommandDeps
        {
            Settings = _settings, Store = () => _t.Store, Keys = _vault, Agent = _agent,
            Embedder = new FakeEmbedder(), Thumbnailer = new FakeThumbs(), PurgeAll = _ => Task.FromResult(0),
        });
        _pipes = new PipeManager(_settings, processor, @"C:\NotTheInstallDir", () => _t.Store, (k, s) => k switch
        {
            PipeKind.Ui => $"BRtest.{_tag}.ui",
            PipeKind.Mcp => $"BRtest.{_tag}.mcp",
            _ => $"BRtest.{_tag}.{s.Ipc.PipeName}",
        });
        await _pipes.StartAsync();
    }

    public async Task DisposeAsync()
    {
        await _pipes.DisposeAsync();
        _vault.Dispose();
        _t.Dispose();
        _home.Dispose();
    }

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    private async Task<PipeClient> Connect(PipeKind k)
    {
        var (r, c) = await PipeClient.ConnectAsync(Name(k), Self, 2000);
        Assert.Equal(PipeConnectResult.Ok, r);
        return c!;
    }

    [Fact]
    public async Task Public_pipe_round_trip_with_client_trust_check()
    {
        await using var c = await Connect(PipeKind.Public);
        Assert.Equal(Environment.ProcessId, c.ServerPid);
        var (ok, d, _) = Envelope.Parse((await c.RequestAsync("ping"))!);
        Assert.True(ok);
        Assert.True(d.GetProperty("pong").GetBoolean());
        // same connection, second request
        Assert.True(Envelope.Parse((await c.RequestAsync("VERSION"))!).Ok);
        var audit = _t.Store.ReadAudit(5);
        Assert.Contains(audit, a => a.Verb == "connect" && a.Pipe == "ipc" && a.ClientPid == Environment.ProcessId);
    }

    [Fact]
    public async Task Client_rejects_wrong_server_image_and_reports_not_running()
    {
        var (r, c) = await PipeClient.ConnectAsync(Name(PipeKind.Public), @"C:\Program Files\BetterRecall\BetterRecall.Agent.exe", 2000);
        Assert.Equal(PipeConnectResult.NotTrusted, r);
        Assert.Null(c);
        (r, c) = await PipeClient.ConnectAsync($"BRtest.{_tag}.nothing", Self, 300);
        Assert.Equal(PipeConnectResult.NotRunning, r);
    }

    [Fact]
    public async Task Ui_pipe_rejects_other_images()
    {
        await using var c = await Connect(PipeKind.Ui);
        var line = await c.ReadLineAsync();
        Assert.Equal("{\"ok\":false,\"error\":\"Client not allowed.\"}", line);
        Assert.Null(await c.ReadLineAsync());
        await Task.Delay(100);
        Assert.Contains(_t.Store.ReadAudit(10), a => a.Verb == "rejected" && a.Pipe == "ui");
    }

    [Fact]
    public async Task AllowClients_narrows_the_public_pipe()
    {
        Set("{\"ipc\":{\"allowClients\":[\"C:\\\\Tools\\\\other.exe\"]}}");
        await using (var c = await Connect(PipeKind.Public))
            Assert.Equal("Client not allowed.", Envelope.Parse((await c.ReadLineAsync())!).Error);
        Set("{\"ipc\":{\"allowClients\":[" + JsonSerializer.Serialize(Self.ToUpperInvariant()) + "]}}");
        await using (var c = await Connect(PipeKind.Public))
            Assert.True(Envelope.Parse((await c.RequestAsync("ping"))!).Ok);
    }

    [Fact]
    public void Security_descriptor_has_deny_network_user_only_and_medium_label()
    {
        var probe = new PipeServer($"BRtest.{_tag}.sddl", PipeKind.Public, _ => true, (_, _) => Task.CompletedTask);
        using var inst = probe.CreateInstance(first: true);
        var sddl = PipeServer.ReadSddl(inst.SafePipeHandle)!;
        File.WriteAllText(Path.Combine(TestEnv.Root, "pipe-sddl.txt"), sddl);
        var sid = PipeNames.UserSid();
        Assert.Contains($"O:{sid}", sddl);
        Assert.Contains("(D;;FA;;;NU)", sddl); // GA maps to FILE_ALL_ACCESS on read-back
        Assert.Contains("D:P", sddl);
        Assert.Matches(@"S:[A-Z]*\(ML;;NWNR;;;ME\)", sddl);
        Assert.DoesNotContain(";;;SY)", sddl);
        Assert.DoesNotContain(";;;BA)", sddl);
        Console.WriteLine("pipe SDDL read back: " + sddl);
    }

    [Fact]
    public void First_pipe_instance_flag_refuses_a_second_server()
    {
        var other = new PipeServer(Name(PipeKind.Public), PipeKind.Public, _ => true, (_, _) => Task.CompletedTask);
        Assert.ThrowsAny<IOException>(() => other.Start());
    }

    [Fact]
    public async Task Settings_toggle_pipes_live()
    {
        Assert.Null(_pipes.Mcp);
        Set("{\"mcp\":{\"enabled\":true}}");
        await WaitFor(() => _pipes.Mcp != null);
        await using (var c = await Connect(PipeKind.Mcp))
        {
            // MCP pipe image rule: only <install>\betterrecall-mcp.exe
            Assert.Equal("Client not allowed.", Envelope.Parse((await c.ReadLineAsync())!).Error);
        }
        var oldName = Name(PipeKind.Public);
        Set("{\"ipc\":{\"pipeName\":\"Renamed\"}}");
        await WaitFor(() => _pipes.Public?.Name.EndsWith("Renamed") == true);
        Assert.Equal(PipeConnectResult.NotRunning, (await PipeClient.ConnectAsync(oldName, Self, 300)).Result);
        await using (var c = await Connect(PipeKind.Public))
            Assert.True(Envelope.Parse((await c.RequestAsync("ping"))!).Ok);
        Set("{\"ipc\":{\"enabled\":false},\"mcp\":{\"enabled\":false}}");
        await WaitFor(() => _pipes.Public == null && _pipes.Mcp == null);
        Assert.Equal(PipeConnectResult.NotRunning, (await PipeClient.ConnectAsync(Name(PipeKind.Public), Self, 300)).Result);
    }

    [Fact]
    public async Task Four_concurrent_clients()
    {
        var clients = new List<PipeClient>();
        for (int i = 0; i < 4; i++) clients.Add(await Connect(PipeKind.Public));
        var replies = await Task.WhenAll(clients.Select(c => c.RequestAsync("ping")));
        Assert.All(replies, r => Assert.True(Envelope.Parse(r!).Ok));
        foreach (var c in clients) await c.DisposeAsync();
    }

    [Fact]
    public async Task Oversized_line_gets_error_and_close()
    {
        await using var c = await Connect(PipeKind.Public);
        var line = await c.RequestAsync("search " + new string('x', 5000));
        Assert.Equal("{\"ok\":false,\"error\":\"Line exceeds 4096 bytes.\"}", line);
        Assert.Null(await c.ReadLineAsync());
    }

    [Fact]
    public async Task Bom_and_crlf_on_the_wire()
    {
        var stream = new NamedPipeClientStream(".", Name(PipeKind.Public), PipeDirection.InOut, PipeOptions.Asynchronous);
        await stream.ConnectAsync(2000);
        await stream.WriteAsync(new byte[] { 0xEF, 0xBB, 0xBF, (byte)'p', (byte)'i', (byte)'n', (byte)'g', (byte)'\r', (byte)'\n' });
        var reader = new LineReader(stream);
        Assert.True(Envelope.Parse((await reader.ReadLineAsync(1 << 20, default))!).Ok);
        await stream.DisposeAsync();
    }

    [Fact]
    public async Task Subscribe_status_and_captures_streams()
    {
        await using var s = await Connect(PipeKind.Public);
        Assert.Equal("{\"ok\":true,\"data\":{\"subscribed\":true}}", await s.RequestAsync("subscribe"));
        _agent.RaiseStatus();
        var (ok, d, _) = Envelope.Parse((await s.ReadLineAsync())!);
        Assert.True(ok);
        Assert.True(d.GetProperty("capturing").GetBoolean());

        await using var c = await Connect(PipeKind.Public);
        Assert.Equal("{\"ok\":true,\"data\":{\"subscribed\":\"captures\"}}", await c.RequestAsync("subscribe captures"));
        _agent.RaiseCapture(new CaptureMeta("s_1", DateTimeOffset.Parse("2026-11-10T07:40:12Z"), CaptureKind.Screen, "chrome.exe", "SECRET TITLE", 0, 0, true, true));
        var ev = await c.ReadLineAsync();
        Assert.Equal("{\"ok\":true,\"data\":{\"id\":\"s_1\",\"ts\":\"2026-11-10T07:40:12Z\",\"kind\":\"screen\",\"app\":\"chrome.exe\"}}", ev);
        Assert.DoesNotContain("SECRET", ev);
    }

    [Fact]
    public async Task Slow_subscriber_is_disconnected()
    {
        await using var s = await Connect(PipeKind.Public);
        Assert.True(Envelope.Parse((await s.RequestAsync("subscribe"))!).Ok);
        // Never read while thousands of events arrive: the pipe buffer fills, 64 messages queue, server drops us.
        for (int i = 0; i < 5000; i++) _agent.RaiseStatus();
        int lines = 0;
        using var cts = new CancellationTokenSource(10000);
        while (await s.ReadLineAsync(cts.Token) != null) lines++;
        Assert.InRange(lines, 0, 4999); // closed by the server (a cancelled read would have thrown)
    }

    [Fact]
    public async Task Idle_connection_is_closed()
    {
        Set("{\"ipc\":{\"idleTimeoutS\":10}}");
        await using var c = await Connect(PipeKind.Public);
        var sw = System.Diagnostics.Stopwatch.StartNew();
        Assert.Null(await c.ReadLineAsync(new CancellationTokenSource(20000).Token));
        Assert.InRange(sw.Elapsed.TotalSeconds, 9, 15);
    }

    private static async Task WaitFor(Func<bool> cond)
    {
        for (int i = 0; i < 100 && !cond(); i++) await Task.Delay(50);
        Assert.True(cond());
    }
}
