// SPDX-License-Identifier: AGPL-3.0-only
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
        EmptyText.Visibility = Visibility.Collapsed;
        if (!r.Ok)
        {
            ErrorBar.Message = Loc.Error(r.Kind, r.Error);
            List.ItemsSource = null;
            return;
        }
        List.ItemsSource = r.Value!.Select(x => new AuditRow(x)).ToList();
        EmptyText.Visibility = r.Value!.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    private void Refresh_Click(object sender, RoutedEventArgs e) => Reload();

    private bool _narrow;

    /// <summary>Narrow page: Results and PID columns go, so the Client column does not clip (D14).</summary>
    private void Root_SizeChanged(object sender, SizeChangedEventArgs e)
    {
        bool narrow = BetterRecall.UI.Logic.AuditLayout.Narrow(e.NewSize.Width);
        if (narrow == _narrow) return;
        _narrow = narrow;
        ApplyColumns(HeaderRow);
        for (int i = 0; i < List.Items.Count; i++)
            if (List.ContainerFromIndex(i) is ListViewItem { ContentTemplateRoot: Grid g }) ApplyColumns(g);
    }

    private void List_ContainerContentChanging(ListViewBase sender, ContainerContentChangingEventArgs args)
    {
        if (args.ItemContainer.ContentTemplateRoot is Grid g) ApplyColumns(g);
    }

    private void ApplyColumns(Grid g)
    {
        var widths = BetterRecall.UI.Logic.AuditLayout.FixedWidths(_narrow);
        for (int i = 0; i < widths.Length && i < g.ColumnDefinitions.Count; i++) g.ColumnDefinitions[i].Width = new GridLength(widths[i]);
        foreach (var child in g.Children)
            if (child is FrameworkElement f && Grid.GetColumn(f) is 3 or 4) f.Visibility = _narrow ? Visibility.Collapsed : Visibility.Visible;
    }
}
