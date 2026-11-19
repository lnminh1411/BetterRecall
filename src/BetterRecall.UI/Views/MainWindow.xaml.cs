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
    private Type? _pageType;
    private bool _dropped;      // the page was dropped for a lock: it is created again after the unlock
    private bool _skipReload;   // ...and a page created that way loads by itself; the Unlocked event must not load it twice

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
        s.Unlocked += () =>
        {
            if (_skipReload) { _skipReload = false; return; }
            (ContentFrame.Content as IDataPage)?.Reload();
        };
        s.Reconnected += () => { if (!s.Locked) (ContentFrame.Content as IDataPage)?.Reload(); };
        Unlock.UnlockedOk += UpdateLock;
        Unlock.WipeRequested += () => Navigate("danger:wipe");
        s.PurgeAllCompleted += OnPurgeAllCompleted;
        StatusBar.SizeChanged += (_, _) => UpdateStatus();   // fewer status parts on narrow windows (D16)
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
        if (Content?.XamlRoot is { } root) Services.Current.SensitiveDialogs.HideAll(d => d.Hide(), root);   // C2
        AppWindow.Hide();
        DropContent();   // drop page images from memory
        _page = "";
        _pageType = null;
        _dropped = false;
        App.Current.OnWindowHidden();
    }

    private void DropContent()
    {
        if (ContentFrame.Content is DangerZonePage dz) dz.WipeCancelled -= OnWipeCancelled;
        ContentFrame.Content = null;
        ContentFrame.BackStack.Clear();
    }

    /// <summary>Data pages that show captured data: on lock they are dropped (their data leaves memory) and created again
    /// after the unlock. The benchmark and danger zone show none and are only hidden, so a running benchmark goes on.</summary>
    private static bool DropsOnLock(Type? type) =>
        type is not null && typeof(IDataPage).IsAssignableFrom(type)
        && !(type == typeof(BenchPage) || type == typeof(DangerZonePage));

    private void NavigateFrame(Type type, string page)
    {
        if (ContentFrame.Content is DangerZonePage prev) prev.WipeCancelled -= OnWipeCancelled;
        ContentFrame.Navigate(type, page.Contains(':') ? page.Split(':', 2)[1] : null, new SuppressNavigationTransitionInfo());
        if (ContentFrame.Content is DangerZonePage dz) dz.WipeCancelled += OnWipeCancelled;
    }

    private void OnWipeCancelled()
    {
        if (_page == "danger:wipe") Navigate("danger");
    }

    private void OnPurgeAllCompleted(PurgeResult r)
    {
        if (_page == "danger:wipe") { _page = "danger"; UpdateLock(); }
        if (!Services.Current.Locked) return;
        ActionBar.Severity = r.Undeleted is > 0 ? InfoBarSeverity.Warning : InfoBarSeverity.Success;
        ActionBar.Message = string.Join(" ", PurgeFlow.AllDoneText(r).Select(Loc.Text));
        ActionBar.IsOpen = true;
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
        _pageType = type;
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
        else if (Services.Current.Locked && DropsOnLock(type))
        {
            // Locked: the data page is not created (nothing to load, nothing to reach behind the unlock panel); the
            // unlock creates it.
            DropContent();
            _dropped = true;
            _page = page;
        }
        else if (page != _page || ContentFrame.Content is null)
        {
            NavigateFrame(type, page);
            _dropped = false;
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

    /// <summary>
    /// The lock screen replaces pages that show captured data. The page under it is dropped (timeline, chat, replays,
    /// audit: their decrypted data leaves memory) or, for the benchmark and danger zone, collapsed, so neither Tab nor
    /// UI Automation reaches anything behind the panel. The unlock creates the dropped page again. Settings stay reachable.
    /// </summary>
    private void UpdateLock()
    {
        var s = Services.Current;
        bool dataPage = _pageType is not null && typeof(IDataPage).IsAssignableFrom(_pageType);
        bool wipeWhileLocked = s.Locked && _page == "danger:wipe";
        bool show = s.Locked && dataPage && !wipeWhileLocked;
        bool was = Unlock.Visibility == Visibility.Visible;
        if (s.Locked) _skipReload = false;
        if (show && DropsOnLock(_pageType) && ContentFrame.Content is not null)
        {
            DropContent();
            _dropped = true;
        }
        if (s.Locked) s.Thumbs.Clear();
        ContentFrame.Visibility = show ? Visibility.Collapsed : Visibility.Visible;
        Unlock.Visibility = show ? Visibility.Visible : Visibility.Collapsed;
        if (show && !was) Unlock.Refresh();
        if (!s.Locked && _dropped && _pageType is not null && AppWindow.IsVisible)
        {
            _dropped = false;
            _skipReload = true;   // the new page loads on Loaded
            NavigateFrame(_pageType, _page);
        }
        LockButton.Content = Loc.Get(s.Locked ? "Main_Unlock" : "Main_Lock");
        UpdateStatus();   // no capture counts while locked
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
        if (kind == AgentErrorKind.NotRunning) OfflineBar.ActionButton ??= WindowHelpers.StartAgentButton();
        else OfflineBar.ActionButton = null;
    }

    private void UpdateStatus()
    {
        var st = Services.Current.Status;
        var now = DateTimeOffset.UtcNow;
        bool locked = Services.Current.Locked;   // counts stay hidden while locked (sweep 2 D37)
        var state = st is null ? "" : string.Join("  ·  ", PauseControl.StateParts(st, now, locked).Select(Loc.Text));
        if (StateLine.Text != state) StateLine.Text = state;   // the live region speaks only when this changes
        var detail = st is null ? "" : string.Join("  ·  ", StatusText.Fit(PauseControl.DetailParts(st, now, locked), StatusBar.ActualWidth > 0 ? StatusBar.ActualWidth : 1200).Select(Loc.Text));
        StatusLine.Text = detail.Length == 0 ? "" : "·  " + detail;   // the column spacing stands in for the two spaces before the dot (D17)
        bool connected = Services.Current.Connection is not (AgentErrorKind.NotRunning or AgentErrorKind.NotTrusted);
        PauseButton.Content = Loc.Get(PauseControl.LabelKey(st));
        PauseButton.IsEnabled = PauseControl.Enabled(st, connected);
        LockButton.Content = Loc.Get(Services.Current.Locked ? "Main_Unlock" : "Main_Lock");
        LockButton.IsEnabled = connected;
        // Offline: the last status is stale, so its banners go until the agent is back (D10).
        UpdateStorageBar(connected ? st : null);
        UpdateStoreBar(connected ? st : null);
        UpdateWarningBars(connected ? st : null);
        if (!connected) ActionBar.IsOpen = false;
        bool dataPage = _pageType is not null && typeof(IDataPage).IsAssignableFrom(_pageType);
        ContentFrame.Opacity = connected || !dataPage ? 1 : 0.5;   // what a data page shows may be out of date
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

    private string[]? _warnings;

    /// <summary>One banner per status warning (B3): settings.json unreadable (with Reset, D4), names and addresses not
    /// redacted, OCR failing, VC++ runtime missing. The agent used to report them only to the CLI and MCP.</summary>
    private void UpdateWarningBars(AgentStatus? st)
    {
        var codes = st?.Warnings;
        if (WarningBanners.Same(codes, _warnings)) return;
        _warnings = codes;
        WarningBars.Children.Clear();
        foreach (var w in WarningBanners.For(codes))
        {
            var bar = new InfoBar
            {
                Title = Loc.Get(w.TitleKey),
                Message = w.Code == "settings-unreadable" ? Loc.Format(w.MessageKey, DataPaths.SettingsFile(DataPaths.Home()))
                    : w.TitleKey == "Warn_OtherTitle" ? Loc.Format(w.MessageKey, w.Code) : Loc.Get(w.MessageKey),
                Severity = w.Error ? InfoBarSeverity.Error : InfoBarSeverity.Warning,
                IsClosable = !w.Error,
                IsOpen = true,
                Margin = new Thickness(24, 8, 24, 0),
            };
            if (w.CanReset)
            {
                var b = new Button { Content = Loc.Get("Warn_SettingsReset") };
                b.Click += async (_, _) => await ResetSettingsAsync(b);
                bar.ActionButton = b;
            }
            bar.Closed += (sender, _) => { WarningBars.Children.Remove(sender); WarningBars.Visibility = WarningBars.Children.Count > 0 ? Visibility.Visible : Visibility.Collapsed; };
            WarningBars.Children.Add(bar);
        }
        WarningBars.Visibility = WarningBars.Children.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    /// <summary>"Reset settings" on the settings-unreadable banner: confirm, then write the defaults in use (D4).</summary>
    private async Task ResetSettingsAsync(Button button)
    {
        if (Content?.XamlRoot is not { } root) return;
        var dialog = WindowHelpers.Dialog(root, Loc.Get("Warn_ResetTitle"),
            new TextBlock { Text = Loc.Format("Warn_ResetBody", DataPaths.SettingsFile(DataPaths.Home()) + ".bad"), TextWrapping = TextWrapping.Wrap },
            Loc.Get("Warn_SettingsReset"));
        if (await dialog.ShowAsync() != ContentDialogResult.Primary) return;
        button.IsEnabled = false;
        var r = await Services.Current.ResetSettingsAsync();
        button.IsEnabled = true;
        ShowActionResult(r.Ok, r.Kind, r.Error);
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
        if (_page == "danger:wipe") _page = "danger";
        ContentFrame.Visibility = Visibility.Collapsed;
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
