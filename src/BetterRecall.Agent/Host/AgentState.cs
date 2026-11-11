// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Host;

/// <summary>Windows Hello read gate: open for security.unlockTimeoutMin after an unlock, closed on lock or session lock.</summary>
internal sealed class ReadGate(Func<DateTimeOffset> now)
{
    private readonly object _gate = new();
    private DateTimeOffset? _until;

    public ReadGate() : this(() => DateTimeOffset.UtcNow) { }

    public bool IsOpen(bool requireUnlock)
    {
        if (!requireUnlock) return true;
        lock (_gate) return _until is { } u && now() < u;
    }

    public DateTimeOffset? Until
    {
        get { lock (_gate) return _until is { } u && now() < u ? u : null; }
    }

    public DateTimeOffset Open(int timeoutMin)
    {
        lock (_gate)
        {
            _until = now() + TimeSpan.FromMinutes(Math.Max(1, timeoutMin));
            return _until.Value;
        }
    }

    public void Close()
    {
        lock (_gate) _until = null;
    }
}

/// <summary>
/// Capture pause reasons. Several can hold at once; the status reports the highest priority one.
/// consent and keylocked block everything; lock pauses capture; user/ipc pause until a time or until resume;
/// fullscreen only pauses keyframes and inference.
/// </summary>
internal sealed class PauseController(Func<DateTimeOffset> now)
{
    private static readonly string[] Priority = ["consent", "keylocked", "lock", "storage", "user", "ipc", "fullscreen", "battery"];
    private readonly object _gate = new();
    private readonly Dictionary<string, DateTimeOffset?> _reasons = new();

    public PauseController() : this(() => DateTimeOffset.UtcNow) { }

    public event Action? Changed;

    public void Set(string reason, DateTimeOffset? until = null)
    {
        lock (_gate) _reasons[reason] = until;
        Changed?.Invoke();
    }

    public void Clear(string reason)
    {
        bool removed;
        lock (_gate) removed = _reasons.Remove(reason);
        if (removed) Changed?.Invoke();
    }

    public bool Has(string reason)
    {
        lock (_gate)
        {
            Expire();
            return _reasons.ContainsKey(reason);
        }
    }

    /// <summary>Paused for keyframes (any reason).</summary>
    public bool KeyframesPaused => Current().Reason is not null;

    /// <summary>Paused for the replay ring and audio: every reason except fullscreen (replay.duringFullscreen decides that),
    /// battery and storage (the ring lives in RAM; only keyframes fill the disk).</summary>
    public bool ContinuousPaused(bool duringFullscreen)
    {
        lock (_gate)
        {
            Expire();
            foreach (var r in _reasons.Keys)
            {
                if (r == "fullscreen" && duringFullscreen) continue;
                if (r is "battery" or "storage") continue;
                return true;
            }
            return false;
        }
    }

    public (string? Reason, DateTimeOffset? Until) Current()
    {
        bool expired;
        (string?, DateTimeOffset?) result = (null, null);
        lock (_gate)
        {
            expired = Expire();
            foreach (var p in Priority)
                if (_reasons.TryGetValue(p, out var until)) { result = (p, until); break; }
            if (result.Item1 is null && _reasons.Count > 0)
            {
                var first = _reasons.First();
                result = (first.Key, first.Value);
            }
        }
        if (expired) Changed?.Invoke();
        return result;
    }

    private bool Expire()
    {
        var t = now();
        var gone = _reasons.Where(kv => kv.Value is { } u && t >= u).Select(kv => kv.Key).ToList();
        foreach (var g in gone) _reasons.Remove(g);
        return gone.Count > 0;
    }
}
