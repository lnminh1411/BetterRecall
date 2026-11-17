// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Runtime.InteropServices;
using BetterRecall.Contracts;

namespace BetterRecall.Stub;

/// <summary>
/// What the autostart stub tidies at logon, when no MCP client or launcher holds anything yet. Since C6 autostart no
/// longer runs the launcher, so without this an old version folder that was busy at upgrade time (an MCP server, a
/// scanner) stayed until the next download ran (S3), and a bin\betterrecall-mcp.exe in use at upgrade time kept its old
/// copy (M1). Runs under the launcher mutex, skipped when a launcher holds it.
/// </summary>
public static partial class Maintenance
{
    /// <summary>The single-file launcher's mutex (Launcher/Program.cs LauncherMutex): install, start and clean-up.</summary>
    public const string LauncherMutex = @"Local\BetterRecall.Launcher";

    public static void Run(string root, string current)
    {
        using var mutex = new Mutex(false, LauncherMutex);
        bool owned;
        try { owned = mutex.WaitOne(0); }
        catch (AbandonedMutexException) { owned = true; }
        if (!owned) return;   // a launcher is installing or cleaning up: it does this itself
        try
        {
            var bin = Path.Combine(root, InstallLayout.BinDir);
            SweepAside(bin);
            RefreshMcpStub(root, current);
            SweepOldVersions(root, current);
        }
        finally { mutex.ReleaseMutex(); }
    }

    /// <summary>bin\betterrecall-mcp.exe = the current install's stub (an MCP client may hold an older one).</summary>
    public static bool RefreshMcpStub(string root, string current)
    {
        var stub = Path.Combine(current, InstallLayout.StubExe);
        byte[] bytes;
        try { bytes = File.ReadAllBytes(stub); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return false; }
        return ReplaceFile(Path.Combine(root, InstallLayout.BinDir, InstallLayout.McpExe), bytes);
    }

    /// <summary>Writes content to path unless it is there already. A running exe cannot be overwritten but can be
    /// renamed: it moves aside as &lt;name&gt;.old-&lt;guid&gt; (SweepAside deletes it once it stops) and the new file takes
    /// its place. False when the file could not be replaced.</summary>
    public static bool ReplaceFile(string path, byte[] content)
    {
        try { if (File.Exists(path) && new FileInfo(path).Length == content.Length && File.ReadAllBytes(path).AsSpan().SequenceEqual(content)) return true; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { }
        var tmp = path + ".tmp";
        try { File.WriteAllBytes(tmp, content); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return false; }
        try { File.Move(tmp, path, overwrite: true); return true; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { }
        var aside = path + ".old-" + Guid.NewGuid().ToString("N");
        try { File.Move(path, aside); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { TryDelete(tmp); return false; }
        try { File.Move(tmp, path); return true; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            try { File.Move(aside, path); } catch (Exception e2) when (e2 is IOException or UnauthorizedAccessException) { }
            TryDelete(tmp);
            return false;
        }
    }

    /// <summary>Deletes the bin\*.old-* files ReplaceFile left; one still running stays. Returns how many went.</summary>
    public static int SweepAside(string bin)
    {
        int n = 0;
        if (!Directory.Exists(bin)) return 0;
        foreach (var f in Directory.EnumerateFiles(bin, "*.old-*"))
            if (TryDelete(f)) n++;
        return n;
    }

    /// <summary>Removes old install folders under root: version folders with the launcher's .payload marker, other than
    /// current and not newer than it, and .old-* leftovers. A folder a BetterRecall process runs from is skipped;
    /// the rest go rename first, then delete (a folder something holds open fails the rename as a unit, C3), so a
    /// running install is never touched. Returns the folder names removed.</summary>
    public static List<string> SweepOldVersions(string root, string current)
    {
        var removed = new List<string>();
        var currentName = Path.GetFileName(current.TrimEnd('\\'));
        var ours = FolderVersion(currentName);
        foreach (var dir in Directory.EnumerateDirectories(root))
        {
            var name = Path.GetFileName(dir);
            if (string.Equals(name, currentName, StringComparison.OrdinalIgnoreCase)) continue;
            bool aside = name.StartsWith(".old-", StringComparison.Ordinal);
            if (!aside)
            {
                if (!InstallLayout.IsVersionFolder(name) || !File.Exists(Path.Combine(dir, ".payload"))) continue;
                if (ours is null || FolderVersion(name) is not { } v || v > ours) continue;   // a newer (or unknown) one stays
                // A rename is refused only while something holds the folder open (a working directory); a running exe
                // alone does not stop it, and the delete would then strip a running install. Look at the processes too.
                if (BetterRecallRunsFrom(dir)) continue;
            }
            try
            {
                var doomed = aside ? dir : Path.Combine(root, ".old-" + Guid.NewGuid().ToString("N"));
                if (!aside) Directory.Move(dir, doomed);
                Directory.Delete(doomed, recursive: true);
                removed.Add(name);
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { }
        }
        return removed;
    }

    /// <summary>Numeric version of "&lt;version&gt;-&lt;hash12&gt;": "0.2.3-dev-1df40c2e4158" -> 0.2.3. Null when it does not parse.</summary>
    internal static Version? FolderVersion(string name)
    {
        int dash = name.LastIndexOf('-');
        if (dash <= 0) return null;
        var v = name[..dash];
        int end = v.IndexOfAny(['-', '+']);
        return Version.TryParse(end < 0 ? v : v[..end], out var r) ? r : null;
    }

    /// <summary>True when a BetterRecall* process (agent, worker, UI, CLI, MCP server) runs from under dir. Image paths
    /// through PROCESS_QUERY_LIMITED_INFORMATION (memory isolation denies more).</summary>
    internal static bool BetterRecallRunsFrom(string dir)
    {
        var prefix = Path.GetFullPath(dir).TrimEnd('\\') + "\\";
        foreach (var p in Process.GetProcesses())
        {
            using (p)
            {
                string name;
                try { name = p.ProcessName; } catch (InvalidOperationException) { continue; }
                if (!name.StartsWith("BetterRecall", StringComparison.OrdinalIgnoreCase)) continue;
                if (ImagePath(p.Id) is { } image && image.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)) return true;
            }
        }
        return false;
    }

    private static string? ImagePath(int pid)
    {
        nint h = OpenProcess(0x1000 /* PROCESS_QUERY_LIMITED_INFORMATION */, 0, (uint)pid);
        if (h == 0) return null;
        try
        {
            var buf = new char[1024];
            uint size = (uint)buf.Length;
            return QueryFullProcessImageNameW(h, 0, buf, ref size) != 0 ? new string(buf, 0, (int)size) : null;
        }
        finally { CloseHandle(h); }
    }

    [LibraryImport("kernel32.dll")]
    private static partial nint OpenProcess(uint access, int inherit, uint pid);

    [LibraryImport("kernel32.dll", StringMarshalling = StringMarshalling.Utf16)]
    private static partial int QueryFullProcessImageNameW(nint process, uint flags, [Out] char[] buffer, ref uint size);

    [LibraryImport("kernel32.dll")]
    private static partial int CloseHandle(nint handle);

    private static bool TryDelete(string file)
    {
        try { File.Delete(file); return true; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return false; }
    }
}
