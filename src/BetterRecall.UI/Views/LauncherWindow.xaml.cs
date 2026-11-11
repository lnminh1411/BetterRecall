// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Collections.Specialized;
using System.ComponentModel;
using System.Runtime.InteropServices.WindowsRuntime;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Native;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage;
using Windows.Storage.Pickers;
using Windows.Storage.Streams;
using Windows.System;

namespace BetterRecall.UI.Views;

/// <summary>
/// Borderless search launcher. Created hidden at start and kept warm: show is a move, Show() and focus.
/// Esc hides it and drops results, previews, history and the detail image from memory.
/// Results come in sections (Text, Visual, Audio, Replay matches); each card has a ... menu (also right click,
/// Shift+F10): copy, save as, delete, delete all from the app or site.
/// </summary>
public sealed partial class LauncherWindow : Window
{
    private const double TileStride = 272;
    private readonly SearchViewModel _vm;
    private readonly DetailViewModel _detail;
    private readonly nint _hwnd;
    private bool _syncingSelection;
    private bool _fillingFacets;
    private int _modal;
    private bool _menuOnResults;   // mock demo (--mock-menu): open the first card's menu once results arrive   // dialogs and pickers in flight: deactivation must not hide the launcher

    private enum DeleteScope { One, App, Site }

    public LauncherWindow()
    {
        InitializeComponent();
        var s = Services.Current;
        _vm = new SearchViewModel(s.Api);
        _detail = new DetailViewModel(s.MediaApi);
        Detail.ViewModel = _detail;
        GroupsSource.Source = _vm.Groups;
        ResultsGrid.ItemsSource = GroupsSource.View;
        HistoryList.ItemsSource = _vm.History;
        _vm.Groups.CollectionChanged += OnGroupsChanged;
        _vm.Results.CollectionChanged += (_, _) =>
        {
            UpdateEmpty();
            if (_menuOnResults && _vm.Results.Count > 0)
            {
                _menuOnResults = false;
                var first = _vm.Results[0];
                DispatcherQueue.TryEnqueue(Microsoft.UI.Dispatching.DispatcherQueuePriority.Low, () =>
                {
                    if (ResultsGrid.ContainerFromItem(first) is FrameworkElement c) BuildMenu(first).ShowAt(c, new FlyoutShowOptions { Placement = FlyoutPlacementMode.RightEdgeAlignedTop });
                });
            }
        };
        _vm.History.CollectionChanged += (_, _) => UpdateEmpty();
        _ = new ThumbLoader(ResultsGrid);
        _vm.PropertyChanged += OnVmChanged;
        FillFacets();

        _hwnd = WindowHelpers.Hwnd(this);
        WindowHelpers.Prepare(this);
        Title = Loc.Get("App_Name");
        if (AppWindow.Presenter is OverlappedPresenter p)
        {
            p.SetBorderAndTitleBar(true, false);
            p.IsMaximizable = false;
            p.IsMinimizable = false;
            p.IsAlwaysOnTop = true;
        }
        AppWindow.IsShownInSwitchers = false;
        AppWindow.Closing += (_, e) => { e.Cancel = true; HideLauncher(); };
        Activated += (_, e) =>
        {
            // Click-away hides, like PowerToys Run. Not while a dropdown, dialog or file picker is open.
            if (e.WindowActivationState == WindowActivationState.Deactivated && IsShown && _modal == 0 && !AnyDropDownOpen && !Services.Current.Locked)
                HideLauncher();
        };
        var closeName = Loc.Get("Launcher_Close");
        AutomationProperties.SetName(CloseButton, closeName);
        ToolTipService.SetToolTip(CloseButton, closeName);

        s.LockedChanged += UpdateLock;
        s.StatusChanged += UpdateStatus;
        s.ConnectionChanged += UpdateOffline;
        Unlock.UnlockedOk += () =>
        {
            UpdateLock();
            SearchBox.Focus(FocusState.Programmatic);
            _ = LoadSideDataAsync();
            if (_vm.Query.Trim().Length > 0) _ = _vm.SearchNowAsync();
        };
    }

    public bool IsShown => AppWindow.IsVisible;

    private bool AnyDropDownOpen => RangeBox.IsDropDownOpen || KindBox.IsDropDownOpen || AppBox.IsDropDownOpen || SiteBox.IsDropDownOpen;

    public void ShowLauncher(string? query = null, bool openMenu = false)
    {
        if (openMenu) _menuOnResults = true;
        WindowHelpers.CenterOnCursorMonitor(AppWindow, 1220, 740, _hwnd);
        WindowSecurity.Apply(_hwnd);   // hidden windows are skipped when the setting changes
        if (AppWindow.Presenter is OverlappedPresenter p) p.IsAlwaysOnTop = Services.Current.SearchAlwaysOnTop;
        AppWindow.Show(true);
        Activate();
        Win32.SetForegroundWindow(_hwnd);
        UpdateLock();
        UpdateStatus();
        UpdateOffline();
        if (Services.Current.Locked) Unlock.Refresh();
        else
        {
            SearchBox.Focus(FocusState.Programmatic);
            if (query is not null) SearchBox.Text = query;   // mock demo (--mock-query); TextChanged runs the search
            SearchBox.SelectAll();
            if (query is null && _vm.Query.Trim().Length > 0) _ = _vm.SearchNowAsync();
            _ = LoadSideDataAsync();
        }
        _ = Services.Current.RefreshStatusAsync();
    }

    public void HideLauncher()
    {
        if (_vm.Query.Trim().Length > 0 && _vm.Results.Count > 0) _ = _vm.CommitHistoryAsync();
        AppWindow.Hide();
        _vm.CancelPending();
        DropData();
    }

    /// <summary>Everything decrypted leaves memory: results, previews, history, facets, the detail image.</summary>
    private void DropData()
    {
        _vm.ClearResults();
        _vm.History.Clear();
        _detail.Clear();
        Services.Current.Thumbs.Clear();
    }

    private async Task LoadSideDataAsync()
    {
        await _vm.LoadHistoryAsync();
        await _vm.LoadFacetsAsync();
    }

    // ---------------- state ----------------

    private void OnVmChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(SearchViewModel.IsBusy):
                Busy.IsActive = _vm.IsBusy;
                break;
            case nameof(SearchViewModel.Message):
                MessageText.Text = Loc.Text(_vm.Message);
                break;
            case nameof(SearchViewModel.Query):
                UpdateEmpty();
                break;
            case nameof(SearchViewModel.Facets):
                FillFacets();
                break;
            case nameof(SearchViewModel.SelectedIndex):
                _syncingSelection = true;
                ResultsGrid.SelectedIndex = _vm.SelectedIndex;
                _syncingSelection = false;
                _ = _detail.LoadAsync(_vm.SelectedIndex >= 0 && _vm.SelectedIndex < _vm.Results.Count ? _vm.Results[_vm.SelectedIndex] : null);
                break;
        }
    }

    /// <summary>Section titles, "Show all (N)" and card times in the current language, set before the cards render.</summary>
    private void OnGroupsChanged(object? sender, NotifyCollectionChangedEventArgs e)
    {
        if (e.NewItems is null) return;
        var now = DateTimeOffset.UtcNow;
        foreach (ResultGroup g in e.NewItems)
        {
            g.Header = Loc.Get("Group_" + g.Kind);
            g.MoreText = Loc.Format("Group_ShowAll", g.Total);
            foreach (var i in g.All) i.Ago = Loc.Text(RelativeTime.Of(i.Ts, now));
        }
    }

    /// <summary>Recent searches when the box is empty, the hint when there is nothing at all.</summary>
    private void UpdateEmpty()
    {
        bool emptyQuery = _vm.Query.Trim().Length == 0;
        bool showHistory = emptyQuery && _vm.History.Count > 0 && !Services.Current.Locked;
        HistoryPanel.Visibility = showHistory ? Visibility.Visible : Visibility.Collapsed;
        EmptyHint.Visibility = _vm.Results.Count == 0 && !showHistory ? Visibility.Visible : Visibility.Collapsed;
    }

    private void FillFacets()
    {
        _fillingFacets = true;
        try
        {
            Fill(AppBox, Loc.Get("Launcher_AppAll"), _vm.Facets.Apps, _vm.App, CaptureItem.AppName);
            Fill(SiteBox, Loc.Get("Launcher_SiteAll"), _vm.Facets.Sites, _vm.Site, n => n);
        }
        finally { _fillingFacets = false; }

        static void Fill(ComboBox box, string all, IReadOnlyList<FacetCount> facets, string current, Func<string, string> label)
        {
            box.Items.Clear();
            box.Items.Add(new ComboBoxItem { Content = all, Tag = "" });
            int sel = 0;
            foreach (var f in facets)
            {
                box.Items.Add(new ComboBoxItem { Content = Loc.Format("Facet_Item", label(f.Name), f.Count), Tag = f.Name });
                if (string.Equals(f.Name, current, StringComparison.OrdinalIgnoreCase)) sel = box.Items.Count - 1;
            }
            if (sel == 0 && current.Length > 0)
            {
                // The filter in use has no rows in this range: keep it visible and selected.
                box.Items.Add(new ComboBoxItem { Content = label(current), Tag = current });
                sel = box.Items.Count - 1;
            }
            box.SelectedIndex = sel;
            box.IsEnabled = facets.Count > 0 || current.Length > 0;
        }
    }

    private void UpdateLock()
    {
        bool locked = Services.Current.Locked;
        var was = Unlock.Visibility == Visibility.Visible;
        Unlock.Visibility = locked ? Visibility.Visible : Visibility.Collapsed;
        if (locked && !was)
        {
            DropData();
            if (IsShown) Unlock.Refresh();
        }
        UpdateEmpty();
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
    }

    // ---------------- input ----------------

    private void Close_Click(object sender, RoutedEventArgs e) => HideLauncher();

    private void Root_PreviewKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key == VirtualKey.Escape)
        {
            if (AnyDropDownOpen || _modal > 0) return;
            e.Handled = true;
            HideLauncher();
        }
        else if ((e.Key == VirtualKey.F && IsCtrl()) || e.Key == VirtualKey.F3)
        {
            e.Handled = true;
            SearchBox.Focus(FocusState.Keyboard);
            SearchBox.SelectAll();
        }
    }

    private static bool IsCtrl() =>
        Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Control).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);

    private void SearchBox_TextChanged(object sender, TextChangedEventArgs e)
    {
        _vm.Query = SearchBox.Text;
        _ = _vm.ScheduleAsync();
    }

    private async void SearchBox_PreviewKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key == VirtualKey.Down && _vm.Results.Count > 0)
        {
            e.Handled = true;
            if (_vm.SelectedIndex < 0) _vm.SelectedIndex = 0;
            FocusSelected();
        }
        else if (e.Key == VirtualKey.Down && HistoryPanel.Visibility == Visibility.Visible)
        {
            e.Handled = true;
            HistoryList.Focus(FocusState.Keyboard);
        }
        else if (e.Key == VirtualKey.Enter)
        {
            e.Handled = true;
            _vm.CancelPending();
            await _vm.SearchNowAsync();
            await _vm.CommitHistoryAsync();
        }
    }

    private void FocusSelected()
    {
        ResultsGrid.ScrollIntoView(_vm.Results[Math.Max(0, _vm.SelectedIndex)]);
        if (ResultsGrid.ContainerFromIndex(Math.Max(0, _vm.SelectedIndex)) is GridViewItem item) item.Focus(FocusState.Keyboard);
        else ResultsGrid.Focus(FocusState.Keyboard);
    }

    private int Columns => Math.Max(1, (int)((ResultsGrid.ActualWidth - 16) / TileStride));

    private void ResultsGrid_PreviewKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key == VirtualKey.Up && ResultsGrid.SelectedIndex < Columns)
        {
            e.Handled = true;
            SearchBox.Focus(FocusState.Keyboard);
        }
        else if (e.Key == VirtualKey.Delete && Selected is { } item)
        {
            e.Handled = true;
            _ = DeleteAsync(item, DeleteScope.One);
        }
        else if (e.Key == VirtualKey.C && IsCtrl() && Selected is { } copy)
        {
            e.Handled = true;
            _ = CopyAsync(copy);
        }
    }

    private CaptureItem? Selected => _vm.SelectedIndex >= 0 && _vm.SelectedIndex < _vm.Results.Count ? _vm.Results[_vm.SelectedIndex] : null;

    private void ResultsGrid_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_syncingSelection) return;
        _vm.SelectedIndex = ResultsGrid.SelectedIndex;
    }

    private void ResultsGrid_ItemClick(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is not CaptureItem item) return;
        _vm.SelectedIndex = _vm.Results.IndexOf(item);
        _ = _vm.CommitHistoryAsync();
    }

    private void ShowAll_Click(object sender, RoutedEventArgs e)
    {
        if ((sender as FrameworkElement)?.DataContext is ResultGroup g) _vm.Expand(g);
    }

    private void Filter_Changed(object sender, SelectionChangedEventArgs e)
    {
        if (_vm is null) return;
        var range = (TimeRange)Math.Max(0, RangeBox.SelectedIndex);
        bool rangeChanged = range != _vm.Range;
        _vm.Range = range;
        _vm.Kind = (KindFilter)Math.Max(0, KindBox.SelectedIndex);
        if (rangeChanged && IsShown && !Services.Current.Locked) _ = _vm.LoadFacetsAsync();
        _ = _vm.ScheduleAsync();
    }

    private void Facet_Changed(object sender, SelectionChangedEventArgs e)
    {
        if (_vm is null || _fillingFacets) return;
        _vm.App = (AppBox.SelectedItem as ComboBoxItem)?.Tag as string ?? "";
        _vm.Site = (SiteBox.SelectedItem as ComboBoxItem)?.Tag as string ?? "";
        _ = _vm.ScheduleAsync();
    }

    // ---------------- search history ----------------

    private void History_ItemClick(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is not string q) return;
        SearchBox.Text = q;   // TextChanged runs the search
        SearchBox.SelectionStart = q.Length;
        SearchBox.Focus(FocusState.Programmatic);
        _ = _vm.CommitHistoryAsync();
    }

    private void DeleteHistory_Click(object sender, RoutedEventArgs e)
    {
        if ((sender as FrameworkElement)?.Tag is string q) _ = _vm.DeleteHistoryAsync(q);
    }

    private void ClearHistory_Click(object sender, RoutedEventArgs e) => _ = _vm.ClearHistoryAsync();

    // ---------------- card menu ----------------

    private void More_Click(object sender, RoutedEventArgs e)
    {
        if (sender is not FrameworkElement b || b.DataContext is not CaptureItem item) return;
        _vm.SelectedIndex = _vm.Results.IndexOf(item);
        BuildMenu(item).ShowAt(b, new FlyoutShowOptions { Placement = FlyoutPlacementMode.BottomEdgeAlignedRight });
    }

    private void ResultsGrid_ContextRequested(UIElement sender, ContextRequestedEventArgs e)
    {
        var item = (e.OriginalSource as FrameworkElement)?.DataContext as CaptureItem ?? Selected;
        if (item is null) return;
        e.Handled = true;
        _vm.SelectedIndex = _vm.Results.IndexOf(item);
        var menu = BuildMenu(item);
        if (e.TryGetPosition(ResultsGrid, out var pt)) menu.ShowAt(ResultsGrid, new FlyoutShowOptions { Position = pt });
        else if (ResultsGrid.ContainerFromItem(item) is FrameworkElement c) menu.ShowAt(c);
        else menu.ShowAt(ResultsGrid);
    }

    private MenuFlyout BuildMenu(CaptureItem item)
    {
        var m = new MenuFlyout();
        // The menu is a windowed popup: opening it deactivates the launcher, which must not hide it.
        m.Opening += (_, _) => _modal++;
        m.Closed += (_, _) => _modal--;
        m.Items.Add(Entry(item.HasImage ? "Menu_CopyImage" : "Menu_CopyText", "\uE8C8", () => _ = CopyAsync(item), VirtualKey.C));
        m.Items.Add(Entry("Menu_SaveAs", "\uE792", () => _ = SaveAsAsync(item)));
        m.Items.Add(new MenuFlyoutSeparator());
        m.Items.Add(Entry("Menu_Delete", "\uE74D", () => _ = DeleteAsync(item, DeleteScope.One), VirtualKey.Delete));
        if (item.App.Length > 0)
            m.Items.Add(EntryText(Loc.Format("Menu_DeleteFrom", CaptureItem.AppName(item.App)), "\uE74D", () => _ = DeleteAsync(item, DeleteScope.App)));
        if (item.HasSite)
            m.Items.Add(EntryText(Loc.Format("Menu_DeleteFrom", item.Site), "\uE774", () => _ = DeleteAsync(item, DeleteScope.Site)));
        return m;

        static MenuFlyoutItem Entry(string key, string glyph, Action act, VirtualKey? accel = null)
        {
            var i = EntryText(Loc.Get(key), glyph, act);
            if (accel is { } k) i.KeyboardAcceleratorTextOverride = k == VirtualKey.C ? "Ctrl+C" : "Del";
            return i;
        }

        static MenuFlyoutItem EntryText(string text, string glyph, Action act)
        {
            var i = new MenuFlyoutItem { Text = text, Icon = new FontIcon { Glyph = glyph } };
            i.Click += (_, _) => act();
            return i;
        }
    }

    /// <summary>Image as PNG for screen captures, the transcript for audio. Kept out of clipboard history and cloud sync.</summary>
    private async Task CopyAsync(CaptureItem item)
    {
        var api = Services.Current.MediaApi;
        var dp = new DataPackage();
        if (item.HasImage)
        {
            var kf = await api.KeyframeAsync(item.Id);
            if (!kf.Ok) { MessageText.Text = Loc.Error(kf.Kind, kf.Error); return; }
            var png = await Images.ToPngAsync(kf.Value!.Bytes);
            if (png is null) { MessageText.Text = Loc.Get("Err_Internal"); return; }
            var stream = new InMemoryRandomAccessStream();
            await stream.WriteAsync(png.AsBuffer());
            stream.Seek(0);
            dp.SetBitmap(RandomAccessStreamReference.CreateFromStream(stream));
        }
        else
        {
            var d = await api.GetAsync(item.Id);
            if (!d.Ok) { MessageText.Text = Loc.Error(d.Kind, d.Error); return; }
            dp.SetText(d.Value!.Text.Length > 0 ? d.Value.Text : item.DisplayTitle);
        }
        Clipboard.SetContentWithOptions(dp, new ClipboardContentOptions { IsAllowedInHistory = false, IsRoamable = false });
        MessageText.Text = Loc.Get("Card_Copied");
    }

    /// <summary>Decrypted copy to a file the user picks: PNG for screen captures, text for audio.</summary>
    private async Task SaveAsAsync(CaptureItem item)
    {
        var api = Services.Current.MediaApi;
        var picker = new FileSavePicker { SuggestedFileName = "BetterRecall " + item.Ts.ToLocalTime().ToString("yyyy-MM-dd HHmmss", System.Globalization.CultureInfo.InvariantCulture) };
        WinRT.Interop.InitializeWithWindow.Initialize(picker, _hwnd);
        if (item.HasImage) picker.FileTypeChoices.Add(Loc.Get("Save_Png"), [".png"]);
        else picker.FileTypeChoices.Add(Loc.Get("Save_Text"), [".txt"]);
        StorageFile? file;
        _modal++;
        try { file = await picker.PickSaveFileAsync(); }
        finally { _modal--; }
        if (file is null) return;
        if (item.HasImage)
        {
            var kf = await api.KeyframeAsync(item.Id);
            if (!kf.Ok) { MessageText.Text = Loc.Error(kf.Kind, kf.Error); return; }
            var png = await Images.ToPngAsync(kf.Value!.Bytes);
            if (png is null) { MessageText.Text = Loc.Get("Err_Internal"); return; }
            await FileIO.WriteBytesAsync(file, png);
        }
        else
        {
            var d = await api.GetAsync(item.Id);
            if (!d.Ok) { MessageText.Text = Loc.Error(d.Kind, d.Error); return; }
            await FileIO.WriteTextAsync(file, d.Value!.Text);
        }
        MessageText.Text = Loc.Format("Card_Saved", file.Name);
    }

    /// <summary>Count, confirm, then delete with a fresh token (tokens expire after 30 s, a dialog can take longer).</summary>
    private async Task DeleteAsync(CaptureItem item, DeleteScope scope)
    {
        var api = Services.Current.Api;
        Task<AgentResult<PurgeConfirmReply>> Ask() => scope switch
        {
            DeleteScope.App => api.PurgeAppAsync(item.App),
            DeleteScope.Site => api.PurgeSiteAsync(item.Site),
            _ => api.PurgeIdAsync(item.Id),
        };
        _modal++;
        try
        {
            var first = await Ask();
            if (!first.Ok) { MessageText.Text = Loc.Error(first.Kind, first.Error); return; }
            int rows = first.Value!.Rows ?? 0;
            if (rows == 0) { MessageText.Text = Loc.Get("Err_NotFound"); return; }
            string name = scope == DeleteScope.Site ? item.Site : CaptureItem.AppName(item.App);
            var dialog = WindowHelpers.Dialog(Root.XamlRoot,
                Loc.Get(scope == DeleteScope.One ? "Delete_OneTitle" : "Delete_ManyTitle"),
                new TextBlock { Text = scope == DeleteScope.One ? Loc.Get("Delete_OneBody") : Loc.Format("Delete_ManyBody", rows, name), TextWrapping = TextWrapping.Wrap },
                Loc.Get("Delete_Confirm"), danger: true);
            if (await dialog.ShowAsync() != ContentDialogResult.Primary) return;
            var again = await Ask();
            if (!again.Ok) { MessageText.Text = Loc.Error(again.Kind, again.Error); return; }
            var done = await api.PurgeConfirmAsync(again.Value!.Confirm);
            if (!done.Ok) { MessageText.Text = Loc.Error(done.Kind, done.Error); return; }
            Func<CaptureItem, bool> match = scope switch
            {
                DeleteScope.App => i => string.Equals(i.App, item.App, StringComparison.OrdinalIgnoreCase),
                DeleteScope.Site => i => string.Equals(i.Site, item.Site, StringComparison.OrdinalIgnoreCase),
                _ => i => i.Id == item.Id,
            };
            _vm.RemoveWhere(match);
            Services.Current.Thumbs.Clear();
            MessageText.Text = Loc.Format("Card_Deleted", done.Value);
            _ = _vm.LoadFacetsAsync();
            if (ResultsGrid.Items.Count > 0) FocusSelected();
            else SearchBox.Focus(FocusState.Programmatic);
        }
        finally { _modal--; }
    }

    private void OpenTimeline_Click(object sender, RoutedEventArgs e) => App.Current.ShowMain("timeline");
    private void OpenReplays_Click(object sender, RoutedEventArgs e) => App.Current.ShowMain("replays");
    private void OpenSettings_Click(object sender, RoutedEventArgs e) => App.Current.ShowMain("settings:capture");
}
