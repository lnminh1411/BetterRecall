// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace BetterRecall.UI.Views;

public sealed partial class UpdatePage : Page
{
    private UpdateStatus? _status;
    private bool _checking;
    private bool _applying;
    private bool _wasDownloading;
    private bool _syncingToggle;

    public UpdatePage()
    {
        InitializeComponent();
        Loaded += OnLoaded;
        Unloaded += OnUnloaded;
    }

    private void OnLoaded(object sender, RoutedEventArgs e)
    {
        var dl = Services.Current.UpdateDownload;
        dl.Changed += OnDownloadChanged;
        _wasDownloading = dl.IsDownloading;
        SyncDownloadState();
        _ = LoadStatusAsync();
    }

    private void OnUnloaded(object sender, RoutedEventArgs e)
    {
        Services.Current.UpdateDownload.Changed -= OnDownloadChanged;
    }

    private async Task LoadStatusAsync()
    {
        CheckingRing.IsActive = true;
        var r = await Services.Current.Api.UpdateStatusAsync();
        CheckingRing.IsActive = false;
        if (!r.Ok || r.Value is null)
        {
            ShowResult(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
            return;
        }
        _status = r.Value;
        RenderStatus();
    }

    private void RenderStatus()
    {
        var st = _status;
        var dl = Services.Current.UpdateDownload;
        bool busy = _checking || _applying || dl.IsDownloading || st?.State is "checking" or "downloading" or "applying";

        CurrentVersionText.Text = st is null
            ? ""
            : Loc.Format("UpdatePage_InstalledVersion", st.CurrentVersion);

        if (st?.LastCheck is { } lastCheck)
            LastCheckText.Text = Loc.Format("UpdatePage_LastChecked", lastCheck.ToLocalTime().ToString("g"));
        else
            LastCheckText.Text = Loc.Get("UpdatePage_NeverChecked");

        CheckButton.IsEnabled = !busy;

        var kind = UpdateLogic.Classify(st);
        switch (kind)
        {
            case UpdateStatusKind.Applying:
                StatusHeadlineText.Text = Loc.Get("UpdatePage_Installing");
                StatusDetailText.Visibility = Visibility.Collapsed;
                break;
            case UpdateStatusKind.Ready:
            case UpdateStatusKind.Available:
                StatusHeadlineText.Text = Loc.Format("UpdatePage_Available", st!.LatestVersion ?? "");
                if (st.InstallerBytes is { } bytes and > 0)
                {
                    StatusDetailText.Text = Loc.Format("UpdatePage_DownloadSize", UpdateLogic.FormatSize(bytes));
                    StatusDetailText.Visibility = Visibility.Visible;
                }
                else
                {
                    StatusDetailText.Visibility = Visibility.Collapsed;
                }
                break;
            case UpdateStatusKind.UpToDate:
                StatusHeadlineText.Text = Loc.Get("UpdatePage_UpToDate");
                StatusDetailText.Visibility = Visibility.Collapsed;
                break;
            case UpdateStatusKind.Error:
                StatusHeadlineText.Text = "";
                StatusDetailText.Visibility = Visibility.Collapsed;
                if (!string.IsNullOrWhiteSpace(st?.Error))
                    ShowResult(InfoBarSeverity.Error, st!.Error!);
                break;
            default:
                StatusHeadlineText.Text = "";
                StatusDetailText.Visibility = Visibility.Collapsed;
                break;
        }

        if (st is { CanInstall: false } && !string.IsNullOrWhiteSpace(st.Reason))
        {
            NoInstallReasonText.Text = st.Reason;
            NoInstallReasonText.Visibility = Visibility.Visible;
        }
        else
        {
            NoInstallReasonText.Visibility = Visibility.Collapsed;
        }

        bool showDownload = st is { Available: true } && kind != UpdateStatusKind.Applying;
        DownloadButton.Visibility = showDownload ? Visibility.Visible : Visibility.Collapsed;
        DownloadButton.IsEnabled = UpdateLogic.CanDownload(st, dl.IsDownloading, _applying) && !_checking;

        bool hasNotes = st is { Available: true } && UpdateLogic.IsAllowedNotesUrl(st.NotesUrl);
        NotesLink.Visibility = hasNotes ? Visibility.Visible : Visibility.Collapsed;

        _syncingToggle = true;
        AutoToggle.IsOn = st?.Auto ?? false;
        AutoToggle.IsEnabled = UpdateLogic.CanToggleAuto(st) && !_applying;
        _syncingToggle = false;
    }

    private void OnDownloadChanged()
    {
        var dl = Services.Current.UpdateDownload;
        bool finishedNow = _wasDownloading && !dl.IsDownloading;
        _wasDownloading = dl.IsDownloading;
        SyncDownloadState();
        RenderStatus();
        if (finishedNow && dl.FinalResult is { Phase: "done" })
        {
            _ = ApplyAfterDownloadAsync();
        }
        else if (finishedNow)
        {
            _ = LoadStatusAsync();
        }
    }

    private void SyncDownloadState()
    {
        var dl = Services.Current.UpdateDownload;
        ProgressPanel.Visibility = dl.IsDownloading ? Visibility.Visible : Visibility.Collapsed;
        if (dl.IsDownloading)
        {
            var p = dl.LastProgress;
            if (p is null || p.Phase == "checking")
            {
                DownloadProgressBar.IsIndeterminate = true;
                ProgressText.Text = Loc.Get("UpdatePage_Checking");
            }
            else if (p.Phase == "verifying")
            {
                DownloadProgressBar.IsIndeterminate = true;
                ProgressText.Text = Loc.Get("UpdatePage_Verifying");
            }
            else if (p.TotalBytes <= 0)
            {
                DownloadProgressBar.IsIndeterminate = true;
                ProgressText.Text = Loc.Get("UpdatePage_Checking");
            }
            else
            {
                DownloadProgressBar.IsIndeterminate = false;
                DownloadProgressBar.Maximum = p.TotalBytes;
                DownloadProgressBar.Value = Math.Min(p.DoneBytes, p.TotalBytes);
                ProgressText.Text = UpdateLogic.FormatProgress(p.DoneBytes, p.TotalBytes, dl.BytesPerSecond);
            }
        }

        if (!dl.IsDownloading && dl.FinalResult is { } final)
        {
            switch (final.Phase)
            {
                case "done":
                    break;
                case "cancelled":
                    ShowResult(InfoBarSeverity.Informational, Loc.Get("UpdatePage_Cancelled"));
                    break;
                case "offline":
                    ShowResult(InfoBarSeverity.Warning, Loc.Get("UpdatePage_Offline"));
                    break;
                default:
                    ShowResult(InfoBarSeverity.Error, Loc.Format("UpdatePage_Failed", final.Error ?? dl.ErrorMessage ?? ""));
                    break;
            }
        }
    }

    private async Task ApplyAfterDownloadAsync()
    {
        if (_applying) return;
        _applying = true;
        StatusHeadlineText.Text = Loc.Get("UpdatePage_Installing");
        RenderStatus();

        var r = await Services.Current.Api.UpdateApplyAsync();
        if (!r.Ok || r.Value is null)
        {
            _applying = false;
            ShowResult(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
            await LoadStatusAsync();
            return;
        }
        _status = r.Value;
        RenderStatus();
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
            Services.Current.UpdateDownload.ClearResult();
    }

    private async void Check_Click(object sender, RoutedEventArgs e)
    {
        if (_checking) return;
        _checking = true;
        ResultBar.IsOpen = false;
        CheckingRing.IsActive = true;
        RenderStatus();

        await using var client = Services.Current.CreateRunClient();
        var api = new AgentApi(client);
        var r = await api.UpdateCheckAsync();

        _checking = false;
        CheckingRing.IsActive = false;
        if (!r.Ok || r.Value is null)
        {
            ShowResult(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
            RenderStatus();
            return;
        }
        _status = r.Value;
        RenderStatus();
    }

    private async void Download_Click(object sender, RoutedEventArgs e)
    {
        var st = _status;
        if (st is null || !st.Available || !st.CanInstall || XamlRoot is null) return;

        var version = st.LatestVersion ?? "";
        var sizeText = st.InstallerBytes is { } b and > 0
            ? UpdateLogic.FormatSize(b)
            : Loc.Get("UpdatePage_SizeUnknown");

        var body = new StackPanel { Spacing = 6 };
        body.Children.Add(new TextBlock
        {
            Text = Loc.Get("UpdatePage_WarnSource"),
            TextWrapping = TextWrapping.Wrap,
        });
        body.Children.Add(new TextBlock
        {
            Text = Loc.Format("UpdatePage_WarnSize", sizeText),
            TextWrapping = TextWrapping.Wrap,
        });
        body.Children.Add(new TextBlock
        {
            Text = Loc.Get("UpdatePage_WarnVerify"),
            TextWrapping = TextWrapping.Wrap,
        });
        body.Children.Add(new TextBlock
        {
            Text = Loc.Get("UpdatePage_WarnRestart"),
            TextWrapping = TextWrapping.Wrap,
        });

        var dialog = WindowHelpers.Dialog(
            XamlRoot,
            Loc.Format("UpdatePage_WarnTitle", version),
            body,
            Loc.Get("UpdatePage_WarnPrimary"),
            Loc.Get("Common_Cancel"));
        dialog.DefaultButton = ContentDialogButton.Close;

        if (await dialog.ShowAsync() != ContentDialogResult.Primary) return;
        ResultBar.IsOpen = false;
        // Already downloaded (page was left during the download, or auto-update fetched it):
        // install directly; the agent re-verifies signature, size and hash before it runs.
        if (UpdateLogic.Classify(_status) == UpdateStatusKind.Ready
            && string.Equals(_status?.DownloadedVersion, version, StringComparison.Ordinal))
        {
            _ = ApplyAfterDownloadAsync();
            return;
        }
        _ = Services.Current.UpdateDownload.StartAsync();
    }

    private void Cancel_Click(object sender, RoutedEventArgs e) =>
        Services.Current.UpdateDownload.Cancel();

    private void Notes_Click(object sender, RoutedEventArgs e)
    {
        var url = _status?.NotesUrl;
        if (!UpdateLogic.IsAllowedNotesUrl(url)) return;
        try
        {
            Process.Start(new ProcessStartInfo(url!) { UseShellExecute = true });
        }
        catch (Exception ex)
        {
            ShowResult(InfoBarSeverity.Error, Loc.Format("UpdatePage_OpenNotesFailed", ex.Message));
        }
    }

    private async void AutoToggle_Toggled(object sender, RoutedEventArgs e)
    {
        if (_syncingToggle) return;
        bool next = AutoToggle.IsOn;
        var r = await Services.Current.Settings.SaveKeyAsync(Services.Current.Api, "update.auto", JsonValue.Create(next));
        if (!r.Ok)
        {
            ShowResult(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
            _syncingToggle = true;
            AutoToggle.IsOn = !next;
            _syncingToggle = false;
            return;
        }
        if (_status is not null)
            _status = _status with { Auto = next };
    }
}
