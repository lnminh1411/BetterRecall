// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Globalization;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;

namespace BetterRecall.UI.Views;

/// <summary>Start page of the main window: capture state with pause and lock, store figures, quick access to the
/// other pages, the global hotkeys, and switches for the main features. A switch saves its one key at once; the
/// chevron next to it opens that feature's settings group.</summary>
public sealed partial class HomePage : Page
{
    private static readonly (string Glyph, string Label, string Page)[] TileList =
    [
        ("\uE721", "Main_Search/Content", ""),   // "" opens the search window
        ("\uE81C", "Nav_Timeline/Content", "timeline"),
        ("\uE714", "Nav_Replays/Content", "replays"),
        ("\uEC4A", "Nav_Bench/Content", "bench"),
        ("\uE9D9", "Nav_Audit/Content", "audit"),
        ("\uE7BA", "Nav_Danger/Content", "danger"),
        ("\uE713", "Nav_Settings/Content", "settings"),
    ];

    private static readonly (string Key, string Glyph)[] FeatureList =
    [
        ("capture.enabled", "\uE7F4"),
        ("keyframes.enabled", "\uEB9F"),
        ("replay.enabled", "\uE714"),
        ("audio.system", "\uE767"),
        ("audio.mic", "\uE720"),
        ("privacy.nsfwFilter", "\uEA18"),
        ("ipc.enabled", "\uE943"),
        ("mcp.enabled", "\uE99A"),
    ];

    private static readonly string[] HotkeyNames = ["search", "pause", "replay", "panic"];

    private readonly Dictionary<string, ToggleSwitch> _switches = [];
    private bool _syncing;

    public HomePage()
    {
        InitializeComponent();
        AppName.Text = Loc.Get("App_Name");
        BuildTiles();
        BuildFeatures();
        Loaded += (_, _) =>
        {
            var s = Services.Current;
            s.StatusChanged += UpdateStatus;
            s.LockedChanged += UpdateStatus;
            s.ConnectionChanged += UpdateStatus;
            s.Settings.Changed += SyncSettings;
            UpdateStatus();
            SyncSettings();
        };
        Unloaded += (_, _) =>
        {
            var s = Services.Current;
            s.StatusChanged -= UpdateStatus;
            s.LockedChanged -= UpdateStatus;
            s.ConnectionChanged -= UpdateStatus;
            s.Settings.Changed -= SyncSettings;
        };
    }

    /// <summary>App styles with ThemeResource setters: they follow ui.theme, unlike brushes read in code.</summary>
    private static Style St(string key) => (Style)Application.Current.Resources[key];

    private void BuildTiles()
    {
        foreach (var (glyph, label, page) in TileList)
        {
            var text = Loc.Get(label);
            var content = new StackPanel { Spacing = 8, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center };
            content.Children.Add(new FontIcon { Glyph = glyph, FontSize = 24, Style = St("AccentIconStyle") });
            content.Children.Add(new TextBlock { Text = text, TextAlignment = TextAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis, MaxWidth = 108 });
            var b = new Button
            {
                Content = content,
                HorizontalAlignment = HorizontalAlignment.Stretch,
                VerticalAlignment = VerticalAlignment.Stretch,
                Margin = new Thickness(0, 0, 8, 8),
                Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent),
                BorderBrush = new SolidColorBrush(Microsoft.UI.Colors.Transparent),
            };
            AutomationProperties.SetName(b, text);
            b.Click += (_, _) =>
            {
                if (page.Length == 0) App.Current.ShowLauncher();
                else App.Current.ShowMain(page);
            };
            Tiles.Children.Add(b);
        }
    }

    private void BuildFeatures()
    {
        foreach (var (key, glyph) in FeatureList)
        {
            var d = SettingsSchema.Find(key);
            if (d is null) continue;
            var label = Loc.Get(d.LabelKey);
            var row = new Grid { ColumnSpacing = 12, MinHeight = 44 };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });

            row.Children.Add(new FontIcon { Glyph = glyph, FontSize = 16, VerticalAlignment = VerticalAlignment.Center });
            var text = new TextBlock { Text = label, VerticalAlignment = VerticalAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis };
            Grid.SetColumn(text, 1);
            row.Children.Add(text);

            var sw = new ToggleSwitch { OnContent = "", OffContent = "", MinWidth = 0, Tag = key, VerticalAlignment = VerticalAlignment.Center };
            AutomationProperties.SetName(sw, label);
            sw.Toggled += Feature_Toggled;
            Grid.SetColumn(sw, 2);
            row.Children.Add(sw);
            _switches[key] = sw;

            var open = new Button
            {
                Content = new FontIcon { Glyph = "\uE76C", FontSize = 12 },
                Padding = new Thickness(10, 8, 10, 8),
                VerticalAlignment = VerticalAlignment.Center,
                Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent),
                BorderBrush = new SolidColorBrush(Microsoft.UI.Colors.Transparent),
            };
            var tip = Loc.Format("Home_OpenGroup", Loc.Get("G_" + d.Group));
            ToolTipService.SetToolTip(open, tip);
            AutomationProperties.SetName(open, tip);
            open.Click += (_, _) => App.Current.ShowMain("settings:" + d.Group);
            Grid.SetColumn(open, 3);
            row.Children.Add(open);
            Features.Children.Add(row);
        }
    }

    /// <summary>Switches and hotkeys show saved values, not pending edits from the settings page.</summary>
    private void SyncSettings()
    {
        var s = Services.Current;
        bool usable = s.Settings.Loaded && s.Connection == AgentErrorKind.None;
        _syncing = true;
        foreach (var (key, sw) in _switches)
        {
            sw.IsOn = SettingsSchema.Get(s.Settings.Original, key)?.GetValue<bool>() == true;
            sw.IsEnabled = usable;
        }
        _syncing = false;

        Shortcuts.Children.Clear();
        foreach (var name in HotkeyNames)
        {
            var row = new Grid { MinHeight = 36 };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.Children.Add(new TextBlock { Text = Loc.Get("S_hotkeys_" + name), VerticalAlignment = VerticalAlignment.Center, TextTrimming = TextTrimming.CharacterEllipsis });
            var keys = SettingsSchema.Get(s.Settings.Original, "hotkeys." + name)?.GetValue<string>() ?? "";
            var chips = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4, VerticalAlignment = VerticalAlignment.Center };
            if (keys.Length == 0)
                chips.Children.Add(new TextBlock { Text = Loc.Get("Home_NotSet"), Style = St("SecondaryTextStyle") });
            else
                foreach (var part in keys.Split('+'))
                    chips.Children.Add(new Border
                    {
                        Style = St("KeyChipStyle"),
                        Child = new TextBlock { Text = part, Style = (Style)Application.Current.Resources["CaptionTextBlockStyle"] },
                    });
            AutomationProperties.SetName(chips, keys.Length == 0 ? Loc.Get("Home_NotSet") : keys);
            Grid.SetColumn(chips, 1);
            row.Children.Add(chips);
            Shortcuts.Children.Add(row);
        }
    }

    private void UpdateStatus()
    {
        var s = Services.Current;
        var st = s.Status;
        bool offline = s.Connection is AgentErrorKind.NotRunning or AgentErrorKind.NotTrusted;
        if (offline || st is null)
        {
            StateText.Text = offline ? Loc.Error(s.Connection, null) : "";
            StateDot.Style = St("DotCriticalStyle");
        }
        else
        {
            var parts = StatusText.Describe(st, DateTimeOffset.UtcNow);
            StateText.Text = string.Join("  ·  ", parts.Take(2).Select(Loc.Text));   // capture state, lock state
            StateDot.Style = St(st.Paused ? "DotCautionStyle" : st.Capturing ? "DotSuccessStyle" : "DotNeutralStyle");
        }

        bool paused = st?.Paused == true && st.PauseReason is "user" or "ipc";
        PauseButton.Content = Loc.Get(paused ? "Main_Resume" : "Main_Pause");
        PauseButton.IsEnabled = !offline && st is not null && (st.PauseReason is null or "user" or "ipc");
        LockButton.Content = Loc.Get(s.Locked ? "Main_Unlock" : "Main_Lock");
        LockButton.IsEnabled = !offline;

        Stats.Children.Clear();
        if (st is null || offline) return;
        Stat("Home_Captures", st.Store.Rows.ToString("N0", CultureInfo.CurrentCulture));
        Stat("Home_Storage", StatusText.FormatBytes(st.Store.Bytes));
        Stat("Home_Since", st.Store.Oldest is { } o ? o.ToLocalTime().ToString("d", CultureInfo.CurrentCulture) : "-");
        Stat("Home_Waiting", st.Queue.Pending.ToString("N0", CultureInfo.CurrentCulture));
        Stat("Home_Device", st.Device);
        Stat("Home_Retention", Loc.Format("Home_Days", st.RetentionDays));
        SyncSettings();   // connection changes enable or disable the switches
    }

    private void Stat(string labelKey, string value)
    {
        var p = new StackPanel { Spacing = 2 };
        p.Children.Add(new TextBlock { Text = Loc.Get(labelKey), Style = St("CaptionSecondaryTextStyle") });
        p.Children.Add(new TextBlock { Text = value, Style = (Style)Application.Current.Resources["BodyStrongTextBlockStyle"], TextTrimming = TextTrimming.CharacterEllipsis });
        AutomationProperties.SetName(p, Loc.Get(labelKey) + ": " + value);
        Stats.Children.Add(p);
    }

    private async void Feature_Toggled(object sender, RoutedEventArgs e)
    {
        if (_syncing || sender is not ToggleSwitch sw || sw.Tag is not string key || SettingsSchema.Find(key) is not { } d) return;
        var s = Services.Current;
        bool on = sw.IsOn;
        if (on && d.WarningKey is not null)
        {
            var body = new TextBlock { Text = Loc.Get(d.WarningKey), TextWrapping = TextWrapping.Wrap };
            var dialog = WindowHelpers.Dialog(XamlRoot, Loc.Get(d.LabelKey), body, Loc.Get("Home_TurnOn"));
            if (await dialog.ShowAsync() != ContentDialogResult.Primary) { SyncSettings(); return; }
        }
        sw.IsEnabled = false;
        var r = await s.Settings.SaveKeyAsync(s.Api, key, on);
        if (r.Ok)
        {
            ErrorBar.IsOpen = false;
            s.ApplySecurity();
            await s.RefreshStatusAsync();
        }
        else
        {
            ErrorBar.Message = Loc.Error(r.Kind, r.Error);
            ErrorBar.IsOpen = true;
        }
        SyncSettings();
    }

    private async void Pause_Click(object sender, RoutedEventArgs e)
    {
        var s = Services.Current;
        var r = s.Status?.Paused == true ? await s.Api.ResumeAsync() : await s.Api.PauseAsync();
        if (r.Ok) await s.RefreshStatusAsync();
    }

    private async void Lock_Click(object sender, RoutedEventArgs e)
    {
        var s = Services.Current;
        if (s.Locked) { App.Current.ShowUnlock(); return; }
        if ((await s.Api.LockAsync()).Ok) await s.RefreshStatusAsync();
    }

    private void EditShortcuts_Click(object sender, RoutedEventArgs e) => App.Current.ShowMain("settings:hotkeys");

    /// <summary>Two columns when there is room, else features below the shortcuts.</summary>
    private void Root_SizeChanged(object sender, SizeChangedEventArgs e)
    {
        bool narrow = e.NewSize.Width < 820;
        RightColumn.Width = narrow ? new GridLength(0) : new GridLength(1, GridUnitType.Star);
        Root.ColumnSpacing = narrow ? 0 : 16;
        Grid.SetColumn(FeaturesCard, narrow ? 0 : 1);
        Grid.SetRow(FeaturesCard, narrow ? 3 : 2);
    }
}
