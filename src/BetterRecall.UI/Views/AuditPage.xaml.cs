// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Globalization;
using BetterRecall.Contracts;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace BetterRecall.UI.Views;

public sealed class AuditRow(AuditEntry e)
{
    public AuditEntry Entry { get; } = e;
    public string Time => Entry.Ts.ToLocalTime().ToString("G", CultureInfo.CurrentCulture);
    public string Count => Entry.ResultCount.ToString(CultureInfo.CurrentCulture);
    public string Pid => Entry.ClientPid.ToString(CultureInfo.InvariantCulture);
    /// <summary>The audit keeps only a hash of query text; show its start so repeated queries can be matched.</summary>
    public string QueryHash => Entry.QuerySha256.Length == 0 ? "" : Loc.Format("Audit_QueryHash", Entry.QuerySha256[..Math.Min(16, Entry.QuerySha256.Length)]);
    public string AccessibleName => string.Join(", ", Time, Entry.Pipe, Entry.Verb, Count, Entry.ClientImage);
}

public sealed partial class AuditPage : Page, IDataPage
{
    public AuditPage()
    {
        InitializeComponent();
        Loaded += (_, _) => Reload();
    }

    public void Reload() => _ = LoadAsync();

    private async Task LoadAsync()
    {
        Busy.IsActive = true;
        var r = await Services.Current.Api.AuditAsync(500);
        Busy.IsActive = false;
        ErrorBar.IsOpen = !r.Ok;
        if (!r.Ok)
        {
            ErrorBar.Message = Loc.Error(r.Kind, r.Error);
            List.ItemsSource = null;
            return;
        }
        List.ItemsSource = r.Value!.Select(x => new AuditRow(x)).ToList();
    }

    private void Refresh_Click(object sender, RoutedEventArgs e) => Reload();
}
