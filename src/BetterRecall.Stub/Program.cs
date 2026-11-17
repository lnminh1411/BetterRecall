// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Runtime.InteropServices;
using BetterRecall.Contracts;

namespace BetterRecall.Stub;

/// <summary>
/// D6: the launcher copies this exe (shipped in every install as BetterRecall.Stub.exe) to &lt;root&gt;\bin, where its path
/// never changes. The file name picks the role:
/// - bin\betterrecall-mcp.exe: the MCP server of MCP client configs. Relays stdio to the current install's server and
///   follows upgrades (McpRelay; B5/C4).
/// - bin\BetterRecall.exe (any other name): what autostart runs (C6). Starts the agent of the install bin\current
///   names with these arguments, or, when this data folder's agent already runs, shows its search window; then tidies
///   bin and old install folders (Maintenance).
/// </summary>
internal static partial class Program
{
    private static int Main(string[] args)
    {
        var self = Environment.ProcessPath;
        bool relay = self is not null && string.Equals(Path.GetFileName(self), InstallLayout.McpExe, StringComparison.OrdinalIgnoreCase);
        try
        {
            if (self is null) throw new InvalidOperationException("own path unknown");
            var root = Path.GetDirectoryName(Path.GetDirectoryName(self)!)!;
            var realRoot = RealPath(root);
            if (InstallLayout.CurrentInstall(root, realRoot) is not { } install || !File.Exists(Path.Combine(install, InstallLayout.AgentExe)))
            {
                Fail("BetterRecall is not installed here: run the BetterRecall download once, then retry.", relay);
                return 1;
            }
            if (relay) return Relay(root, realRoot, args);
            Start(self, install, args);
            Maintenance.Run(root, install);
            return 0;
        }
        catch (Exception ex)
        {
            Fail($"BetterRecall could not start.\n\n{ex.Message}", relay);
            return 1;
        }
    }

    private static int Relay(string root, string realRoot, string[] args)
    {
        using var stdin = Console.OpenStandardInput();
        using var stdout = Console.OpenStandardOutput();
        using var stderr = Console.OpenStandardError();
        var relay = new McpRelay(stdin, stdout, () => InstallLayout.CurrentInstall(root, realRoot), install => new ProcessChild(install, args, stderr));
        return relay.RunAsync().GetAwaiter().GetResult();
    }

    private static void Start(string self, string install, string[] args)
    {
        var home = DataPaths.Home();   // the same data folder the agent, UI, CLI and MCP resolve
        if (Mutex.TryOpenExisting(PipeNames.AgentMutexFor(home), out var running))
        {
            running.Dispose();
            // Running already (this version or another): only bring up search, as the launcher does.
            if (SignalEvent(PipeNames.ShowLauncherEventFor(home))) return;
            Spawn(Path.Combine(install, InstallLayout.UiExe), [], install, null);
            return;
        }
        Spawn(Path.Combine(install, InstallLayout.AgentExe), args, install, self);
    }

    /// <summary>launcher: set as BETTERRECALL_LAUNCHER for the agent (installed layout; autostart keeps this stub).</summary>
    private static void Spawn(string exe, IReadOnlyList<string> args, string workDir, string? launcher)
    {
        var psi = new ProcessStartInfo(exe) { UseShellExecute = false, WorkingDirectory = workDir };
        foreach (var a in args) psi.ArgumentList.Add(a);
        if (launcher is not null) psi.Environment[InstallLayout.LauncherEnv] = launcher;
        using var _ = Process.Start(psi) ?? throw new InvalidOperationException($"could not start {exe}");
    }

    /// <summary>The relay's parent is an MCP client with no console and no one to click a dialog: stderr only (its log).</summary>
    private static void Fail(string message, bool relay)
    {
        if (relay)
        {
            try { Console.Error.WriteLine("betterrecall-mcp: " + message.Replace("\n\n", " ")); } catch (IOException) { }
            return;
        }
        if (AttachConsole(ATTACH_PARENT_PROCESS)) Console.Error.WriteLine(message);
        else MessageBoxW(0, message, "BetterRecall", MB_ICONERROR);
    }

    private static bool SignalEvent(string name)
    {
        nint ev = OpenEventW(EVENT_MODIFY_STATE, 0, name);
        if (ev == 0) return false;
        try { return SetEvent(ev) != 0; }
        finally { CloseHandle(ev); }
    }

    /// <summary>The path with SUBST drives, junctions and symbolic links resolved (GetFinalPathNameByHandle), as the
    /// launcher writes bin\current (T4); the input when it does not resolve.</summary>
    private static string RealPath(string dir)
    {
        nint h = CreateFileW(dir, 0, 7 /* share read, write, delete */, 0, 3 /* OPEN_EXISTING */, 0x02000000 /* FILE_FLAG_BACKUP_SEMANTICS */, 0);
        if (h == -1) return dir;
        try
        {
            var buf = new char[1024];
            uint n = GetFinalPathNameByHandleW(h, buf, (uint)buf.Length, 0);
            if (n == 0 || n >= buf.Length) return dir;
            var s = new string(buf, 0, (int)n);
            if (s.StartsWith(@"\\?\UNC\", StringComparison.OrdinalIgnoreCase)) return @"\\" + s[8..];
            if (s.StartsWith(@"\\?\", StringComparison.Ordinal)) s = s[4..];
            return s.Length > 3 ? s.TrimEnd('\\') : s;
        }
        finally { CloseHandle(h); }
    }

    private const uint ATTACH_PARENT_PROCESS = unchecked((uint)-1);
    private const uint MB_ICONERROR = 0x10;
    private const uint EVENT_MODIFY_STATE = 0x0002;

    [LibraryImport("kernel32.dll")] [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool AttachConsole(uint processId);

    [LibraryImport("user32.dll", StringMarshalling = StringMarshalling.Utf16)]
    private static partial int MessageBoxW(nint hwnd, string text, string caption, uint type);

    [LibraryImport("kernel32.dll", StringMarshalling = StringMarshalling.Utf16)]
    private static partial nint OpenEventW(uint access, int inherit, string name);

    [LibraryImport("kernel32.dll")]
    private static partial int SetEvent(nint handle);

    [LibraryImport("kernel32.dll")]
    private static partial int CloseHandle(nint handle);

    [LibraryImport("kernel32.dll", StringMarshalling = StringMarshalling.Utf16, SetLastError = true)]
    private static partial nint CreateFileW(string name, uint access, uint share, nint security, uint disposition, uint flags, nint template);

    [LibraryImport("kernel32.dll", StringMarshalling = StringMarshalling.Utf16)]
    private static partial uint GetFinalPathNameByHandleW(nint file, [Out] char[] buffer, uint size, uint flags);
}
