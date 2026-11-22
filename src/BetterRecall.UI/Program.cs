// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.UI.Logic;
using BetterRecall.UI.Native;
using Microsoft.UI.Dispatching;
using Microsoft.Windows.AppLifecycle;

namespace BetterRecall.UI;

public static class Program
{
    /// <summary>
    /// Single instance through AppInstance redirection: a second launch (the agent's hotkey or notification click)
    /// hands its arguments to the running instance and exits before any XAML is loaded.
    /// </summary>
    [STAThread]
    public static int Main(string[] args)
    {
        var uiDir = AppContext.BaseDirectory.TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
        var installRoot = Path.GetDirectoryName(uiDir) ?? uiDir;
        BetterRecall.Contracts.IntegrityCheck.EnforceOrExit(installRoot, gui: true);

        WinRT.ComWrappersSupport.InitializeComWrappers();
        var parsed = AppArgs.Parse(args);
        // One UI per data folder: a UI of another folder (dev build, second home) never takes this one's activations.
        var key = AppInstance.FindOrRegisterForKey(parsed.Mock ? "BetterRecall.UI.mock"
            : BetterRecall.Contracts.PipeNames.UiInstanceKeyFor(BetterRecall.Contracts.DataPaths.Home()));
        if (!key.IsCurrent)
        {
            // We may hold the foreground right the agent granted; pass it on so the launcher can take focus.
            Win32.AllowSetForegroundWindow(key.ProcessId);
            var activation = AppInstance.GetCurrent().GetActivatedEventArgs();
            Task.Run(() => key.RedirectActivationToAsync(activation).AsTask()).Wait();
            return 0;
        }

        key.Activated += (_, e) => App.OnRedirected(e);

        Microsoft.UI.Xaml.Application.Start(p =>
        {
            var ctx = new DispatcherQueueSynchronizationContext(DispatcherQueue.GetForCurrentThread());
            SynchronizationContext.SetSynchronizationContext(ctx);
            _ = new App(parsed);
        });
        return 0;
    }
}
