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
        Loaded += (_, _) => Reload();
        Unloaded += (_, _) => { _vm.Items.Clear(); _detail.Clear(); };
    }

    public void Reload() => _ = LoadAsync();

    private async Task LoadAsync()
    {
        _detail.Clear();
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

    /// <summary>Narrow windows give the tiles and toolbar room: the detail pane shrinks from 380 to 280 px.</summary>
    private void Root_SizeChanged(object sender, SizeChangedEventArgs e)
        => DetailColumn.Width = new GridLength(e.NewSize.Width < 900 ? 280 : 380);

    private void Items_SelectionChanged(object sender, SelectionChangedEventArgs e) =>
        _ = _detail.LoadAsync(Items.SelectedItem as CaptureItem);
}
