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

    /// <summary>Smallest window the pages are laid out for (the 640 x 480 checks).</summary>
    private const int MinWidthDip = 640, MinHeightDip = 480;

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
        if (AppWindow.Presenter is OverlappedPresenter op)
        {
            double scale = Math.Max(1, Win32.GetDpiForWindow(_hwnd) / 96.0);   // the presenter takes physical pixels
            op.PreferredMinimumWidth = (int)(MinWidthDip * scale);
            op.PreferredMinimumHeight = (int)(MinHeightDip * scale);
        }
        Nav.Loaded += (_, _) =>
        {
            // The built-in settings item shows the WinUI text in the OS language; use the app's string.
            if (Nav.SettingsItem is NavigationViewItem si)
            {
                var text = Loc.Get("Nav_Settings/Content");
                si.Content = text;
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(si, text);
                ToolTipService.SetToolTip(si, text);
            }
        };

        var s = Services.Current;
        s.StatusChanged += UpdateStatus;
        s.LockedChanged += UpdateLock;
        s.ConnectionChanged += UpdateOffline;
        s.ConnectionChanged += UpdateStatus;   // pause and lock need a connection
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
            "chat" => typeof(ChatPage),
            "replays" => typeof(ReplaysPage),
            "settings" => typeof(SettingsShellPage),
            "danger" => typeof(DangerZonePage),
            "audit" => typeof(AuditPage),
            "bench" => typeof(BenchPage),
            "shortcuts" => typeof(ShortcutsPage),
            "consent" => typeof(ConsentPage),
            _ => null,
        };
        if (type is null) { type = typeof(HomePage); page = "home"; }
        bool consent = type == typeof(ConsentPage);
        Nav.IsPaneVisible = !consent;
        // home shows the same state and buttons; first-run consent comes before pause, lock and search
        StatusBar.Visibility = type == typeof(HomePage) || consent ? Visibility.Collapsed : Visibility.Visible;
        ActionBar.IsOpen = false;
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
        // Minimal (narrow window): the menu button floats over the page's top-left corner; move the page past it.
        ContentHost.Margin = args.DisplayMode == NavigationViewDisplayMode.Minimal ? new Thickness(MinimalIndent, 0, 0, 0) : new Thickness(0);
    }

    /// <summary>Menu button width (40) plus its margin, less the 24 px page padding, plus a gap.</summary>
    private const double MinimalIndent = 28;

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
        var now = DateTimeOffset.UtcNow;
        var state = st is null ? "" : string.Join("  ·  ", PauseControl.StateParts(st, now).Select(Loc.Text));
        if (StateLine.Text != state) StateLine.Text = state;   // the live region speaks only when this changes
        var detail = st is null ? "" : string.Join("  ·  ", PauseControl.DetailParts(st, now).Select(Loc.Text));
        StatusLine.Text = detail.Length == 0 ? "" : "  ·  " + detail;
        bool connected = Services.Current.Connection is not (AgentErrorKind.NotRunning or AgentErrorKind.NotTrusted);
        PauseButton.Content = Loc.Get(PauseControl.LabelKey(st));
        PauseButton.IsEnabled = PauseControl.Enabled(st, connected);
        LockButton.Content = Loc.Get(Services.Current.Locked ? "Main_Unlock" : "Main_Lock");
        LockButton.IsEnabled = connected;
        UpdateStorageBar(st);
        UpdateStoreBar(st);
    }

    private string? _storeBarReason;

    /// <summary>Banner while the store cannot open: keylost points at purge all in Danger zone, storeerror says it retries.</summary>
    private void UpdateStoreBar(AgentStatus? st)
    {
        var reason = st?.PauseReason is "keylost" or "storeerror" ? st.PauseReason : null;
        if (reason == _storeBarReason) return;
        _storeBarReason = reason;
        if (reason is null) { StoreBar.IsOpen = false; return; }
        bool lost = reason == "keylost";
        StoreBar.Severity = lost ? InfoBarSeverity.Error : InfoBarSeverity.Warning;
        StoreBar.Title = Loc.Get(lost ? "StoreBar_KeyLostTitle" : "StoreBar_ErrorTitle");
        StoreBar.Message = Loc.Get(lost ? "StoreBar_KeyLostMessage" : "StoreBar_ErrorMessage");
        if (lost)
        {
            var b = new Button { Content = Loc.Get("StoreBar_OpenDanger") };
            b.Click += (_, _) => ShowPage("danger");
            StoreBar.ActionButton = b;
        }
        else StoreBar.ActionButton = null;
        StoreBar.IsOpen = true;
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
        var r = PauseControl.UserPaused(s.Status) ? await s.Api.ResumeAsync() : await s.Api.PauseAsync();
        ShowActionResult(r.Ok, r.Kind, r.Error);
        if (r.Ok) await s.RefreshStatusAsync();
    }

    /// <summary>Pause and lock replies: an error shows in a bar instead of being dropped.</summary>
    private void ShowActionResult(bool ok, AgentErrorKind kind, string? error)
    {
        if (ok) { ActionBar.IsOpen = false; return; }
        ActionBar.Severity = WindowHelpers.Severity(kind);
        ActionBar.Message = Loc.Error(kind, error);
        ActionBar.IsOpen = true;
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
        var r = await s.Api.LockAsync();
        ShowActionResult(r.Ok, r.Kind, r.Error);
        if (r.Ok) await s.RefreshStatusAsync();
    }

    private void Search_Click(object sender, RoutedEventArgs e) => App.Current.ShowLauncher();
}
