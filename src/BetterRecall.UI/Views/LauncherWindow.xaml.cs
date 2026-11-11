// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.ComponentModel;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Native;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Windows.System;

namespace BetterRecall.UI.Views;

/// <summary>
/// Borderless search launcher. Created hidden at start and kept warm: show is a move, Show() and focus.
/// Esc hides it and drops results, previews and the detail image from memory.
/// </summary>
public sealed partial class LauncherWindow : Window
{
    private const double TileStride = 236;
    private readonly SearchViewModel _vm;
    private readonly DetailViewModel _detail;
    private readonly nint _hwnd;
    private bool _syncingSelection;

    public LauncherWindow()
    {
        InitializeComponent();
        var s = Services.Current;
        _vm = new SearchViewModel(s.Api);
        _detail = new DetailViewModel(s.MediaApi);
        Detail.ViewModel = _detail;
        ResultsGrid.ItemsSource = _vm.Results;
        _vm.Results.CollectionChanged += (_, _) => EmptyHint.Visibility = _vm.Results.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
        _ = new ThumbLoader(ResultsGrid);
        _vm.PropertyChanged += OnVmChanged;

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
            // Click-away hides, like PowerToys Run. Not while a filter dropdown is open (its popup can take activation).
            if (e.WindowActivationState == WindowActivationState.Deactivated && IsShown
                && !RangeBox.IsDropDownOpen && !KindBox.IsDropDownOpen && !Services.Current.Locked)
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
            if (_vm.Query.Trim().Length > 0) _ = _vm.SearchNowAsync();
        };
    }

    public bool IsShown => AppWindow.IsVisible;

    public void ShowLauncher(string? query = null)
    {
        WindowHelpers.CenterOnCursorMonitor(AppWindow, 1180, 720, _hwnd);
        WindowSecurity.Apply(_hwnd);   // hidden windows are skipped when the setting changes
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
        }
        _ = Services.Current.RefreshStatusAsync();
    }

    public void HideLauncher()
    {
        AppWindow.Hide();
        _vm.CancelPending();
        _vm.Results.Clear();
        _detail.Clear();
        Services.Current.Thumbs.Clear();
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
            case nameof(SearchViewModel.SelectedIndex):
                _syncingSelection = true;
                ResultsGrid.SelectedIndex = _vm.SelectedIndex;
                _syncingSelection = false;
                _ = _detail.LoadAsync(_vm.SelectedIndex >= 0 && _vm.SelectedIndex < _vm.Results.Count ? _vm.Results[_vm.SelectedIndex] : null);
                break;
        }
    }

    private void UpdateLock()
    {
        bool locked = Services.Current.Locked;
        var was = Unlock.Visibility == Visibility.Visible;
        Unlock.Visibility = locked ? Visibility.Visible : Visibility.Collapsed;
        if (locked && !was)
        {
            _vm.Results.Clear();
            _detail.Clear();
            Services.Current.Thumbs.Clear();
            if (IsShown) Unlock.Refresh();
        }
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
            if (RangeBox.IsDropDownOpen || KindBox.IsDropDownOpen) return;
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
        else if (e.Key == VirtualKey.Enter)
        {
            e.Handled = true;
            _vm.CancelPending();
            await _vm.SearchNowAsync();
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
    }

    private void ResultsGrid_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_syncingSelection) return;
        _vm.SelectedIndex = ResultsGrid.SelectedIndex;
    }

    private void ResultsGrid_ItemClick(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is CaptureItem item) _vm.SelectedIndex = _vm.Results.IndexOf(item);
    }

    private void Filter_Changed(object sender, SelectionChangedEventArgs e)
    {
        if (_vm is null) return;
        _vm.Range = (TimeRange)Math.Max(0, RangeBox.SelectedIndex);
        _vm.Kind = (KindFilter)Math.Max(0, KindBox.SelectedIndex);
        _ = _vm.ScheduleAsync();
    }

    private void AppBox_TextChanged(AutoSuggestBox sender, AutoSuggestBoxTextChangedEventArgs args)
    {
        if (args.Reason != AutoSuggestionBoxTextChangeReason.UserInput) return;
        var text = sender.Text.Trim();
        sender.ItemsSource = _vm.Results.Select(r => r.App).Distinct(StringComparer.OrdinalIgnoreCase)
            .Where(a => text.Length == 0 || a.Contains(text, StringComparison.OrdinalIgnoreCase)).Take(8).ToList();
        _vm.App = text;
        _ = _vm.ScheduleAsync();
    }

    private void AppBox_QuerySubmitted(AutoSuggestBox sender, AutoSuggestBoxQuerySubmittedEventArgs args)
    {
        _vm.App = (args.ChosenSuggestion as string ?? args.QueryText).Trim();
        sender.Text = _vm.App;
        _ = _vm.ScheduleAsync();
    }

    private void OpenTimeline_Click(object sender, RoutedEventArgs e) => App.Current.ShowMain("timeline");
    private void OpenReplays_Click(object sender, RoutedEventArgs e) => App.Current.ShowMain("replays");
    private void OpenSettings_Click(object sender, RoutedEventArgs e) => App.Current.ShowMain("settings:capture");
}
