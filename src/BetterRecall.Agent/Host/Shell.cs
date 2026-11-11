// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;
using Microsoft.Win32;

namespace BetterRecall.Agent.Host;

/// <summary>HKCU Run key and the InstallPath value the CLI and MCP clients use for their trust check.</summary>
internal static class StartupRegistration
{
    private const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
    private const string AppKey = @"Software\BetterRecall";
    private const string ValueName = "BetterRecall";

    public static void Register(string installDir)
    {
        // Started by the single-file launcher: autostart runs the launcher, so upgrades keep working.
        var launcher = Environment.GetEnvironmentVariable(InstallLayout.LauncherEnv);
        var exe = !string.IsNullOrWhiteSpace(launcher) && File.Exists(launcher)
            ? Path.GetFullPath(launcher)
            : InstallLayout.Full(installDir, InstallLayout.AgentExe);
        string cmd = $"\"{exe}\"";
        using (var run = Registry.CurrentUser.CreateSubKey(RunKey))
        {
            if (run.GetValue(ValueName) as string != cmd) run.SetValue(ValueName, cmd);
        }
        using var app = Registry.CurrentUser.CreateSubKey(AppKey);
        if (app.GetValue("InstallPath") as string != installDir) app.SetValue("InstallPath", installDir);
        Log.Info("startup registration updated");
    }

    public static void Unregister()
    {
        using var run = Registry.CurrentUser.OpenSubKey(RunKey, writable: true);
        run?.DeleteValue(ValueName, throwOnMissingValue: false);
    }

    public static bool IsRegistered()
    {
        using var run = Registry.CurrentUser.OpenSubKey(RunKey);
        return run?.GetValue(ValueName) is string;
    }
}

/// <summary>
/// Shows the WinUI launcher: sets PipeNames.ShowLauncherEvent when the UI is running (its single instance waits on it),
/// otherwise starts ui\BetterRecall.UI.exe. Either way the UI gets foreground rights.
/// </summary>
internal static partial class UiLauncher
{
    private const uint EVENT_MODIFY_STATE = 0x0002;

    /// <summary>Search hotkey and tray: signal the running UI, else start it.</summary>
    public static bool ShowLauncher(string installDir)
    {
        AllowRunningUi(installDir);
        nint ev = OpenEventW(EVENT_MODIFY_STATE, 0, PipeNames.ShowLauncherEvent);
        if (ev != 0)
        {
            try { return SetEvent(ev) != 0; }
            finally { Win32.CloseHandle(ev); }
        }
        return Start(installDir);
    }

    /// <summary>Starts the UI exe with arguments; a running instance receives them through its single-instance redirect.</summary>
    public static bool Start(string installDir, params string[] args)
    {
        var exe = InstallLayout.Full(installDir, InstallLayout.UiExe);
        try
        {
            AllowRunningUi(installDir);
            if (!File.Exists(exe))
            {
                Log.Warn("UI exe not found");
                return false;
            }
            var psi = new ProcessStartInfo(exe) { UseShellExecute = false, WorkingDirectory = Path.GetDirectoryName(exe)! };
            foreach (var a in args) psi.ArgumentList.Add(a);
            using var proc = Process.Start(psi);
            if (proc is not null) Win32.AllowSetForegroundWindow((uint)proc.Id);
            return proc is not null;
        }
        catch (Exception ex)
        {
            Log.Warn("UI launch", ex);
            return false;
        }
    }

    /// <summary>
    /// Grants foreground rights to the running UI. Only a process whose full image path equals
    /// install\ui\BetterRecall.UI.exe qualifies (never the name alone, never ASFW_ANY).
    /// Called from the hotkey or tray handler while the agent still holds the foreground right.
    /// </summary>
    private static void AllowRunningUi(string installDir)
    {
        var exe = InstallLayout.Full(installDir, InstallLayout.UiExe);
        foreach (var p in Process.GetProcessesByName(Path.GetFileNameWithoutExtension(exe)))
        {
            using (p)
            {
                var path = Win32.GetProcessImagePath((uint)p.Id);
                if (path is not null && string.Equals(Path.GetFullPath(path), exe, StringComparison.OrdinalIgnoreCase))
                    Win32.AllowSetForegroundWindow((uint)p.Id);
            }
        }
    }

    [System.Runtime.InteropServices.LibraryImport("kernel32.dll", StringMarshalling = System.Runtime.InteropServices.StringMarshalling.Utf16)]
    private static partial nint OpenEventW(uint access, int inherit, string name);

    [System.Runtime.InteropServices.LibraryImport("kernel32.dll")]
    private static partial int SetEvent(nint handle);
}
