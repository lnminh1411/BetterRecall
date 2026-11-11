// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.ComponentModel;
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;

namespace BetterRecall.UI.Views;

/// <summary>Full keyframe, title, app, time and transcript for the selected capture.</summary>
public sealed partial class DetailPane : UserControl
{
    private DetailViewModel? _vm;

    public DetailPane()
    {
        InitializeComponent();
        Render();
    }

    internal DetailViewModel? ViewModel
    {
        get => _vm;
        set
        {
            if (_vm is not null) _vm.PropertyChanged -= OnChanged;
            _vm = value;
            if (_vm is not null) _vm.PropertyChanged += OnChanged;
            Render();
        }
    }

    private void OnChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName == nameof(DetailViewModel.Image)) _ = ShowImageAsync();
        else Render();
    }

    private async Task ShowImageAsync()
    {
        var bytes = _vm?.Image;
        KeyframeImage.Source = bytes is null ? null : await Images.DecodeAsync(bytes);
    }

    private void Render()
    {
        var item = _vm?.Item;
        bool has = item is not null;
        EmptyText.Visibility = has ? Visibility.Collapsed : Visibility.Visible;
        ImageHost.Visibility = Meta.Visibility = has ? Visibility.Visible : Visibility.Collapsed;
        Busy.IsActive = _vm?.IsBusy == true;
        if (_vm?.Image is null) KeyframeImage.Source = null;
        if (!has)
        {
            TranscriptScroll.Visibility = Visibility.Collapsed;
            return;
        }
        TitleText.Text = item!.DisplayTitle;
        AppText.Text = item.App;
        TimeText.Text = item.DurationS is { } d && item.IsAudio
            ? Loc.Format("Detail_TimeDuration", item.TimeText, StatusText.FormatDuration(d * 1000))
            : item.TimeText;
        AudioIcon.Visibility = item.IsAudio ? Visibility.Visible : Visibility.Collapsed;
        TranscriptScroll.Visibility = item.IsAudio && _vm!.Text.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
        TranscriptText.Text = item.IsAudio ? _vm!.Text : "";
        if (!item.IsAudio && _vm!.Text.Length > 0) TitleText.Text = _vm.Text;
        ErrorBar.Message = Loc.Text(_vm!.Message);
        ErrorBar.IsOpen = !_vm.Message.IsEmpty;
        AutomationProperties.SetName(KeyframeImage, Loc.Format("Detail_ImageName", item.DisplayTitle));
    }
}
