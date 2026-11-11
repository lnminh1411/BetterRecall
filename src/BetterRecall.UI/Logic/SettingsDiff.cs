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
    /// Request lines for a diff. One line when it fits the 4096-byte request limit, else one per section.
    /// Returns null when a single section is still too large (a deny list that long has to be shortened).
    /// </summary>
    public static List<string>? ToRequestLines(JsonObject diff)
    {
        if (diff.Count == 0) return [];
        var whole = "settings set " + diff.ToJsonString();
        if (Encoding.UTF8.GetByteCount(whole) <= LineProtocol.MaxRequestBytes) return [whole];

        var lines = new List<string>();
        foreach (var (section, value) in diff)
        {
            var part = new JsonObject { [section] = value?.DeepClone() };
            var line = "settings set " + part.ToJsonString();
            if (Encoding.UTF8.GetByteCount(line) > LineProtocol.MaxRequestBytes) return null;
            lines.Add(line);
        }
        return lines;
    }
}
