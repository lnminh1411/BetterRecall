// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;

namespace BetterRecall.Core.Store;

/// <summary>
/// Running plaintext exports (capture export zips, decrypted replay exports) write to a ".partial" file in a folder the
/// user picked, and their cleanup runs only when the export fails in this process. A killed agent (crash, Task Manager,
/// launcher upgrade, logoff) leaves the partial behind, readable (C3). Each partial path is listed in
/// &lt;data&gt;\exports.pending while it exists; Core deletes the listed partials at the next start. Purge all also lists
/// the replay files it could not delete (a player holds one open), so the next start retries them.
/// </summary>
public static class ExportJournal
{
    private const string Suffix = ".partial";
    private static readonly Lock Gate = new();

    public static string PathFor(string home) => Path.Combine(home, "exports.pending");

    /// <summary>Only these are ever deleted: ".partial" files, and files directly in &lt;data&gt;\replays.</summary>
    private static bool Deletable(string home, string path) =>
        path.EndsWith(Suffix, StringComparison.OrdinalIgnoreCase)
        || string.Equals(Path.GetDirectoryName(path), Path.GetFullPath(DataPaths.ReplaysDir(home)), StringComparison.OrdinalIgnoreCase);

    /// <summary>Records a partial file before it is created, or a replay file purge all could not delete. No-op without
    /// a data folder.</summary>
    public static void Add(string? home, string partial)
    {
        if (home is null) return;
        var full = Path.GetFullPath(partial);
        if (!Deletable(home, full)) return;
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

    /// <summary>Forgets a path whether or not the file exists (the partial could not be created, so a file of that name
    /// is not ours).</summary>
    public static void Forget(string? home, string partial)
    {
        if (home is null) return;
        var full = Path.GetFullPath(partial);
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

    /// <summary>Start only (no export runs yet): deletes every listed file that is still there. Only ".partial" files and
    /// files in &lt;data&gt;\replays are ever deleted, never a finished export. Returns how many were deleted.</summary>
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
                    if (!Deletable(home, p)) continue;
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
        if (n > 0) Log.Info($"exports: {n} unfinished export or leftover replay file(s) deleted");
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
