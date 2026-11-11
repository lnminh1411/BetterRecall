// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Globalization;
using BetterRecall.UI.Logic;
using Microsoft.Windows.ApplicationModel.Resources;

namespace BetterRecall.UI.Views;

/// <summary>Strings from Strings\&lt;lang&gt;\Resources.resw. Missing keys show the key so gaps are visible.</summary>
internal static class Loc
{
    private static ResourceLoader? _loader;

    public static string Get(string key)
    {
        if (string.IsNullOrEmpty(key)) return "";
        try
        {
            _loader ??= new ResourceLoader();
            var s = _loader.GetString(key);
            return string.IsNullOrEmpty(s) ? key : s;
        }
        catch
        {
            return key;
        }
    }

    /// <summary>Never throws: a placeholder without an argument shows "?" (reason codes from the agent may carry fewer args).</summary>
    public static string Format(string key, params object?[] args) =>
        TextFormat.Fill(Get(key), args.Select(a => a is LocalizedText t ? Text(t) : a).ToArray(), CultureInfo.CurrentCulture);

    public static string Text(LocalizedText? t)
    {
        if (t is null || t.IsEmpty) return "";
        return t.Args.Length == 0 ? Get(t.Key) : Format(t.Key, t.Args);
    }

    public static string Error(AgentErrorKind kind, string? error) => Text(SearchViewModel.ErrorText(kind, error));
}
