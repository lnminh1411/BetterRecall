// SPDX-License-Identifier: MIT
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

    public event Action? Exited;

    public int Pid { get { lock (_gate) return _pid; } }

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
        }
        old?.Dispose();
        Process p;
        try { p = Process.GetProcessById(pid); }
        catch (ArgumentException) { Raise(pid); return; }   // already gone
        catch (InvalidOperationException) { return; }
        try
        {
            p.EnableRaisingEvents = true;
            p.Exited += (_, _) => Raise(pid);
        }
        catch (Exception ex) when (ex is InvalidOperationException or System.ComponentModel.Win32Exception or NotSupportedException)
        {
            p.Dispose();   // cannot wait on it: no watch, the offline banner still shows
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
