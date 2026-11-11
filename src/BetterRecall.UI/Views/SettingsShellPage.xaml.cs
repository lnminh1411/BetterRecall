// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media.Animation;
using Microsoft.UI.Xaml.Navigation;

namespace BetterRecall.UI.Views;

/// <summary>Full settings page opened from the gear button: group list on the left, the group's cards on the right.
/// Pending edits live in the shared settings editor, so switching groups keeps them.</summary>
public sealed partial class SettingsShellPage : Page
{
    private static readonly Dictionary<string, string> Glyphs = new()
    {
        ["appearance"] = "\uE790", ["capture"] = "\uE7F4", ["keyframes"] = "\uEB9F", ["replay"] = "\uE714", ["audio"] = "\uE767",
        ["privacy"] = "\uEA18", ["index"] = "\uE721", ["inference"] = "\uE945", ["governance"] = "\uE916",
        ["security"] = "\uE72E", ["hotkeys"] = "\uE765", ["ipc"] = "\uE71B", ["storage"] = "\uEDA2", ["retention"] = "\uE823", ["log"] = "\uE8A5",
    };

    private string _group = "";

    public SettingsShellPage()
    {
        InitializeComponent();
        foreach (var g in SettingsSchema.Groups)
            Groups.MenuItems.Add(new NavigationViewItem
            {
                Content = Loc.Get("G_" + g),
                Tag = g,
                Icon = new FontIcon { Glyph = Glyphs.GetValueOrDefault(g, "\uE713") },
            });
    }

    protected override void OnNavigatedTo(NavigationEventArgs e) => Show(e.Parameter as string);

    /// <summary>Opens a group; unknown or empty opens the first.</summary>
    public void Show(string? group)
    {
        if (group is null || !SettingsSchema.Groups.Contains(group)) group = _group.Length > 0 ? _group : SettingsSchema.Groups[0];
        Groups.SelectedItem = Groups.MenuItems.OfType<NavigationViewItem>().FirstOrDefault(i => Equals(i.Tag, group));
        if (group == _group && GroupFrame.Content is not null) return;
        _group = group;
        GroupFrame.Navigate(typeof(SettingsPage), group, new SuppressNavigationTransitionInfo());
        GroupFrame.BackStack.Clear();
    }

    private void Groups_ItemInvoked(NavigationView sender, NavigationViewItemInvokedEventArgs args)
    {
        if (args.InvokedItemContainer?.Tag is string g) Show(g);
    }
}
