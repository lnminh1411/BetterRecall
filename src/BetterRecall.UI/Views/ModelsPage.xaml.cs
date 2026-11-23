// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace BetterRecall.UI.Views;

public sealed class ModelComponentView
{
    public ModelComponentView(ModelComponent component, bool downloading)
    {
        Id = component.Id;
        Title = component.Title;
        Description = component.Description;
        string size = ModelsFormat.FormatSize(component.Bytes);
        string state = component.State switch
        {
            "complete" => Loc.Get("ModelsPage_StateComplete"),
            "partial" => Loc.Get("ModelsPage_StatePartial"),
            _ => Loc.Get("ModelsPage_StateMissing"),
        };
        var setLabels = new List<string>();
        if (component.Sets.Any(s => string.Equals(s, "minimal", StringComparison.OrdinalIgnoreCase)))
            setLabels.Add(Loc.Get("ModelsPage_SetMinimal"));
        if (component.Sets.Any(s => string.Equals(s, "full", StringComparison.OrdinalIgnoreCase)))
            setLabels.Add(Loc.Get("ModelsPage_SetFull"));
        string sets = string.Join(", ", setLabels);
        MetaLine = string.IsNullOrEmpty(sets) ? $"{size}  ·  {state}" : $"{size}  ·  {state}  ·  {sets}";
        DownloadLabel = Loc.Get("ModelsPage_Download");
        // F5: every button says "Download"; screen readers need the component name too.
        DownloadAutomationName = $"{DownloadLabel}: {Title}";
        DownloadVisibility = component.State == "complete" ? Visibility.Collapsed : Visibility.Visible;
        CanDownload = !downloading && component.State != "complete";
    }

    public string Id { get; }
    public string Title { get; }
    public string Description { get; }
    public string MetaLine { get; }
    public string DownloadLabel { get; }
    public string DownloadAutomationName { get; }
    public Visibility DownloadVisibility { get; }
    public bool CanDownload { get; }
}

public sealed partial class ModelsPage : Page
{
    private ModelsList? _list;
    private bool _wasDownloading;

    public ModelsPage()
    {
        InitializeComponent();
        Loaded += OnLoaded;
        Unloaded += OnUnloaded;
    }

    private void OnLoaded(object sender, RoutedEventArgs e)
    {
        var dl = Services.Current.ModelsDownload;
        dl.Changed += OnDownloadChanged;
        _wasDownloading = dl.IsDownloading;
        SyncDownloadState();
        _ = LoadAsync();
    }

    private void OnUnloaded(object sender, RoutedEventArgs e)
    {
        Services.Current.ModelsDownload.Changed -= OnDownloadChanged;
    }

    private async Task LoadAsync()
    {
        LoadingRing.IsActive = true;
        var r = await Services.Current.Api.ModelsListAsync();
        LoadingRing.IsActive = false;
        if (!r.Ok || r.Value is null)
        {
            ShowResult(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
            return;
        }
        _list = r.Value;
        RenderList();
    }

    private void RenderList()
    {
        var list = _list;
        bool busy = Services.Current.ModelsDownload.IsDownloading || (list?.Downloading ?? false);
        if (list is null)
        {
            DownloadMinimalButton.IsEnabled = false;
            DownloadFullButton.IsEnabled = false;
            return;
        }
        DownloadMinimalButton.Content = Loc.Format("ModelsPage_DownloadMinimal", ModelsFormat.FormatSize(list.MinimalBytes));
        DownloadFullButton.Content = Loc.Format("ModelsPage_DownloadFull", ModelsFormat.FormatSize(list.FullBytes));
        DownloadMinimalButton.IsEnabled = !busy;
        DownloadFullButton.IsEnabled = !busy;
        FolderPathText.Text = list.ModelsDir;
        ComponentsList.ItemsSource = list.Components.Select(c => new ModelComponentView(c, busy)).ToList();
    }

    private void OnDownloadChanged()
    {
        var dl = Services.Current.ModelsDownload;
        bool finishedNow = _wasDownloading && !dl.IsDownloading;
        _wasDownloading = dl.IsDownloading;
        SyncDownloadState();
        RenderList();
        if (finishedNow) _ = LoadAsync();
    }

    private void SyncDownloadState()
    {
        var dl = Services.Current.ModelsDownload;
        ProgressPanel.Visibility = dl.IsDownloading ? Visibility.Visible : Visibility.Collapsed;
        if (dl.IsDownloading)
        {
            var p = dl.LastProgress;
            if (p is null || p.Phase == "checking" || p.TotalBytes <= 0)
            {
                DownloadProgressBar.IsIndeterminate = true;
                ProgressText.Text = Loc.Get("ModelsPage_Checking");
            }
            else
            {
                DownloadProgressBar.IsIndeterminate = false;
                DownloadProgressBar.Maximum = p.TotalBytes;
                DownloadProgressBar.Value = Math.Min(p.DoneBytes, p.TotalBytes);
                ProgressText.Text = ModelsFormat.FormatProgress(p.DoneBytes, p.TotalBytes, dl.BytesPerSecond);
            }
        }

        if (!dl.IsDownloading && dl.FinalResult is { } final)
        {
            switch (final.Phase)
            {
                case "done":
                    ShowResult(InfoBarSeverity.Success, Loc.Get("ModelsPage_Done"));
                    break;
                case "cancelled":
                    ShowResult(InfoBarSeverity.Informational, Loc.Get("ModelsPage_Cancelled"));
                    break;
                case "offline":
                    ShowResult(InfoBarSeverity.Warning, Loc.Get("ModelsPage_Offline"));
                    break;
                default:
                    ShowResult(InfoBarSeverity.Error, Loc.Format("ModelsPage_Failed", final.Error ?? dl.ErrorMessage ?? ""));
                    break;
            }
        }
    }

    private void ShowResult(InfoBarSeverity severity, string message)
    {
        ResultBar.Severity = severity;
        ResultBar.Message = message;
        ResultBar.IsOpen = !string.IsNullOrEmpty(message);
    }

    private void ResultBar_Closed(InfoBar sender, InfoBarClosedEventArgs args)
    {
        if (args.Reason == InfoBarCloseReason.CloseButton)
            Services.Current.ModelsDownload.ClearResult();
    }

    private async void DownloadMinimal_Click(object sender, RoutedEventArgs e) =>
        await ConfirmAndDownloadAsync("minimal", Loc.Get("ModelsPage_WarnTargetMinimal"));

    private async void DownloadFull_Click(object sender, RoutedEventArgs e) =>
        await ConfirmAndDownloadAsync("full", Loc.Get("ModelsPage_WarnTargetFull"));

    private async void DownloadComponent_Click(object sender, RoutedEventArgs e)
    {
        if (sender is not Button { Tag: string id } || _list is null) return;
        var comp = _list.Components.FirstOrDefault(c => c.Id == id);
        await ConfirmAndDownloadAsync(id, comp?.Title ?? id);
    }

    private async Task ConfirmAndDownloadAsync(string target, string targetLabel)
    {
        if (_list is null || XamlRoot is null) return;
        long size = ModelsFormat.DownloadSizeFor(_list, target);
        long free = _list.FreeBytes;
        bool enoughSpace = ModelsFormat.HasEnoughSpace(size, free);
        long missing = ModelsFormat.MissingSpaceBytes(size, free);

        var body = new StackPanel { Spacing = 6 };
        body.Children.Add(new TextBlock
        {
            Text = Loc.Format("ModelsPage_WarnSource", _list.Repo, ModelsFormat.ShortRevision(_list.Revision)),
            TextWrapping = TextWrapping.Wrap,
        });
        body.Children.Add(new TextBlock
        {
            Text = Loc.Format("ModelsPage_WarnTarget", targetLabel),
            TextWrapping = TextWrapping.Wrap,
        });
        body.Children.Add(new TextBlock
        {
            Text = Loc.Format("ModelsPage_WarnSize", ModelsFormat.FormatSize(size)),
            TextWrapping = TextWrapping.Wrap,
        });
        body.Children.Add(new TextBlock
        {
            Text = Loc.Format("ModelsPage_WarnDest", _list.ModelsDir),
            TextWrapping = TextWrapping.Wrap,
        });
        body.Children.Add(new TextBlock
        {
            Text = Loc.Format("ModelsPage_WarnFree", ModelsFormat.FormatSize(free)),
            TextWrapping = TextWrapping.Wrap,
        });
        body.Children.Add(new TextBlock
        {
            Text = Loc.Get("ModelsPage_WarnHash"),
            TextWrapping = TextWrapping.Wrap,
            Style = (Style)Application.Current.Resources["CaptionSecondaryTextStyle"],
        });
        if (!enoughSpace)
        {
            body.Children.Add(new TextBlock
            {
                Text = Loc.Format("ModelsPage_WarnNoSpace", ModelsFormat.FormatSize(missing)),
                TextWrapping = TextWrapping.Wrap,
                Style = (Style)Application.Current.Resources["CriticalTextStyle"],
            });
        }

        var dialog = WindowHelpers.Dialog(
            XamlRoot,
            Loc.Get("ModelsPage_WarnTitle"),
            body,
            Loc.Get("ModelsPage_WarnDownload"),
            Loc.Get("Common_Cancel"));
        dialog.DefaultButton = ContentDialogButton.Close;
        dialog.IsPrimaryButtonEnabled = enoughSpace;

        if (await dialog.ShowAsync() != ContentDialogResult.Primary) return;
        ResultBar.IsOpen = false;
        _ = Services.Current.ModelsDownload.StartAsync(target);
    }

    private void Cancel_Click(object sender, RoutedEventArgs e) => Services.Current.ModelsDownload.Cancel();

    private void OpenFolder_Click(object sender, RoutedEventArgs e)
    {
        var path = _list?.ModelsDir;
        if (string.IsNullOrWhiteSpace(path)) return;
        try
        {
            Directory.CreateDirectory(path);
            Process.Start(new ProcessStartInfo("explorer.exe", "\"" + path + "\"") { UseShellExecute = true });
        }
        catch (Exception ex)
        {
            ShowResult(InfoBarSeverity.Error, Loc.Format("ModelsPage_OpenFolderFailed", ex.Message));
        }
    }

    private void Refresh_Click(object sender, RoutedEventArgs e) => _ = LoadAsync();
}
