// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Collections.Concurrent;
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Tests;

/// <summary>In-memory IStore: enough behaviour for agent tests, no encryption.</summary>
internal sealed class FakeStore : IStore
{
    public readonly ConcurrentDictionary<string, NewScreenCapture> Screens = new();
    public readonly ConcurrentDictionary<string, NewAudioCapture> Audio = new();
    public readonly ConcurrentDictionary<string, float[]> Vectors = new();
    public readonly ConcurrentDictionary<string, string> Dropped = new();
    public readonly ConcurrentDictionary<string, string> Failed = new();
    public readonly ConcurrentQueue<string> Pending = new();
    public readonly List<PurgeScope> Purges = new();
    public EmbeddingInfo? Space;
    public int Maintenance;
    private int _next;

    public string AddScreenCapture(NewScreenCapture c)
    {
        var id = $"s{Interlocked.Increment(ref _next)}";
        Screens[id] = c with { Image = (byte[])c.Image.Clone() };
        Pending.Enqueue(id);
        return id;
    }

    public string AddAudioCapture(NewAudioCapture c)
    {
        var id = $"a{Interlocked.Increment(ref _next)}";
        Audio[id] = c;
        Pending.Enqueue(id);
        return id;
    }

    public IReadOnlyList<PendingJob> TakePendingEmbeddings(int max)
    {
        var list = new List<PendingJob>();
        while (list.Count < max && Pending.TryDequeue(out var id))
        {
            if (Screens.TryGetValue(id, out var s)) list.Add(new PendingJob(id, CaptureKind.Screen, (byte[])s.Image.Clone(), s.ImageMime, null));
            else if (Audio.TryGetValue(id, out var a)) list.Add(new PendingJob(id, CaptureKind.Audio, null, null, a.Text));
        }
        return list;
    }

    public void SetEmbedding(string id, ReadOnlySpan<float> vector, EmbeddingInfo info) => Vectors[id] = vector.ToArray();
    public void FailEmbedding(string id, string reason) => Failed[id] = reason;
    public void DropCapture(string id, string reason) { Dropped[id] = reason; Screens.TryRemove(id, out _); }
    public SearchResponse Search(SearchRequest request, ReadOnlySpan<float> queryVector, int maxResults, int maxResponseBytes) => new(0, false, 0, []);
    public CaptureDetail? Get(string id) => null;

    public ThumbData? GetKeyframe(string id) =>
        Screens.TryGetValue(id, out var s) ? new ThumbData(id, s.ImageMime, s.Width, s.Height, (byte[])s.Image.Clone()) : null;

    public IReadOnlyList<CaptureMeta> Timeline(DateTimeOffset from, DateTimeOffset to, int limit) => [];
    public int CountPurge(PurgeScope scope) => 0;
    public int Purge(PurgeScope scope) { lock (Purges) Purges.Add(scope); return 0; }
    public void AppendAudit(AuditEntry entry) { }
    public IReadOnlyList<AuditEntry> ReadAudit(int limit) => [];
    public StoreStats Stats() => new(Screens.Count + Audio.Count, 0, 0, null, null, Pending.Count, Vectors.Count);
    public void RunMaintenance(Settings settings) => Maintenance++;
    /// <summary>Storage guard tests: bytes on disk and reusable; DeleteForSpace frees FreedPerRow per row up to Deletable rows.</summary>
    public StorageUse Use = new(0, 0);
    public int Deletable;
    public long FreedPerRow;
    public int DeleteCalls;
    public StorageUse GetStorageUse() => Use;
    public int DeleteForSpace(int maxRows)
    {
        DeleteCalls++;
        int n = Math.Min(maxRows, Deletable);
        Deletable -= n;
        Use = new StorageUse(Use.FileBytes, Math.Min(Use.FileBytes, Use.FreeBytes + n * FreedPerRow));
        return n;
    }
    public void EnsureEmbeddingSpace(EmbeddingInfo info) => Space = info;
    public void Dispose() { }
}

internal sealed class FakeSettingsStore : ISettingsStore
{
    public Settings Current { get; set; } = new();
    public event Action<Settings>? Changed;
    public string? TryUpdate(JsonElement partial) { Changed?.Invoke(Current); return null; }
    public void Save() { }
}

internal sealed class FakeKeyVault : IKeyVault
{
    public string Protection => "dpapi";
    public bool HelloAvailable => true;
    public bool KeyLoaded => true;
    public bool VerifyResult = true;
    public bool UnlockWithPassphrase(string passphrase) => true;
    public Task<bool> VerifyUserAsync(nint hwnd, string reason, CancellationToken ct) => Task.FromResult(VerifyResult);
    public void DestroyKey() { }
}

/// <summary>"Encrypts" by XOR so tests can tell the bytes went through the cipher.</summary>
internal sealed class FakeBlobCipher : IBlobCipher
{
    public Stream OpenWrite(string path) => new XorStream(File.Create(path));
    public Stream OpenRead(string path) => new XorStream(File.OpenRead(path));

    private sealed class XorStream(Stream inner) : Stream
    {
        public override bool CanRead => inner.CanRead;
        public override bool CanSeek => false;
        public override bool CanWrite => inner.CanWrite;
        public override long Length => inner.Length;
        public override long Position { get => inner.Position; set => throw new NotSupportedException(); }
        public override void Flush() => inner.Flush();
        public override int Read(byte[] buffer, int offset, int count)
        {
            int n = inner.Read(buffer, offset, count);
            for (int i = 0; i < n; i++) buffer[offset + i] ^= 0x5A;
            return n;
        }
        public override void Write(byte[] buffer, int offset, int count)
        {
            var copy = new byte[count];
            for (int i = 0; i < count; i++) copy[i] = (byte)(buffer[offset + i] ^ 0x5A);
            inner.Write(copy, 0, count);
        }
        public override void Write(ReadOnlySpan<byte> buffer)
        {
            var copy = buffer.ToArray();
            for (int i = 0; i < copy.Length; i++) copy[i] ^= 0x5A;
            inner.Write(copy, 0, copy.Length);
        }
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        protected override void Dispose(bool disposing) { if (disposing) inner.Dispose(); base.Dispose(disposing); }
    }
}

internal sealed class FakeCoreHost : ICoreHost
{
    public FakeSettingsStore SettingsStore { get; } = new();
    public ISettingsStore Settings => SettingsStore;
    public IKeyVault Keys { get; } = new FakeKeyVault();
    public IBlobCipher Blobs { get; } = new FakeBlobCipher();
    public FakeStore FakeStore { get; } = new();
    public IStore? Store { get; private set; }
    public IAgentControl? Agent;
    public Task OpenStoreAsync(CancellationToken ct) { Store = FakeStore; return Task.CompletedTask; }
    public void StartPipes(IAgentControl agent, IQueryEmbedder embedder, IThumbnailer thumbnailer) => Agent = agent;
    public Task StopPipesAsync() => Task.CompletedTask;
    public ValueTask DisposeAsync() => ValueTask.CompletedTask;
}
