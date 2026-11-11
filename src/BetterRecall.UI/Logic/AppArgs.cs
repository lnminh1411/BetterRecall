// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
namespace BetterRecall.UI.Logic;

/// <summary>
/// Command line. The agent starts the UI with no arguments (search hotkey), --replays (notification click)
/// or --settings (tray menu).
/// --page opens a main-window page: home, timeline, replays, danger, audit, consent, or settings:&lt;group&gt;.
/// --mock runs an in-process fake agent; --mock-locked, --mock-passphrase, --mock-firstrun, --mock-capturable, --mock-storage shape it.
/// --hidden starts warm without showing a window.
/// </summary>
public sealed record AppArgs(
    bool Mock = false,
    bool MockLocked = false,
    bool MockPassphrase = false,
    bool MockFirstRun = false,
    bool MockCapturable = false,
    string? Page = null,
    bool Hidden = false,
    string? MockQuery = null,
    bool MockMenu = false,
    bool MockPassword = false,
    bool MockStorage = false)
{
    public static AppArgs Parse(IEnumerable<string> args)
    {
        var a = new AppArgs();
        using var e = args.GetEnumerator();
        while (e.MoveNext())
        {
            var s = e.Current.Trim().Trim('"');
            switch (s.ToLowerInvariant())
            {
                case "--mock": a = a with { Mock = true }; break;
                case "--mock-locked": a = a with { Mock = true, MockLocked = true }; break;
                case "--mock-passphrase": a = a with { Mock = true, MockPassphrase = true }; break;
                case "--mock-password": a = a with { Mock = true, MockLocked = true, MockPassword = true }; break;   // unlock method password ("wrong" fails)
                case "--mock-firstrun": a = a with { Mock = true, MockFirstRun = true }; break;
                case "--mock-capturable": a = a with { Mock = true, MockCapturable = true }; break;
                case "--mock-storage": a = a with { Mock = true, MockStorage = true }; break;   // capture paused for storage
                case "--replays": a = a with { Page = "replays" }; break;
                case "--settings": a = a with { Page = "settings" }; break;
                case "--hidden": a = a with { Hidden = true }; break;
                case "--mock-menu":    // demo screenshots: open the first result's menu after --mock-query
                    a = a with { Mock = true, MockMenu = true };
                    break;
                case "--mock-query":   // demo screenshots: open search with this query (mock mode only)
                    if (e.MoveNext()) a = a with { Mock = true, MockQuery = e.Current.Trim().Trim('"') };
                    break;
                case "--page":
                    if (e.MoveNext()) a = a with { Page = e.Current.Trim().Trim('"').ToLowerInvariant() };
                    break;
            }
        }
        return a;
    }

    /// <summary>Splits a Windows command line (as carried by redirected activation) into arguments. Quotes group words.</summary>
    public static List<string> SplitCommandLine(string? commandLine)
    {
        var list = new List<string>();
        if (string.IsNullOrWhiteSpace(commandLine)) return list;
        var cur = new System.Text.StringBuilder();
        bool quoted = false, any = false;
        foreach (var c in commandLine)
        {
            if (c == '"') { quoted = !quoted; any = true; continue; }
            if (char.IsWhiteSpace(c) && !quoted)
            {
                if (any) { list.Add(cur.ToString()); cur.Clear(); any = false; }
                continue;
            }
            cur.Append(c);
            any = true;
        }
        if (any) list.Add(cur.ToString());
        return list;
    }
}
