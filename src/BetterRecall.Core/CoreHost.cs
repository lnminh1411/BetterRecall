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
    private IThumbnailer? _thumbs;

    /// <summary>Cloud describe redactor, set by the agent before StartPipes (OCR + privacy.redact* on the outgoing image).</summary>
    public Ai.IDescribeRedactor? DescribeRedactor { get; set; }

    public CoreHostImpl(CoreHostOptions options)
    {
        ArgumentNullException.ThrowIfNull(options);
        _options = options with { Home = Path.GetFullPath(options.Home), InstallDir = Path.GetFullPath(options.InstallDir) };
        Directory.CreateDirectory(_options.Home);
        if (!Log.Initialized) Log.Init(DataPaths.LogsDir(_options.Home), "core");
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

    public async Task OpenStoreAsync(CancellationToken ct)
    {
        await _storeGate.WaitAsync(ct).ConfigureAwait(false);
        try
        {
            if (_store != null) return;
            var loaded = _vault.WhenLoaded;
            if (!await Task.Run(_vault.TryLoadSilent, ct).ConfigureAwait(false))
            {
                Log.Info("data key needs the passphrase; waiting for unlock");
                await loaded.WaitAsync(ct).ConfigureAwait(false);
            }
            _store = await Task.Run(() => new EncryptedStore(_options.Home, () => _vault.Derive("betterrecall-sqlite")), ct).ConfigureAwait(false);
            Log.Info("store opened");
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
            Blobs = _blobs,
            Home = _options.Home,
            Password = new UnlockPassword(_options.Home),
            AiKey = new Ai.ApiKeyStore(_options.Home),
            Describer = new Ai.CloudDescriber(),
            // Redaction hook for cloud describe: the agent's keyframe redactor when set, else pass-through (preview says not redacted).
            Redactor = DescribeRedactor ?? new Ai.PassThroughRedactor(),
        });
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
    internal async Task<int> PurgeAllAsync(CancellationToken ct)
    {
        await _storeGate.WaitAsync(ct).ConfigureAwait(false);
        try
        {
            var store = _store;
            int rows = 0;
            if (store != null)
            {
                try { rows = store.CountPurge(new PurgeScope("all")); } catch { }
                _store = null;
                store.DeleteAllFiles();
            }
            _vault.DestroyKey();
            var replays = DataPaths.ReplaysDir(_options.Home);
            if (Directory.Exists(replays))
                foreach (var f in Directory.GetFiles(replays))
                    try { File.Delete(f); } catch (Exception ex) { Log.Error("replay delete failed", ex); }
            try { _thumbs?.ClearCache(); } catch (Exception ex) { Log.Warn("thumbnail cache clear failed", ex); }
            Log.Info("purge all done");
            if (_vault.TryLoadSilent())
                _store = new EncryptedStore(_options.Home, () => _vault.Derive("betterrecall-sqlite"));
            else
                _ = Task.Run(async () => { await _vault.WhenLoaded.ConfigureAwait(false); await OpenStoreAsync(CancellationToken.None).ConfigureAwait(false); });
            return rows;
        }
        finally { _storeGate.Release(); }
    }

    public async ValueTask DisposeAsync()
    {
        await StopPipesAsync().ConfigureAwait(false);
        _store?.Dispose();
        _store = null;
        _vault.Dispose();
    }
}
