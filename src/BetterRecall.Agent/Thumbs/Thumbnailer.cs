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
                    var (bytes, w, h) = ImageCodec.ResizeToWebp(kf.Bytes, width, 70);
                    var thumb = new ThumbData(id, "image/webp", w, h, bytes);
                    _cache.Put((id, width), thumb);
                    return thumb;
                }
                finally { Array.Clear(kf.Bytes); }
            }, ct).ConfigureAwait(false);
        }
        finally { _encodeSlots.Release(); }
    }

    public void ClearCache() => _cache.Clear();
}
