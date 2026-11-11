// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Text.Json.Nodes;
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Navigation;
using Microsoft.Windows.Storage.Pickers;

namespace BetterRecall.UI.Views;

/// <summary>
/// One settings group, built from SettingsSchema. Edits go to the shared SettingsEditor, so switching groups keeps them;
/// Save sends only changed keys and shows "Invalid key." errors under the control they belong to.
/// </summary>
public sealed partial class SettingsPage : Page
{
    private string _group = SettingsSchema.Groups[0];
    private readonly Dictionary<string, TextBlock> _errors = [];
    private readonly Dictionary<string, FrameworkElement> _controls = [];

    public SettingsPage()
    {
        InitializeComponent();
        Loaded += (_, _) =>
        {
            var s = Services.Current;
            s.SettingsLoaded += Build;
            s.Settings.Changed += UpdateState;
            if (!s.Settings.Loaded) _ = s.RefreshSettingsAsync();
            Build();
        };
        Unloaded += (_, _) =>
        {
            Services.Current.SettingsLoaded -= Build;
            Services.Current.Settings.Changed -= UpdateState;
        };
    }

    protected override void OnNavigatedTo(NavigationEventArgs e)
    {
        if (e.Parameter is string g && SettingsSchema.Groups.Contains(g)) _group = g;
    }

    private SettingsEditor Editor => Services.Current.Settings;

    private void Build()
    {
        Cards.Children.Clear();
        _errors.Clear();
        _controls.Clear();
        Cards.Children.Add(new TextBlock { Text = Loc.Get("G_" + _group), Style = (Style)Application.Current.Resources["PageTitleStyle"] }
            .With(t => AutomationProperties.SetHeadingLevel(t, Microsoft.UI.Xaml.Automation.Peers.AutomationHeadingLevel.Level1)));

        if (!Editor.Loaded)
        {
            var retry = new Button { Content = Loc.Get("Common_Retry") };
            retry.Click += async (_, _) => await Services.Current.RefreshSettingsAsync();
            Cards.Children.Add(new InfoBar { IsOpen = true, IsClosable = false, Severity = InfoBarSeverity.Error, Title = Loc.Get("Settings_Unavailable"),
                Message = Loc.Error(Services.Current.Connection == AgentErrorKind.None ? AgentErrorKind.NotRunning : Services.Current.Connection, null), ActionButton = retry });
            UpdateState();
            return;
        }

        if (_group == "security")
            Cards.Children.Add(new InfoBar { IsOpen = true, IsClosable = false, Severity = InfoBarSeverity.Informational, Title = Loc.Get("Sec_NoteTitle"), Message = Loc.Get("Sec_Note") });

        if (_group is "inference" or "replay") Cards.Children.Add(BenchLinkCard());

        foreach (var d in SettingsSchema.InGroup(_group))
        {
            if (d.WarningKey is not null)
                Cards.Children.Add(new InfoBar { IsOpen = true, IsClosable = false, Severity = InfoBarSeverity.Warning, Message = Loc.Get(d.WarningKey), Margin = new Thickness(0, 6, 0, 0) });
            Cards.Children.Add(Card(d));
        }

        if (_group == "inference") Cards.Children.Add(BenchCard());
        if (_group == "security") Cards.Children.Add(SecretCards.PasswordCard(this));
        if (_group == "ai")
        {
            Cards.Children.Insert(1, new InfoBar { IsOpen = true, IsClosable = false, Severity = InfoBarSeverity.Informational, Title = Loc.Get("Ai_NoteTitle"), Message = Loc.Get("Ai_Note") });
            if (NetworkGuarantee.IsOff(Editor.Original))   // saved on: the guarantee is gone until it is turned off
                Cards.Children.Insert(1, new InfoBar { IsOpen = true, IsClosable = false, Severity = InfoBarSeverity.Warning, Title = Loc.Get(NetworkGuarantee.OffLineKey) });
            Cards.Children.Add(SecretCards.ApiKeyCard(this));
        }
        UpdateState();
    }

    // ---------------- cards ----------------

    private Border Card(SettingDescriptor d)
    {
        var label = Loc.Get(d.LabelKey);
        var desc = Loc.Get(d.DescriptionKey);
        bool wide = d.Type is SettingType.StringList or SettingType.Path;
        var grid = new Grid { ColumnSpacing = 16, RowSpacing = wide ? 8 : 0 };
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        grid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        if (wide) grid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });

        var text = new StackPanel { Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
        text.Children.Add(new TextBlock { Text = label, TextWrapping = TextWrapping.Wrap });
        if (desc != d.DescriptionKey)
            text.Children.Add(new TextBlock
            {
                Text = desc, TextWrapping = TextWrapping.Wrap,
                Style = (Style)Application.Current.Resources["CaptionSecondaryTextStyle"],
            });
        var error = new TextBlock
        {
            TextWrapping = TextWrapping.Wrap,
            Visibility = Visibility.Collapsed,
            Style = (Style)Application.Current.Resources["CaptionCriticalTextStyle"],
        };
        AutomationProperties.SetLiveSetting(error, Microsoft.UI.Xaml.Automation.Peers.AutomationLiveSetting.Assertive);
        text.Children.Add(error);
        _errors[d.Key] = error;
        grid.Children.Add(text);

        var control = Control(d);
        _controls[d.Key] = control;
        AutomationProperties.SetName(control, label);
        if (desc != d.DescriptionKey) AutomationProperties.SetHelpText(control, desc);
        if (wide)
        {
            Grid.SetRow(control, 1);
            Grid.SetColumnSpan(control, 2);
        }
        else
        {
            Grid.SetColumn(control, 1);
            control.VerticalAlignment = VerticalAlignment.Center;
        }
        grid.Children.Add(control);
        var card = new Border { Style = (Style)Application.Current.Resources["CardStyle"], Child = grid };
        if (desc == d.DescriptionKey && !wide) card.Padding = new Thickness(16, 6, 16, 6);   // one line: compact row
        return card;
    }

    private FrameworkElement Control(SettingDescriptor d)
    {
        var value = Editor.Get(d.Key);
        switch (d.Type)
        {
            case SettingType.ReadOnlyBool:
                return new TextBlock { Text = Loc.Get(value?.GetValue<bool>() == true ? "Common_Yes" : "Common_No") };

            case SettingType.Bool:
            {
                var t = new ToggleSwitch { IsOn = value?.GetValue<bool>() == true, OnContent = Loc.Get("Common_On"), OffContent = Loc.Get("Common_Off"), MinWidth = 0 };
                bool reverting = false;
                t.Toggled += async (_, _) =>
                {
                    if (reverting) return;
                    if (NetworkGuarantee.NeedsConfirm(d.Key, t.IsOn, NetworkGuarantee.IsOff(Editor.Original))
                        && !await SecretCards.ConfirmCloudAsync(XamlRoot, Editor.Working))
                    {
                        reverting = true;
                        t.IsOn = false;   // Cancel leaves it off
                        reverting = false;
                        return;
                    }
                    Editor.Set(d.Key, t.IsOn);
                };
                return t;
            }

            case SettingType.Int:
            case SettingType.Double:
            {
                SettingsSchema.TryNumber(value, out var n);
                var nb = new NumberBox
                {
                    Value = n,
                    SpinButtonPlacementMode = NumberBoxSpinButtonPlacementMode.Compact,
                    ValidationMode = NumberBoxValidationMode.Disabled,
                    SmallChange = d.Type == SettingType.Double ? (d.Max <= 1 ? 0.01 : 0.5) : Step(d),
                    LargeChange = d.Type == SettingType.Double ? (d.Max <= 1 ? 0.05 : 5) : Step(d) * 10,
                    MinWidth = 160,
                    NumberFormatter = new Windows.Globalization.NumberFormatting.DecimalFormatter
                    {
                        FractionDigits = d.Type == SettingType.Double ? 2 : 0,
                        IsGrouped = false,
                        NumberRounder = new Windows.Globalization.NumberFormatting.IncrementNumberRounder { Increment = d.Type == SettingType.Double ? 0.01 : 1 },
                    },
                };
                if (d.Min is { } lo && d.Max is { } hi) nb.PlaceholderText = Loc.Format("Settings_Range", Num(lo), Num(hi));
                nb.ValueChanged += (_, e) =>
                {
                    if (double.IsNaN(e.NewValue)) Editor.Set(d.Key, null);
                    else Editor.Set(d.Key, d.Type == SettingType.Int ? JsonValue.Create((long)Math.Round(e.NewValue)) : JsonValue.Create(Math.Round(e.NewValue, 4)));
                };
                return nb;
            }

            case SettingType.Choice:
            case SettingType.IntChoice:
            {
                var cb = new ComboBox { MinWidth = 160 };
                string current = d.Type == SettingType.IntChoice && SettingsSchema.TryNumber(value, out var cn)
                    ? ((long)cn).ToString(CultureInfo.InvariantCulture) : value?.GetValue<string>() ?? "";
                foreach (var c in d.Choices!)
                {
                    var text = Loc.Get(d.ChoiceKey(c));
                    cb.Items.Add(new ComboBoxItem { Content = text == d.ChoiceKey(c) ? c : text, Tag = c });
                    if (c == current) cb.SelectedIndex = cb.Items.Count - 1;
                }
                cb.SelectionChanged += (_, _) =>
                {
                    if (cb.SelectedItem is not ComboBoxItem { Tag: string v }) return;
                    Editor.Set(d.Key, d.Type == SettingType.IntChoice ? JsonValue.Create(int.Parse(v, CultureInfo.InvariantCulture)) : JsonValue.Create(v));
                };
                return cb;
            }

            case SettingType.Adapter:
            {
                var autoLabel = Loc.Get(d.ChoiceKey("auto"));
                var cb = new ComboBox { IsEditable = true, MinWidth = 260 };
                cb.Items.Add(autoLabel);
                var current = value?.GetValue<string>() ?? "auto";
                bool filling = true;
                if (string.Equals(current, "auto", StringComparison.OrdinalIgnoreCase)) cb.SelectedIndex = 0; else cb.Text = current;
                filling = false;
                string Map(string? t) => string.IsNullOrWhiteSpace(t) ? "" : (t.Trim() == autoLabel || string.Equals(t.Trim(), "auto", StringComparison.OrdinalIgnoreCase)) ? "auto" : t.Trim();
                cb.SelectionChanged += (_, _) => { if (!filling && cb.SelectedItem is string s) Editor.Set(d.Key, Map(s)); };
                cb.TextSubmitted += (_, a) => { Editor.Set(d.Key, Map(a.Text)); a.Handled = true; };
                _ = FillAdaptersAsync(cb, current, () => filling = true, () => filling = false);
                return cb;
            }

            case SettingType.Text:
            {
                var tb = new TextBox { Text = value?.GetValue<string>() ?? "", MinWidth = 220 };
                tb.TextChanged += (_, _) => Editor.Set(d.Key, tb.Text.Trim());
                return tb;
            }

            case SettingType.Path:
            {
                var panel = new Grid { ColumnSpacing = 8 };
                panel.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                panel.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                var tb = new TextBox { Text = value?.GetValue<string>() ?? "", PlaceholderText = Loc.Get("Settings_DefaultPath") };
                tb.TextChanged += (_, _) => Editor.Set(d.Key, tb.Text.Trim());
                var browse = new Button { Content = Loc.Get("Settings_Browse") };
                AutomationProperties.SetName(browse, Loc.Format("Settings_BrowseName", Loc.Get(d.LabelKey)));
                browse.Click += async (_, _) =>
                {
                    var picker = new FolderPicker(XamlRoot.ContentIslandEnvironment.AppWindowId);
                    var r = await picker.PickSingleFolderAsync();
                    if (r is not null && !string.IsNullOrEmpty(r.Path)) tb.Text = r.Path;
                };
                Grid.SetColumn(browse, 1);
                panel.Children.Add(tb);
                panel.Children.Add(browse);
                return panel;
            }

            case SettingType.IntList:
            {
                var tb = new TextBox
                {
                    Text = string.Join(", ", (value as JsonArray)?.Select(x => x?.ToJsonString()) ?? []),
                    PlaceholderText = Loc.Get("Settings_AllMonitors"),
                    MinWidth = 160,
                };
                tb.TextChanged += (_, _) =>
                {
                    var arr = new JsonArray();
                    foreach (var part in tb.Text.Split([',', ' ', ';'], StringSplitOptions.RemoveEmptyEntries))
                        arr.Add(int.TryParse(part, NumberStyles.None, CultureInfo.InvariantCulture, out var i) ? JsonValue.Create(i) : JsonValue.Create(part));
                    Editor.Set(d.Key, arr);
                };
                return tb;
            }

            case SettingType.StringList:
            {
                var editor = new StringListEditor((value as JsonArray)?.Select(x => x?.GetValue<string>() ?? "") ?? [], Loc.Get(d.LabelKey));
                editor.Changed += items => Editor.Set(d.Key, new JsonArray(items.Select(i => (JsonNode)JsonValue.Create(i)!).ToArray()));
                return editor;
            }

            case SettingType.Hotkey:
            {
                var rec = new HotkeyRecorder(value?.GetValue<string>() ?? "");
                rec.Recorded += hk => Editor.Set(d.Key, hk);
                return rec;
            }
        }
        return new TextBlock();
    }

    private static double Step(SettingDescriptor d) => d.Max switch { >= 100000 => 1000, >= 10000 => 100, >= 1000 => 10, _ => 1 };

    private static string Num(double v) => v.ToString(CultureInfo.CurrentCulture);

    private Border BenchCard()
    {
        var panel = new StackPanel { Spacing = 8 };
        panel.Children.Add(new TextBlock { Text = Loc.Get("Bench_Title") });
        panel.Children.Add(new TextBlock
        {
            Text = Loc.Get("Bench_Desc"), TextWrapping = TextWrapping.Wrap,
            Style = (Style)Application.Current.Resources["CaptionSecondaryTextStyle"],
        });
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        var run = new Button { Content = Loc.Get("Bench_Run") };
        var ring = new ProgressRing { IsActive = false, Width = 20, Height = 20 };
        row.Children.Add(run);
        row.Children.Add(ring);
        panel.Children.Add(row);
        var results = new StackPanel { Spacing = 4 };
        AutomationProperties.SetLiveSetting(results, Microsoft.UI.Xaml.Automation.Peers.AutomationLiveSetting.Polite);
        panel.Children.Add(results);
        run.Click += async (_, _) =>
        {
            run.IsEnabled = false;
            ring.IsActive = true;
            results.Children.Clear();
            var r = await Services.Current.Api.BenchAsync();
            ring.IsActive = false;
            run.IsEnabled = true;
            if (!r.Ok)
            {
                results.Children.Add(new TextBlock { Text = Loc.Error(r.Kind, r.Error), TextWrapping = TextWrapping.Wrap });
                return;
            }
            results.Children.Add(new TextBlock { Text = Loc.Format("Bench_Chosen", r.Value!.ChosenAdapter ?? r.Value!.Chosen), Style = (Style)Application.Current.Resources["BodyStrongTextBlockStyle"] });
            foreach (var dev in r.Value.Devices)
                results.Children.Add(new TextBlock
                {
                    TextWrapping = TextWrapping.Wrap,
                    Text = dev.Ok
                        ? Loc.Format("Bench_Row", dev.Adapter ?? dev.Device, dev.Ep, dev.ImageMs.ToString("0.#", CultureInfo.CurrentCulture),
                            dev.TextMs.ToString("0.#", CultureInfo.CurrentCulture), dev.Cosine.ToString("0.0000", CultureInfo.CurrentCulture))
                        : Loc.Format("Bench_RowFailed", dev.Adapter ?? dev.Device, dev.Ep, dev.Error ?? ""),
                });
        };
        return new Border { Style = (Style)Application.Current.Resources["CardStyle"], Child = panel };
    }

    // ---------------- state ----------------

    private void UpdateState()
    {
        foreach (var (key, block) in _errors)
        {
            if (Editor.Errors.TryGetValue(key, out _) && SettingsSchema.Find(key) is { } d)
            {
                block.Text = InlineError(d);
                block.Visibility = Visibility.Visible;
            }
            else block.Visibility = Visibility.Collapsed;
        }
        var keys = Editor.Loaded ? SettingsDiff.Keys(Editor.Diff()).ToList() : [];
        PendingText.Text = keys.Count == 0 ? "" : Loc.Format("Settings_Pending", keys.Count);
        PendingText.Visibility = SaveBar.IsOpen ? Visibility.Collapsed : Visibility.Visible;
        SaveButton.IsEnabled = keys.Count > 0;
        DiscardButton.IsEnabled = keys.Count > 0;
        Footer.Visibility = keys.Count > 0 || SaveBar.IsOpen ? Visibility.Visible : Visibility.Collapsed;
        foreach (var (key, c) in _controls) if (SettingsSchema.Find(key) is { } dd) SetEnabled(c, dd.IsEnabled(Editor.Working));
    }

    /// <summary>Panels (list editor, hotkey recorder, path row) have no IsEnabled; gate the controls inside.</summary>
    private static void SetEnabled(FrameworkElement e, bool on)
    {
        if (e is Control c) c.IsEnabled = on;
        else if (e is Panel p) foreach (var child in p.Children) if (child is FrameworkElement f) SetEnabled(f, on);
    }

    private Border BenchLinkCard()
    {
        var grid = new Grid { ColumnSpacing = 16 };
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });

        var text = new TextBlock
        {
            Text = Loc.Get("Bench_Link"),
            TextWrapping = TextWrapping.Wrap,
            VerticalAlignment = VerticalAlignment.Center,
        };
        grid.Children.Add(text);

        var button = new HyperlinkButton
        {
            Content = Loc.Get("Bench_LinkButton"),
        };
        Grid.SetColumn(button, 1);
        button.Click += (_, _) => App.Current.ShowMain("bench");
        grid.Children.Add(button);

        return new Border { Style = (Style)Application.Current.Resources["CardStyle"], Child = grid };
    }

    private static async Task FillAdaptersAsync(ComboBox cb, string current, Action startFilling, Action stopFilling)
    {
        var rep = await Services.Current.GetLastBenchAsync();
        if (rep is null) return;
        startFilling();
        try
        {
            string? match = null;
            foreach (var name in SettingsSchema.AdapterChoices(rep.Hardware.Gpus))
            {
                cb.Items.Add(name);
                if (match is null && string.Equals(current, name, StringComparison.OrdinalIgnoreCase))
                    match = name;
            }
            if (match is not null) cb.SelectedItem = match;
        }
        finally
        {
            stopFilling();
        }
    }

    private static string InlineError(SettingDescriptor d) => d.Type switch
    {
        SettingType.Int or SettingType.Double or SettingType.IntList when d.Min is { } lo && d.Max is { } hi
            => Loc.Format("Settings_RangeError", Num(lo), d.Key == "ipc.defaultResults" ? Loc.Get("S_ipc_maxResults") : Num(hi)),
        SettingType.Hotkey => Loc.Get("Settings_HotkeyError"),
        SettingType.Path => Loc.Get("Settings_PathError"),
        SettingType.StringList when d.Key == "ipc.allowClients" => Loc.Get("Settings_PathError"),
        _ => Loc.Get("Settings_InvalidError"),
    };

    private async void Save_Click(object sender, RoutedEventArgs e)
    {
        SaveButton.IsEnabled = false;
        var s = Services.Current;
        var (outcome, last) = await Editor.SaveAsync(s.Api);
        switch (outcome)
        {
            case SaveOutcome.Saved:
                s.ApplySecurity();
                ShowBar(InfoBarSeverity.Success, Loc.Get("Settings_Saved"));
                Build();
                break;
            case SaveOutcome.Invalid:
                var keys = Editor.Errors.Keys.ToList();
                var other = keys.Where(k => SettingsSchema.Find(k)?.Group != _group).Select(k => Loc.Get("G_" + (SettingsSchema.Find(k)?.Group ?? ""))).Distinct().ToList();
                ShowBar(InfoBarSeverity.Error, other.Count == 0 ? Loc.Get("Settings_FixErrors") : Loc.Format("Settings_FixErrorsIn", string.Join(", ", other)));
                break;
            case SaveOutcome.TooLarge:
                ShowBar(InfoBarSeverity.Error, Loc.Get("Settings_TooLarge"));
                break;
            case SaveOutcome.Failed:
                ShowBar(InfoBarSeverity.Error, Loc.Error(last?.Kind ?? AgentErrorKind.Server, last?.Error));
                break;
        }
        UpdateState();
    }

    private void Discard_Click(object sender, RoutedEventArgs e)
    {
        Editor.Discard();
        SaveBar.IsOpen = false;
        Build();
    }

    private void ShowBar(InfoBarSeverity sev, string message)
    {
        SaveBar.Severity = sev;
        SaveBar.Message = message;
        SaveBar.IsOpen = true;
        SaveBar.Closed -= OnBarClosed;
        SaveBar.Closed += OnBarClosed;
    }

    private void OnBarClosed(InfoBar sender, InfoBarClosedEventArgs args) => UpdateState();
}

internal static class FluentExtensions
{
    public static T With<T>(this T o, Action<T> a)
    {
        a(o);
        return o;
    }
}
