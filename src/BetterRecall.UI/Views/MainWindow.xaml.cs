// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Native;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media.Animation;

namespace BetterRecall.UI.Views;

/// <summary>Pages that read data and must reload after an unlock.</summary>
internal interface IDataPage
{
    void Reload();
}

/// <summary>Home, timeline, replays, settings, benchmark, danger zone, audit log, first-run consent. Hidden, not closed, so it reopens fast.</summary>
public sealed partial class MainWindow : Window
{
    private readonly nint _hwnd;
    private string _page = "";

    public MainWindow()
    {
        InitializeComponent();
        _hwnd = WindowHelpers.Hwnd(this);
        WindowHelpers.Prepare(this);
        Title = Loc.Get("App_Name");
        TitleText.Text = Title;
        ExtendsContentIntoTitleBar = true;
        SetTitleBar(TitleBarArea);
        AppWindow.TitleBar.PreferredHeightOption = TitleBarHeightOption.Tall;
        WindowHelpers.ApplyTheme(this);   // caption button colours need ExtendsContentIntoTitleBar set first
        AppWindow.Closing += (_, e) => { e.Cancel = true; HideMain(); };
        WindowHelpers.CenterOnCursorMonitor(AppWindow, 1200, 800, _hwnd);

        var s = Services.Current;
        s.StatusChanged += UpdateStatus;
        s.LockedChanged += UpdateLock;
        s.ConnectionChanged += UpdateOffline;
        s.Unlocked += () => (ContentFrame.Content as IDataPage)?.Reload();
        Unlock.UnlockedOk += UpdateLock;
        UpdateStatus();
        UpdateOffline();
    }

    public void ShowPage(string page)
    {
        Navigate(page);
        WindowSecurity.Apply(_hwnd);   // hidden windows are skipped when the setting changes
        AppWindow.Show(true);
        Activate();
        Win32.SetForegroundWindow(_hwnd);
    }

    private void HideMain()
    {
        AppWindow.Hide();
        ContentFrame.Content = null;   // drop page images from memory
        ContentFrame.BackStack.Clear();
        _page = "";
    }

    private void Navigate(string page)
    {
        var parts = page.Split(':', 2);
        var key = parts[0];
        string? group = parts.Length > 1 ? parts[1] : null;   // settings without a group keeps the last one
        Type? type = key switch
        {
            "home" => typeof(HomePage),
            "timeline" => typeof(TimelinePage),
            "replays" => typeof(ReplaysPage),
            "settings" => typeof(SettingsShellPage),
            "danger" => typeof(DangerZonePage),
            "audit" => typeof(AuditPage),
            "bench" => typeof(BenchPage),
            "consent" => typeof(ConsentPage),
            _ => null,
        };
        if (type is null) { type = typeof(HomePage); page = "home"; }
        bool consent = type == typeof(ConsentPage);
        Nav.IsPaneVisible = !consent;
        StatusBar.Visibility = type == typeof(HomePage) ? Visibility.Collapsed : Visibility.Visible;   // home shows the same state and buttons
        // Settings is its own page with a group list; give it the room by folding the main pane to icons.
        if (key == "settings") Nav.IsPaneOpen = false;
        else if (Nav.DisplayMode == NavigationViewDisplayMode.Expanded) Nav.IsPaneOpen = true;
        if (key == "settings" && ContentFrame.Content is SettingsShellPage shell)
        {
            shell.Show(group);
            _page = page;
        }
        else if (page != _page || ContentFrame.Content is null)
        {
            ContentFrame.Navigate(type, page.Contains(':') ? page.Split(':', 2)[1] : null, new SuppressNavigationTransitionInfo());
            _page = page;
        }
        SelectNavItem(page);
        UpdateLock();
    }

    private void SelectNavItem(string page)
    {
        if (page.StartsWith("settings", StringComparison.Ordinal)) { Nav.SelectedItem = Nav.SettingsItem; return; }
        var key = page.Split(':')[0];
        Nav.SelectedItem = Nav.MenuItems.OfType<NavigationViewItem>().FirstOrDefault(i => Equals(i.Tag, key));
    }

    /// <summary>The window opens before the pane settles into Expanded; keep it folded while settings show.</summary>
    private void Nav_DisplayModeChanged(NavigationView sender, NavigationViewDisplayModeChangedEventArgs args)
    {
        if (args.DisplayMode == NavigationViewDisplayMode.Expanded) sender.IsPaneOpen = !_page.StartsWith("settings", StringComparison.Ordinal);
    }

    private void Nav_ItemInvoked(NavigationView sender, NavigationViewItemInvokedEventArgs args)
    {
        if (args.IsSettingsInvoked) Navigate("settings");
        else if (args.InvokedItemContainer?.Tag is string tag) Navigate(tag);
    }

    /// <summary>The lock screen covers pages that show captured data. Settings stay reachable.</summary>
    private void UpdateLock()
    {
        bool dataPage = ContentFrame.Content is IDataPage;
        bool show = Services.Current.Locked && dataPage;
        bool was = Unlock.Visibility == Visibility.Visible;
        Unlock.Visibility = show ? Visibility.Visible : Visibility.Collapsed;
        if (show && !was) Unlock.Refresh();
        LockButton.Content = Loc.Get(Services.Current.Locked ? "Main_Unlock" : "Main_Lock");
    }

    private void UpdateOffline()
    {
        var kind = Services.Current.Connection;
        OfflineBar.IsOpen = kind is AgentErrorKind.NotRunning or AgentErrorKind.NotTrusted;
        if (OfflineBar.IsOpen)
        {
            OfflineBar.Title = Loc.Error(kind, null);
            OfflineBar.Message = Loc.Get(kind == AgentErrorKind.NotTrusted ? "Err_NotTrusted_Detail" : "Err_NotRunning_Detail");
        }
    }

    private void UpdateStatus()
    {
        var st = Services.Current.Status;
        StatusLine.Text = st is null ? "" : string.Join("  ·  ", StatusText.Describe(st, DateTimeOffset.UtcNow).Select(Loc.Text));
        bool paused = st?.Paused == true && st.PauseReason is "user" or "ipc";
        PauseButton.Content = Loc.Get(paused ? "Main_Resume" : "Main_Pause");
        PauseButton.IsEnabled = st is not null && (st.PauseReason is null or "user" or "ipc");
        LockButton.Content = Loc.Get(Services.Current.Locked ? "Main_Unlock" : "Main_Lock");
        UpdateStorageBar(st);
    }

    private bool _storageBarClosed;

    /// <summary>Banner while capture is paused for storage (storage.minFreeGb / maxGb); closing hides it until the next pause.</summary>
    private void UpdateStorageBar(AgentStatus? st)
    {
        bool storage = st?.Paused == true && st.PauseReason == "storage";
        if (!storage) _storageBarClosed = false;
        bool open = storage && !_storageBarClosed;
        if (open && StorageBar.ActionButton is null)
        {
            StorageBar.Title = Loc.Get("StorageBar_Title");
            StorageBar.Message = Loc.Get("StorageBar_Message");
            var b = new Button { Content = Loc.Get("StorageBar_Open") };
            b.Click += (_, _) => ShowPage("settings:storage");
            StorageBar.ActionButton = b;
        }
        if (StorageBar.IsOpen != open) StorageBar.IsOpen = open;
    }

    private void StorageBar_Closed(InfoBar sender, InfoBarClosedEventArgs args)
    {
        if (args.Reason == InfoBarCloseReason.CloseButton) _storageBarClosed = true;
    }

    private async void Pause_Click(object sender, RoutedEventArgs e)
    {
        var s = Services.Current;
        bool paused = s.Status?.Paused == true;
        var r = paused ? await s.Api.ResumeAsync() : await s.Api.PauseAsync();
        if (r.Ok) await s.RefreshStatusAsync();
    }

    /// <summary>Shows the unlock panel over the current page (lock buttons here and on the home page).</summary>
    public void ShowUnlock()
    {
        Unlock.Visibility = Visibility.Visible;
        Unlock.Refresh();
    }

    private async void Lock_Click(object sender, RoutedEventArgs e)
    {
        var s = Services.Current;
        if (s.Locked) { ShowUnlock(); return; }
        if ((await s.Api.LockAsync()).Ok) await s.RefreshStatusAsync();
    }

    private void Search_Click(object sender, RoutedEventArgs e) => App.Current.ShowLauncher();
}
