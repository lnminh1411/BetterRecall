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
///   names with these arguments, or, when this data folder's agent already runs, shows its search window.
/// </summary>
internal static partial class Program
{
    private static int Main(string[] args)
    {
        try
        {
            var self = Environment.ProcessPath ?? throw new InvalidOperationException("own path unknown");
            var root = Path.GetDirectoryName(Path.GetDirectoryName(self)!)!;
            if (InstallLayout.CurrentInstall(root) is not { } install || !File.Exists(Path.Combine(install, InstallLayout.AgentExe)))
            {
                Fail("BetterRecall is not installed here: run the BetterRecall download once, then retry.");
                return 1;
            }
            if (string.Equals(Path.GetFileName(self), InstallLayout.McpExe, StringComparison.OrdinalIgnoreCase))
                return Relay(root, args);
            return Start(self, install, args);
        }
        catch (Exception ex)
        {
            Fail($"BetterRecall could not start.\n\n{ex.Message}");
            return 1;
        }
    }

    private static int Relay(string root, string[] args)
    {
        using var stdin = Console.OpenStandardInput();
        using var stdout = Console.OpenStandardOutput();
        using var stderr = Console.OpenStandardError();
        var relay = new McpRelay(stdin, stdout, () => InstallLayout.CurrentInstall(root), install => new ProcessChild(install, args, stderr));
        return relay.RunAsync().GetAwaiter().GetResult();
    }

    private static int Start(string self, string install, string[] args)
    {
        var home = DataPaths.Home();   // the same data folder the agent, UI, CLI and MCP resolve
        if (Mutex.TryOpenExisting(PipeNames.AgentMutexFor(home), out var running))
        {
            running.Dispose();
            // Running already (this version or another): only bring up search, as the launcher does.
            if (SignalEvent(PipeNames.ShowLauncherEventFor(home))) return 0;
            Spawn(Path.Combine(install, InstallLayout.UiExe), [], install, null);
            return 0;
        }
        Spawn(Path.Combine(install, InstallLayout.AgentExe), args, install, self);
        return 0;
    }

    /// <summary>launcher: set as BETTERRECALL_LAUNCHER for the agent (installed layout; autostart keeps this stub).</summary>
    private static void Spawn(string exe, IReadOnlyList<string> args, string workDir, string? launcher)
    {
        var psi = new ProcessStartInfo(exe) { UseShellExecute = false, WorkingDirectory = workDir };
        foreach (var a in args) psi.ArgumentList.Add(a);
        if (launcher is not null) psi.Environment[InstallLayout.LauncherEnv] = launcher;
        using var _ = Process.Start(psi) ?? throw new InvalidOperationException($"could not start {exe}");
    }

    private static void Fail(string message)
    {
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
}
