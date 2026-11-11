// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Security.AccessControl;
using System.Security.Principal;

namespace BetterRecall.UI.Logic;

/// <summary>
/// Named auto-reset event (PipeNames.ShowLauncherEvent) the agent sets to show the launcher of the running UI,
/// instead of starting a second exe. The DACL grants only the current user, so other accounts cannot signal it.
/// A background thread waits on it and calls onSignal (off the UI thread).
/// </summary>
public sealed class LauncherSignal : IDisposable
{
    private readonly EventWaitHandle _event;
    private readonly ManualResetEvent _stop = new(false);
    private readonly Thread _thread;

    /// <summary>False when the event already existed (another process created it first).</summary>
    public bool CreatedNew { get; }

    public LauncherSignal(string name, Action onSignal)
    {
        var sec = new EventWaitHandleSecurity();
        sec.AddAccessRule(new EventWaitHandleAccessRule(WindowsIdentity.GetCurrent().User!,
            EventWaitHandleRights.Synchronize | EventWaitHandleRights.Modify, AccessControlType.Allow));
        _event = EventWaitHandleAcl.Create(false, EventResetMode.AutoReset, name, out var created, sec);
        CreatedNew = created;
        _thread = new Thread(() =>
        {
            var handles = new WaitHandle[] { _event, _stop };
            while (WaitHandle.WaitAny(handles) == 0) onSignal();
        })
        { IsBackground = true, Name = "LauncherSignal" };
        _thread.Start();
    }

    public void Dispose()
    {
        _stop.Set();
        _thread.Join(1000);
        _event.Dispose();
        _stop.Dispose();
    }
}
