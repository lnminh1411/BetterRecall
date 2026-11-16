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
/// consent, keylocked and keylost (data key file present but its TPM/DPAPI key gone) block everything; lock pauses capture; user/ipc pause until a time or until resume;
/// fullscreen only pauses keyframes and inference.
/// </summary>
internal sealed class PauseController(Func<DateTimeOffset> now)
{
    private static readonly string[] Priority = ["consent", "keylocked", "keylost", "storeerror", "lock", "storage", "user", "ipc", "fullscreen", "battery"];
    private readonly object _gate = new();
    private readonly Dictionary<string, DateTimeOffset?> _reasons = new();

    public PauseController() : this(() => DateTimeOffset.UtcNow) { }

    public event Action? Changed;

    /// <summary>A timed reason ran out (user or ipc pause "for N min"). Raised once per expiry, outside the lock, by
    /// whichever call noticed it, so the agent can log the resume (sweep 3 T6).</summary>
    public event Action<string>? Expired;
    private readonly List<string> _expired = [];

    /// <summary>Adds or updates a reason. Changed fires only when something changed: setting a reason that is already
    /// set with the same end time re-applied every setting (and rebuilt the replay) on each storeerror retry (D29).</summary>
    public void Set(string reason, DateTimeOffset? until = null)
    {
        lock (_gate)
        {
            if (_reasons.TryGetValue(reason, out var old) && old == until) return;
            _reasons[reason] = until;
        }
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
        bool has;
        lock (_gate)
        {
            Expire();
            has = _reasons.ContainsKey(reason);
        }
        RaiseExpired();
        return has;
    }

    /// <summary>Paused for keyframes (any reason).</summary>
    public bool KeyframesPaused => Current().Reason is not null;

    /// <summary>Paused for the replay ring and audio: every reason except fullscreen (replay.duringFullscreen decides that),
    /// battery and storage (the ring lives in RAM; only keyframes fill the disk).</summary>
    public bool ContinuousPaused(bool duringFullscreen)
    {
        bool paused = false;
        lock (_gate)
        {
            Expire();
            foreach (var r in _reasons.Keys)
            {
                if (r == "fullscreen" && duringFullscreen) continue;
                if (r is "battery" or "storage") continue;
                paused = true;
                break;
            }
        }
        RaiseExpired();
        return paused;
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
        RaiseExpired();
        if (expired) Changed?.Invoke();
        return result;
    }

    private bool Expire()
    {
        var t = now();
        var gone = _reasons.Where(kv => kv.Value is { } u && t >= u).Select(kv => kv.Key).ToList();
        foreach (var g in gone) _reasons.Remove(g);
        _expired.AddRange(gone);
        return gone.Count > 0;
    }

    private void RaiseExpired()
    {
        string[] list;
        lock (_gate)
        {
            if (_expired.Count == 0) return;
            list = [.. _expired];
            _expired.Clear();
        }
        foreach (var r in list) Expired?.Invoke(r);
    }
}
