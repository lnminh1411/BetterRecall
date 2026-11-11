// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
namespace BetterRecall.UI.Logic;

/// <summary>Hotkey strings as stored in settings: modifiers in the order Ctrl, Alt, Shift, Win, then one key, joined by '+'.</summary>
public static class HotkeyFormat
{
    private static readonly string[] ModifierOrder = ["Ctrl", "Alt", "Shift", "Win"];

    private static readonly Dictionary<int, string> Named = new()
    {
        [0x20] = "Space", [0x21] = "PageUp", [0x22] = "PageDown", [0x23] = "End", [0x24] = "Home",
        [0x25] = "Left", [0x26] = "Up", [0x27] = "Right", [0x28] = "Down",
        [0x2C] = "PrintScreen", [0x2D] = "Insert", [0x2E] = "Delete", [0x13] = "Pause",
        [0xBA] = ";", [0xBB] = "=", [0xBC] = ",", [0xBD] = "-", [0xBE] = ".", [0xBF] = "/", [0xC0] = "`",
        [0xDB] = "[", [0xDC] = "\\", [0xDD] = "]", [0xDE] = "'",
    };

    /// <summary>Key name for a virtual-key code, or null for modifiers and keys that cannot be a hotkey.</summary>
    public static string? KeyName(int vk)
    {
        if (vk is >= 0x41 and <= 0x5A) return ((char)vk).ToString();               // A-Z
        if (vk is >= 0x30 and <= 0x39) return ((char)vk).ToString();               // 0-9
        if (vk is >= 0x60 and <= 0x69) return "NumPad" + (vk - 0x60);              // NumPad0-9
        if (vk is >= 0x70 and <= 0x87) return "F" + (vk - 0x6F);                   // F1-F24
        return Named.GetValueOrDefault(vk);
    }

    /// <summary>Virtual-key code of a canonical key name (inverse of <see cref="KeyName"/>), or null.</summary>
    public static int? VirtualKey(string key)
    {
        if (key.Length == 1 && char.IsAsciiLetterOrDigit(key[0])) return char.ToUpperInvariant(key[0]);
        if (key.StartsWith("NumPad", StringComparison.Ordinal) && key.Length == 7 && char.IsAsciiDigit(key[6])) return 0x60 + (key[6] - '0');
        if (key.Length >= 2 && key[0] == 'F' && int.TryParse(key.AsSpan(1), out var f) && f is >= 1 and <= 24) return 0x6F + f;
        foreach (var (vk, name) in Named)
            if (name == key) return vk;
        return null;
    }

    /// <summary>Builds a hotkey from a key press. Null when no key, or no Ctrl/Alt/Win modifier (Shift alone is not enough).</summary>
    public static string? FromKeyPress(int vk, bool ctrl, bool alt, bool shift, bool win)
    {
        var key = KeyName(vk);
        if (key is null || !(ctrl || alt || win)) return null;
        var parts = new List<string>();
        if (ctrl) parts.Add("Ctrl");
        if (alt) parts.Add("Alt");
        if (shift) parts.Add("Shift");
        if (win) parts.Add("Win");
        parts.Add(key);
        return string.Join('+', parts);
    }

    /// <summary>Canonical form of a stored hotkey, or null when it is not valid.</summary>
    public static string? Normalize(string? text)
    {
        if (string.IsNullOrWhiteSpace(text)) return null;
        var raw = text.Trim();
        // "+" itself cannot be a key here; "=" is the stored name for that key.
        var parts = raw.Split('+', StringSplitOptions.TrimEntries);
        if (parts.Length < 2 || parts.Any(p => p.Length == 0)) return null;
        var mods = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        for (int i = 0; i < parts.Length - 1; i++)
        {
            var m = parts[i].ToLowerInvariant() switch
            {
                "ctrl" or "control" => "Ctrl",
                "alt" => "Alt",
                "shift" => "Shift",
                "win" or "windows" => "Win",
                _ => null,
            };
            if (m is null || !mods.Add(m)) return null;
        }
        if (!(mods.Contains("Ctrl") || mods.Contains("Alt") || mods.Contains("Win"))) return null;
        var key = CanonicalKey(parts[^1]);
        if (key is null) return null;
        return string.Join('+', ModifierOrder.Where(mods.Contains).Append(key));
    }

    private static string? CanonicalKey(string k)
    {
        if (k.Length == 1 && char.IsAsciiLetterOrDigit(k[0])) return k.ToUpperInvariant();
        if (k.Length == 1 && Named.ContainsValue(k)) return k;
        foreach (var name in Named.Values)
            if (string.Equals(name, k, StringComparison.OrdinalIgnoreCase)) return name;
        if (k.StartsWith("NumPad", StringComparison.OrdinalIgnoreCase) && k.Length == 7 && char.IsAsciiDigit(k[6])) return "NumPad" + k[6];
        if ((k[0] == 'F' || k[0] == 'f') && int.TryParse(k.AsSpan(1), out var f) && f is >= 1 and <= 24) return "F" + f;
        return null;
    }
}
