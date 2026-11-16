// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Reflection;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Ai;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;
using BetterRecall.Core.Store;

namespace BetterRecall.Core.Tests;

/// <summary>Bug sweep 3, batch J (privacy and purge reach), Core side. Built on the cross-check probes (A checks B).</summary>
public sealed class Sweep3JCoreTests : IAsyncDisposable
{
    private readonly TempStore _t = new();
    private readonly TempHome _home = new(null);
    private readonly SettingsStore _settings;
    private readonly FakeKeys _keys = new();
    private readonly FakeAgent _agent = new();
    private readonly FakeHttpServer _http = new();
    private readonly CommandProcessor _p;
    private readonly ClientSession _ui = new(PipeKind.Ui, 101, @"C:\app\ui\BetterRecall.UI.exe");
    private readonly ClientSession _ipc = new(PipeKind.Public, 202, @"C:\tools\script.exe");
    private DateTimeOffset _now = DateTimeOffset.UtcNow;
    private static readonly byte[] Image = [.. "RIFF"u8, 0, 0, 0, 0, .. "WEBPVP8 test-image-bytes"u8];

    public Sweep3JCoreTests()
    {
        _settings = SettingsStore.Load(_home.Path);
        _p = new CommandProcessor(new CommandDeps
        {
            Settings = _settings,
            Store = () => _t.Store,
            Keys = _keys,
            Agent = _agent,
            Embedder = new FakeEmbedder(),
            Thumbnailer = new FakeThumbs(),
            PurgeAll = _ => Task.FromResult(0),
            Password = new UnlockPassword(_home.Path, () => _now, memKiB: 1024, passes: 1, lanes: 1),
            AiKey = new ApiKeyStore(_home.Path),
            Describer = new CloudDescriber(),
        })
        { Now = () => _now };
    }

    public async ValueTask DisposeAsync()
    {
        await _http.DisposeAsync();
        _t.Dispose();
        _home.Dispose();
    }

    private async Task<(bool Ok, JsonElement Data, string? Error)> Call(string line, ClientSession? s = null)
    {
        var r = await _p.HandleAsync(s ?? _ui, line, CancellationToken.None);
        return Envelope.Parse(r.Line);
    }

    private void Set(string json) => Assert.Null(_settings.TryUpdate(JsonDocument.Parse(json).RootElement));

    private static CommandProcessor Processor(SettingsStore st, IKeyVault keys, FakeAgent agent, IStore? store, string home) => new(new CommandDeps
    {
        Settings = st, Store = () => store, Keys = keys, Agent = agent,
        Embedder = new FakeEmbedder(), Thumbnailer = new FakeThumbs(), PurgeAll = _ => Task.FromResult(0),
        Password = new UnlockPassword(home, memKiB: 1024, passes: 1, lanes: 1),
    });

    // ---- B6: groups that change protection are refused while the gate is closed; replay save stays

    [Theory]
    [InlineData("{\"replay\":{\"encryptExports\":false}}")]
    [InlineData("{\"replay\":{\"exportDir\":\"C:\\\\Users\\\\Public\"}}")]
    [InlineData("{\"index\":{\"dim\":256}}")]
    [InlineData("{\"audio\":{\"mic\":true}}")]
    [InlineData("{\"keyframes\":{\"format\":\"jpeg\"}}")]
    [InlineData("{\"inference\":{\"modelDir\":\"C:\\\\evil\"}}")]
    [InlineData("{\"inference\":{\"modelDir\":\"C:\\\\evil\"},\"inference\":{}}")]   // a repeated group does not hide the key
    public async Task B6_guarded_settings_refused_while_locked(string json)
    {
        var before = JsonSerializer.Serialize(_settings.Current, CoreJson.Default.Settings);
        _agent.Unlocked = false;
        Assert.Equal(CommandProcessor.ErrLocked, (await Call("settings set " + json)).Error);
        Assert.Equal(before, JsonSerializer.Serialize(_settings.Current, CoreJson.Default.Settings));
    }

    [Fact]
    public async Task B6_unguarded_keys_and_the_replay_hotkey_save_still_work_while_locked()
    {
        _agent.Unlocked = false;
        var r = await Call("settings set {\"ui\":{\"theme\":\"dark\"}}");
        Assert.True(r.Ok, r.Error);
        var save = await Call("replay save");   // the documented hotkey save while locked
        Assert.True(save.Ok, save.Error);
        _agent.Unlocked = true;
        Assert.True((await Call("settings set {\"audio\":{\"mic\":true}}")).Ok);
    }

    // ---- B7: passphrase protection with the password method needs a passphrase-wrapped key first

    [Fact]
    public async Task B7_passphrase_with_password_method_needs_the_passphrase_first()
    {
        using var h = new TempHome("dpapi");
        var st = SettingsStore.Load(h.Path);
        using var vault = new KeyVault(h.Path, () => st.Current, probeHello: false);
        Assert.True(vault.TryLoadSilent());
        var agent = new FakeAgent();
        var p = Processor(st, vault, agent, _t.Store, h.Path);
        async Task<(bool Ok, JsonElement Data, string? Error)> C(string l) => Envelope.Parse((await p.HandleAsync(_ui, l, CancellationToken.None)).Line);
        Assert.True((await C("password set {\"new\":\"gate-pw-1\"}")).Ok);
        Assert.Equal(CommandProcessor.ErrPassphraseFirst, (await C("settings set {\"security\":{\"keyProtection\":\"passphrase\",\"unlockMethod\":\"password\"}}")).Error);
        Assert.Equal("dpapi", st.Current.Security.KeyProtection);

        // The way through: passphrase with Hello, unlock with it (re-wraps), then the password method.
        Assert.True((await C("settings set {\"security\":{\"keyProtection\":\"passphrase\"}}")).Ok);
        Assert.Equal(CommandProcessor.ErrPassphraseFirst, (await C("settings set {\"security\":{\"unlockMethod\":\"password\"}}")).Error);
        Assert.True((await C("unlock my long passphrase")).Ok);
        Assert.Equal(KeyVault.ModePassphrase, vault.Protection);
        Assert.True((await C("settings set {\"security\":{\"unlockMethod\":\"password\"}}")).Ok);
    }

    // ---- B8: describe send of a capture purged after the preview

    [Fact]
    public async Task B8_describe_send_refuses_a_purged_capture()
    {
        var shot = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-3), "chrome.exe", "Acme invoice", image: Image));
        _t.Store.SetEmbedding(shot, Vec.Seeded(3), Vec.Info);
        Set($"{{\"ai\":{{\"enabled\":true,\"provider\":\"gemini\",\"endpoint\":\"{_http.BaseUrl}\"}}}}");
        Assert.True((await Call("ai setkey gem-key-123")).Ok);
        _http.ResponseBody = """{"candidates":[{"content":{"parts":[{"text":"described"}],"role":"model"}}]}""";
        var pv = await Call("describe preview " + shot);
        Assert.True(pv.Ok, pv.Error);
        var token = pv.Data.GetProperty("token").GetString()!;
        Assert.True(_t.Store.Purge(PurgeScope.Last(TimeSpan.FromMinutes(10))) >= 1);   // panic, then lock and unlock
        _agent.Lock();
        _agent.Unlocked = true;
        Assert.Equal(CommandProcessor.ErrNotFound, (await Call("describe send " + token)).Error);
        Assert.Empty(_http.Requests);
    }

    // ---- B9: audit query hashes are keyed, and cleared for searches in a purged window

    [Fact]
    public async Task B9_audit_query_hash_is_keyed_and_purge_window_clears_it()
    {
        using var h = new TempHome("dpapi");
        var st = SettingsStore.Load(h.Path);
        using var vault = new KeyVault(h.Path, () => st.Current, probeHello: false);
        Assert.True(vault.TryLoadSilent());
        var p = Processor(st, vault, new FakeAgent(), _t.Store, h.Path);
        _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-3), "chrome.exe", "pornhub video"));
        Assert.True(Envelope.Parse((await p.HandleAsync(_ui, "search pornhub", CancellationToken.None)).Line).Ok);
        var a = Assert.Single(_t.Store.ReadAudit(50), x => x.Verb == "search");
        Assert.Equal(64, a.QuerySha256.Length);
        Assert.NotEqual(CommandProcessor.Sha256("pornhub"), a.QuerySha256);   // a dictionary of words does not find it
        Assert.Equal(p.QueryHash("pornhub"), a.QuerySha256);

        _t.Store.Purge(PurgeScope.Last(TimeSpan.FromMinutes(5)));
        a = Assert.Single(_t.Store.ReadAudit(50), x => x.Verb == "search");
        Assert.Equal("", a.QuerySha256);   // the row stays, the hash of what was searched goes
    }

    /// <summary>Review S4: a site purge blanks the audit hashes of searches for the purged terms, saved searches naming
    /// them included, so searching the term again does not reveal the old row on the Audit page.</summary>
    [Fact]
    public async Task B9_site_purge_clears_the_audit_hashes_of_its_terms()
    {
        using var h = new TempHome("dpapi");
        var st = SettingsStore.Load(h.Path);
        using var vault = new KeyVault(h.Path, () => st.Current, probeHello: false);
        Assert.True(vault.TryLoadSilent());
        var p = Processor(st, vault, new FakeAgent(), _t.Store, h.Path);
        async Task<(bool Ok, JsonElement Data, string? Error)> C(string l) => Envelope.Parse((await p.HandleAsync(_ui, l, CancellationToken.None)).Line);
        var id = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddDays(-2), "chrome.exe", "video page") with { Site = "pornhub.com" });
        _t.Store.SetEmbedding(id, Vec.Seeded(9), Vec.Info);
        Assert.True((await C("search pornhub")).Ok);
        Assert.True((await C("search pornhub videos")).Ok);
        Assert.True((await C("history add pornhub videos")).Ok);
        Assert.True((await C("search invoice")).Ok);   // unrelated: keeps its hash
        var preview = await C("purge site pornhub.com");
        Assert.True(preview.Ok, preview.Error);
        Assert.True((await C("purge confirm " + preview.Data.GetProperty("confirm").GetString())).Ok);
        var audit = _t.Store.ReadAudit(50).Where(a => a.Verb == "search").ToList();
        Assert.Equal(3, audit.Count);
        Assert.Equal(2, audit.Count(a => a.QuerySha256 == ""));
        Assert.Contains(audit, a => a.QuerySha256 == p.QueryHash("invoice"));
    }

    // ---- B11: a throwing maintenance still raises the retention notice

    [Fact]
    public void B11_maintenance_that_throws_still_raises_the_retention_notice()
    {
        var old = _t.Store.AddAudioCapture(Caps.Audio(DateTimeOffset.UtcNow.AddDays(-40), "old secret"));
        var notices = new List<PurgeNotice>();
        _t.Store.Purged += notices.Add;
        var kfDir = Path.Combine(_t.Home.Path, "keyframes");
        Directory.CreateDirectory(kfDir);
        File.WriteAllBytes(Path.Combine(kfDir, "2099-01.db"), RandomNumberGenerator.GetBytes(8192));   // damaged month file
        var s = new Settings();
        s.Retention.Days = 30;
        Assert.NotNull(Record.Exception(() => _t.Store.RunMaintenance(s)));
        Assert.Null(_t.Store.Get(old));
        var n = Assert.Single(notices);
        Assert.Equal("retention", n.Reason);
        Assert.Contains(old, n.Ids);
    }

    // ---- B12: an export that a purge reaches is aborted

    [Fact]
    public void B12_export_aborts_when_a_purge_reaches_it()
    {
        var a = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-4), "chrome.exe", "FIRST-ROW-SECRET", image: Image));
        _t.Store.SetEmbedding(a, Vec.Seeded(5), Vec.Info);
        var b = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-3), "chrome.exe", "second row", image: Image));
        _t.Store.SetEmbedding(b, Vec.Seeded(6), Vec.Info);
        var path = Path.Combine(_home.Path, "out.zip");
        int calls = 0;
        ThumbData Transcode(ThumbData kf, string target)
        {
            if (calls++ == 0) _t.Store.Purge(PurgeScope.Last(TimeSpan.FromMinutes(10)));   // panic flush during the export
            return kf;
        }
        Assert.Throws<ExportPurgedException>(() => CaptureExporter.Write(_t.Store,
            new ExportRequest(DateTimeOffset.UtcNow.AddHours(-1), DateTimeOffset.UtcNow, path, Jpeg: true), Transcode, CancellationToken.None));
        Assert.Null(_t.Store.Get(a));
        Assert.False(File.Exists(path));
        Assert.False(File.Exists(path + ".partial"));
    }

    [Fact]
    public void B12_unrelated_purge_does_not_abort_an_export()
    {
        var kept = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-4), "chrome.exe", "kept", image: Image));
        _t.Store.SetEmbedding(kept, Vec.Seeded(7), Vec.Info);
        var other = _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddDays(-3), "notepad.exe", "outside the window", image: Image));
        _t.Store.SetEmbedding(other, Vec.Seeded(8), Vec.Info);
        var path = Path.Combine(_home.Path, "out2.zip");
        int calls = 0;
        ThumbData Transcode(ThumbData kf, string target)
        {
            if (calls++ == 0) _t.Store.Purge(PurgeScope.ForId(other));
            return kf;
        }
        var r = CaptureExporter.Write(_t.Store, new ExportRequest(DateTimeOffset.UtcNow.AddHours(-1), DateTimeOffset.UtcNow, path, Jpeg: true), Transcode, CancellationToken.None);
        Assert.Equal(1, r.Count);
        Assert.True(File.Exists(path));
    }

    // ---- B14: a new data-key passphrase needs at least 8 characters

    [Fact]
    public async Task B14_short_passphrase_neither_creates_nor_rewraps_the_key()
    {
        using var h = new TempHome("passphrase");
        var st = SettingsStore.Load(h.Path);
        using (var v = new KeyVault(h.Path, () => st.Current, probeHello: false))
        {
            Assert.False(v.TryLoadSilent());
            Assert.True(v.WouldSetPassphrase);
            Assert.False(v.UnlockWithPassphrase("x"));
            Assert.False(v.KeyLoaded);
            var p = Processor(st, v, new FakeAgent(), null, h.Path);
            Assert.Equal(CommandProcessor.ErrPassphraseRule, Envelope.Parse((await p.HandleAsync(_ui, "unlock short", CancellationToken.None)).Line).Error);
            Assert.True(v.UnlockWithPassphrase("a longer passphrase"));
        }
        using var h2 = new TempHome("dpapi");
        var st2 = SettingsStore.Load(h2.Path);
        using var v2 = new KeyVault(h2.Path, () => st2.Current, probeHello: false);
        Assert.True(v2.TryLoadSilent());
        Assert.Null(st2.TryUpdate(JsonDocument.Parse("{\"security\":{\"keyProtection\":\"passphrase\"}}").RootElement));
        Assert.False(v2.UnlockWithPassphrase("y"));
        Assert.Equal(KeyVault.ModeDpapi, v2.Protection);
        Assert.True(v2.UnlockWithPassphrase("long enough"));
        Assert.Equal(KeyVault.ModePassphrase, v2.Protection);
        Assert.False(v2.WouldSetPassphrase);
        Assert.True(KeyVault.ValidNewPassphrase("12345678"));
        Assert.False(KeyVault.ValidNewPassphrase("        "));
    }

    /// <summary>Review S1: an install already in the B7 state (keyProtection passphrase, unlockMethod password, key still
    /// dpapi) with a 6-character unlock password still unlocks: the 8-character passphrase rule is not applied to it.</summary>
    [Fact]
    public async Task B14_rule_does_not_apply_to_a_short_unlock_password()
    {
        using var h = new TempHome("dpapi");
        var st = SettingsStore.Load(h.Path);
        using var vault = new KeyVault(h.Path, () => st.Current, probeHello: false);
        Assert.True(vault.TryLoadSilent());
        var agent = new FakeAgent();
        var p = Processor(st, vault, agent, _t.Store, h.Path);
        async Task<(bool Ok, JsonElement Data, string? Error)> C(string l) => Envelope.Parse((await p.HandleAsync(_ui, l, CancellationToken.None)).Line);
        Assert.True((await C("password set {\"new\":\"abc123\"}")).Ok);
        Assert.Null(st.TryUpdate(JsonDocument.Parse("{\"security\":{\"keyProtection\":\"passphrase\",\"unlockMethod\":\"password\"}}").RootElement));   // legacy state
        Assert.True(vault.WouldSetPassphrase);
        agent.Unlocked = false;
        var r = await C("unlock abc123");
        Assert.True(r.Ok, r.Error);
    }

    // ---- B16: the unlock password alone cannot move a passphrase-wrapped key off passphrase

    [Fact]
    public async Task B16_password_unlock_cannot_strip_the_passphrase_wrap()
    {
        using var h = new TempHome("passphrase");
        var host = (CoreHostImpl)CoreHost.Create(new CoreHostOptions(h.Path, h.Path));
        await using (host)
        {
            var open = host.OpenStoreAsync(CancellationToken.None);
            Assert.True(host.Keys.UnlockWithPassphrase("correct horse battery"));
            await open;
            var agent = new FakeAgent();
            var p = Processor((SettingsStore)host.Settings, host.Keys, agent, host.Store, h.Path);
            async Task<(bool Ok, JsonElement Data, string? Error)> C(string l) => Envelope.Parse((await p.HandleAsync(_ui, l, CancellationToken.None)).Line);
            Assert.True((await C("password set {\"new\":\"six666\"}")).Ok);
            Assert.True((await C("settings set {\"security\":{\"unlockMethod\":\"password\"}}")).Ok);
            agent.Unlocked = false;
            Assert.True((await C("unlock six666")).Ok);   // the weaker gate secret
            Assert.Equal(CommandProcessor.ErrPassphraseNeeded, (await C("settings set {\"security\":{\"keyProtection\":\"dpapi\"}}")).Error);
            Assert.Equal(CommandProcessor.ErrPassphraseNeeded, (await C("settings set {\"security\":{\"unlockMethod\":\"hello\",\"keyProtection\":\"dpapi\"}}")).Error);
            Assert.Equal(KeyVault.ModePassphrase, host.Keys.Protection);

            // With the passphrase checked at unlock, the change goes through.
            Assert.True((await C("settings set {\"security\":{\"unlockMethod\":\"hello\"}}")).Ok);
            Assert.True((await C("lock")).Ok);
            Assert.True((await C("unlock correct horse battery")).Ok);
            Assert.True((await C("settings set {\"security\":{\"keyProtection\":\"dpapi\"}}")).Ok);
            Assert.Equal(KeyVault.ModeDpapi, host.Keys.Protection);
        }
    }

    // ---- B17: the key is scrubbed before the error message is cut

    [Fact]
    public async Task B17_describe_error_never_shows_a_key_prefix()
    {
        var key = "sk-" + new string('A', 20) + "SECRETTAIL" + new string('Z', 20);
        _http.Status = 401;
        _http.ResponseBody = JsonSerializer.Serialize(new { error = new { message = new string('m', 190) + key } });
        var ai = new AiSettings { Enabled = true, Provider = AiDefaults.OpenAi, Endpoint = _http.BaseUrl, Model = "m" };
        var ex = await Assert.ThrowsAsync<DescribeException>(() => new CloudDescriber().DescribeAsync(ai, key, new OutgoingImage(Image, "image/webp", 1, 1, false), CancellationToken.None));
        Assert.DoesNotContain("sk-A", ex.Message);
        Assert.Contains("***", ex.Message);
    }

    // ---- B18: capture events follow the subscriber's own pipe gate

    private sealed class HoldStream : Stream
    {
        private readonly TaskCompletionSource<int> _eof = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly StringBuilder Written = new();
        public void End() => _eof.TrySetResult(0);
        public override bool CanRead => true; public override bool CanSeek => false; public override bool CanWrite => true;
        public override long Length => 0; public override long Position { get => 0; set { } }
        public override void Flush() { }
        public override Task FlushAsync(CancellationToken ct) => Task.CompletedTask;
        public override int Read(byte[] b, int o, int c) => _eof.Task.Result;
        public override async ValueTask<int> ReadAsync(Memory<byte> m, CancellationToken ct = default) => await _eof.Task.WaitAsync(ct);
        public override Task<int> ReadAsync(byte[] b, int o, int c, CancellationToken ct) => _eof.Task.WaitAsync(ct);
        public override long Seek(long o, SeekOrigin s) => 0; public override void SetLength(long v) { }
        public override void Write(byte[] b, int o, int c) { lock (Written) Written.Append(Encoding.UTF8.GetString(b, o, c)); }
        public override ValueTask WriteAsync(ReadOnlyMemory<byte> m, CancellationToken ct = default) { lock (Written) Written.Append(Encoding.UTF8.GetString(m.Span)); return ValueTask.CompletedTask; }
        public override Task WriteAsync(byte[] b, int o, int c, CancellationToken ct) { Write(b, o, c); return Task.CompletedTask; }
    }

    [Fact]
    public async Task B18_ui_capture_stream_is_quiet_while_the_ui_gate_is_closed()
    {
        Set("{\"ipc\":{\"requireUnlocked\":false}}");
        var s = new HoldStream();
        var run = _p.StreamAsync(s, Subscription.Captures, Envelope.Ok("{\"subscribed\":\"captures\"}"), CancellationToken.None, PipeKind.Ui);
        await Task.Delay(300);
        _agent.RaiseCapture(new CaptureMeta("s_OPENCAP", DateTimeOffset.UtcNow, CaptureKind.Screen, "notepad.exe", "Notes", 0, 0, true, false));
        _agent.Unlocked = false;   // UI gate closed; the public pipe stays open (requireUnlocked false)
        Assert.False(_p.IsLocked(PipeKind.Public));
        _agent.RaiseCapture(new CaptureMeta("s_LOCKEDCAP", DateTimeOffset.UtcNow, CaptureKind.Screen, "bank.exe", "Bank", 0, 0, true, false));
        await Task.Delay(1000);
        s.End();
        await run.WaitAsync(TimeSpan.FromSeconds(10));
        string w; lock (s.Written) w = s.Written.ToString();
        Assert.Contains("s_OPENCAP", w);
        Assert.DoesNotContain("s_LOCKEDCAP", w);
        Assert.DoesNotContain("bank.exe", w);
    }

    // ---- B19 + AB-S1: purged rows leave no sign bits and no names in the RAM index

    [Fact]
    public void B19_purged_rows_leave_no_bits_or_names_in_the_ram_index()
    {
        var index = new BinaryVectorIndex();
        using var home = new TempHome(null);
        var key = RandomNumberGenerator.GetBytes(32);
        using var store = new EncryptedStore(home.Path, () => (byte[])key.Clone(), index);
        var keep = store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-3), "notepad.exe", "kept") with { Site = "" });
        store.SetEmbedding(keep, Vec.Seeded(41), Vec.Info);
        var id = store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-2), "tor.exe", "secret site") with { Site = "secret-site.example" });
        store.SetEmbedding(id, Vec.Seeded(42), Vec.Info);
        var before = index.NameCounts();
        store.Purge(PurgeScope.ForSite("secret-site.example"));
        Assert.Equal(1, index.Count);
        Assert.Equal((before.Apps - 1, before.Sites - 1), index.NameCounts());   // "tor.exe" and the host are gone
        Assert.Equal(-2, index.SiteId("secret-site.example"));
        Assert.Equal(-2, index.AppId("tor.exe"));
        Assert.NotEqual(-2, index.AppId("notepad.exe"));
        Assert.False(index.HasStaleBits(Vec.Seeded(42).Length));
    }

    [Fact]
    public void B19_compaction_clears_the_tail()
    {
        var index = new BinaryVectorIndex();
        var v = new float[64];
        for (int i = 0; i < 64; i++) v[i] = 1;
        for (long r = 1; r <= 6000; r++) index.Upsert(r, v, r, 0, "app" + (r % 7), "");
        for (long r = 1; r <= 5000; r++) index.Remove(r);   // over 4096 dead and a quarter: compacts
        Assert.Equal(1000, index.Count);
        Assert.False(index.HasStaleBits(64));
        Assert.Equal(7, index.NameCounts().Apps);   // every app still has live rows
        Assert.Single(index.Scan(v, VectorFilter.All, 1));
    }

    // ---- D12: no store stats over the public pipe while locked

    [Fact]
    public async Task D12_public_status_hides_store_stats_while_locked()
    {
        _agent.Status = _agent.Status with { Store = new StoreStats(42, 1000, 10, DateTimeOffset.UtcNow.AddDays(-1), DateTimeOffset.UtcNow, 0, 42) };
        Set("{\"ipc\":{\"enabled\":true}}");
        var open = await Call("status", _ipc);
        Assert.True(open.Ok, open.Error);
        Assert.Equal(42, open.Data.GetProperty("store").GetProperty("rows").GetInt64());
        _agent.Unlocked = false;
        var locked = await Call("status", _ipc);
        Assert.True(locked.Ok, locked.Error);
        Assert.Equal(0, locked.Data.GetProperty("store").GetProperty("rows").GetInt64());
        Assert.False(locked.Data.GetProperty("store").TryGetProperty("newest", out var newest) && newest.ValueKind != JsonValueKind.Null);
        var ui = await Call("status");
        Assert.Equal(42, ui.Data.GetProperty("store").GetProperty("rows").GetInt64());   // the UI's own pipe keeps them
    }

    // ---- D13: purge app/site take what users type

    [Theory]
    [InlineData("claude.exe", "claude")]
    [InlineData("CLAUDE.EXE", "CLAUDE")]
    [InlineData(@"C:\Program Files\Claude\claude.exe", "claude")]
    [InlineData("claude", "claude")]
    public void D13_purge_app_name(string typed, string expected) => Assert.Equal(expected, CommandProcessor.PurgeAppName(typed));

    [Theory]
    [InlineData("https://www.google.com/path?q=1", "www.google.com")]
    [InlineData("www.google.com/path", "www.google.com")]
    [InlineData("example.org:8080", "example.org")]
    [InlineData("google.com", "google.com")]
    public void D13_purge_site_host(string typed, string expected) => Assert.Equal(expected, CommandProcessor.PurgeSiteHost(typed));

    [Fact]
    public async Task D13_purge_verbs_accept_exe_urls_and_report_overflow()
    {
        _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-3), "claude", "chat") with { Site = "" });   // the agent stores the name without .exe
        _t.Store.AddScreenCapture(Caps.Screen(DateTimeOffset.UtcNow.AddMinutes(-2), "chrome.exe", "search") with { Site = "google.com" });
        var app = await Call("purge app claude.exe");
        Assert.True(app.Ok, app.Error);
        Assert.Equal(1, app.Data.GetProperty("rows").GetInt32());
        Assert.Equal(1, (await Call("purge app chrome")).Data.GetProperty("rows").GetInt32());   // and rows stored with .exe match the bare name
        var site = await Call("purge site https://www.google.com/path");
        Assert.True(site.Ok, site.Error);
        Assert.Equal(1, site.Data.GetProperty("rows").GetInt32());
        Assert.Equal("Invalid seconds.", (await Call("purge last 99999999999999999999")).Error);
        Assert.StartsWith("Usage: purge", (await Call("purge last abc")).Error);
    }

    // ---- D4: Lock with security.requireUnlock off locks the data verbs

    [Fact]
    public async Task D4_agent_lock_without_require_unlock_closes_the_data_verbs()
    {
        Set("{\"security\":{\"requireUnlock\":false}}");
        Assert.True((await Call("timeline 2000-01-01T00:00:00Z 2100-01-01T00:00:00Z")).Ok);
        _agent.Unlocked = false;   // the agent's gate after Lock
        Assert.Equal(CommandProcessor.ErrLocked, (await Call("timeline 2000-01-01T00:00:00Z 2100-01-01T00:00:00Z")).Error);
        Assert.True((await Call("unlock")).Ok);
        Assert.True((await Call("timeline 2000-01-01T00:00:00Z 2100-01-01T00:00:00Z")).Ok);
    }
}
