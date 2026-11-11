// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace BetterRecall.UI.Views;

/// <summary>First run: what is captured, where it is stored, and the same-user-malware limit. Nothing is captured before "consent".</summary>
public sealed partial class ConsentPage : Page
{
    public ConsentPage()
    {
        InitializeComponent();
        Loaded += (_, _) => AgreeButton.Focus(FocusState.Programmatic);
    }

    private async void Agree_Click(object sender, RoutedEventArgs e)
    {
        var s = Services.Current;
        AgreeButton.IsEnabled = false;
        Busy.IsActive = true;
        var r = await s.Api.ConsentAsync();
        Busy.IsActive = false;
        AgreeButton.IsEnabled = true;
        if (!r.Ok)
        {
            ErrorBar.Message = Loc.Error(r.Kind, r.Error);
            ErrorBar.IsOpen = true;
            return;
        }
        await s.RefreshSettingsAsync();
        await s.RefreshStatusAsync();
        ConsentButtons.Visibility = Visibility.Collapsed;
        BenchOffer.Visibility = Visibility.Visible;
        BenchOffer.UpdateLayout();   // measure the card before scrolling to it
        BenchOffer.StartBringIntoView();
        BenchRunButton.Focus(FocusState.Programmatic);
    }

    private void BenchRun_Click(object sender, RoutedEventArgs e) => App.Current.ShowMain("bench:run");

    private void BenchSkip_Click(object sender, RoutedEventArgs e) => App.Current.ShowMain("home");

    /// <summary>Hides the window; capture stays off until consent.</summary>
    private void NotNow_Click(object sender, RoutedEventArgs e) =>
        Microsoft.UI.Windowing.AppWindow.GetFromWindowId(XamlRoot.ContentIslandEnvironment.AppWindowId)?.Hide();
}
