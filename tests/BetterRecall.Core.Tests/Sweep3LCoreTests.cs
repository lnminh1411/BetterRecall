// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;

namespace BetterRecall.Core.Tests;

/// <summary>Bug sweep 3, batch L: audit flood (D9), CLI IPC-off hint (D8), log timestamps (D19).</summary>
public sealed class Sweep3LCoreTests : IDisposable
{
    private readonly TempStore _t = new();
    private readonly TempHome _settingsHome = new();
    private readonly SettingsStore _settings;
    private readonly KeyVault _vault;
    private readonly CommandProcessor _p;
    private readonly string _id;

    private static readonly ClientSession Pub = new(PipeKind.Public, 100, @"C:\tools\client.exe");
    private static readonly ClientSession Ui = new(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");

    public Sweep3LCoreTests()
    {
        _settings = SettingsStore.Load(_settingsHome.Path);
        _vault = new KeyVault(_settingsHome.Path, () => _settings.Current, probeHello: false);
        _p = new CommandProcessor(new CommandDeps
        {
            Settings = _settings,
            Store = () => _t.Store,
            Keys = _vault,
            Agent = new FakeAgent(),
            Embedder = new FakeEmbedder(),
            Thumbnailer = new FakeThumbs(),
            PurgeAll = _ => Task.FromResult(0),
        });
        _id = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-5), "chrome.exe", "Acme invoice 4471 - Gmail"));
    }

    public void Dispose()
    {
        _vault.Dispose();
        _t.Dispose();
        _settingsHome.Dispose();
    }

    [Fact]
    public async Task Ui_thumbs_are_not_audited_but_public_ones_are()
    {
        Assert.Null(_settings.TryUpdate(JsonDocument.Parse("{\"ipc\":{\"allowThumbnails\":true}}").RootElement));
        for (int i = 0; i < 9; i++)
            Assert.True(Envelope.Parse((await _p.HandleAsync(Ui, "thumb " + _id, CancellationToken.None)).Line).Ok);
        Assert.DoesNotContain(_t.Store.ReadAudit(50), a => a.Verb == "thumb");

        Assert.True(Envelope.Parse((await _p.HandleAsync(Pub, "thumb " + _id, CancellationToken.None)).Line).Ok);
        var rows = _t.Store.ReadAudit(50).Where(a => a.Verb == "thumb").ToList();
        Assert.Single(rows);
        Assert.Equal("ipc", rows[0].Pipe);
    }

    [Fact]
    public void Cli_says_ipc_is_off_when_the_agent_mutex_is_held()
    {
        var home = Path.Combine(TestEnv.Root, "s3l-" + Guid.NewGuid().ToString("N")[..12]);
        Assert.False(AgentPresence.Running(home));
        Assert.Equal((AgentPresence.ErrNotRunning, 2), AgentPresence.NotAnswering(home, ipcEnabled: false));
        using (var m = new Mutex(true, PipeNames.AgentMutexFor(home), out bool created))
        {
            Assert.True(created);
            Assert.True(AgentPresence.Running(home));
            var (line, exit) = AgentPresence.NotAnswering(home, ipcEnabled: false);
            Assert.Equal(3, exit);
            Assert.Contains("Allow scripts (IPC)", line);
            // IPC on but no answer: the agent may be starting; keep the plain message
            Assert.Equal((AgentPresence.ErrNotRunning, 2), AgentPresence.NotAnswering(home, ipcEnabled: true));
            m.ReleaseMutex();
        }
        Assert.False(AgentPresence.Running(home));
    }

    [Fact]
    public void Core_log_stamps_local_time_with_offset()
    {
        var t = new DateTimeOffset(2026, 11, 16, 4, 5, 6, 789, TimeSpan.FromHours(7));
        Assert.Equal("2026-11-16T04:05:06.789+07:00", Log.Stamp(t));
    }
}
