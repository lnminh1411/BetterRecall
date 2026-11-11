// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json;

namespace BetterRecall.Contracts;

/// <summary>Encrypted store. Implemented in Core. Thread-safe.</summary>
public interface IStore : IDisposable
{
    string AddScreenCapture(NewScreenCapture capture);
    string AddAudioCapture(NewAudioCapture capture);

    /// <summary>Oldest pending jobs first. Lease semantics: a returned job is held in memory as in flight and not
    /// returned again until SetEmbedding, FailEmbedding or DropCapture settles it. In-flight leases do not survive
    /// a store reopen, so after an agent restart unfinished jobs come back. On a failed worker call, call FailEmbedding.</summary>
    IReadOnlyList<PendingJob> TakePendingEmbeddings(int max);
    void SetEmbedding(string id, ReadOnlySpan<float> vector, EmbeddingInfo info);
    void FailEmbedding(string id, string reason);

    /// <summary>Deletes the row and its keyframe without indexing (NSFW gate).</summary>
    void DropCapture(string id, string reason);

    /// <summary>queryVector is empty for SearchMode.Text. maxResponseBytes trims results from the end.</summary>
    SearchResponse Search(SearchRequest request, ReadOnlySpan<float> queryVector, int maxResults, int maxResponseBytes);
    CaptureDetail? Get(string id);
    /// <summary>Stored keyframe bytes at stored size, or null.</summary>
    ThumbData? GetKeyframe(string id);
    IReadOnlyList<CaptureMeta> Timeline(DateTimeOffset from, DateTimeOffset to, int limit);

    /// <summary>Counts rows a purge would delete, without deleting.</summary>
    int CountPurge(PurgeScope scope);
    int Purge(PurgeScope scope);

    void AppendAudit(AuditEntry entry);
    IReadOnlyList<AuditEntry> ReadAudit(int limit);

    StoreStats Stats();

    /// <summary>Applies retention.days and progressive thinning (retention.progressive*). Agent calls it hourly and at start.</summary>
    void RunMaintenance(Settings settings);

    /// <summary>Store file sizes and their reusable free pages (storage.maxGb, storage.minFreeGb).</summary>
    StorageUse GetStorageUse() => new(Stats().Bytes, 0);

    /// <summary>storage.overwrite: deletes up to maxRows captures in progressive order (oldest day first; in a day
    /// the smallest changes first, window/app switches last) and returns the freed pages to the disk where it can.
    /// Returns rows deleted, 0 when nothing is left.</summary>
    int DeleteForSpace(int maxRows) => 0;

    /// <summary>Records the EmbeddingInfo in use. A smaller dim truncates and renormalises stored vectors in place.
    /// A different model re-queues embedding jobs for rows whose keyframe or text still exists; those rows stay
    /// visible to text search and timeline meanwhile.</summary>
    void EnsureEmbeddingSpace(EmbeddingInfo info);

    /// <summary>wal_checkpoint(TRUNCATE) on every open database (panic flush).</summary>
    void Checkpoint() { }

    /// <summary>Top apps and sites among searchable rows in [from, to), most rows first.</summary>
    FacetsReply Facets(DateTimeOffset? from, DateTimeOffset? to, int limit) => new([], []);

    /// <summary>Saved searches, newest first.</summary>
    IReadOnlyList<HistoryEntry> History(int limit) => [];
    /// <summary>Saves or bumps a search; keeps the newest max entries.</summary>
    void AddHistory(string q, DateTimeOffset ts, int max) { }
    /// <summary>Removes one saved search, or all of them when q is null. Returns rows removed.</summary>
    int DeleteHistory(string? q) => 0;

    /// <summary>Stores a frame sampled from a saved replay (kind replay) and queues its embedding.</summary>
    string AddReplayFrame(NewReplayFrame frame) => throw new NotSupportedException();
    /// <summary>Deletes every frame of a replay (replay deleted). Returns rows removed.</summary>
    int DeleteReplayFrames(string replayId) => 0;
}

public sealed record PurgeScope(string Kind, DateTimeOffset? From = null, DateTimeOffset? To = null, string? App = null, string? Site = null, string? Id = null)
{
    public static PurgeScope ForSite(string site) => new("site", Site: site);
    public static PurgeScope ForId(string id) => new("id", Id: id);
    public static PurgeScope Last(TimeSpan span) => new("last", DateTimeOffset.UtcNow - span, DateTimeOffset.UtcNow);
    public static PurgeScope Range(DateTimeOffset from, DateTimeOffset to) => new("range", from, to);
    public static PurgeScope ForApp(string app) => new("app", App: app);
}

/// <summary>On-demand preview from the stored keyframe, cached in RAM only. Implemented in the agent.</summary>
public interface IThumbnailer
{
    /// <summary>width 64..1280. WebP. Null when the row or keyframe is gone.</summary>
    Task<ThumbData?> GetThumbAsync(string id, int width, CancellationToken ct);
    void ClearCache();

    /// <summary>Stored keyframe to a format the reader can decode. target "ui": AVIF and JPEG XL become WebP, others
    /// pass through; "jpeg": always JPEG. The default passes everything through.</summary>
    ThumbData Transcode(ThumbData keyframe, string target) => keyframe;
}

/// <summary>AES-256-GCM file encryption with the data key, 64 KiB chunks. Implemented in Core.</summary>
public interface IBlobCipher
{
    Stream OpenWrite(string path);   // encrypting stream; dispose to finish
    Stream OpenRead(string path);    // decrypting stream; throws on tamper
}

/// <summary>settings.json. Implemented in Core.</summary>
public interface ISettingsStore
{
    Settings Current { get; }
    event Action<Settings>? Changed;

    /// <summary>Merges a partial settings object. Returns null on success, else "Invalid section.key.". Saves on success.</summary>
    string? TryUpdate(JsonElement partial);
    void Save();
}

/// <summary>Data key and the Windows Hello read gate. Implemented in Core.</summary>
public interface IKeyVault
{
    string Protection { get; }          // tpm | passphrase | dpapi
    bool HelloAvailable { get; }

    /// <summary>True when the data key is unwrapped in memory.</summary>
    bool KeyLoaded { get; }

    /// <summary>True in passphrase mode while the key is not yet loaded.</summary>
    bool NeedsPassphrase => false;

    /// <summary>Passphrase mode: unwraps the key. False on a wrong passphrase.</summary>
    bool UnlockWithPassphrase(string passphrase);

    /// <summary>Shows Windows Hello (or UserConsentVerifier) owned by hwnd. True when the user passed.</summary>
    Task<bool> VerifyUserAsync(nint hwnd, string reason, CancellationToken ct);

    /// <summary>Overwrites keys\datakey.bin. Every store file becomes unreadable.</summary>
    void DestroyKey();
}

/// <summary>Agent state the command processor reads and changes. Implemented in the agent.</summary>
public interface IAgentControl
{
    AgentStatus GetStatus();
    void Pause(TimeSpan? duration, string reason);
    void Resume(string reason);
    bool IsUnlocked { get; }
    Task<bool> UnlockAsync(CancellationToken ct);
    void Lock();
    void Consent();
    Task<ReplayClip> SaveReplayAsync(CancellationToken ct);
    IReadOnlyList<ReplayClip> ListReplays();
    Task ExportReplayAsync(string id, string path, CancellationToken ct);
    void DeleteReplay(string id);
    void Panic();
    Task<BenchResult> BenchAsync(CancellationToken ct);

    /// <summary>Hardware scan, worker bench (devices x image tokens), encoder bench, capture bench.
    /// Search, Recommendations stay empty: Core adds them. Takes up to a few minutes.</summary>
    Task<BenchReport> BenchFullAsync(CancellationToken ct) => throw new NotSupportedException();

    /// <summary>Opens the read gate for security.unlockTimeoutMin without a Windows Hello prompt. Core calls it only after it
    /// verified the unlock password (security.unlockMethod password) or the data-key passphrase.</summary>
    void OpenReadGate() => throw new NotSupportedException();
    event Action<AgentStatus>? StatusChanged;
    event Action<CaptureMeta>? CaptureStored;
}

/// <summary>Embeds a search query with the text tower. Implemented in the agent (forwards to the worker).</summary>
public interface IQueryEmbedder
{
    Task<float[]> EmbedQueryAsync(string text, CancellationToken ct);
}

/// <summary>Everything Core gives the agent. Core implements it as BetterRecall.Core.CoreHost.Create(CoreHostOptions).</summary>
public interface ICoreHost : IAsyncDisposable
{
    ISettingsStore Settings { get; }
    IKeyVault Keys { get; }
    IBlobCipher Blobs { get; }

    /// <summary>Null until OpenStoreAsync completes.</summary>
    IStore? Store { get; }

    /// <summary>Unwraps the data key (no prompt; passphrase mode waits for UnlockWithPassphrase), opens store.db, loads the vector index.</summary>
    Task OpenStoreAsync(CancellationToken ct);

    /// <summary>Starts the UI pipe always, the public and MCP pipes per settings. Follows settings changes live.</summary>
    void StartPipes(IAgentControl agent, IQueryEmbedder embedder, IThumbnailer thumbnailer);
    Task StopPipesAsync();

    /// <summary>Connected IPC/MCP client count changed (tray indicator).</summary>
    event Action<int>? ClientsChanged { add { } remove { } }
}

public sealed record CoreHostOptions(string Home, string InstallDir);
