// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Collections.ObjectModel;
using BetterRecall.UI.Logic;
using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Windows.System;
using Windows.UI.Core;

namespace BetterRecall.UI.Views;

/// <summary>Deny-list editor: list, add box, remove selected. Keyboard: Enter adds, Delete removes the selected entry.</summary>
internal sealed partial class StringListEditor : Grid
{
    private readonly ObservableCollection<string> _items;
    private readonly ListView _list;
    private readonly TextBox _input;

    public event Action<IReadOnlyList<string>>? Changed;

    public StringListEditor(IEnumerable<string> items, string label)
    {
        _items = new ObservableCollection<string>(items);
        RowSpacing = 8;
        ColumnSpacing = 8;
        RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });

        _list = new ListView
        {
            ItemsSource = _items,
            SelectionMode = ListViewSelectionMode.Single,
            MaxHeight = 248,
            Style = (Style)Application.Current.Resources["OutlinedListViewStyle"],
        };
        AutomationProperties.SetName(_list, label);
        _list.KeyDown += (_, e) => { if (e.Key == VirtualKey.Delete) { e.Handled = true; RemoveSelected(); } };
        SetColumnSpan(_list, 3);
        Children.Add(_list);

        _input = new TextBox { PlaceholderText = Loc.Get("List_AddPlaceholder") };
        AutomationProperties.SetName(_input, Loc.Format("List_AddName", label));
        _input.KeyDown += (_, e) => { if (e.Key == VirtualKey.Enter) { e.Handled = true; Add(); } };
        SetRow(_input, 1);
        Children.Add(_input);

        var add = new Button { Content = Loc.Get("List_Add") };
        add.Click += (_, _) => Add();
        SetRow(add, 1);
        SetColumn(add, 1);
        Children.Add(add);

        var remove = new Button { Content = Loc.Get("List_Remove") };
        remove.Click += (_, _) => RemoveSelected();
        SetRow(remove, 1);
        SetColumn(remove, 2);
        Children.Add(remove);
    }

    private void Add()
    {
        var v = _input.Text.Trim();
        if (v.Length == 0 || _items.Contains(v, StringComparer.OrdinalIgnoreCase)) return;
        _items.Add(v);
        _input.Text = "";
        Changed?.Invoke([.. _items]);
    }

    private void RemoveSelected()
    {
        if (_list.SelectedIndex < 0) return;
        int i = _list.SelectedIndex;
        _items.RemoveAt(i);
        if (_items.Count > 0) _list.SelectedIndex = Math.Min(i, _items.Count - 1);
        Changed?.Invoke([.. _items]);
    }
}

/// <summary>Key-combo recorder: press the button (or Enter/Space on it), then the combination. Esc cancels.</summary>
internal sealed partial class HotkeyRecorder : StackPanel
{
    private readonly Button _button;
    private readonly TextBlock _hint;
    private string _value;
    private bool _recording;

    public event Action<string>? Recorded;

    public HotkeyRecorder(string value)
    {
        _value = value;
        Orientation = Orientation.Horizontal;
        Spacing = 8;
        _button = new Button { MinWidth = 180 };
        _button.Click += (_, _) => Start();
        _button.PreviewKeyDown += OnKey;
        _button.LostFocus += (_, _) => Stop();
        _hint = new TextBlock
        {
            VerticalAlignment = VerticalAlignment.Center,
            Style = (Style)Application.Current.Resources["CaptionSecondaryTextStyle"],
        };
        AutomationProperties.SetLiveSetting(_hint, Microsoft.UI.Xaml.Automation.Peers.AutomationLiveSetting.Polite);
        Children.Add(_button);
        Children.Add(_hint);
        Render();
    }

    private void Start()
    {
        _recording = true;
        Render();
    }

    private void Stop()
    {
        _recording = false;
        Render();
    }

    private void Render()
    {
        _button.Content = _recording ? Loc.Get("Hotkey_Press") : _value;
        _hint.Text = _recording ? Loc.Get("Hotkey_Hint") : "";
        AutomationProperties.SetHelpText(_button, Loc.Get("Hotkey_Help"));
    }

    private static bool Down(VirtualKey k) => InputKeyboardSource.GetKeyStateForCurrentThread(k).HasFlag(CoreVirtualKeyStates.Down);

    private void OnKey(object sender, KeyRoutedEventArgs e)
    {
        if (!_recording) return;
        e.Handled = true;
        if (e.Key == VirtualKey.Escape) { Stop(); return; }
        bool win = Down(VirtualKey.LeftWindows) || Down(VirtualKey.RightWindows);
        var hk = HotkeyFormat.FromKeyPress((int)e.Key, Down(VirtualKey.Control), Down(VirtualKey.Menu), Down(VirtualKey.Shift), win);
        if (hk is null)
        {
            if (e.Key is not (VirtualKey.Control or VirtualKey.Menu or VirtualKey.Shift or VirtualKey.LeftWindows or VirtualKey.RightWindows
                or VirtualKey.LeftControl or VirtualKey.RightControl or VirtualKey.LeftMenu or VirtualKey.RightMenu or VirtualKey.LeftShift or VirtualKey.RightShift))
                _hint.Text = Loc.Get("Hotkey_NeedModifier");
            return;
        }
        _value = hk;
        Stop();
        Recorded?.Invoke(hk);
    }
}
