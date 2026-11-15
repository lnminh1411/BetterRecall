// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

namespace BetterRecall.Core.Ipc;

/// <summary>
/// Rate limit for audit rows and log lines that a client can trigger at will (connect, rejected): at most one row per
/// key and window, and at most maxPerWindow rows per window in total. Repeats are merged into the key's next row.
/// </summary>
internal sealed class AuditThrottle(TimeSpan window, int maxPerWindow = 30, int maxKeys = 512)
{
    private readonly Lock _gate = new();
    private readonly Dictionary<string, (DateTimeOffset Start, int Merged)> _keys = new(StringComparer.OrdinalIgnoreCase);
    private DateTimeOffset _windowStart = DateTimeOffset.MinValue;
    private int _rows;

    /// <summary>0: skip this event. Otherwise write one row standing for that many events (this one plus repeats merged
    /// since the key's last row).</summary>
    public int Admit(string key, DateTimeOffset now)
    {
        lock (_gate)
        {
            if (now - _windowStart >= window) { _windowStart = now; _rows = 0; }
            if (_keys.TryGetValue(key, out var e) && now - e.Start < window)
            {
                _keys[key] = (e.Start, e.Merged + 1);
                return 0;
            }
            int merged = e.Merged;
            if (_rows >= maxPerWindow)
            {
                // Over the total budget: remember the event for this key's next row.
                if (_keys.Count < maxKeys || _keys.ContainsKey(key)) _keys[key] = (now, merged + 1);
                return 0;
            }
            if (_keys.Count >= maxKeys && !_keys.ContainsKey(key))
            {
                foreach (var old in _keys.Where(kv => now - kv.Value.Start >= window && kv.Value.Merged == 0).Select(kv => kv.Key).ToList())
                    _keys.Remove(old);
                if (_keys.Count >= maxKeys) return 0;
            }
            _keys[key] = (now, 0);
            _rows++;
            return merged + 1;
        }
    }
}
