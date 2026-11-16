// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.UI.Logic;
using BetterRecall.UI.Native;
using BetterRecall.UI.Views;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.Windows.AppLifecycle;

namespace BetterRecall.UI;

public partial class App : Application
{
    private static App? _current;
    private readonly AppArgs _args;
    private LauncherWindow? _launcher;
    private MainWindow? _main;
    private LauncherSignal? _signal;

    public App(AppArgs args)
    {
        _args = args;
        _current = this;
        InitializeComponent();
        UnhandledException += (_, e) => e.Handled = true;   // never crash with captured content on screen; nothing is logged
    }

    public static new App Current => _current!;

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        var services = Services.Create(_args, DispatcherQueue.GetForCurrentThread());
        // Defaults are on; the saved settings may relax them once they arrive.
        WindowSecurity.Start();
        ProcessHardening.Apply(true);

        _launcher = new LauncherWindow();
        // Fast path for the search hotkey: the agent sets this event instead of starting a second exe.
        var eventName = BetterRecall.Contracts.PipeNames.ShowLauncherEventFor(BetterRecall.Contracts.DataPaths.Home()) + (_args.Mock ? ".mock" : "");
        try
        {
            _signal = new LauncherSignal(eventName, () =>
                services.Ui.TryEnqueue(DispatcherQueuePriority.High, () => _launcher?.ShowLauncher()));
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or WaitHandleCannotBeOpenedException)
        {
            // Someone else holds the name with a DACL we cannot use; the second-exe path still works.
        }
        Handle(_args, firstLaunch: true);
        services.AgentExited += OnAgentExited;
        services.ConsentNeeded += () => ShowMain("consent");   // a restarted agent reset capture.consented (D2)
        _ = InitAsync(services);
    }

    /// <summary>The agent quit (tray Quit, quit event) or died: a hidden UI has nothing left to serve, so it exits too. A
    /// window the user has open stays, with the offline banner.</summary>
    private void OnAgentExited()
    {
        if (_launcher?.IsShown == true || _main?.AppWindow.IsVisible == true) return;
        _signal?.Dispose();
        _signal = null;
        Exit();
    }

    private async Task InitAsync(Services services)
    {
        await services.InitAsync();
        if (services.Settings.Loaded && !services.Consented) ShowMain("consent");
    }

    /// <summary>Second launch redirected here (agent hotkey, notification click, or a user starting the exe again).</summary>
    public static void OnRedirected(AppActivationArguments e)
    {
        string? cmd = null;
        if (e.Kind == ExtendedActivationKind.Launch && e.Data is Windows.ApplicationModel.Activation.ILaunchActivatedEventArgs l) cmd = l.Arguments;
        var parsed = AppArgs.Parse(AppArgs.SplitCommandLine(cmd));
        Services.Current?.Ui.TryEnqueue(DispatcherQueuePriority.High, () => _current?.Handle(parsed, firstLaunch: false));
    }

    private void Handle(AppArgs a, bool firstLaunch)
    {
        if (a.Page is not null) ShowMain(a.Page);
        else if (!a.Hidden) _launcher!.ShowLauncher(a.Mock ? a.MockQuery : null, a.Mock && a.MockMenu);
    }

    public void ShowLauncher() => _launcher?.ShowLauncher();

    public void ShowUnlock() => _main?.ShowUnlock();

    public void ShowMain(string page)
    {
        _main ??= new MainWindow();
        _main.ShowPage(page);
        _launcher?.HideLauncher();
    }
}
