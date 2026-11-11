// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Globalization;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.Windows.Storage.Pickers;

namespace BetterRecall.UI.Views;

public sealed class ReplayRow(ReplayClip clip)
{
    public ReplayClip Clip { get; } = clip;
    public string Id => Clip.Id;
    public string When => Clip.Ts.ToLocalTime().ToString("f", CultureInfo.CurrentCulture);
    public string Details => Loc.Format(Clip.Encrypted ? "Replays_DetailsEncrypted" : "Replays_DetailsPlain",
        StatusText.FormatDuration(Clip.DurationMs), StatusText.FormatBytes(Clip.Bytes));
    public string PlayText => Loc.Get("Replays_Play");
    public string DeleteText => Loc.Get("Replays_Delete");
    public string PlayName => Loc.Format("Replays_PlayName", When);
    public string DeleteName => Loc.Format("Replays_DeleteName", When);
    public string AccessibleName => When + ", " + Details;
}

/// <summary>
/// Replay list. Play exports a decrypted MP4 to a path the user picks (after a warning) and opens it.
/// </summary>
public sealed partial class ReplaysPage : Page, IDataPage
{
    private readonly List<ReplayRow> _rows = [];

    public ReplaysPage()
    {
        InitializeComponent();
        Loaded += (_, _) => Reload();
    }

    public void Reload() => _ = LoadAsync();

    private async Task LoadAsync()
    {
        Busy.IsActive = true;
        var r = await Services.Current.Api.ReplayListAsync();
        Busy.IsActive = false;
        _rows.Clear();
        if (!r.Ok)
        {
            Show(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
            List.ItemsSource = null;
            return;
        }
        _rows.AddRange(r.Value!.OrderByDescending(c => c.Ts).Select(c => new ReplayRow(c)));
        List.ItemsSource = _rows.ToList();
        EmptyText.Visibility = _rows.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    private void Show(InfoBarSeverity sev, string message, string? title = null)
    {
        ResultBar.Severity = sev;
        ResultBar.Title = title ?? "";
        ResultBar.Message = message;
        ResultBar.ActionButton = null;
        ResultBar.IsOpen = true;
    }

    private async void SaveNow_Click(object sender, RoutedEventArgs e)
    {
        SaveNow.IsEnabled = false;
        Busy.IsActive = true;
        var r = await Services.Current.Api.ReplaySaveAsync();
        Busy.IsActive = false;
        SaveNow.IsEnabled = true;
        if (r.Ok) Show(InfoBarSeverity.Success, Loc.Format("Replays_Saved", StatusText.FormatDuration(r.Value!.DurationMs)));
        else Show(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
        await LoadAsync();
    }

    private void Refresh_Click(object sender, RoutedEventArgs e) => Reload();

    private async void Play_Click(object sender, RoutedEventArgs e)
    {
        if ((sender as FrameworkElement)?.Tag is not string id || _rows.FirstOrDefault(x => x.Id == id) is not { } row) return;

        var warn = WindowHelpers.Dialog(XamlRoot, Loc.Get("Replays_WarnTitle"),
            new TextBlock { Text = Loc.Get("Replays_WarnBody"), TextWrapping = TextWrapping.Wrap }, Loc.Get("Replays_WarnContinue"));
        if (await warn.ShowAsync() != ContentDialogResult.Primary) return;

        var picker = new FileSavePicker(XamlRoot.ContentIslandEnvironment.AppWindowId)
        {
            SuggestedFileName = "BetterRecall-" + row.Clip.Ts.ToLocalTime().ToString("yyyyMMdd-HHmmss", CultureInfo.InvariantCulture),
            SuggestedStartLocation = PickerLocationId.VideosLibrary,
        };
        picker.FileTypeChoices.Add(Loc.Get("Replays_FileType"), [".mp4"]);
        var picked = await picker.PickSaveFileAsync();
        if (picked is null || string.IsNullOrEmpty(picked.Path)) return;

        Busy.IsActive = true;
        var r = await Services.Current.Api.ReplayExportAsync(id, picked.Path);
        Busy.IsActive = false;
        if (!r.Ok)
        {
            Show(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
            return;
        }
        var path = picked.Path;
        Show(InfoBarSeverity.Success, Loc.Format("Replays_Exported", path), Loc.Get("Replays_ExportedTitle"));
        if (File.Exists(path))
        {
            var open = new Button { Content = Loc.Get("Replays_Open") };
            open.Click += (_, _) => Process.Start(new ProcessStartInfo(path) { UseShellExecute = true });
            ResultBar.ActionButton = open;
            Process.Start(new ProcessStartInfo(path) { UseShellExecute = true });
        }
    }

    private async void Delete_Click(object sender, RoutedEventArgs e)
    {
        if ((sender as FrameworkElement)?.Tag is not string id || _rows.FirstOrDefault(x => x.Id == id) is not { } row) return;
        var confirm = WindowHelpers.Dialog(XamlRoot, Loc.Get("Replays_DeleteTitle"),
            new TextBlock { Text = Loc.Format("Replays_DeleteBody", row.When), TextWrapping = TextWrapping.Wrap }, Loc.Get("Replays_Delete"), danger: true);
        if (await confirm.ShowAsync() != ContentDialogResult.Primary) return;
        var r = await Services.Current.Api.ReplayDeleteAsync(id);
        if (!r.Ok) Show(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
        await LoadAsync();
    }
}
