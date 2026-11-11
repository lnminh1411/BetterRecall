// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Host;

/// <summary>A parsed hotkey: RegisterHotKey modifiers (MOD_*) and a virtual-key code.</summary>
internal readonly record struct Hotkey(uint Modifiers, uint VirtualKey)
{
    public const uint ModAlt = 0x1, ModControl = 0x2, ModShift = 0x4, ModWin = 0x8;
}

/// <summary>Parses strings such as "Ctrl+Alt+Space", "Win+Shift+F12", "Ctrl+Alt+Shift+X".</summary>
internal static class HotkeyParser
{
    private static readonly Dictionary<string, uint> Named = new(StringComparer.OrdinalIgnoreCase)
    {
        ["Space"] = 0x20, ["Spacebar"] = 0x20, ["Enter"] = 0x0D, ["Return"] = 0x0D, ["Tab"] = 0x09,
        ["Esc"] = 0x1B, ["Escape"] = 0x1B, ["Backspace"] = 0x08, ["Back"] = 0x08,
        ["Insert"] = 0x2D, ["Ins"] = 0x2D, ["Delete"] = 0x2E, ["Del"] = 0x2E,
        ["Home"] = 0x24, ["End"] = 0x23, ["PageUp"] = 0x21, ["PgUp"] = 0x21, ["PageDown"] = 0x22, ["PgDn"] = 0x22,
        ["Left"] = 0x25, ["Up"] = 0x26, ["Right"] = 0x27, ["Down"] = 0x28,
        ["PrintScreen"] = 0x2C, ["PrtSc"] = 0x2C, ["Pause"] = 0x13, ["ScrollLock"] = 0x91,
        ["Plus"] = 0xBB, ["+"] = 0xBB, ["="] = 0xBB, ["Minus"] = 0xBD, ["-"] = 0xBD, [","] = 0xBC, ["Comma"] = 0xBC,
        ["."] = 0xBE, ["Period"] = 0xBE, ["/"] = 0xBF, [";"] = 0xBA, ["'"] = 0xDE, ["["] = 0xDB, ["]"] = 0xDD,
        ["\\"] = 0xDC, ["`"] = 0xC0, ["Backtick"] = 0xC0,
    };

    /// <summary>Names the UI recorder emits (v1-design section 5 hotkey grammar).</summary>
    private static readonly Dictionary<uint, string> Canonical = new()
    {
        [0x20] = "Space", [0x21] = "PageUp", [0x22] = "PageDown", [0x23] = "End", [0x24] = "Home",
        [0x25] = "Left", [0x26] = "Up", [0x27] = "Right", [0x28] = "Down", [0x2D] = "Insert", [0x2E] = "Delete",
        [0x13] = "Pause", [0x2C] = "PrintScreen",
        [0xBA] = ";", [0xBB] = "=", [0xBC] = ",", [0xBD] = "-", [0xBE] = ".", [0xBF] = "/", [0xC0] = "`",
        [0xDB] = "[", [0xDC] = "\\", [0xDD] = "]", [0xDE] = "'",
        [0x0D] = "Enter", [0x09] = "Tab", [0x1B] = "Esc", [0x08] = "Backspace", [0x91] = "ScrollLock",
    };

    /// <summary>Returns null when the string is empty, has no key, repeats a part or names an unknown key.</summary>
    public static Hotkey? Parse(string? text)
    {
        if (string.IsNullOrWhiteSpace(text)) return null;
        // "Ctrl++" style: a trailing '+' after a separator is the Plus key.
        var raw = text.Trim();
        var parts = new List<string>();
        int start = 0;
        for (int i = 0; i < raw.Length; i++)
        {
            if (raw[i] == '+' && i > start)
            {
                parts.Add(raw[start..i].Trim());
                start = i + 1;
            }
        }
        if (start < raw.Length) parts.Add(raw[start..].Trim());
        else if (raw.EndsWith('+')) parts.Add("Plus");

        uint mods = 0;
        uint? vk = null;
        foreach (var part in parts)
        {
            if (part.Length == 0) return null;
            uint m = part.ToLowerInvariant() switch
            {
                "ctrl" or "control" => Hotkey.ModControl,
                "alt" => Hotkey.ModAlt,
                "shift" => Hotkey.ModShift,
                "win" or "windows" or "meta" or "super" => Hotkey.ModWin,
                _ => 0,
            };
            if (m != 0)
            {
                if ((mods & m) != 0) return null;
                mods |= m;
                continue;
            }
            if (vk is not null) return null;
            vk = KeyCode(part);
            if (vk is null) return null;
        }
        if (vk is null) return null;
        return new Hotkey(mods, vk.Value);
    }

    private static uint? KeyCode(string key)
    {
        if (key.Length == 1)
        {
            char c = char.ToUpperInvariant(key[0]);
            if (c is >= 'A' and <= 'Z' or >= '0' and <= '9') return c;
        }
        if (Named.TryGetValue(key, out var v)) return v;
        if ((key[0] == 'F' || key[0] == 'f') && int.TryParse(key.AsSpan(1), out int f) && f is >= 1 and <= 24)
            return (uint)(0x70 + f - 1);
        // NumPad0-NumPad9 (UI recorder grammar), Num0-Num9 accepted as a shorthand.
        foreach (var prefix in new[] { "NumPad", "Num" })
            if (key.Length == prefix.Length + 1 && key.StartsWith(prefix, StringComparison.OrdinalIgnoreCase) && char.IsAsciiDigit(key[^1]))
                return (uint)(0x60 + (key[^1] - '0'));
        return null;
    }

    public static string Format(Hotkey hk)
    {
        var parts = new List<string>();
        if ((hk.Modifiers & Hotkey.ModControl) != 0) parts.Add("Ctrl");
        if ((hk.Modifiers & Hotkey.ModAlt) != 0) parts.Add("Alt");
        if ((hk.Modifiers & Hotkey.ModShift) != 0) parts.Add("Shift");
        if ((hk.Modifiers & Hotkey.ModWin) != 0) parts.Add("Win");
        parts.Add(hk.VirtualKey switch
        {
            >= 0x70 and <= 0x87 => "F" + (hk.VirtualKey - 0x70 + 1),
            >= 'A' and <= 'Z' or >= '0' and <= '9' => ((char)hk.VirtualKey).ToString(),
            >= 0x60 and <= 0x69 => "NumPad" + (hk.VirtualKey - 0x60),
            _ => Canonical.TryGetValue(hk.VirtualKey, out var n) ? n : $"0x{hk.VirtualKey:X2}",
        });
        return string.Join('+', parts);
    }
}
