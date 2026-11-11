// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json.Nodes;

namespace BetterRecall.UI.Logic;

/// <summary>Global shortcut actions (settings keys "hotkeys.&lt;action&gt;") and the checks the Shortcuts page runs before saving.</summary>
public static class ShortcutRules
{
    /// <summary>Display order. Label resource "S_hotkeys_" + action, description "Shortcut_" + action + "_D".</summary>
    public static readonly string[] Actions = ["search", "timeline", "home", "pause", "replay", "lock", "panic"];

    public static string Key(string action) => "hotkeys." + action;

    /// <summary>Saved combination of an action, "" when not assigned.</summary>
    public static string Get(JsonObject settings, string action) =>
        SettingsSchema.Get(settings, Key(action)) is JsonValue v && v.TryGetValue<string>(out var s) ? s : "";

    /// <summary>The other action that already uses this combination, or null.</summary>
    public static string? UsedBy(JsonObject settings, string action, string combo) =>
        combo.Length == 0 ? null
        : Actions.FirstOrDefault(a => a != action && string.Equals(Get(settings, a), combo, StringComparison.OrdinalIgnoreCase));

    /// <summary>Win32 modifiers (MOD_ALT 1, MOD_CONTROL 2, MOD_SHIFT 4, MOD_WIN 8) and virtual key of a stored combination.</summary>
    public static bool TryParse(string combo, out uint modifiers, out uint vk)
    {
        modifiers = 0;
        vk = 0;
        if (HotkeyFormat.Normalize(combo) is not { } n) return false;
        var parts = n.Split('+');
        foreach (var m in parts[..^1])
            modifiers |= m switch { "Alt" => 1u, "Ctrl" => 2u, "Shift" => 4u, _ => 8u };
        return HotkeyFormat.VirtualKey(parts[^1]) is { } k && (vk = (uint)k) != 0;
    }
}
