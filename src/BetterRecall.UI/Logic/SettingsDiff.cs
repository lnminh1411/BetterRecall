// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic;

/// <summary>Builds the partial object for "settings set": only keys whose value changed. Arrays are replaced whole.</summary>
public static class SettingsDiff
{
    public static JsonObject Diff(JsonObject original, JsonObject edited)
    {
        var result = new JsonObject();
        foreach (var (name, value) in edited)
        {
            original.TryGetPropertyValue(name, out var before);
            if (value is JsonObject eo && before is JsonObject oo)
            {
                var sub = Diff(oo, eo);
                if (sub.Count > 0) result[name] = sub;
            }
            else if (!JsonNode.DeepEquals(before, value))
            {
                result[name] = value?.DeepClone();
            }
        }
        return result;
    }

    /// <summary>Resource key of the save bar count: "1 unsaved change", "{0} unsaved changes" (D18).</summary>
    public static string PendingKey(int count) => count == 1 ? "Settings_PendingOne" : "Settings_Pending";

    /// <summary>Dotted keys present in a diff, for the "unsaved changes" list and tests.</summary>
    public static IEnumerable<string> Keys(JsonObject diff, string prefix = "")
    {
        foreach (var (name, value) in diff)
        {
            var key = prefix.Length == 0 ? name : prefix + "." + name;
            if (value is JsonObject o) foreach (var k in Keys(o, key)) yield return k;
            else yield return key;
        }
    }

    /// <summary>
    /// The one request line for a diff, so a save is applied whole or not at all (a split save stopped at the first
    /// error after earlier sections were already saved). Null when the line exceeds the UI pipe's request limit
    /// (a list that long has to be shortened); "" when there is nothing to send.
    /// </summary>
    public static string? ToRequestLine(JsonObject diff)
    {
        if (diff.Count == 0) return "";
        var line = "settings set " + diff.ToJsonString();
        return Encoding.UTF8.GetByteCount(line) <= LineProtocol.MaxUiRequestBytes ? line : null;
    }
}
