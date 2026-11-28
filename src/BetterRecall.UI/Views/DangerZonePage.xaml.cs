// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Globalization;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.Windows.Storage.Pickers;

namespace BetterRecall.UI.Views;

/// <summary>Panic flush, purge by range or app (preview count, then confirm), purge everything (typed phrase, Hello, then confirm).</summary>
public sealed partial class DangerZonePage : Page, IDataPage
{
    private bool _wipeOnly;
    private CancellationTokenSource? _exportCts;

    public event Action? WipeCancelled;

    public DangerZonePage()
    {
        InitializeComponent();
        PanicButton.Style = WindowHelpers.DangerButtonStyle();
        AllGo.Style = WindowHelpers.DangerButtonStyle();
        var now = DateTimeOffset.Now;
        FromDate.Date = now.Date;
        ToDate.Date = now.Date;
        FromTime.Time = TimeSpan.Zero;
        ToTime.Time = new TimeSpan(now.Hour, now.Minute, 0);
        ExportFromDate.Date = now.Date;
        ExportToDate.Date = now.Date;
        ExportFromTime.Time = TimeSpan.Zero;
        ExportToTime.Time = new TimeSpan(now.Hour, now.Minute, 0);
        AllPrompt.Text = Loc.Format("Danger_AllPrompt", Loc.Get("Danger_AllPhrase"));
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(AllPhrase, AllPrompt.Text);
        Loaded += (_, _) =>
        {
            Services.Current.LockedChanged -= OnLockedChanged;
            Services.Current.LockedChanged += OnLockedChanged;
            ApplyWipeOnly();
        };
        Unloaded += (_, _) =>
        {
            Services.Current.LockedChanged -= OnLockedChanged;
            CancelExport();
        };
    }

    protected override void OnNavigatedTo(Microsoft.UI.Xaml.Navigation.NavigationEventArgs e)
    {
        _wipeOnly = string.Equals(e.Parameter as string, "wipe", StringComparison.OrdinalIgnoreCase) && Services.Current.Locked;
        ApplyWipeOnly();
    }

    private void OnLockedChanged()
    {
        if (Services.Current.Locked) CancelExport();
        else if (_wipeOnly) { _wipeOnly = false; ApplyWipeOnly(); }
    }

    private void CancelExport()
    {
        try { _exportCts?.Cancel(); } catch (ObjectDisposedException) { }
    }

    private void ApplyWipeOnly()
    {
        bool wipe = _wipeOnly && Services.Current.Locked;
        var vis = wipe ? Visibility.Collapsed : Visibility.Visible;
        PanicCard.Visibility = vis;
        ExportCard.Visibility = vis;
        RangeCard.Visibility = vis;
        AppCard.Visibility = vis;
        if (wipe)
        {
            AllStart.Visibility = Visibility.Collapsed;
            AllConfirm.Visibility = Visibility.Visible;
            AllPass.Visibility = Visibility.Collapsed;
            AllPass.Password = "";
            AllPhrase.Text = "";
            AllGo.IsEnabled = false;
            AllPhrase.Focus(FocusState.Programmatic);
        }
    }

    public void Reload()
    {
        ResultBar.IsOpen = false;
        ApplyWipeOnly();
    }

    /// <summary>Narrow windows: the date and time pickers (242 px each) stack instead of overflowing the card.</summary>
    private void Root_SizeChanged(object sender, SizeChangedEventArgs e)
    {
        var o = e.NewSize.Width < 620 ? Orientation.Vertical : Orientation.Horizontal;
        foreach (var row in new[] { ExportFromRow, ExportToRow, FromRow, ToRow, AppRow }) row.Orientation = o;
    }

    private void Show(InfoBarSeverity sev, string message)
    {
        ResultBar.Severity = sev;
        ResultBar.Message = message;
        ResultBar.ActionButton = null;
        ResultBar.IsOpen = true;
    }

    /// <summary>Export: count first, warn that the zip is not encrypted, pick the file, then write it.</summary>
    private async void Export_Click(object sender, RoutedEventArgs e)
    {
        if (ExportFromDate.Date is not { } fd || ExportToDate.Date is not { } td) return;
        var from = Logic.LocalTime.At(fd.Date + ExportFromTime.Time);
        var to = Logic.LocalTime.At(td.Date + ExportToTime.Time);
        if (from >= to)
        {
            Show(InfoBarSeverity.Error, Loc.Get("Danger_RangeInvalid"));
            return;
        }
        var app = ExportApp.Text.Trim();
        var request = new ExportRequest(from, to, "", app.Length == 0 ? null : app, ExportJpeg.IsChecked == true, DryRun: true);
        var api = Services.Current.Api;
        ExportButton.IsEnabled = false;
        try
        {
            var count = await api.ExportAsync(request);
            if (!count.Ok)
            {
                Show(InfoBarSeverity.Error, Loc.Error(count.Kind, count.Error));
                return;
            }
            if (count.Value!.Count == 0)
            {
                Show(InfoBarSeverity.Informational, Loc.Get("Danger_ExportNone"));
                return;
            }
            var warn = WindowHelpers.Dialog(XamlRoot, Loc.Get("Danger_ExportWarnTitle"),
                new TextBlock { Text = Loc.Format("Danger_ExportWarnBody", count.Value.Count, count.Value.Images), TextWrapping = TextWrapping.Wrap },
                Loc.Get("Danger_ExportContinue"));
            if (await warn.ShowAsync() != ContentDialogResult.Primary || Services.Current.Locked) return;

            var picker = new FileSavePicker(XamlRoot.ContentIslandEnvironment.AppWindowId)
            {
                SuggestedFileName = "BetterRecall-export-" + from.ToString("yyyyMMdd-HHmm", CultureInfo.InvariantCulture),
                SuggestedStartLocation = PickerLocationId.DocumentsLibrary,
            };
            picker.FileTypeChoices.Add(Loc.Get("Danger_ExportFileType"), [".zip"]);
            var picked = await picker.PickSaveFileAsync();
            if (picked is null || string.IsNullOrEmpty(picked.Path) || Services.Current.Locked) return;
            var path = picked.Path;

            // Own connection: other pages keep working during a long export, and Cancel (or lock) stops it on the agent.
            ExportBusy.IsActive = true;
            using var cancel = new CancellationTokenSource();
            _exportCts = cancel;
            Show(InfoBarSeverity.Informational, Loc.Get("Danger_Exporting"));
            var stop = new Button { Content = Loc.Get("Common_Cancel") };
            stop.Click += (_, _) => { try { cancel.Cancel(); } catch (ObjectDisposedException) { } };
            ResultBar.ActionButton = stop;
            AgentResult<ExportResult> r;
            try
            {
                r = await OwnConnection.RunAsync(Services.Current.CreateRunClient,
                    (own, ct) => own.ExportAsync(request with { Path = path, DryRun = false }, ct), cancel.Token);
            }
            catch (OperationCanceledException)
            {
                Show(InfoBarSeverity.Informational, Loc.Get("Danger_ExportCancelled"));
                return;
            }
            finally
            {
                if (ReferenceEquals(_exportCts, cancel)) _exportCts = null;
            }
            ExportBusy.IsActive = false;
            if (!r.Ok)
            {
                Show(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
                return;
            }
            if (Services.Current.Locked) return;
            Show(InfoBarSeverity.Success, Loc.Format("Danger_Exported", r.Value!.Count, path));
            if (File.Exists(path))
            {
                var open = new Button { Content = Loc.Get("Danger_ExportOpen") };
                open.Click += (_, _) => { if (!Services.Current.Locked) Process.Start(new ProcessStartInfo("explorer.exe", "/select,\"" + path + "\"") { UseShellExecute = true }); };
                ResultBar.ActionButton = open;
            }
        }
        finally
        {
            ExportBusy.IsActive = false;
            ExportButton.IsEnabled = true;
        }
    }

    private async void Panic_Click(object sender, RoutedEventArgs e)
    {
        var r = await Services.Current.Api.PanicAsync();
        if (r.Ok) Show(InfoBarSeverity.Success, Loc.Get("Danger_PanicDone"));
        else Show(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
    }

    private async void PurgeRange_Click(object sender, RoutedEventArgs e)
    {
        if (FromDate.Date is not { } fd || ToDate.Date is not { } td) return;
        var from = Logic.LocalTime.At(fd.Date + FromTime.Time);
        var to = Logic.LocalTime.At(td.Date + ToTime.Time);
        if (from >= to)
        {
            Show(InfoBarSeverity.Error, Loc.Get("Danger_RangeInvalid"));
            return;
        }
        await ConfirmAsync(() => Services.Current.Api.PurgeRangeAsync(from, to), Loc.Format("Danger_RangeConfirm", from.ToString("g"), to.ToString("g")));
    }

    private async void PurgeApp_Click(object sender, RoutedEventArgs e)
    {
        var app = AppName.Text.Trim();
        if (app.Length == 0) return;
        await ConfirmAsync(() => Services.Current.Api.PurgeAppAsync(app), Loc.Format("Danger_AppConfirm", app));
    }

    /// <summary>Count, confirm, then purge with a token asked after the dialog closed: tokens expire after 30 s and the
    /// dialog can stay open longer. If more captures match by then, the new count is confirmed first.</summary>
    private async Task ConfirmAsync(Func<Task<AgentResult<PurgeConfirmReply>>> ask, string what)
    {
        var r = await PurgeFlow.ConfirmFreshAsync(Services.Current.Api, ask, async rows =>
        {
            var body = new TextBlock { TextWrapping = TextWrapping.Wrap, Text = what + "\n\n" + Loc.Format("Danger_RowsCount", rows) };
            var dialog = WindowHelpers.Dialog(XamlRoot, Loc.Get("Danger_ConfirmTitle"), body, Loc.Get("Danger_ConfirmDelete"), danger: true);
            return await dialog.ShowAsync() == ContentDialogResult.Primary;
        });
        if (r is null) return;   // cancelled
        if (r.Value.Ok) Show(InfoBarSeverity.Success, Loc.Format("Danger_Purged", r.Value.Value));
        else Show(InfoBarSeverity.Error, Loc.Error(r.Value.Kind, r.Value.Error));
        await Services.Current.RefreshStatusAsync();
    }

    private void AllStart_Click(object sender, RoutedEventArgs e)
    {
        AllStart.Visibility = Visibility.Collapsed;
        AllConfirm.Visibility = Visibility.Visible;
        var mode = Services.Current.UnlockMode;
        AllPass.Visibility = mode != UnlockMode.Hello ? Visibility.Visible : Visibility.Collapsed;
        AllPass.PlaceholderText = Loc.Get(mode == UnlockMode.Password ? "Unlock_PasswordHint" : "Unlock_PassphraseHint");
        AllPhrase.Text = "";
        AllPhrase.Focus(FocusState.Programmatic);
    }

    private void AllCancel_Click(object sender, RoutedEventArgs e)
    {
        AllConfirm.Visibility = Visibility.Collapsed;
        AllStart.Visibility = Visibility.Visible;
        AllPhrase.Text = "";
        AllPass.Password = "";
        if (_wipeOnly)
        {
            _wipeOnly = false;
            ApplyWipeOnly();
            WipeCancelled?.Invoke();
        }
    }

    private void AllPhrase_TextChanged(object sender, TextChangedEventArgs e) =>
        AllGo.IsEnabled = PurgeFlow.PhraseMatches(AllPhrase.Text, Loc.Get("Danger_AllPhrase"));

    private async void AllGo_Click(object sender, RoutedEventArgs e)
    {
        if (!PurgeFlow.PhraseMatches(AllPhrase.Text, Loc.Get("Danger_AllPhrase"))) return;
        var s = Services.Current;
        string? pass = !_wipeOnly && AllPass.Visibility == Visibility.Visible && AllPass.Password.Length > 0 ? AllPass.Password : null;
        AllPass.Password = "";
        AllGo.IsEnabled = false;
        AllBusy.IsActive = true;
        if (pass is null) s.AllowAgentForeground();
        var r = await PurgeFlow.PurgeAllAsync(s.Api, pass);
        pass = null;
        AllBusy.IsActive = false;
        if (!r.Ok)
        {
            Show(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
            if (_wipeOnly)
            {
                AllGo.IsEnabled = PurgeFlow.PhraseMatches(AllPhrase.Text, Loc.Get("Danger_AllPhrase"));
                return;
            }
            AllCancel_Click(sender, e);
            await s.RefreshStatusAsync();
            return;
        }
        _wipeOnly = false;
        ApplyWipeOnly();
        AllCancel_Click(sender, e);
        Show(r.Value!.Undeleted is > 0 ? InfoBarSeverity.Warning : InfoBarSeverity.Success, string.Join(" ", PurgeFlow.AllDoneText(r.Value).Select(Loc.Text)));
        await s.RefreshStatusAsync();
        s.NotifyPurgeAll(r.Value);
    }
}
