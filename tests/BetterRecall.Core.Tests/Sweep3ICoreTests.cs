// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Ai;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;

namespace BetterRecall.Core.Tests;

/// <summary>Bug sweep 3, batch I: B1 (purge all while the key waits for its passphrase).</summary>
public class Sweep3IStoreTests
{
    [Fact]
    public async Task B1_purge_all_runs_while_the_store_waits_for_a_forgotten_passphrase()
    {
        using var h = new TempHome("passphrase");
        var host1 = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host1)
        {
            var open = host1.OpenStoreAsync(CancellationToken.None);
            Assert.True(host1.Keys.UnlockWithPassphrase("correct horse"));
            await open.WaitAsync(TimeSpan.FromSeconds(30));
            host1.Store!.AddAudioCapture(Caps.Audio(DateTimeOffset.UtcNow, "secret"));
        }
        // Restart: the agent's store open waits for the passphrase. The user forgot it and runs purge all.
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            using var life = new CancellationTokenSource();
            var waiting = host.OpenStoreAsync(life.Token);
            await Task.Delay(300);
            Assert.False(waiting.IsCompleted);
            await host.PurgeAllAsync(CancellationToken.None).WaitAsync(TimeSpan.FromSeconds(30));
            // The first open now waits for the new key (passphrase mode: the next passphrase creates it), not for ever on
            // the destroyed one.
            Assert.False(waiting.IsCompleted);
            Assert.True(host.Keys.UnlockWithPassphrase("new passphrase"));
            await waiting.WaitAsync(TimeSpan.FromSeconds(30));
            Assert.NotNull(host.Store);
            Assert.Equal(0, host.Store!.CountPurge(new PurgeScope("all")));
            life.Cancel();
        }
    }
}

/// <summary>Bug sweep 3, batch I: T5 (no Windows Hello locks the user out) and B15 (password method with a missing or
/// damaged unlock.bin).</summary>
public sealed class Sweep3IUnlockTests : IAsyncDisposable
{
    private readonly TempStore _t = new();
    private readonly TempHome _home = new(null);
    private readonly SettingsStore _settings;
    private readonly FakeKeys _keys = new();   // Hello = false: no Windows Hello on this PC
    private readonly FakeAgent _agent = new() { Unlocked = false, UnlockResult = false };   // a Hello prompt fails
    private readonly CommandProcessor _p;
    private readonly ClientSession _ui = new(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");
    private DateTimeOffset _now = DateTimeOffset.UtcNow;
    private readonly UnlockPassword _pw;

    public Sweep3IUnlockTests()
    {
        _settings = SettingsStore.Load(_home.Path);
        _pw = new UnlockPassword(_home.Path, () => _now, memKiB: 1024, passes: 1, lanes: 1);
        _p = new CommandProcessor(new CommandDeps
        {
            Settings = _settings,
            Store = () => _t.Store,
            Keys = _keys,
            Agent = _agent,
            Embedder = new FakeEmbedder(),
            Thumbnailer = new FakeThumbs(),
            PurgeAll = _ => Task.FromResult(0),
            Password = _pw,
            AiKey = new ApiKeyStore(_home.Path),
            Describer = new CloudDescriber(),
        })
        { Now = () => _now };
    }

    public ValueTask DisposeAsync()
    {
        _t.Dispose();
        _home.Dispose();
        return ValueTask.CompletedTask;
    }

    private async Task<(bool Ok, JsonElement Data, string? Error)> Call(string line)
        => Envelope.Parse((await _p.HandleAsync(_ui, line, CancellationToken.None)).Line);

    [Fact]
    public async Task T5_no_hello_defaults_have_an_in_app_way_in()
    {
        Assert.Equal("hello", _settings.Current.Security.UnlockMethod);
        Assert.True(_settings.Current.Security.RequireUnlock);
        // Unlock first says what is missing instead of "cancelled".
        Assert.Equal(CommandProcessor.ErrNoPassword, (await Call("unlock")).Error);
        Assert.True((await Call("password set {\"new\":\"open sesame\"}")).Ok);   // allowed while locked without Hello
        Assert.Equal(CommandProcessor.ErrHelloMissing, (await Call("unlock")).Error);
        // Switching the method is allowed while locked, since nothing else could unlock.
        var set = await Call("settings set {\"security\":{\"unlockMethod\":\"password\"}}");
        Assert.True(set.Ok, set.Error);
        Assert.Equal("password", _settings.Current.Security.UnlockMethod);
        _agent.Calls.Clear();
        Assert.Equal(CommandProcessor.ErrWrongPassword, (await Call("unlock open sesam")).Error);
        Assert.False(_agent.Unlocked);
        Assert.True((await Call("unlock open sesame")).Ok);
        Assert.True(_agent.Unlocked);
        Assert.DoesNotContain("unlock", _agent.Calls);   // no Hello prompt
    }

    [Fact]
    public async Task T5_hello_method_takes_the_password_when_hello_is_missing()
    {
        _pw.Set("open sesame");
        Assert.Equal(CommandProcessor.ErrWrongPassword, (await Call("unlock nope")).Error);
        var ok = await Call("unlock open sesame");
        Assert.True(ok.Ok, ok.Error);
        Assert.True(_agent.Unlocked);
        Assert.Contains(("unlock password (no Hello)", 1), _t.Store.ReadAudit(20).Select(a => (a.Verb, a.ResultCount)));
    }

    [Fact]
    public async Task T5_locked_settings_stay_guarded()
    {
        // Only the unlock method alone, and only while no unlock path exists.
        Assert.Equal(CommandProcessor.ErrLocked, (await Call("settings set {\"security\":{\"unlockMethod\":\"password\",\"requireUnlock\":false}}")).Error);
        Assert.Equal(CommandProcessor.ErrLocked, (await Call("settings set {\"security\":{\"unlockMethod\":\"password\"},\"ocr\":{\"enabled\":false}}")).Error);
        Assert.Equal(CommandProcessor.ErrNoPassword, (await Call("settings set {\"security\":{\"unlockMethod\":\"password\"}}")).Error);
        _keys.Hello = true;   // Hello works: the normal lock applies
        _pw.Set("open sesame");
        Assert.Equal(CommandProcessor.ErrLocked, (await Call("settings set {\"security\":{\"unlockMethod\":\"password\"}}")).Error);
        Assert.Equal("hello", _settings.Current.Security.UnlockMethod);
    }

    [Fact]
    public async Task B15_missing_or_damaged_password_file_falls_back_to_hello()
    {
        _pw.Set("open sesame");
        Assert.Null(_settings.TryUpdate(JsonDocument.Parse("{\"security\":{\"unlockMethod\":\"password\"}}").RootElement));
        var file = DataPaths.UnlockPasswordFile(_home.Path);
        File.WriteAllBytes(file, [1, 2, 3]);   // damaged
        _keys.Hello = true;
        _agent.UnlockResult = true;
        var r = await Call("unlock whatever");
        Assert.True(r.Ok, r.Error);
        Assert.Contains("unlock", _agent.Calls);   // Windows Hello opened it
        Assert.Contains(("unlock hello (no password file)", 1), _t.Store.ReadAudit(20).Select(a => (a.Verb, a.ResultCount)));
        // Purge all is reachable again (it needs a fresh unlock).
        Assert.True((await Call("purge all")).Ok);

        // No Hello either: password set is allowed while locked, and so is switching the method.
        _agent.Lock();
        _keys.Hello = false;
        _agent.UnlockResult = false;
        File.Delete(file);
        Assert.Equal(CommandProcessor.ErrNoPassword, (await Call("unlock whatever")).Error);
        Assert.True((await Call("settings set {\"security\":{\"unlockMethod\":\"hello\"}}")).Ok);
    }

    [Fact]
    public void B15_lockout_survives_no_clock_step_back()
    {
        _pw.Set("open sesame");
        for (int i = 0; i < 10; i++)   // wrong tries, each after the wait, up to the 15 min wait
        {
            _now += TimeSpan.FromSeconds(_pw.RetryAfterSeconds + 1);
            Assert.Equal(PasswordCheck.Wrong, _pw.Verify("nope"));
        }
        Assert.Equal(UnlockPassword.MaxWaitSeconds, _pw.RetryAfterSeconds);
        _now -= TimeSpan.FromHours(5);   // clock stepped back: the stored time is now 5 h 15 min ahead
        Assert.Equal(UnlockPassword.MaxWaitSeconds, _pw.RetryAfterSeconds);
        _now += TimeSpan.FromSeconds(UnlockPassword.MaxWaitSeconds + 1);
        Assert.Equal(0, _pw.RetryAfterSeconds);
        Assert.Equal(PasswordCheck.Ok, _pw.Verify("open sesame"));
    }
}
