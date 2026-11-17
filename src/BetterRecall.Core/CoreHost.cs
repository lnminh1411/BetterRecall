// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;
using BetterRecall.Core.Ipc;
using BetterRecall.Core.Store;

namespace BetterRecall.Core;

/// <summary>Entry point the agent uses to get every Core service.</summary>
public static class CoreHost
{
    public static ICoreHost Create(CoreHostOptions options) => new CoreHostImpl(options);
}

/// <summary>
/// ICoreHost implementation. Notes for the agent:
/// - In passphrase mode OpenStoreAsync waits for `unlock &lt;passphrase&gt;` on the UI pipe, so call StartPipes first.
/// - Store is replaced after `purge all`; always read host.Store, never cache it.
/// - Panic flush: call Store.Purge(PurgeScope.Last(TimeSpan.FromMinutes(privacy.panicPurgeMinutes))); with 0 minutes
///   it deletes nothing and still runs wal_checkpoint(TRUNCATE). EncryptedStore.Checkpoint() does only the checkpoint.
/// </summary>
public sealed class CoreHostImpl : ICoreHost
{
    private readonly CoreHostOptions _options;
    private readonly SettingsStore _settings;
    private readonly KeyVault _vault;
    private readonly BlobCipher _blobs;
    private readonly SemaphoreSlim _storeGate = new(1, 1);
    private volatile EncryptedStore? _store;
    private PipeManager? _pipes;
    private CommandProcessor? _processor;
    private IThumbnailer? _thumbs;

    /// <summary>Cloud describe redactor, set by the agent before StartPipes (OCR + privacy.redact* on the outgoing image).</summary>
    public Ai.IDescribeRedactor? DescribeRedactor { get; set; }

    public CoreHostImpl(CoreHostOptions options)
    {
        ArgumentNullException.ThrowIfNull(options);
        _options = options with { Home = Path.GetFullPath(options.Home), InstallDir = Path.GetFullPath(options.InstallDir) };
        Directory.CreateDirectory(_options.Home);
        if (!Log.Initialized) Log.Init(DataPaths.LogsDir(_options.Home), "core");
        ExportJournal.Sweep(_options.Home);   // plaintext export partials a killed agent left behind (C3)
        _settings = SettingsStore.Load(_options.Home);
        Log.Level = Log.ParseLevel(_settings.Current.Log.Level);
        _settings.Changed += s =>
        {
            Log.Level = Log.ParseLevel(s.Log.Level);
            try { _vault?.MigrateIfNeeded(); } catch (Exception ex) { Log.Warn("key re-wrap failed", ex); }
        };
        _vault = new KeyVault(_options.Home, () => _settings.Current);
        _blobs = BlobCipher.FromVault(_vault);
    }

    public ISettingsStore Settings => _settings;
    public IKeyVault Keys => _vault;
    public IBlobCipher Blobs => _blobs;
    public IStore? Store => _store;

    /// <summary>The concrete store (for Checkpoint and the index), null until opened.</summary>
    public EncryptedStore? EncryptedStore => _store;

    /// <summary>Total connected pipe clients (tray indicator). Raised from pipe threads.</summary>
    public event Action<int>? ClientsChanged;

    /// <summary>Rows left the store (every store instance, and purge all). See ICoreHost.Purged.</summary>
    public event Action<PurgeNotice>? Purged;

    private EncryptedStore Wire(EncryptedStore store)
    {
        store.Purged += OnStorePurged;
        // A store opened after purge all refuses what was captured before it (A-H1).
        if (_purgedAllAt is { } at) store.FenceUpTo(at);
        return store;
    }

    /// <summary>When the last purge all finished (RAM only), for the fence of the stores opened after it. Taken after the
    /// files are gone, not at the notice: an item grabbed in between (seconds; ring and grab queue already cleared) is
    /// refused too, which also absorbs small drift between audio QPC stamps and the wall clock.</summary>
    private DateTimeOffset? _purgedAllAt;

    private volatile bool _purgingAll;

    /// <summary>Purge all is running: set before its notice, cleared before the new store opens. The agent refuses
    /// replay saves and exports that would start inside it (they could open a clip under the old key).</summary>
    public bool PurgingAll => _purgingAll;

    /// <summary>Replay files the last purge all could not delete (held open elsewhere); listed in exports.pending so the
    /// next start retries them. Reported in the purge all reply.</summary>
    public int LastPurgeAllUndeleted { get; private set; }

    /// <summary>Fans a purge out: chat state in Core first, then the agent's listeners (thumbnail cache, replay ring,
    /// samples, queued jobs). A failing listener does not stop the others. Purge all raises it while holding _storeGate:
    /// a listener must never wait on the gate (OpenStoreAsync, PurgeAllAsync).</summary>
    private void OnStorePurged(PurgeNotice n)
    {
        try { _processor?.OnStorePurged(n); } catch (Exception ex) { Log.Warn("purge: chat cleanup failed", ex); }
        if (Purged is not { } h) return;
        foreach (var d in h.GetInvocationList())
        {
            try { ((Action<PurgeNotice>)d)(n); }
            catch (Exception ex) { Log.Warn("purge listener failed", ex); }
        }
    }

    public async Task OpenStoreAsync(CancellationToken ct)
    {
        while (true)
        {
            if (_store != null) return;
            // B1: the passphrase wait holds no gate, so purge all (the way out of a forgotten passphrase) can run meanwhile.
            // DestroyKey completes the old wait; the loop then waits on the new key.
            var loaded = _vault.WhenLoaded;
            if (!await Task.Run(_vault.TryLoadSilent, ct).ConfigureAwait(false))
            {
                Log.Info("data key needs the passphrase; waiting for unlock");
                await loaded.WaitAsync(ct).ConfigureAwait(false);
            }
            if (await OpenLoadedAsync(ct).ConfigureAwait(false)) return;
        }
    }

    /// <summary>Opens the store under the gate. False when the key went away first (purge all destroyed it).</summary>
    private async Task<bool> OpenLoadedAsync(CancellationToken ct)
    {
        await _storeGate.WaitAsync(ct).ConfigureAwait(false);
        try
        {
            if (_store != null) return true;
            if (!_vault.KeyLoaded) return false;
            try { _store = Wire(await Task.Run(() => new EncryptedStore(_options.Home, () => _vault.Derive("betterrecall-sqlite")), ct).ConfigureAwait(false)); }
            catch (Microsoft.Data.Sqlite.SqliteException ex) when (ex.SqliteErrorCode == 26)   // SQLITE_NOTADB
            {
                // The loaded key does not decrypt store.db (a key file swapped in from elsewhere, or a replaced key): a retry
                // cannot help, only purge all.
                throw new KeyLostException("The data key does not open this store.", ex);
            }
            Log.Info("store opened");
            return true;
        }
        finally { _storeGate.Release(); }
    }

    public void StartPipes(IAgentControl agent, IQueryEmbedder embedder, IThumbnailer thumbnailer)
    {
        ArgumentNullException.ThrowIfNull(agent);
        ArgumentNullException.ThrowIfNull(embedder);
        ArgumentNullException.ThrowIfNull(thumbnailer);
        if (_pipes != null) return;
        _thumbs = thumbnailer;
        var processor = new CommandProcessor(new CommandDeps
        {
            Settings = _settings,
            Store = () => _store,
            Keys = _vault,
            Agent = agent,
            Embedder = embedder,
            Thumbnailer = thumbnailer,
            PurgeAll = PurgeAllAsync,
            PurgeAllUndeleted = () => LastPurgeAllUndeleted,
            Blobs = _blobs,
            Home = _options.Home,
            Password = new UnlockPassword(_options.Home),
            AiKey = new Ai.ApiKeyStore(_options.Home),
            Describer = new Ai.CloudDescriber(),
            // Redaction hook for cloud describe: the agent's keyframe redactor when set, else pass-through (preview says not redacted).
            Redactor = DescribeRedactor ?? new Ai.PassThroughRedactor(),
        });
        _processor = processor;
        _pipes = new PipeManager(_settings, processor, _options.InstallDir, () => _store);
        _pipes.ClientsChanged += n => ClientsChanged?.Invoke(n);
        _pipes.StartAsync().GetAwaiter().GetResult();
    }

    public async Task StopPipesAsync()
    {
        var p = _pipes;
        _pipes = null;
        if (p != null) await p.DisposeAsync().ConfigureAwait(false);
    }

    /// <summary>purge all: destroys the data key and deletes every store, keyframe and replay file, then starts empty.</summary>
    internal Task<int> PurgeAllAsync(CancellationToken ct) => PurgeAllGatedAsync(ct);

    private async Task<int> PurgeAllGatedAsync(CancellationToken ct)
    {
        await _storeGate.WaitAsync(ct).ConfigureAwait(false);
        try
        {
            var store = _store;
            int rows = 0;
            if (store != null)
                try { rows = store.CountPurge(new PurgeScope("all")); } catch { }
            _purgingAll = true;
            _store = null;   // nothing new is written to the old store from here on
            if (store != null)
            {
                store.Purged -= OnStorePurged;   // its own "all" reaches only its direct listeners (a running capture export)
                // Close before the agent hears: a reader still holding the old store (the embedding scheduler) can no
                // longer refill its queue with pre-purge images after the agent cleared it.
                store.CloseForPurgeAll();
            }
            // Everything in RAM goes next, before any file is deleted (A-H1, C1): the replay ring, samples and queued jobs
            // would otherwise outlive the data key; a replay save in flight is discarded and a replay export cancelled
            // (its source file can then be deleted below).
            OnStorePurged(new PurgeNotice("all", [], DateTimeOffset.MinValue, DateTimeOffset.UtcNow));
            EncryptedStore.DeleteFiles(_options.Home);   // also when the store never opened: its key is lost or locked
            _vault.DestroyKey();
            var replays = DataPaths.ReplaysDir(_options.Home);
            int undeleted = 0;
            if (Directory.Exists(replays))
            {
                // A cancelled replay export or save lets go of its file a moment after the notice: retry briefly.
                for (int attempt = 0; ; attempt++)
                {
                    bool last = attempt >= 20;
                    int failed = 0;
                    foreach (var f in Directory.GetFiles(replays))
                        try { File.Delete(f); }
                        catch (Exception ex)
                        {
                            failed++;
                            if (!last) continue;
                            Log.Error("replay delete failed", ex);
                            ExportJournal.Add(_options.Home, f);   // the next start retries it
                        }
                    if (failed == 0 || last) { undeleted = failed; break; }
                    await Task.Delay(100, CancellationToken.None).ConfigureAwait(false);
                }
            }
            LastPurgeAllUndeleted = undeleted;
            // A plain clip held open by a player stays readable until it is deleted: say so (reply and log), never "all gone".
            if (undeleted > 0) Log.Error($"purge all: {undeleted} replay file(s) could not be deleted (in use); deleted at the next start");
            try { _thumbs?.ClearCache(); } catch (Exception ex) { Log.Warn("thumbnail cache clear failed", ex); }
            Log.Info("purge all done");
            _purgedAllAt = DateTimeOffset.UtcNow;
            _purgingAll = false;
            if (_vault.TryLoadSilent())
                _store = Wire(new EncryptedStore(_options.Home, () => _vault.Derive("betterrecall-sqlite")));
            else
                _ = Task.Run(async () => { await _vault.WhenLoaded.ConfigureAwait(false); await OpenStoreAsync(CancellationToken.None).ConfigureAwait(false); });
            return rows;
        }
        finally
        {
            _purgingAll = false;
            _storeGate.Release();
        }
    }

    public async ValueTask DisposeAsync()
    {
        await StopPipesAsync().ConfigureAwait(false);
        _store?.Dispose();
        _store = null;
        _vault.Dispose();
    }
}
