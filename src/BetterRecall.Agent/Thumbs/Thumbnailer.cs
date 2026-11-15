// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Agent.Capture;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Thumbs;

/// <summary>Byte-bounded LRU. Evicted and cleared values are zeroed. Thread-safe.</summary>
internal sealed class ByteLruCache<TKey> where TKey : notnull
{
    private readonly object _gate = new();
    private readonly Dictionary<TKey, LinkedListNode<(TKey Key, ThumbData Value)>> _map = new();
    private readonly LinkedList<(TKey Key, ThumbData Value)> _order = new();
    private long _bytes;

    public ByteLruCache(long maxBytes) { MaxBytes = maxBytes; }

    public long MaxBytes { get; set; }
    public long Bytes { get { lock (_gate) return _bytes; } }
    public int Count { get { lock (_gate) return _map.Count; } }

    public bool TryGet(TKey key, out ThumbData value)
    {
        lock (_gate)
        {
            if (_map.TryGetValue(key, out var node))
            {
                _order.Remove(node);
                _order.AddFirst(node);
                value = node.Value.Value;
                return true;
            }
        }
        value = null!;
        return false;
    }

    public void Put(TKey key, ThumbData value)
    {
        long size = value.Bytes.Length;
        lock (_gate)
        {
            if (_map.TryGetValue(key, out var old))
            {
                _order.Remove(old);
                _map.Remove(key);
                _bytes -= old.Value.Value.Bytes.Length;
            }
            if (size > MaxBytes) return;
            var node = _order.AddFirst((key, value));
            _map[key] = node;
            _bytes += size;
            Trim();
        }
    }

    private void Trim()
    {
        while (_bytes > MaxBytes && _order.Last is { } last)
        {
            _order.RemoveLast();
            _map.Remove(last.Value.Key);
            _bytes -= last.Value.Value.Bytes.Length;
            // Not zeroed: a caller may still hold this ThumbData for its response.
        }
    }

    public void Resize(long maxBytes)
    {
        lock (_gate) { MaxBytes = maxBytes; Trim(); }
    }

    public void Clear()
    {
        lock (_gate)
        {
            foreach (var (_, v) in _order) Array.Clear(v.Bytes);
            _order.Clear();
            _map.Clear();
            _bytes = 0;
        }
    }
}

/// <summary>On-demand previews from stored keyframes (v1-design 3.5). RAM only, never written to disk.</summary>
internal sealed class Thumbnailer : IThumbnailer
{
    private readonly Func<IStore?> _store;
    private readonly Func<Settings> _settings;
    private readonly ByteLruCache<(string, int)> _cache;
    private readonly SemaphoreSlim _encodeSlots = new(2, 2);
    private long _generation;   // bumped by ClearCache: a thumbnail read before a purge is not cached after it

    public Thumbnailer(Func<IStore?> store, Func<Settings> settings)
    {
        _store = store;
        _settings = settings;
        _cache = new ByteLruCache<(string, int)>(settings().Keyframes.ThumbCacheMb * 1024L * 1024L);
    }

    public ByteLruCache<(string, int)> Cache => _cache;

    public async Task<ThumbData?> GetThumbAsync(string id, int width, CancellationToken ct)
    {
        width = Math.Clamp(width, 64, 1280);
        _cache.Resize(_settings().Keyframes.ThumbCacheMb * 1024L * 1024L);
        long gen = Interlocked.Read(ref _generation);
        if (_cache.TryGet((id, width), out var hit)) return hit;
        var store = _store() ?? throw new InvalidOperationException("Store is locked.");
        await _encodeSlots.WaitAsync(ct).ConfigureAwait(false);
        try
        {
            return await Task.Run(() =>
            {
                var kf = store.GetKeyframe(id);
                if (kf is null) return null;
                try
                {
                    var (bytes, w, h) = ImageCodec.ResizeToWebp(kf.Bytes, kf.Mime, width, 70);
                    var thumb = new ThumbData(id, "image/webp", w, h, bytes);
                    // A purge (ClearCache) ran while this was read: the row may be gone, so it is not cached (A10).
                    if (Interlocked.Read(ref _generation) == gen) _cache.Put((id, width), thumb);
                    return thumb;
                }
                finally { Array.Clear(kf.Bytes); }
            }, ct).ConfigureAwait(false);
        }
        finally { _encodeSlots.Release(); }
    }

    /// <summary>Lock, panic and every purge or delete (ICoreHost.Purged): cached thumbnails are zeroed and dropped.</summary>
    public void ClearCache()
    {
        Interlocked.Increment(ref _generation);
        _cache.Clear();
    }

    /// <summary>"ui": AVIF and JPEG XL become WebP (quality 90), others pass through; "jpeg": JPEG (quality 90).
    /// The input is not cleared: the caller owns it.</summary>
    public ThumbData Transcode(ThumbData keyframe, string target)
    {
        string? to = target switch
        {
            "jpeg" when keyframe.Mime != "image/jpeg" => "jpeg",
            "ui" when !ImageCodec.CommonlyDecodable(keyframe.Mime) => "webp",
            _ => null,
        };
        if (to is null) return keyframe;
        var (bytes, mime, w, h) = ImageCodec.Transcode(keyframe.Bytes, keyframe.Mime, to, 90);
        return new ThumbData(keyframe.Id, mime, w, h, bytes);
    }
}
