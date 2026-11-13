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

/// <summary>Panic flush, purge by range or app (preview count, then confirm), purge everything (typed phrase, Hello, then confirm).</summary>
public sealed partial class DangerZonePage : Page, IDataPage
{
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
    }

    public void Reload() => ResultBar.IsOpen = false;

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
            if (await warn.ShowAsync() != ContentDialogResult.Primary) return;

            var picker = new FileSavePicker(XamlRoot.ContentIslandEnvironment.AppWindowId)
            {
                SuggestedFileName = "BetterRecall-export-" + from.ToString("yyyyMMdd-HHmm", CultureInfo.InvariantCulture),
                SuggestedStartLocation = PickerLocationId.DocumentsLibrary,
            };
            picker.FileTypeChoices.Add(Loc.Get("Danger_ExportFileType"), [".zip"]);
            var picked = await picker.PickSaveFileAsync();
            if (picked is null || string.IsNullOrEmpty(picked.Path)) return;
            var path = picked.Path;

            ExportBusy.IsActive = true;
            var r = await api.ExportAsync(request with { Path = path, DryRun = false });
            ExportBusy.IsActive = false;
            if (!r.Ok)
            {
                Show(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
                return;
            }
            Show(InfoBarSeverity.Success, Loc.Format("Danger_Exported", r.Value!.Count, path));
            if (File.Exists(path))
            {
                var open = new Button { Content = Loc.Get("Danger_ExportOpen") };
                open.Click += (_, _) => Process.Start(new ProcessStartInfo("explorer.exe", "/select,\"" + path + "\"") { UseShellExecute = true });
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
        var preview = await Services.Current.Api.PurgeRangeAsync(from, to);
        await ConfirmAsync(preview, Loc.Format("Danger_RangeConfirm", from.ToString("g"), to.ToString("g")));
    }

    private async void PurgeApp_Click(object sender, RoutedEventArgs e)
    {
        var app = AppName.Text.Trim();
        if (app.Length == 0) return;
        var preview = await Services.Current.Api.PurgeAppAsync(app);
        await ConfirmAsync(preview, Loc.Format("Danger_AppConfirm", app));
    }

    private async Task ConfirmAsync(AgentResult<PurgeConfirmReply> preview, string what)
    {
        if (!preview.Ok)
        {
            Show(InfoBarSeverity.Error, Loc.Error(preview.Kind, preview.Error));
            return;
        }
        var body = new TextBlock { TextWrapping = TextWrapping.Wrap, Text = what + "\n\n" + Loc.Format("Danger_RowsCount", preview.Value!.Rows ?? 0) };
        var dialog = WindowHelpers.Dialog(XamlRoot, Loc.Get("Danger_ConfirmTitle"), body, Loc.Get("Danger_ConfirmDelete"), danger: true);
        if (await dialog.ShowAsync() != ContentDialogResult.Primary) return;
        var r = await Services.Current.Api.PurgeConfirmAsync(preview.Value.Confirm);
        if (r.Ok) Show(InfoBarSeverity.Success, Loc.Format("Danger_Purged", r.Value));
        else Show(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
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
    }

    private void AllPhrase_TextChanged(object sender, TextChangedEventArgs e) =>
        AllGo.IsEnabled = PurgeFlow.PhraseMatches(AllPhrase.Text, Loc.Get("Danger_AllPhrase"));

    private async void AllGo_Click(object sender, RoutedEventArgs e)
    {
        if (!PurgeFlow.PhraseMatches(AllPhrase.Text, Loc.Get("Danger_AllPhrase"))) return;
        var s = Services.Current;
        string? pass = AllPass.Visibility == Visibility.Visible && AllPass.Password.Length > 0 ? AllPass.Password : null;
        AllPass.Password = "";
        AllGo.IsEnabled = false;
        AllBusy.IsActive = true;
        if (pass is null) s.AllowAgentForeground();
        var r = await PurgeFlow.PurgeAllAsync(s.Api, pass);
        pass = null;
        AllBusy.IsActive = false;
        AllCancel_Click(sender, e);
        if (r.Ok) Show(InfoBarSeverity.Success, Loc.Format("Danger_AllDone", r.Value));
        else Show(InfoBarSeverity.Error, Loc.Error(r.Kind, r.Error));
        await s.RefreshStatusAsync();
    }
}
