// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace BetterRecall.UI.Views;

/// <summary>Shown over a window while the store is locked. The passphrase is passed straight to the agent and never kept.</summary>
public sealed partial class UnlockPanel : UserControl
{
    public UnlockPanel()
    {
        InitializeComponent();
        // The mode comes from settings; they can arrive after the panel is first shown.
        Services.Current.SettingsLoaded += () => { if (Visibility == Visibility.Visible) Refresh(); };
    }

    public event Action? UnlockedOk;

    /// <summary>Picks Hello or passphrase mode from security.keyProtection and focuses the right control.</summary>
    public void Refresh()
    {
        bool pass = UnlockLogic.UsesPassphrase(Services.Current.KeyProtection);
        PassPanel.Visibility = pass ? Visibility.Visible : Visibility.Collapsed;
        HelloButton.Visibility = pass ? Visibility.Collapsed : Visibility.Visible;
        BodyText.Text = Loc.Get(pass ? "Unlock_BodyPassphrase" : "Unlock_BodyHello");
        ErrorBar.IsOpen = false;
        Pass.Password = "";
        if (pass) Pass.Focus(FocusState.Programmatic);
        else HelloButton.Focus(FocusState.Programmatic);
    }

    private async void Hello_Click(object sender, RoutedEventArgs e) => await UnlockAsync(null);

    private async void Pass_Click(object sender, RoutedEventArgs e) => await UnlockFromBoxAsync();

    private async void Pass_KeyDown(object sender, Microsoft.UI.Xaml.Input.KeyRoutedEventArgs e)
    {
        if (e.Key != Windows.System.VirtualKey.Enter) return;
        e.Handled = true;
        await UnlockFromBoxAsync();
    }

    private async Task UnlockFromBoxAsync()
    {
        if (string.IsNullOrEmpty(Pass.Password)) return;
        var passphrase = Pass.Password;
        Pass.Password = "";
        await UnlockAsync(passphrase);
    }

    private async Task UnlockAsync(string? passphrase)
    {
        var s = Services.Current;
        SetBusy(true);
        if (passphrase is null) s.AllowAgentForeground();
        var r = await s.Api.UnlockAsync(passphrase);
        passphrase = null;
        SetBusy(false);
        if (r.Ok && r.Value!.Unlocked)
        {
            s.MarkUnlocked();
            UnlockedOk?.Invoke();
            return;
        }
        ErrorBar.Message = Loc.Error(r.Kind, r.Error);
        ErrorBar.IsOpen = true;
        if (PassPanel.Visibility == Visibility.Visible) Pass.Focus(FocusState.Programmatic);
    }

    private void SetBusy(bool busy)
    {
        Busy.IsActive = busy;
        HelloButton.IsEnabled = !busy;
        PassButton.IsEnabled = !busy;
        Pass.IsEnabled = !busy;
    }
}
