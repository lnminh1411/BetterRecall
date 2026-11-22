// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;
using StepClock = BetterRecall.Core.Ipc.StepClock;

namespace BetterRecall.Core.Tests;

/// <summary>Sweep 4 batch L: process trust and backward clock steps (T2, B12, S6). Seams run over real pipes: the real
/// PipeManager and CommandProcessor play the agent, this test process is its image.</summary>
public sealed class Sweep4LTests : IAsyncLifetime
{
    private static readonly string Self = Environment.ProcessPath!;
    private readonly TempStore _t = new();
    private readonly TempHome _home = new();
    private readonly string _tag = Guid.NewGuid().ToString("N")[..10];
    private readonly string _root = Path.Combine(TestEnv.Root, "s4l-" + Guid.NewGuid().ToString("N")[..10]);
    private SettingsStore _settings = null!;
    private KeyVault _vault = null!;
    private PipeManager _pipes = null!;

    private string PublicName => $"BRtest.{_tag}.{_settings.Current.Ipc.PipeName}";

    public async Task InitializeAsync()
    {
        Directory.CreateDirectory(_root);
        _settings = SettingsStore.Load(_home.Path);
        Set("{\"ipc\":{\"enabled\":true}}");
        _vault = new KeyVault(_home.Path, () => _settings.Current, probeHello: false);
        var processor = new CommandProcessor(new CommandDeps
        {
            Settings = _settings, Store = () => _t.Store, Keys = _vault, Agent = new FakeAgent(),
            Embedder = new FakeEmbedder(), Thumbnailer = new FakeThumbs(), PurgeAll = _ => Task.FromResult(0),
        });
        // The agent's install dir is this test process's folder, so its image is "our own file" for B12.
        _pipes = new PipeManager(_settings, processor, Path.GetDirectoryName(Self)!, () => _t.Store, (k, s) => k switch
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
        try { Directory.Delete(_root, true); } catch (IOException) { } catch (UnauthorizedAccessException) { }
    }

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    // ---------------------------------------------------------------- T2: backward clock step

    [Fact]
    public void T2_StartBound_follows_the_boot_clock_only_after_a_step_back()
    {
        var wall = new DateTime(2026, 11, 16, 11, 51, 52, DateTimeKind.Utc);
        long up = 3_600_000;   // an hour since boot
        // No step: boot stamp + uptime == wall (+ the few seconds the System process starts after boot): wall stays.
        Assert.Equal(wall, ProcessInfo.StartBound(wall, up, wall.AddMilliseconds(-up).AddSeconds(3)));
        // Clock stepped back 2 h after boot: boot stamp + uptime is 2 h ahead of the wall clock and wins.
        var boot = wall.AddMilliseconds(-up).AddHours(2);
        Assert.Equal(boot.AddMilliseconds(up), ProcessInfo.StartBound(wall, up, boot));
        // Stepped forward: the wall clock is ahead, stays the bound. Unknown boot: wall clock.
        Assert.Equal(wall, ProcessInfo.StartBound(wall, up, wall.AddMilliseconds(-up).AddHours(-2)));
        Assert.Equal(wall, ProcessInfo.StartBound(wall, up, null));
        // The real one never falls behind the wall clock.
        var now = DateTime.UtcNow;
        Assert.True(ProcessInfo.StartBound(now) >= now);
    }

    [Fact]
    public async Task T2_Client_still_trusts_the_running_agent_after_the_clock_steps_back_2h()
    {
        var stepped = () => DateTime.UtcNow.AddHours(-2);
        // The old check: the agent (this process) "started after" the stepped connect time.
        Assert.Null(ProcessInfo.ImagePathIfStartedBefore(Environment.ProcessId, stepped()));
        // Real pipe, real agent-side checks, real process start times; only the client's wall clock is stepped.
        var (r, c) = await PipeClient.ConnectAsync(PublicName, Self, 2000, stepped, default);
        Assert.Equal(PipeConnectResult.Ok, r);
        await using (c!)
            Assert.True(Envelope.Parse((await c!.RequestAsync("ping"))!).Ok);
        // The image check still holds.
        (r, c) = await PipeClient.ConnectAsync(PublicName, @"C:\elsewhere\BetterRecall.Agent.exe", 2000, stepped, default);
        Assert.Equal(PipeConnectResult.NotTrusted, r);
    }

    [Fact]
    public void T2_Server_check_admits_a_client_started_before_a_step_back_and_still_refuses_pid_reuse()
    {
        var server = new PipeServer("BRtest.unused." + Guid.NewGuid().ToString("N"), PipeKind.Public, _ => true, (_, _) => Task.CompletedTask);
        var self = ProcessInfo.Read(Environment.ProcessId, ProcessInfo.CurrentSession);
        var wall = DateTime.UtcNow;
        long up = Environment.TickCount64;
        // The UI started before NTP stepped the clock back 2 h: its start shows 2 h "ahead"; so does the boot stamp.
        var steppedBoot = wall.AddMilliseconds(-up).AddHours(2);
        var ui = self with { CreatedUtc = self.CreatedUtc!.Value.AddHours(2) };
        Assert.Equal("pid reuse", server.Check(ui, wall));   // before T2
        Assert.Null(server.Check(ui, ProcessInfo.StartBound(wall, up, steppedBoot)));
        // Without a step a process newer than the connection is still a reused pid.
        var unsteppedBoot = wall.AddMilliseconds(-up);
        Assert.Equal("pid reuse", server.Check(self with { CreatedUtc = wall.AddSeconds(1) }, ProcessInfo.StartBound(wall, up, unsteppedBoot)));
    }

    // ---------------------------------------------------------------- B12: allow list across version folders

    [Theory]
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\betterrecall.exe", @"C:\d\app\0.2.3-dev-00b55ae338ad\betterrecall.exe", true)]
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\betterrecall.exe", @"C:\d\app\bin\betterrecall.cmd", true)]
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\ui\BetterRecall.UI.exe", @"C:\d\app\0.2.3-aaaaaaaaaaaa\ui\BetterRecall.UI.exe", true)]
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\betterrecall-mcp.exe", @"C:\d\app\0.2.3-aaaaaaaaaaaa\betterrecall.exe", false)]   // another file
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\betterrecall-mcp.exe", @"C:\d\app\bin\betterrecall.cmd", false)]
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.3-aaaaaaaaaaaa\betterrecall.exe", @"C:\d\app\0.2.2-cccccccccccc\betterrecall.exe", false)] // client not in the agent's install
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\betterrecall.exe", @"C:\d\app\tools\betterrecall.exe", false)]       // not a version folder
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\d\app\0.2.4-bbbbbbbbbbbb\betterrecall.exe", @"C:\other\app\0.2.3-aaaaaaaaaaaa\betterrecall.exe", false)] // another root
    [InlineData(@"C:\d\app\0.2.4-bbbbbbbbbbbb", @"C:\Tools\my.exe", @"C:\d\app\0.2.3-aaaaaaaaaaaa\betterrecall.exe", false)]
    public void B12_SameInstallFile(string install, string image, string entry, bool allowed)
        => Assert.Equal(allowed, PipeManager.SameInstallFile(install, image, entry));

    [Fact]
    public async Task B12_Allow_list_entry_of_an_older_version_folder_admits_the_same_file_of_this_install()
    {
        // The agent runs from this test's folder; the entry names the same exe in a sibling version folder, as an
        // entry written before an upgrade does.
        var install = Path.GetDirectoryName(Self)!;
        var parent = Path.GetDirectoryName(install)!;
        var old = Path.Combine(parent, "0.2.3-dev-00b55ae338ad", Path.GetFileName(Self));
        Set("{\"ipc\":{\"allowClients\":[" + JsonSerializer.Serialize(old) + "]}}");
        var (r, c) = await PipeClient.ConnectAsync(PublicName, Self, 2000);
        Assert.Equal(PipeConnectResult.Ok, r);
        await using (c!)
            Assert.True(Envelope.Parse((await c!.RequestAsync("ping"))!).Ok);

        // Another file name in that folder, or the same name in a folder that is not a version folder: refused.
        foreach (var entry in new[] { Path.Combine(parent, "0.2.3-dev-00b55ae338ad", "other.exe"), Path.Combine(parent, "tools", Path.GetFileName(Self)) })
        {
            Set("{\"ipc\":{\"allowClients\":[" + JsonSerializer.Serialize(entry) + "]}}");
            (r, c) = await PipeClient.ConnectAsync(PublicName, Self, 2000);
            Assert.Equal(PipeConnectResult.Ok, r);
            await using (c!)
                Assert.Equal("Client not allowed.", Envelope.Parse((await c!.ReadLineAsync())!).Error);
        }
    }

    // ---------------------------------------------------------------- review follow-up (S6)

    [Fact]
    public void S6_Boot_bound_lasts_only_for_the_catch_up_period_and_is_logged_once_per_step()
    {
        var steps = new List<TimeSpan>();
        var clock = new StepClock(steps.Add);
        var boot = new DateTime(2026, 11, 16, 9, 0, 0, DateTimeKind.Utc);
        long ms = 3_600_000;
        DateTime wall = boot.AddMilliseconds(ms).AddSeconds(-3);   // System process starts ~3 s after boot
        DateTime Bound() => clock.Bound(wall, ms, boot);
        void Pass(TimeSpan t) { ms += (long)t.TotalMilliseconds; wall += t; }

        Assert.Equal(wall, Bound());
        Pass(TimeSpan.FromDays(3)); wall += TimeSpan.FromSeconds(8);   // tick drift below the tolerance: nothing
        Assert.Equal(wall, Bound());
        Assert.Empty(steps);

        var before = wall;
        wall -= TimeSpan.FromHours(2);   // NTP steps the clock back 2 h
        Assert.Equal(before, Bound());   // processes stamped before the step pass
        Assert.Single(steps);
        Assert.InRange(steps[0], TimeSpan.FromHours(2) - TimeSpan.FromSeconds(1), TimeSpan.FromHours(2) + TimeSpan.FromSeconds(1));
        Pass(TimeSpan.FromHours(1));
        Assert.Equal(before, Bound());   // the bound stays put, it does not run ahead with the boot clock
        Pass(TimeSpan.FromHours(1) + TimeSpan.FromSeconds(1));
        Assert.Equal(wall, Bound());     // caught up: the wall clock alone, although the boot clock is still 2 h ahead
        Pass(TimeSpan.FromDays(1));
        Assert.Equal(wall, Bound());
        Assert.Single(steps);

        wall += TimeSpan.FromHours(1);   // a step forward, then back again: the second step back is seen
        Assert.Equal(wall, Bound());
        before = wall;
        wall -= TimeSpan.FromHours(1);
        Assert.Equal(before, Bound());
        Assert.Equal(2, steps.Count);
    }
}
