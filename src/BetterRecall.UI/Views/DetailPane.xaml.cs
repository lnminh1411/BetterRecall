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
        AppText.Text = item.HasSite ? item.App + "  ·  " + item.Site : item.App;
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
        RenderActions(item);
    }

    // ---------------- replay link and cloud describe ----------------

    /// <summary>Raised around dialogs, so the launcher does not hide itself while one is open.</summary>
    public event Action<bool>? ModalChanged;

    private string? _describedId;

    private void RenderActions(CaptureItem item)
    {
        if (_describedId != item.Id) { DescribeBar.IsOpen = false; _describedId = null; }
        ReplayButton.Visibility = item.IsReplay ? Visibility.Visible : Visibility.Collapsed;
        if (item.IsReplay) ReplayButton.Content = Loc.Format("Detail_OpenReplay", StatusText.FormatDuration(item.ReplayOffsetMs ?? 0));
        DescribeButton.Content = Loc.Get("Detail_Describe");
        DescribeButton.Visibility = Services.Current.AiEnabled && item.HasImage ? Visibility.Visible : Visibility.Collapsed;
        Actions.Visibility = ReplayButton.Visibility == Visibility.Visible || DescribeButton.Visibility == Visibility.Visible ? Visibility.Visible : Visibility.Collapsed;
    }

    private void Replay_Click(object sender, RoutedEventArgs e)
    {
        if (_vm?.Item is { IsReplay: true } item) App.Current.ShowMain(ReplayLink.Page(item.ReplayId!, item.ReplayOffsetMs ?? 0));
    }

    /// <summary>Preview (provider, model, the exact image) and an explicit Send; nothing leaves the PC before Send.</summary>
    private async void Describe_Click(object sender, RoutedEventArgs e)
    {
        if (_vm?.Item is not { } item) return;
        var s = Services.Current;
        ModalChanged?.Invoke(true);
        DescribeButton.IsEnabled = false;
        DescribeBusy.IsActive = true;
        try
        {
            var p = await s.MediaApi.DescribePreviewAsync(item.Id);
            if (!p.Ok) { ShowDescribe(InfoBarSeverity.Warning, Loc.Error(p.Kind, p.Error), item.Id); return; }
            var preview = p.Value!;
            var body = new StackPanel { Spacing = 10, MaxWidth = 520 };
            body.Children.Add(new TextBlock { TextWrapping = TextWrapping.Wrap, Text = Loc.Format("Describe_Body", Loc.Get("S_ai_provider_" + preview.Provider), preview.Model, preview.Endpoint) });
            var img = new Image { Stretch = Microsoft.UI.Xaml.Media.Stretch.Uniform, MaxHeight = 300 };
            AutomationProperties.SetName(img, Loc.Get("Describe_PreviewName"));
            img.Source = await Images.DecodeAsync(preview.Bytes);
            body.Children.Add(img);
            body.Children.Add(new TextBlock
            {
                TextWrapping = TextWrapping.Wrap,
                Text = Loc.Get(preview.Redacted ? "Describe_Redacted" : "Describe_NotRedacted"),
                Style = (Style)Application.Current.Resources["CaptionSecondaryTextStyle"],
            });
            var dialog = WindowHelpers.Dialog(XamlRoot, Loc.Get("Describe_Title"), new ScrollViewer { Content = body }, Loc.Get("Describe_Send"));
            dialog.DefaultButton = ContentDialogButton.Close;
            var choice = await dialog.ShowAsync();
            img.Source = null;
            if (choice != ContentDialogResult.Primary) return;
            await using var client = s.CreateRunClient();
            var r = await new AgentApi(client).DescribeSendAsync(preview.Token);
            if (r.Ok) ShowDescribe(InfoBarSeverity.Informational, r.Value!.Text, item.Id, Loc.Format("Describe_ResultTitle", r.Value.Model));
            else ShowDescribe(InfoBarSeverity.Warning, Loc.Error(r.Kind, r.Error), item.Id);
        }
        finally
        {
            DescribeBusy.IsActive = false;
            DescribeButton.IsEnabled = true;
            ModalChanged?.Invoke(false);
        }
    }

    private void ShowDescribe(InfoBarSeverity sev, string text, string id, string? title = null)
    {
        if (_vm?.Item?.Id != id) return;   // selection moved on
        _describedId = id;
        DescribeBar.Severity = sev;
        DescribeBar.Title = title ?? "";
        DescribeBar.Message = text;
        DescribeBar.IsOpen = true;
    }
}
