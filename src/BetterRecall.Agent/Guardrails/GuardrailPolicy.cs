// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Guardrails;

internal sealed record WindowInfo(nint Hwnd, uint Pid, string ExePath, string Title, string ClassName)
{
    public string ProcessName => ExePath.Length == 0 ? "" : Path.GetFileName(ExePath);
}

internal enum BadgeResult { NotChecked, NotBrowser, NoBadge, Private, CheckFailed }

internal enum GuardReason
{
    None,
    SessionLocked,
    KeyLocked,
    NoForeground,
    DenyProcess,
    DenyPath,
    DenyTitle,
    DisplayAffinity,
    PrivateBadge,
    BadgeCheckFailed,
}

internal readonly record struct GuardDecision(bool Allow, GuardReason Reason)
{
    public static readonly GuardDecision Allowed = new(true, GuardReason.None);
    public static GuardDecision Deny(GuardReason r) => new(false, r);
}

/// <summary>Pure guardrail decisions (v1-design section 6). No Win32 calls here, so it is unit-testable.</summary>
internal static class GuardrailPolicy
{
    private static readonly string[] Browsers =
    [
        "chrome.exe", "msedge.exe", "brave.exe", "vivaldi.exe", "opera.exe", "opera_gx.exe", "chromium.exe", "arc.exe", "thorium.exe",
    ];

    /// <summary>
    /// Host shown in a browser address bar ("github.com/x", "https://www.example.org/a?b"), lower case, without "www.".
    /// Empty for typed search text, browser pages (chrome://, edge://), files and anything that is not http(s).
    /// Only the host is kept: no path, query or fragment.
    /// </summary>
    public static string SiteFromAddress(string? text)
    {
        if (string.IsNullOrWhiteSpace(text)) return "";
        var t = text.Trim();
        if (t.Length > 2048 || t.Any(char.IsWhiteSpace)) return "";
        int scheme = t.IndexOf("://", StringComparison.Ordinal);
        if (scheme < 0)
        {
            if (t.Contains(':') && !System.Text.RegularExpressions.Regex.IsMatch(t, @"^[^/:]+:\d+(/|$)")) return "";   // about:blank, mailto:x
            t = "https://" + t;
        }
        if (!Uri.TryCreate(t, UriKind.Absolute, out var u) || (u.Scheme != Uri.UriSchemeHttp && u.Scheme != Uri.UriSchemeHttps)) return "";
        var host = u.IdnHost.ToLowerInvariant();
        if (host.StartsWith("www.", StringComparison.Ordinal)) host = host[4..];
        return host.Contains('.') || host == "localhost" ? host : "";
    }

    public static bool IsChromiumBrowser(string processName) =>
        Browsers.Any(b => string.Equals(b, processName, StringComparison.OrdinalIgnoreCase));

    /// <summary>Process, path and title deny lists for one window. Used for the foreground window and for replay masks.</summary>
    public static GuardReason MatchDenyLists(WindowInfo w, PrivacySettings p)
    {
        if (IsDeniedProcess(w.ExePath, p.DenyProcesses)) return GuardReason.DenyProcess;
        if (IsDeniedPath(w.ExePath, p.DenyPathPrefixes)) return GuardReason.DenyPath;
        if (TitleMatches(w.Title, p.DenyTitlePatterns)) return GuardReason.DenyTitle;
        return GuardReason.None;
    }

    public static GuardDecision Evaluate(
        WindowInfo? foreground,
        PrivacySettings p,
        bool sessionLocked,
        bool anyVisibleAffinity,
        BadgeResult badge)
    {
        if (sessionLocked && p.PauseOnLock) return GuardDecision.Deny(GuardReason.SessionLocked);
        if (foreground is null) return GuardDecision.Deny(GuardReason.NoForeground);
        var deny = MatchDenyLists(foreground, p);
        if (deny != GuardReason.None) return GuardDecision.Deny(deny);
        if (p.HonorDisplayAffinity && anyVisibleAffinity) return GuardDecision.Deny(GuardReason.DisplayAffinity);
        if (p.BrowserBadgeDetection && IsChromiumBrowser(foreground.ProcessName))
        {
            if (badge == BadgeResult.Private) return GuardDecision.Deny(GuardReason.PrivateBadge);
            if (badge is BadgeResult.CheckFailed or BadgeResult.NotChecked) return GuardDecision.Deny(GuardReason.BadgeCheckFailed);
        }
        return GuardDecision.Allowed;
    }

    public static bool IsDeniedProcess(string exePath, string[] denyProcesses)
    {
        if (exePath.Length == 0) return false;
        var name = Path.GetFileName(exePath);
        foreach (var d in denyProcesses)
        {
            if (string.IsNullOrWhiteSpace(d)) continue;
            var entry = d.Trim();
            if (entry.Contains('\\') || entry.Contains('/'))
            {
                if (string.Equals(Normalize(entry), Normalize(exePath), StringComparison.OrdinalIgnoreCase)) return true;
            }
            else if (string.Equals(entry, name, StringComparison.OrdinalIgnoreCase)
                     || string.Equals(entry + ".exe", name, StringComparison.OrdinalIgnoreCase))
            {
                return true;
            }
        }
        return false;
    }

    public static bool IsDeniedPath(string exePath, string[] prefixes)
    {
        if (exePath.Length == 0) return false;
        var full = Normalize(exePath);
        foreach (var raw in prefixes)
        {
            if (string.IsNullOrWhiteSpace(raw)) continue;
            var prefix = Normalize(Environment.ExpandEnvironmentVariables(raw.Trim()));
            if (prefix.Length == 0) continue;
            if (full.StartsWith(prefix, StringComparison.OrdinalIgnoreCase))
            {
                // Match on a path boundary: C:\Secret must not match C:\SecretStuff unless the prefix ends with a separator.
                if (full.Length == prefix.Length || prefix.EndsWith('\\') || full[prefix.Length] == '\\') return true;
            }
        }
        return false;
    }

    /// <summary>Case-insensitive substring match; '*' in a pattern matches any run of characters.</summary>
    public static bool TitleMatches(string title, string[] patterns)
    {
        if (title.Length == 0) return false;
        foreach (var raw in patterns)
        {
            if (string.IsNullOrWhiteSpace(raw)) continue;
            var pat = raw.Trim();
            if (!pat.Contains('*'))
            {
                if (title.Contains(pat, StringComparison.OrdinalIgnoreCase)) return true;
                continue;
            }
            if (GlobContains(title, pat)) return true;
        }
        return false;
    }

    /// <summary>Title text that may be stored: empty when any deny pattern matches.</summary>
    public static string FilterTitle(string title, PrivacySettings p) => TitleMatches(title, p.DenyTitlePatterns) ? "" : title;

    private static bool GlobContains(string text, string pattern)
    {
        var pieces = pattern.Split('*', StringSplitOptions.RemoveEmptyEntries);
        int pos = 0;
        foreach (var piece in pieces)
        {
            int i = text.IndexOf(piece, pos, StringComparison.OrdinalIgnoreCase);
            if (i < 0) return false;
            pos = i + piece.Length;
        }
        return true;
    }

    private static string Normalize(string path)
    {
        try { return Path.GetFullPath(path.Replace('/', '\\')).TrimEnd('\\'); }
        catch { return path; }
    }
}
