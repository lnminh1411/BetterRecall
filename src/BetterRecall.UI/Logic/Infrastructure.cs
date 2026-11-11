// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.ComponentModel;
using System.Runtime.CompilerServices;

namespace BetterRecall.UI.Logic;

public abstract class ObservableObject : INotifyPropertyChanged
{
    public event PropertyChangedEventHandler? PropertyChanged;

    protected bool Set<T>(ref T field, T value, [CallerMemberName] string? name = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value)) return false;
        field = value;
        Raise(name!);
        return true;
    }

    protected void Raise(string name) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}

/// <summary>A resource key plus format arguments. Logic returns these; the view resolves them from Resources.resw.</summary>
public sealed record LocalizedText(string Key, params object[] Args)
{
    public static readonly LocalizedText Empty = new("");
    public bool IsEmpty => Key.Length == 0;
}

/// <summary>Runs the last call after a quiet period. Earlier pending calls are cancelled.</summary>
public sealed class Debouncer(TimeSpan delay)
{
    private CancellationTokenSource? _cts;

    public TimeSpan Delay => delay;

    /// <summary>Returns true when the action ran, false when a newer call replaced it.</summary>
    public async Task<bool> RunAsync(Func<CancellationToken, Task> action)
    {
        _cts?.Cancel();
        var cts = _cts = new CancellationTokenSource();
        try
        {
            await Task.Delay(delay, cts.Token);
            await action(cts.Token);
            return true;
        }
        catch (OperationCanceledException) when (cts.IsCancellationRequested)
        {
            return false;
        }
    }

    public void Cancel() => _cts?.Cancel();
}

/// <summary>
/// Small LRU of encoded preview bytes, keyed by id and width. RAM only, cleared when the launcher hides.
/// Concurrent loads of the same key share one request.
/// </summary>
public sealed class ThumbnailCache(long maxBytes)
{
    private readonly object _lock = new();
    private readonly LinkedList<(string Key, byte[] Bytes)> _lru = new();
    private readonly Dictionary<string, LinkedListNode<(string Key, byte[] Bytes)>> _map = new();
    private readonly Dictionary<string, Task<byte[]?>> _inflight = new();
    private long _bytes;

    public long Bytes { get { lock (_lock) return _bytes; } }
    public int Count { get { lock (_lock) return _map.Count; } }

    private static string KeyOf(string id, int width) => id + "@" + width;

    public byte[]? TryGet(string id, int width)
    {
        lock (_lock)
        {
            if (!_map.TryGetValue(KeyOf(id, width), out var node)) return null;
            _lru.Remove(node);
            _lru.AddFirst(node);
            return node.Value.Bytes;
        }
    }

    public void Put(string id, int width, byte[] bytes)
    {
        if (bytes.Length > maxBytes) return;
        var key = KeyOf(id, width);
        lock (_lock)
        {
            if (_map.Remove(key, out var old))
            {
                _lru.Remove(old);
                _bytes -= old.Value.Bytes.Length;
            }
            _map[key] = _lru.AddFirst((key, bytes));
            _bytes += bytes.Length;
            while (_bytes > maxBytes && _lru.Last is { } last)
            {
                _lru.RemoveLast();
                _map.Remove(last.Value.Key);
                _bytes -= last.Value.Bytes.Length;
            }
        }
    }

    /// <summary>
    /// Cached bytes, or loads them once. The load runs with the first caller's token, so an item scrolled out of view
    /// before its request is sent costs nothing. A caller that joined a load cancelled by someone else retries.
    /// </summary>
    public async Task<byte[]?> GetOrLoadAsync(string id, int width, Func<CancellationToken, Task<byte[]?>> load, CancellationToken ct)
    {
        var key = KeyOf(id, width);
        for (int attempt = 0; ; attempt++)
        {
            if (TryGet(id, width) is { } hit) return hit;
            Task<byte[]?> task;
            lock (_lock)
            {
                if (!_inflight.TryGetValue(key, out task!))
                {
                    task = LoadAndStore(id, width, key, load, ct);
                    _inflight[key] = task;
                }
            }
            try
            {
                return await task.WaitAsync(ct).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (!ct.IsCancellationRequested && attempt < 3)
            {
                // Another caller's cancellation ended the shared load; start our own.
            }
        }
    }

    private async Task<byte[]?> LoadAndStore(string id, int width, string key, Func<CancellationToken, Task<byte[]?>> load, CancellationToken ct)
    {
        try
        {
            await Task.Yield();
            var bytes = await load(ct).ConfigureAwait(false);
            if (bytes is not null) Put(id, width, bytes);
            return bytes;
        }
        finally
        {
            lock (_lock) _inflight.Remove(key);
        }
    }

    public void Clear()
    {
        lock (_lock)
        {
            _lru.Clear();
            _map.Clear();
            _bytes = 0;
        }
    }
}
