// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Globalization;
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;

namespace BetterRecall.UI.Views;

public sealed partial class TimelinePage : Page, IDataPage
{
    private readonly TimelineViewModel _vm;
    private readonly DetailViewModel _detail;
    private bool _updating;

    public TimelinePage()
    {
        InitializeComponent();
        var s = Services.Current;
        _vm = new TimelineViewModel(s.Api);
        _detail = new DetailViewModel(s.MediaApi);
        Detail.ViewModel = _detail;
        Items.ItemsSource = _vm.Items;
        _ = new ThumbLoader(Items);
        _vm.PropertyChanged += (_, e) =>
        {
            if (e.PropertyName == nameof(TimelineViewModel.IsBusy)) Busy.IsActive = _vm.IsBusy;
            else if (e.PropertyName == nameof(TimelineViewModel.Message)) MessageText.Text = Loc.Text(_vm.Message);
            else if (e.PropertyName == nameof(TimelineViewModel.HourCounts)) DrawBars();
        };
        for (int h = 0; h < 24; h++) HourBars.ColumnDefinitions.Add(new ColumnDefinition());
        _updating = true;
        DayPicker.Date = DateTime.Now.Date;
        DayPicker.MaxDate = DateTime.Now.Date;
        _updating = false;
        Loaded += (_, _) => { s.Removed += OnRemoved; Reload(); };
        Unloaded += (_, _) => { s.Removed -= OnRemoved; _vm.Clear(); _detail.Clear(); };   // a load still in flight is discarded too
    }

    /// <summary>Captures purged by another client, retention, the storage limit or the NSFW gate leave the day and the
    /// preview (B10); too many to list reloads the day.</summary>
    private void OnRemoved(Contracts.RemovedEvent e)
    {
        if (e.Truncated) { Reload(); return; }
        var match = RemovedMatch.For(e);
        if (_detail.Item is { } shown && match(shown.Id, shown.Ts)) _detail.Clear();
        _vm.RemoveWhere(match);
    }

    public void Reload() => _ = LoadAsync();

    private async Task LoadAsync()
    {
        _detail.Clear();
        _vm.Pending = Services.Current.Status?.Queue.Pending ?? 0;   // an empty day may only be waiting for indexing
        await _vm.LoadAsync();
    }

    private void DrawBars()
    {
        HourBars.Children.Clear();
        int max = Math.Max(1, _vm.HourCounts.Max());
        var accent = (Style)Application.Current.Resources["BarAccentStyle"];
        var muted = (Style)Application.Current.Resources["BarMutedStyle"];
        for (int h = 0; h < 24; h++)
        {
            int count = _vm.HourCounts[h];
            var hour = h;
            var label = new DateTime(2000, 1, 1, h, 0, 0).ToString("t", CultureInfo.CurrentCulture);
            var bar = new Button
            {
                Padding = new Thickness(0),
                MinWidth = 0,
                HorizontalAlignment = HorizontalAlignment.Stretch,
                VerticalAlignment = VerticalAlignment.Stretch,
                Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent),
                BorderThickness = new Thickness(0),
                Content = new Rectangle
                {
                    Style = _vm.Hour == h ? accent : count > 0 ? accent : muted,
                    Opacity = _vm.Hour is null || _vm.Hour == h ? 1 : 0.45,
                    Height = count == 0 ? 2 : 6 + 46.0 * count / max,
                    RadiusX = 2,
                    RadiusY = 2,
                    VerticalAlignment = VerticalAlignment.Bottom,
                },
                VerticalContentAlignment = VerticalAlignment.Bottom,
                HorizontalContentAlignment = HorizontalAlignment.Stretch,
            };
            ToolTipService.SetToolTip(bar, Loc.Format("Timeline_HourBar", label, count));
            AutomationProperties.SetName(bar, Loc.Format("Timeline_HourBar", label, count));
            bar.Click += (_, _) => SelectHour(hour);
            Grid.SetColumn(bar, h);
            HourBars.Children.Add(bar);
        }
    }

    private void SelectHour(int? hour)
    {
        _vm.SelectHour(hour);
        _updating = true;
        AllDay.IsChecked = hour is null;
        if (hour is { } h) HourSlider.Value = h;
        _updating = false;
        _detail.Clear();
        DrawBars();
    }

    private void HourSlider_ValueChanged(object sender, RangeBaseValueChangedEventArgs e)
    {
        if (_updating || _vm is null) return;
        SelectHour((int)e.NewValue);
    }

    private void AllDay_Click(object sender, RoutedEventArgs e) => SelectHour(AllDay.IsChecked == true ? null : (int)HourSlider.Value);

    private async void DayPicker_DateChanged(CalendarDatePicker sender, CalendarDatePickerDateChangedEventArgs args)
    {
        if (_updating || args.NewDate is null) return;
        _vm.Day = DateOnly.FromDateTime(args.NewDate.Value.Date);
        SelectHour(null);
        await LoadAsync();
    }

    private void SetDay(DateOnly day)
    {
        if (day > DateOnly.FromDateTime(DateTime.Now)) return;
        DayPicker.Date = day.ToDateTime(TimeOnly.MinValue);   // DateChanged loads
    }

    private void PrevDay_Click(object sender, RoutedEventArgs e) => SetDay(_vm.Day.AddDays(-1));
    private void NextDay_Click(object sender, RoutedEventArgs e) => SetDay(_vm.Day.AddDays(1));
    private void Today_Click(object sender, RoutedEventArgs e) => SetDay(DateOnly.FromDateTime(DateTime.Now));

    private bool _narrow;

    /// <summary>Narrow windows give the tiles and toolbar room: the detail pane shrinks from 380 to 280 px, and under
    /// 720 px it leaves its column and opens over the tiles only while a capture is selected.</summary>
    private void Root_SizeChanged(object sender, SizeChangedEventArgs e)
    {
        _narrow = e.NewSize.Width < 720;
        DetailColumn.Width = new GridLength(_narrow ? 0 : e.NewSize.Width < 900 ? 280 : 380);
        Grid.SetColumn(DetailHost, _narrow ? 0 : 1);
        Grid.SetRow(DetailHost, _narrow ? 3 : 0);
        Grid.SetRowSpan(DetailHost, _narrow ? 1 : 4);
        DetailHost.HorizontalAlignment = _narrow ? HorizontalAlignment.Right : HorizontalAlignment.Stretch;
        DetailHost.Width = _narrow ? Math.Min(380, Math.Max(0, e.NewSize.Width - 24)) : double.NaN;
        DetailHost.BorderThickness = new Thickness(1, _narrow ? 1 : 0, 0, 0);
        CloseDetail.Visibility = _narrow ? Visibility.Visible : Visibility.Collapsed;
        UpdateDetailHost();
    }

    private void UpdateDetailHost() =>
        DetailHost.Visibility = !_narrow || Items.SelectedItem is not null ? Visibility.Visible : Visibility.Collapsed;

    private void CloseDetail_Click(object sender, RoutedEventArgs e)
    {
        var index = Items.SelectedIndex;
        Items.SelectedItem = null;
        if (index >= 0 && Items.ContainerFromIndex(index) is Control c) c.Focus(FocusState.Programmatic);
        else Items.Focus(FocusState.Programmatic);
    }

    private void Items_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        UpdateDetailHost();
        _ = _detail.LoadAsync(Items.SelectedItem as CaptureItem);
    }
}
