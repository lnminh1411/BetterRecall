// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;

namespace BetterRecall.UI.Views;

/// <summary>
/// Settings cards for secrets that never go into settings.json: the unlock password (security group) and the cloud
/// describe API key (ai group). Values go straight from a PasswordBox to the agent and are cleared at once.
/// </summary>
internal static class SecretCards
{
    public static Border PasswordCard(FrameworkElement owner)
    {
        var panel = new StackPanel { Spacing = 8 };
        panel.Children.Add(new TextBlock { Text = Loc.Get("Pw_Title"), TextWrapping = TextWrapping.Wrap });
        panel.Children.Add(new TextBlock { Text = Loc.Get("Pw_Desc"), TextWrapping = TextWrapping.Wrap, Style = Caption() });
        var state = new TextBlock { TextWrapping = TextWrapping.Wrap, Style = Caption() };
        AutomationProperties.SetLiveSetting(state, Microsoft.UI.Xaml.Automation.Peers.AutomationLiveSetting.Polite);
        panel.Children.Add(state);
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        var set = new Button();
        var remove = new Button { Content = Loc.Get("Pw_Remove") };
        row.Children.Add(set);
        row.Children.Add(remove);
        panel.Children.Add(row);
        bool isSet = false;

        async Task RefreshAsync()
        {
            var r = await Services.Current.Api.PasswordStatusAsync();
            isSet = r.Ok && r.Value!.Set;
            set.Content = Loc.Get(isSet ? "Pw_Change" : "Pw_Set");
            remove.Visibility = isSet ? Visibility.Visible : Visibility.Collapsed;
            state.Text = !r.Ok ? Loc.Error(r.Kind, r.Error)
                : Loc.Get(isSet ? "Pw_StateSet" : "Pw_StateNone") + (r.Value!.HelloAvailable ? "" : " " + Loc.Get("Pw_NoHello"))
                  + (r.Value.RetryAfterS > 0 ? " " + Loc.Format("Err_TooManyTries", r.Value.RetryAfterS) : "");
        }

        set.Click += async (_, _) =>
        {
            var current = new PasswordBox { Header = Loc.Get("Pw_Current"), Visibility = isSet ? Visibility.Visible : Visibility.Collapsed };
            var next = new PasswordBox { Header = Loc.Get("Pw_New") };
            var again = new PasswordBox { Header = Loc.Get("Pw_Again") };
            var error = new TextBlock { TextWrapping = TextWrapping.Wrap, Visibility = Visibility.Collapsed, Style = (Style)Application.Current.Resources["CaptionCriticalTextStyle"] };
            var body = new StackPanel { Spacing = 8, MinWidth = 320 };
            body.Children.Add(new TextBlock { Text = Loc.Get("Pw_DialogBody"), TextWrapping = TextWrapping.Wrap });
            body.Children.Add(current);
            body.Children.Add(next);
            body.Children.Add(again);
            body.Children.Add(error);
            var dialog = WindowHelpers.Dialog(owner.XamlRoot, Loc.Get(isSet ? "Pw_Change" : "Pw_Set"), body, Loc.Get("AiKey_Save"));
            dialog.PrimaryButtonClick += async (d, args) =>
            {
                var deferral = args.GetDeferral();
                try
                {
                    if (next.Password != again.Password) { args.Cancel = true; Show(error, Loc.Get("Pw_Mismatch")); return; }
                    if (next.Password.Length < 6) { args.Cancel = true; Show(error, Loc.Get("Err_PasswordRule")); return; }
                    var r = await Services.Current.Api.PasswordSetAsync(isSet ? current.Password : null, next.Password);
                    if (!r.Ok) { args.Cancel = true; Show(error, Loc.Error(r.Kind, r.Error)); }
                }
                finally
                {
                    current.Password = next.Password = again.Password = "";
                    deferral.Complete();
                }
            };
            await dialog.ShowAsync();
            await RefreshAsync();
        };

        remove.Click += async (_, _) =>
        {
            var current = new PasswordBox { Header = Loc.Get("Pw_Current") };
            var error = new TextBlock { TextWrapping = TextWrapping.Wrap, Visibility = Visibility.Collapsed, Style = (Style)Application.Current.Resources["CaptionCriticalTextStyle"] };
            var body = new StackPanel { Spacing = 8, MinWidth = 320 };
            body.Children.Add(new TextBlock { Text = Loc.Get("Pw_RemoveBody"), TextWrapping = TextWrapping.Wrap });
            body.Children.Add(current);
            body.Children.Add(error);
            var dialog = WindowHelpers.Dialog(owner.XamlRoot, Loc.Get("Pw_Remove"), body, Loc.Get("Pw_Remove"), danger: true);
            dialog.PrimaryButtonClick += async (d, args) =>
            {
                var deferral = args.GetDeferral();
                try
                {
                    var r = await Services.Current.Api.PasswordClearAsync(current.Password);
                    if (!r.Ok) { args.Cancel = true; Show(error, Loc.Error(r.Kind, r.Error)); }
                }
                finally
                {
                    current.Password = "";
                    deferral.Complete();
                }
            };
            await dialog.ShowAsync();
            await RefreshAsync();
        };

        _ = RefreshAsync();
        return new Border { Style = (Style)Application.Current.Resources["CardStyle"], Child = panel };
    }

    public static Border ApiKeyCard(FrameworkElement owner)
    {
        var panel = new StackPanel { Spacing = 8 };
        panel.Children.Add(new TextBlock { Text = Loc.Get("AiKey_Title"), TextWrapping = TextWrapping.Wrap });
        panel.Children.Add(new TextBlock { Text = Loc.Get("AiKey_Desc"), TextWrapping = TextWrapping.Wrap, Style = Caption() });
        var state = new TextBlock { TextWrapping = TextWrapping.Wrap, Style = Caption() };
        AutomationProperties.SetLiveSetting(state, Microsoft.UI.Xaml.Automation.Peers.AutomationLiveSetting.Polite);
        panel.Children.Add(state);
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        var box = new PasswordBox { PlaceholderText = Loc.Get("AiKey_Placeholder"), MinWidth = 280 };
        AutomationProperties.SetName(box, Loc.Get("AiKey_Title"));
        var save = new Button { Content = Loc.Get("AiKey_Save") };
        var remove = new Button { Content = Loc.Get("AiKey_Remove") };
        row.Children.Add(box);
        row.Children.Add(save);
        row.Children.Add(remove);
        panel.Children.Add(row);

        void Render(AgentResult<BetterRecall.Contracts.AiStatus> r)
        {
            bool set = r.Ok && r.Value!.KeySet;
            remove.Visibility = set ? Visibility.Visible : Visibility.Collapsed;
            state.Text = !r.Ok ? Loc.Error(r.Kind, r.Error)
                : Loc.Format(set ? "AiKey_StateSet" : "AiKey_StateNone", r.Value!.Provider, r.Value.Model, r.Value.Endpoint);
        }

        save.Click += async (_, _) =>
        {
            if (box.Password.Length == 0) return;
            var key = box.Password;
            box.Password = "";
            var r = await Services.Current.Api.AiSetKeyAsync(key);
            key = null;
            Render(r);
        };
        remove.Click += async (_, _) => Render(await Services.Current.Api.AiClearKeyAsync());
        async Task LoadAsync() => Render(await Services.Current.Api.AiStatusAsync());
        _ = LoadAsync();
        return new Border { Style = (Style)Application.Current.Resources["CardStyle"], Child = panel };
    }

    private static Style Caption() => (Style)Application.Current.Resources["CaptionSecondaryTextStyle"];

    private static void Show(TextBlock error, string text)
    {
        error.Text = text;
        error.Visibility = Visibility.Visible;
    }
}
