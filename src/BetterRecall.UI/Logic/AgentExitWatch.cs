// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;

namespace BetterRecall.UI.Logic;

/// <summary>
/// Raises Exited once when the watched agent process ends. The agent's hardened DACL still grants SYNCHRONIZE and
/// QUERY_LIMITED_INFORMATION to the user, which is all a wait needs. Watching a new pid (agent restarted) replaces the
/// old watch; a pid that is already gone raises Exited at once.
/// </summary>
public sealed class AgentExitWatch : IDisposable
{
    private readonly Lock _gate = new();
    private Process? _proc;
    private int _pid;
    private bool _disposed;
    private bool _watchFailed;

    public event Action? Exited;

    public int Pid { get { lock (_gate) return _pid; } }
    public bool HasExited { get { lock (_gate) return _pid > 0 && _raised == _pid; } }

    public void Watch(int pid)
    {
        if (pid <= 0) return;
        Process? old;
        lock (_gate)
        {
            if (_disposed || pid == _pid) return;
            old = _proc;
            _proc = null;
            _pid = pid;
            _raised = 0;
            _watchFailed = false;
        }
        old?.Dispose();
        Process p;
        try { p = Process.GetProcessById(pid); }
        catch (ArgumentException) { Raise(pid); return; }   // already gone
        catch (InvalidOperationException) { lock (_gate) if (_pid == pid) _watchFailed = true; return; }
        try
        {
            p.EnableRaisingEvents = true;
            p.Exited += (_, _) => Raise(pid);
        }
        catch (Exception ex) when (ex is InvalidOperationException or System.ComponentModel.Win32Exception or NotSupportedException)
        {
            p.Dispose();   // cannot wait on it: no watch, the offline banner still shows
            lock (_gate) if (_pid == pid) _watchFailed = true;
            return;
        }
        lock (_gate)
        {
            if (_disposed || _pid != pid) { p.Dispose(); return; }
            _proc = p;
        }
        bool exited;
        try { exited = p.HasExited; } catch (InvalidOperationException) { exited = true; }
        if (exited) Raise(pid);
    }

    /// <summary>Re-checks the watched agent process synchronously (used when dismissing a window after Exited fired earlier, C6).</summary>
    public bool IsAlive()
    {
        Process? p;
        int pid;
        lock (_gate)
        {
            if (_disposed) return false;
            if (_pid <= 0) return true;
            if (_raised == _pid) return false;
            if (_watchFailed) return true;
            p = _proc;
            pid = _pid;
        }
        if (p is null) return false;
        try
        {
            if (!p.HasExited) return true;
        }
        catch (InvalidOperationException) { }
        Raise(pid);
        return false;
    }

    private int _raised;

    private void Raise(int pid)
    {
        lock (_gate)
        {
            if (_disposed || _pid != pid || _raised == pid) return;
            _raised = pid;
        }
        Exited?.Invoke();
    }

    public void Dispose()
    {
        Process? p;
        lock (_gate)
        {
            _disposed = true;
            p = _proc;
            _proc = null;
        }
        p?.Dispose();
    }
}

/// <summary>UI exit decision when the agent has exited or a window hides (C6).</summary>
public static class UiExitPolicy
{
    public static bool ShouldExit(bool agentExited, bool launcherVisible, bool mainVisible) =>
        agentExited && !launcherVisible && !mainVisible;
}
