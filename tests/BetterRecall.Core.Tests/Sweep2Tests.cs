// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;
using BetterRecall.Core.Store;
using Microsoft.Data.Sqlite;

namespace BetterRecall.Core.Tests;

/// <summary>Bug sweep 2, batch E: FTS secure-delete (B1) and chats citing deleted captures (B2).</summary>
public sealed class StorePurgeReachTests
{
    private const string Token = "zqxjvbtoken";

    private static string AddReady(EncryptedStore s, DateTimeOffset ts, string title, int seed)
    {
        var id = s.AddScreenCapture(Caps.Screen(ts, "chrome.exe", title));
        s.TakePendingEmbeddings(10);
        s.SetEmbedding(id, Vec.Seeded(seed), Vec.Info);
        return id;
    }

    private static SqliteConnection Raw(TempStore t)
    {
        var c = new SqliteConnection(new SqliteConnectionStringBuilder { DataSource = DataPaths.StoreDb(t.Home.Path), Pooling = false }.ToString());
        c.Open();
        foreach (var sql in new[] { "PRAGMA cipher='sqlcipher'", "PRAGMA legacy=4", "PRAGMA key=\"x'" + Convert.ToHexString(t.Key) + "'\"", "PRAGMA secure_delete=ON" })
            Exec(c, sql);
        return c;
    }

    private static void Exec(SqliteConnection c, string sql)
    {
        using var cmd = c.CreateCommand();
        cmd.CommandText = sql;
        cmd.ExecuteNonQuery();
    }

    /// <summary>Decrypted bytes of a copy of store.db (the store must be closed): every page, free pages included.</summary>
    private static string PlainBytes(TempStore t)
    {
        var copy = Path.Combine(t.Home.Path, "plain-" + Guid.NewGuid().ToString("N")[..8] + ".db");
        File.Copy(DataPaths.StoreDb(t.Home.Path), copy);
        using (var c = new SqliteConnection(new SqliteConnectionStringBuilder { DataSource = copy, Pooling = false }.ToString()))
        {
            c.Open();
            foreach (var sql in new[] { "PRAGMA cipher='sqlcipher'", "PRAGMA legacy=4", "PRAGMA key=\"x'" + Convert.ToHexString(t.Key) + "'\"",
                "PRAGMA journal_mode=DELETE", "PRAGMA rekey=''" })
                Exec(c, sql);
        }
        var bytes = StoreTests.Raw(copy);
        File.Delete(copy);
        var text = Encoding.Latin1.GetString(bytes);
        Assert.StartsWith("SQLite format 3", text);   // really decrypted, so the check below means something
        return text;
    }

    [Fact]
    public void Purged_words_leave_the_fts_index_and_the_file()
    {
        using var t = new TempStore();
        var id = AddReady(t.Store, DateTimeOffset.UtcNow.AddMinutes(-5), "Bank " + Token + " balance", 1);
        AddReady(t.Store, DateTimeOffset.UtcNow.AddMinutes(-4), "Something else", 2);
        t.Store.Dispose();
        Assert.Contains(Token, PlainBytes(t));
        t.Reopen();
        Assert.Equal(1, t.Store.Purge(PurgeScope.ForId(id)));
        t.Store.Dispose();
        Assert.DoesNotContain(Token, PlainBytes(t));
        t.Reopen();
    }

    [Fact]
    public void Existing_store_gets_secure_delete_and_loses_words_of_earlier_deletes()
    {
        using var t = new TempStore();
        AddReady(t.Store, DateTimeOffset.UtcNow.AddMinutes(-5), "Bank " + Token + " balance", 1);
        AddReady(t.Store, DateTimeOffset.UtcNow.AddMinutes(-4), "Something else", 2);
        t.Store.Dispose();
        using (var c = Raw(t))
        {
            // A store from before the fix: option off, a row deleted the old way.
            Exec(c, "INSERT INTO fts(fts, rank) VALUES('secure-delete', 0)");
            Exec(c, "DELETE FROM meta WHERE k='fts_secure_delete'");
            Exec(c, "DELETE FROM fts WHERE rowid = (SELECT rid FROM captures WHERE title LIKE '%" + Token + "%')");
            Exec(c, "DELETE FROM vectors WHERE rid = (SELECT rid FROM captures WHERE title LIKE '%" + Token + "%')");
            Exec(c, "DELETE FROM captures WHERE title LIKE '%" + Token + "%'");
        }
        Assert.Contains(Token, PlainBytes(t));   // the old FTS segment still holds the word
        t.Reopen();
        t.Store.Dispose();
        Assert.DoesNotContain(Token, PlainBytes(t));
        t.Reopen();
        Assert.Single(t.Store.Search(new SearchRequest("something", 10, Mode: SearchMode.Text), [], 10, 1 << 20).Results);
    }

    private static ChatRecord Citing(string chatId, string captureId, DateTimeOffset at) => new(chatId, "t", at, at, "gemini", "m", "[]",
        [new ChatMessage(0, at, "user", "q"), new ChatMessage(1, at, "assistant", "a", [new ChatSource(captureId, at, CaptureKind.Screen, "chrome.exe", "t", "")])]);

    [Fact]
    public void Range_purge_drops_chats_citing_the_range_even_when_the_chat_is_outside_it()
    {
        using var t = new TempStore();
        var old = DateTimeOffset.UtcNow.AddDays(-3);
        var cap = AddReady(t.Store, old, "PIN letter", 1);
        var other = AddReady(t.Store, DateTimeOffset.UtcNow.AddMinutes(-1), "Other", 2);
        t.Store.SaveChat(Citing("c_cite", cap, DateTimeOffset.UtcNow));
        t.Store.SaveChat(Citing("c_other", other, DateTimeOffset.UtcNow));
        Assert.Equal(1, t.Store.Purge(PurgeScope.Range(old.AddHours(-1), old.AddHours(1))));
        Assert.Null(t.Store.GetChat("c_cite"));
        Assert.NotNull(t.Store.GetChat("c_other"));
    }

    [Fact]
    public void Retention_drops_recent_chats_citing_expired_captures()
    {
        using var t = new TempStore();
        var cap = AddReady(t.Store, DateTimeOffset.UtcNow.AddDays(-40), "Old doctor letter", 1);
        var kept = AddReady(t.Store, DateTimeOffset.UtcNow.AddDays(-1), "Recent", 2);
        t.Store.SaveChat(Citing("c_old", cap, DateTimeOffset.UtcNow));
        t.Store.SaveChat(Citing("c_new", kept, DateTimeOffset.UtcNow));
        var s = new Settings();
        s.Retention.Days = 30;
        t.Store.RunMaintenance(s);
        Assert.Null(t.Store.Get(cap));
        Assert.Null(t.Store.GetChat("c_old"));
        Assert.NotNull(t.Store.GetChat("c_new"));
    }

    [Fact]
    public void Open_once_drops_saved_chats_citing_captures_deleted_before_the_fix()
    {
        using var t = new TempStore();
        var cap = AddReady(t.Store, DateTimeOffset.UtcNow.AddMinutes(-5), "x", 1);
        t.Store.SaveChat(Citing("c_live", cap, DateTimeOffset.UtcNow));
        t.Store.SaveChat(Citing("c_dead", "s_gone", DateTimeOffset.UtcNow));
        t.Store.Dispose();
        using (var c = Raw(t)) Exec(c, "DELETE FROM meta WHERE k='chat_cite_sweep'");
        t.Reopen();
        Assert.NotNull(t.Store.GetChat("c_live"));
        Assert.Null(t.Store.GetChat("c_dead"));
    }
}

/// <summary>Bug sweep 2, batch E: keylost, storeerror and unlock (D1, D2, B16, B6, B-checks-D).</summary>
public sealed class KeyStateTests : IDisposable
{
    private readonly TempStore _t = new();
    private readonly TempHome _home = new();   // dpapi
    private readonly SettingsStore _settings;
    private KeyVault _vault;
    private readonly FakeAgent _agent = new();
    private bool _storeOpen = true;
    private int _purgeAll;
    private CommandProcessor _p;
    private static readonly ClientSession Ui = new(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");
    private static readonly ClientSession Pub = new(PipeKind.Public, 100, @"C:\tools\client.exe");

    public KeyStateTests()
    {
        _settings = SettingsStore.Load(_home.Path);
        _vault = new KeyVault(_home.Path, () => _settings.Current, probeHello: false);
        _p = Make();
    }

    private CommandProcessor Make() => new(new CommandDeps
    {
        Settings = _settings,
        Store = () => _storeOpen ? _t.Store : null,
        Keys = _vault,
        Agent = _agent,
        Embedder = new FakeEmbedder(),
        Thumbnailer = new FakeThumbs(),
        PurgeAll = _ => { _purgeAll++; return Task.FromResult(0); },
    });

    private void NewVault()
    {
        _vault.Dispose();
        _vault = new KeyVault(_home.Path, () => _settings.Current, probeHello: false);
        _p = Make();
    }

    public void Dispose()
    {
        _vault.Dispose();
        _t.Dispose();
        _home.Dispose();
    }

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    private async Task<(bool Ok, JsonElement Data, string? Error)> Call(ClientSession s, string line) =>
        Envelope.Parse((await _p.HandleAsync(s, line, CancellationToken.None)).Line);

    private string KeyFile => DataPaths.KeyFile(_home.Path);

    [Fact]
    public async Task Store_not_open_with_the_gate_open_is_not_a_lock()
    {
        _vault.TryLoadSilent();
        _storeOpen = false;
        Assert.Equal("Store is not open.", (await Call(Ui, "search acme")).Error);
        Assert.Equal("Store is not open.", (await Call(Ui, "timeline 2026-01-01T00:00:00Z 2030-01-01T00:00:00Z")).Error);
        Assert.Equal("Store is not open.", (await Call(Pub, "search acme")).Error);
        // Settings saves and the Home switches (S3): same answer, so the UI does not loop on the unlock panel.
        Assert.Equal("Store is not open.", (await Call(Ui, "settings set {\"ipc\":{\"enabled\":true}}")).Error);
        // A closed gate still answers locked.
        _agent.Unlocked = false;
        Assert.Equal("Store is locked.", (await Call(Ui, "search acme")).Error);
    }

    [Fact]
    public async Task Store_waiting_for_its_passphrase_answers_locked()
    {
        Set("{\"security\":{\"keyProtection\":\"passphrase\"}}");
        _storeOpen = false;
        Assert.True(_vault.NeedsPassphrase);
        Assert.Equal("Store is locked.", (await Call(Ui, "search acme")).Error);
    }

    [Fact]
    public async Task Keylocked_hello_without_passphrase_allows_purge_all_only()
    {
        Set("{\"security\":{\"keyProtection\":\"passphrase\"}}");
        _storeOpen = false;
        Assert.Equal("Unlock required.", (await Call(Ui, "purge all")).Error);
        Assert.True((await Call(Ui, "unlock")).Ok);   // Hello passed, no passphrase: nothing opens
        Assert.False(_vault.KeyLoaded);
        Assert.Equal("Store is locked.", (await Call(Ui, "search acme")).Error);
        var (ok, data, _) = await Call(Ui, "purge all");
        Assert.True(ok);
        Assert.True((await Call(Ui, "purge confirm " + data.GetProperty("confirm").GetString())).Ok);
        Assert.Equal(1, _purgeAll);
    }

    [Fact]
    public void Missing_key_file_next_to_data_is_lost_and_no_new_key_is_written()
    {
        File.WriteAllBytes(DataPaths.StoreDb(_home.Path), [1, 2, 3]);
        Assert.Throws<KeyLostException>(() => _vault.TryLoadSilent());
        Assert.False(File.Exists(KeyFile));
        Set("{\"security\":{\"keyProtection\":\"passphrase\"}}");
        Assert.False(_vault.NeedsPassphrase);   // lost, not waiting for a passphrase
        Assert.False(_vault.UnlockWithPassphrase("whatever1"));
        Assert.False(File.Exists(KeyFile));
        File.Delete(DataPaths.StoreDb(_home.Path));
        Directory.CreateDirectory(DataPaths.KeyframesDir(_home.Path));
        File.WriteAllBytes(Path.Combine(DataPaths.KeyframesDir(_home.Path), "2026-11.db"), [1]);
        Set("{\"security\":{\"keyProtection\":\"dpapi\"}}");
        Assert.Throws<KeyLostException>(() => _vault.TryLoadSilent());
    }

    [Theory]
    [InlineData(6)]
    [InlineData(20)]
    public void Truncated_dpapi_key_file_is_lost(int keep)
    {
        Assert.True(_vault.TryLoadSilent());
        var bytes = File.ReadAllBytes(KeyFile);
        File.WriteAllBytes(KeyFile, bytes[..keep]);
        NewVault();
        Assert.Throws<KeyLostException>(() => _vault.TryLoadSilent());
        Assert.False(_vault.NeedsPassphrase);
    }

    [Fact]
    public void Truncated_passphrase_key_file_is_lost_not_locked()
    {
        Set("{\"security\":{\"keyProtection\":\"passphrase\"}}");
        Assert.True(_vault.UnlockWithPassphrase("correct horse"));
        var bytes = File.ReadAllBytes(KeyFile);
        File.WriteAllBytes(KeyFile, bytes[..40]);
        NewVault();
        Assert.False(_vault.NeedsPassphrase);
        Assert.Throws<KeyLostException>(() => _vault.TryLoadSilent());
        Assert.False(_vault.UnlockWithPassphrase("correct horse"));
    }

    [Fact]
    public void Payload_checks_per_mode()
    {
        byte[] F(byte mode, params byte[] payload) => [.. "BRK1"u8, mode, .. payload];
        Assert.False(KeyVault.PayloadValid(F(3), KeyVault.ModeDpapi));
        Assert.False(KeyVault.PayloadValid(F(3, 10, 0, 0, 0, 1, 2), KeyVault.ModeDpapi));
        Assert.True(KeyVault.PayloadValid(F(3, 2, 0, 0, 0, 1, 2), KeyVault.ModeDpapi));
        Assert.False(KeyVault.PayloadValid(F(1, 1, 0, (byte)'k', 4, 0, 0, 0, 1), KeyVault.ModeTpm));
        Assert.True(KeyVault.PayloadValid(F(1, 1, 0, (byte)'k', 1, 0, 0, 0, 9), KeyVault.ModeTpm));
        Assert.False(KeyVault.PayloadValid(F(2, new byte[50]), KeyVault.ModePassphrase));
        Assert.True(KeyVault.TpmTransient(unchecked((int)0x80280400)));
        Assert.False(KeyVault.TpmTransient(unchecked((int)0x80090005)));
    }

    [Fact]
    public async Task Damaged_key_file_ignores_the_passphrase_box_and_purge_all_works()
    {
        Directory.CreateDirectory(Path.GetDirectoryName(KeyFile)!);
        File.WriteAllBytes(KeyFile, [1, 2, 3]);
        Set("{\"security\":{\"keyProtection\":\"passphrase\"}}");
        NewVault();
        _storeOpen = false;
        _agent.Unlocked = false;
        Assert.True((await Call(Ui, "unlock some passphrase")).Ok);   // Hello only
        Assert.Contains("unlock", _agent.Calls);
        Assert.True((await Call(Ui, "purge all")).Ok);
    }

    [Fact]
    public async Task First_passphrase_rewraps_only_after_hello_passes()
    {
        Assert.True(_vault.TryLoadSilent());   // dpapi key file
        var before = File.ReadAllBytes(KeyFile);
        Set("{\"security\":{\"keyProtection\":\"passphrase\"}}");
        _vault.SetHelloAvailableForTest(true);   // sweep 3 T5: a failed prompt without Hello is not a cancel
        _agent.Unlocked = false;
        _agent.UnlockResult = false;   // Hello cancelled
        Assert.Equal("Unlock cancelled.", (await Call(Ui, "unlock intruder pass")).Error);
        Assert.Equal(before, File.ReadAllBytes(KeyFile));
        Assert.Equal(KeyVault.ModeDpapi, _vault.Protection);

        _agent.UnlockResult = true;
        Assert.True((await Call(Ui, "unlock owner passphrase")).Ok);
        Assert.Equal(KeyVault.ModePassphrase, _vault.Protection);
        NewVault();
        Assert.True(_vault.NeedsPassphrase);
        Assert.False(_vault.UnlockWithPassphrase("intruder pass"));
        Assert.True(_vault.UnlockWithPassphrase("owner passphrase"));
    }

    [Fact]
    public async Task Wrong_key_for_the_store_is_reported_as_lost()
    {
        using var other = new TempHome();
        var home = new TempHome();
        try
        {
            await using (var core = new CoreHostImpl(new CoreHostOptions(home.Path, AppContext.BaseDirectory)))
                await core.OpenStoreAsync(CancellationToken.None);
            // Another folder's (valid) key file swapped in: TryLoadSilent loads it, store.db does not open with it.
            using (var v = new KeyVault(other.Path, () => SettingsStore.Load(other.Path).Current, probeHello: false)) Assert.True(v.TryLoadSilent());
            File.Copy(DataPaths.KeyFile(other.Path), DataPaths.KeyFile(home.Path), overwrite: true);
            await using (var core = new CoreHostImpl(new CoreHostOptions(home.Path, AppContext.BaseDirectory)))
                await Assert.ThrowsAsync<KeyLostException>(() => core.OpenStoreAsync(CancellationToken.None));
            File.Delete(DataPaths.KeyFile(home.Path));
            await using (var core = new CoreHostImpl(new CoreHostOptions(home.Path, AppContext.BaseDirectory)))
                await Assert.ThrowsAsync<KeyLostException>(() => core.OpenStoreAsync(CancellationToken.None));
            Assert.False(File.Exists(DataPaths.KeyFile(home.Path)));
        }
        finally { home.Dispose(); }
    }
}

/// <summary>Bug sweep 2, batch E: names scoped per data folder (known issue, C7, C8) and the dev-build guard (C5).</summary>
public sealed class InstanceScopeTests
{
    private static readonly string Default = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "BetterRecall");

    [Fact]
    public void Default_home_keeps_plain_names_other_homes_get_the_mutex_suffix()
    {
        const string sid = "S-1-5-21-1";
        Assert.Equal("BetterRecall.ui." + sid, PipeNames.Ui(sid, Default));
        Assert.Equal("BetterRecall.mcp." + sid, PipeNames.Mcp(sid, Default));
        Assert.Equal("BetterRecall." + sid, PipeNames.Public("BetterRecall", sid, Default));
        Assert.Equal(PipeNames.ShowLauncherEvent, PipeNames.ShowLauncherEventFor(Default));
        Assert.Equal("BetterRecall.UI", PipeNames.UiInstanceKeyFor(Default));

        var other = Path.Combine(TestEnv.Root, "some-home");
        var suffix = PipeNames.AgentMutexFor(other)[PipeNames.AgentMutex.Length..];
        Assert.Matches(@"^\.[0-9A-F]{16}$", suffix);
        Assert.Equal("BetterRecall.ui." + sid + suffix, PipeNames.Ui(sid, other));
        Assert.Equal("BetterRecall.mcp." + sid + suffix, PipeNames.Mcp(sid, other));
        Assert.Equal("x." + sid + suffix, PipeNames.Public(@"\\.\pipe\x", sid, other));
        Assert.Equal(PipeNames.ShowLauncherEvent + suffix, PipeNames.ShowLauncherEventFor(other));
        Assert.Equal("BetterRecall.UI" + suffix, PipeNames.UiInstanceKeyFor(other));
        Assert.Equal(PipeNames.Ui(sid, other), PipeNames.Ui(sid, other + "\\"));   // same folder, same name
        Assert.True(PipeNames.Reserved(PipeNames.Ui(sid, other)));
    }

    [Fact]
    public void Pipe_manager_names_follow_the_home_env()
    {
        var s = new Settings();
        var home = Environment.GetEnvironmentVariable(DataPaths.HomeEnv)!;   // tests run on their own home
        Assert.Equal(PipeNames.Ui(PipeNames.UserSid(), home), PipeManager.DefaultName(PipeKind.Ui, s));
        Assert.NotEqual(PipeNames.Ui(PipeNames.UserSid(), Default), PipeManager.DefaultName(PipeKind.Ui, s));
    }

    [Fact]
    public void Dev_build_refuses_the_default_home_without_the_flag()
    {
        using var dir = new TempHome(null);
        Assert.False(InstallLayout.RefusesHome(dir.Path, Default, false));   // no marker: not a dev build
        File.WriteAllText(Path.Combine(dir.Path, InstallLayout.DevBuildMarker), "dev");
        Assert.True(InstallLayout.RefusesHome(dir.Path, Default, false));
        Assert.False(InstallLayout.RefusesHome(dir.Path, Default, true));
        Assert.False(InstallLayout.RefusesHome(dir.Path, Path.Combine(TestEnv.Root, "scratch-home"), false));
    }
}
