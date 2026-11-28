// SPDX-License-Identifier: AGPL-3.0-only
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
        // The mode comes from the agent's status (unlockNeeds), else from settings; either can arrive after the panel
        // is first shown. Only a different mode rebuilds the panel: a status push must not clear what the user typed (B6).
        Services.Current.SettingsLoaded += RefreshIfModeChanged;
        Services.Current.StatusChanged += RefreshIfModeChanged;
    }

    private UnlockMode? _shown;

    private void RefreshIfModeChanged()
    {
        if (Visibility == Visibility.Visible && Services.Current.UnlockMode != _shown) Refresh();
    }

    public event Action? UnlockedOk;
    public event Action? WipeRequested;

    /// <summary>Picks Hello, passphrase (security.keyProtection) or password (security.unlockMethod) and focuses the right control.</summary>
    public void Refresh()
    {
        var mode = Services.Current.UnlockMode;
        _shown = mode;
        bool pass = mode != UnlockMode.Hello;
        PassPanel.Visibility = pass ? Visibility.Visible : Visibility.Collapsed;
        HelloButton.Visibility = pass ? Visibility.Collapsed : Visibility.Visible;
        BodyText.Text = Loc.Get(mode switch { UnlockMode.Password => "Unlock_BodyPassword", UnlockMode.Passphrase => "Unlock_BodyPassphrase", _ => "Unlock_BodyHello" });
        HelloButton.Content = Loc.Get("Unlock_HelloButton");
        if (mode == UnlockMode.Hello) _ = ShowHelloMissingAsync();
        var hint = Loc.Get(mode == UnlockMode.Password ? "Unlock_PasswordHint" : "Unlock_PassphraseHint");
        Pass.PlaceholderText = hint;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(Pass, hint);
        LessSecureText.Text = Loc.Get("Unlock_PasswordLessSecure");
        LessSecureText.Visibility = mode == UnlockMode.Password ? Visibility.Visible : Visibility.Collapsed;
        ForgotPassphraseLink.Visibility = UnlockLogic.ShowForgotPassphrase(mode) ? Visibility.Visible : Visibility.Collapsed;
        ErrorBar.IsOpen = false;
        Pass.Password = "";
        if (pass) Pass.Focus(FocusState.Programmatic);
        else HelloButton.Focus(FocusState.Programmatic);
    }

    /// <summary>N8: on a PC without Windows Hello, say so instead of offering a Hello unlock that cannot work.</summary>
    private async Task ShowHelloMissingAsync()
    {
        var r = await Services.Current.Api.PasswordStatusAsync();
        if (!r.Ok || r.Value!.HelloAvailable || _shown != UnlockMode.Hello || Visibility != Visibility.Visible) return;
        BodyText.Text = Loc.Get("Unlock_BodyNoHello");
        HelloButton.Content = Loc.Get("Unlock_HelloRetry");
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
        if (UnlockLogic.AsksForPassword(r.Error) && !s.PasswordFallback)
        {
            // No Windows Hello on this PC; the unlock password stands in (B13): show its box under the message.
            s.PasswordFallback = true;
            Refresh();
        }
        ErrorBar.Message = Loc.Error(r.Kind, r.Error);
        ErrorBar.IsOpen = true;
        if (PassPanel.Visibility == Visibility.Visible) Pass.Focus(FocusState.Programmatic);
    }

    private void Forgot_Click(object sender, RoutedEventArgs e) => WipeRequested?.Invoke();

    private void SetBusy(bool busy)
    {
        Busy.IsActive = busy;
        HelloButton.IsEnabled = !busy;
        PassButton.IsEnabled = !busy;
        Pass.IsEnabled = !busy;
        ForgotPassphraseLink.IsEnabled = !busy;
    }
}
