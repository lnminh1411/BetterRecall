// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Windows.System;
using Windows.UI.Core;

namespace BetterRecall.UI.Views;

/// <summary>
/// Global shortcuts. Each action shows its keys, Change and Remove. Change opens a dialog that records the next key
/// combination and checks it before saving: it needs Ctrl, Alt or Win, no other BetterRecall action may use it, and
/// no other app may hold it (a RegisterHotKey probe). Rows whose combination the agent could not register (status
/// HotkeysFailed) carry a warning. Each change saves its one key at once.
/// </summary>
public sealed partial class ShortcutsPage : Page
{
    public ShortcutsPage()
    {
        InitializeComponent();
        Loaded += (_, _) =>
        {
            var s = Services.Current;
            s.StatusChanged += Build;
            s.ConnectionChanged += Build;
            s.Settings.Changed += Build;
            Build();
        };
        Unloaded += (_, _) =>
        {
            var s = Services.Current;
            s.StatusChanged -= Build;
            s.ConnectionChanged -= Build;
            s.Settings.Changed -= Build;
        };
    }

    private static Style St(string key) => (Style)Application.Current.Resources[key];

    private static string Label(string action) => Loc.Get("S_hotkeys_" + action);

    private void Build()
    {
        var s = Services.Current;
        bool usable = s.Settings.Loaded && s.Connection == AgentErrorKind.None;
        var failed = s.Status?.HotkeysFailed ?? [];
        Rows.Children.Clear();
        for (int i = 0; i < ShortcutRules.Actions.Length; i++)
        {
            var action = ShortcutRules.Actions[i];
            var keys = ShortcutRules.Get(s.Settings.Original, action);
            var row = new Grid { MinHeight = 56, ColumnSpacing = 12, Padding = new Thickness(0, 8, 0, 8) };
            if (i > 0) row.BorderThickness = new Thickness(0, 1, 0, 0);
            row.BorderBrush = (Microsoft.UI.Xaml.Media.Brush)Application.Current.Resources["CardStrokeColorDefaultBrush"];
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });

            var text = new StackPanel { Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
            text.Children.Add(new TextBlock { Text = Label(action), TextWrapping = TextWrapping.Wrap });
            text.Children.Add(new TextBlock { Text = Loc.Get("Shortcut_" + action + "_D"), Style = St("CaptionSecondaryTextStyle"), TextWrapping = TextWrapping.Wrap });
            if (keys.Length > 0 && failed.Contains(action))
            {
                var warn = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6, Margin = new Thickness(0, 2, 0, 0) };
                warn.Children.Add(new FontIcon
                {
                    Glyph = "\uE7BA", FontSize = 14,
                    Foreground = (Microsoft.UI.Xaml.Media.Brush)Application.Current.Resources["SystemFillColorCautionBrush"],
                });
                warn.Children.Add(new TextBlock { Text = Loc.Get("Shortcuts_Failed"), Style = St("CaptionSecondaryTextStyle"), TextWrapping = TextWrapping.Wrap });
                text.Children.Add(warn);
            }
            row.Children.Add(text);

            var chips = Chips(keys);
            Grid.SetColumn(chips, 1);
            row.Children.Add(chips);

            var change = new Button { Content = Loc.Get("Shortcuts_Change"), IsEnabled = usable, VerticalAlignment = VerticalAlignment.Center };
            AutomationProperties.SetName(change, Loc.Format("Shortcuts_ChangeName", Label(action)));
            change.Click += async (_, _) => await ChangeAsync(action);
            Grid.SetColumn(change, 2);
            row.Children.Add(change);

            var remove = new Button
            {
                Content = new FontIcon { Glyph = "\uE74D", FontSize = 14 },
                IsEnabled = usable && keys.Length > 0,
                VerticalAlignment = VerticalAlignment.Center,
            };
            var removeName = Loc.Format("Shortcuts_RemoveName", Label(action));
            AutomationProperties.SetName(remove, removeName);
            ToolTipService.SetToolTip(remove, removeName);
            remove.Click += async (_, _) => await SaveAsync(action, "");
            Grid.SetColumn(remove, 3);
            row.Children.Add(remove);

            Rows.Children.Add(row);
        }
    }

    internal static StackPanel Chips(string keys)
    {
        var chips = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4, VerticalAlignment = VerticalAlignment.Center };
        if (keys.Length == 0)
            chips.Children.Add(new TextBlock { Text = Loc.Get("Home_NotSet"), Style = St("SecondaryTextStyle") });
        else
            foreach (var part in keys.Split('+'))
                chips.Children.Add(new Border
                {
                    Style = St("KeyChipStyle"),
                    Child = new TextBlock { Text = part, Style = St("CaptionTextBlockStyle") },
                });
        AutomationProperties.SetName(chips, keys.Length == 0 ? Loc.Get("Home_NotSet") : keys);
        return chips;
    }

    private async Task ChangeAsync(string action)
    {
        var s = Services.Current;
        var current = ShortcutRules.Get(s.Settings.Original, action);
        string? pending = null;

        var preview = new ContentControl { MinHeight = 36, HorizontalContentAlignment = HorizontalAlignment.Center, Content = Chips(current) };
        var box = new Button
        {
            HorizontalAlignment = HorizontalAlignment.Stretch,
            MinHeight = 64,
            Content = preview,
        };
        AutomationProperties.SetName(box, Loc.Get("Shortcuts_DialogHint"));
        var message = new TextBlock { Text = Loc.Get("Shortcuts_DialogHint"), Style = St("CaptionSecondaryTextStyle"), TextWrapping = TextWrapping.Wrap };
        AutomationProperties.SetLiveSetting(message, Microsoft.UI.Xaml.Automation.Peers.AutomationLiveSetting.Polite);
        var panel = new StackPanel { Spacing = 12, MinWidth = 360 };
        panel.Children.Add(box);
        panel.Children.Add(message);

        var dialog = WindowHelpers.Dialog(XamlRoot, Loc.Format("Shortcuts_DialogTitle", Label(action)), panel, Loc.Get("Shortcuts_Save"));
        dialog.IsPrimaryButtonEnabled = false;

        box.PreviewKeyDown += (_, e) =>
        {
            bool ctrl = Down(VirtualKey.Control), alt = Down(VirtualKey.Menu), shift = Down(VirtualKey.Shift);
            bool win = Down(VirtualKey.LeftWindows) || Down(VirtualKey.RightWindows);
            // Tab, Esc and Enter keep their dialog meaning unless a combination modifier is down.
            if (e.Key is (VirtualKey.Tab or VirtualKey.Escape or VirtualKey.Enter) && !(ctrl || alt || win)) return;
            e.Handled = true;
            var hk = HotkeyFormat.FromKeyPress((int)e.Key, ctrl, alt, shift, win);
            if (hk is null)
            {
                if (!IsModifier(e.Key)) message.Text = Loc.Get("Hotkey_NeedModifier");
                return;
            }
            pending = hk;
            preview.Content = Chips(hk);
            var (ok, why) = Check(action, current, hk);
            message.Text = why;
            dialog.IsPrimaryButtonEnabled = ok;
        };
        box.Loaded += (_, _) => box.Focus(FocusState.Programmatic);

        if (await dialog.ShowAsync() != ContentDialogResult.Primary || pending is null || pending == current) return;
        await SaveAsync(action, pending);
    }

    /// <summary>Whether the combination may be saved for this action, and the line shown under it.</summary>
    private static (bool Ok, string Message) Check(string action, string current, string combo)
    {
        if (string.Equals(combo, current, StringComparison.OrdinalIgnoreCase)) return (false, Loc.Get("Shortcuts_Current"));
        if (ShortcutRules.UsedBy(Services.Current.Settings.Original, action, combo) is { } other)
            return (false, Loc.Format("Shortcuts_InUse", Label(other)));
        if (ShortcutRules.TryParse(combo, out var mods, out var vk) && !Native.Win32.HotkeyFree(mods, vk))
            return (false, Loc.Get("Shortcuts_Taken"));
        return (true, Loc.Get("Shortcuts_Free"));
    }

    private async Task SaveAsync(string action, string combo)
    {
        var s = Services.Current;
        var r = await s.Settings.SaveKeyAsync(s.Api, ShortcutRules.Key(action), combo);
        if (!r.Ok)
        {
            ErrorBar.Message = Loc.Error(r.Kind, r.Error);
            ErrorBar.IsOpen = true;
            return;
        }
        ErrorBar.IsOpen = false;
        // The agent re-registers on its own thread; read the status after it had the chance.
        await Task.Delay(300);
        await s.RefreshStatusAsync();
    }

    private static bool Down(VirtualKey k) => InputKeyboardSource.GetKeyStateForCurrentThread(k).HasFlag(CoreVirtualKeyStates.Down);

    private static bool IsModifier(VirtualKey k) => k is VirtualKey.Control or VirtualKey.Menu or VirtualKey.Shift
        or VirtualKey.LeftWindows or VirtualKey.RightWindows or VirtualKey.LeftControl or VirtualKey.RightControl
        or VirtualKey.LeftMenu or VirtualKey.RightMenu or VirtualKey.LeftShift or VirtualKey.RightShift;
}
