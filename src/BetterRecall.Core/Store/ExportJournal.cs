// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Core.Store;

/// <summary>
/// Running plaintext exports (capture export zips, decrypted replay exports) write to a ".partial" file in a folder the
/// user picked, and their cleanup runs only when the export fails in this process. A killed agent (crash, Task Manager,
/// launcher upgrade, logoff) leaves the partial behind, readable (C3). Each partial path is listed in
/// &lt;data&gt;\exports.pending while it exists; Core deletes the listed partials at the next start.
/// </summary>
public static class ExportJournal
{
    private const string Suffix = ".partial";
    private static readonly Lock Gate = new();

    public static string PathFor(string home) => Path.Combine(home, "exports.pending");

    /// <summary>Records a partial file before it is created. No-op without a data folder.</summary>
    public static void Add(string? home, string partial)
    {
        if (home is null || !partial.EndsWith(Suffix, StringComparison.OrdinalIgnoreCase)) return;
        var full = Path.GetFullPath(partial);
        lock (Gate)
        {
            try
            {
                var list = Read(home);
                if (!list.Contains(full, StringComparer.OrdinalIgnoreCase)) list.Add(full);
                Write(home, list);
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { Log.Warn("export journal", ex); }
        }
    }

    /// <summary>Forgets a partial file once it is gone (renamed to the target or deleted). One that still exists stays
    /// listed, so the next start deletes it.</summary>
    public static void Remove(string? home, string partial)
    {
        if (home is null) return;
        var full = Path.GetFullPath(partial);
        if (File.Exists(full)) return;
        lock (Gate)
        {
            try
            {
                var list = Read(home);
                if (list.RemoveAll(p => string.Equals(p, full, StringComparison.OrdinalIgnoreCase)) > 0) Write(home, list);
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { Log.Warn("export journal", ex); }
        }
    }

    /// <summary>Start only (no export runs yet): deletes every listed partial that is still there. Only files ending in
    /// ".partial" are ever deleted, never a finished export. Returns how many were deleted.</summary>
    public static int Sweep(string home)
    {
        int n = 0;
        lock (Gate)
        {
            try
            {
                var list = Read(home);
                if (list.Count == 0) return 0;
                var left = new List<string>();
                foreach (var p in list)
                {
                    if (!p.EndsWith(Suffix, StringComparison.OrdinalIgnoreCase)) continue;
                    try
                    {
                        if (File.Exists(p)) { File.Delete(p); n++; }
                    }
                    catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
                    {
                        Log.Warn("export partial delete failed", ex);
                        left.Add(p);
                    }
                }
                Write(home, left);
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException) { Log.Warn("export journal", ex); }
        }
        if (n > 0) Log.Info($"exports: {n} unfinished export file(s) deleted");
        return n;
    }

    private static List<string> Read(string home)
    {
        var f = PathFor(home);
        if (!File.Exists(f)) return [];
        return File.ReadAllLines(f).Where(l => l.Length > 0).ToList();
    }

    private static void Write(string home, List<string> list)
    {
        var f = PathFor(home);
        if (list.Count == 0) { File.Delete(f); return; }
        Directory.CreateDirectory(home);
        var tmp = f + ".tmp";
        File.WriteAllLines(tmp, list);
        File.Move(tmp, f, overwrite: true);
    }
}
