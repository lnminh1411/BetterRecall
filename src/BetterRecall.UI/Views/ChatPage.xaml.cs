// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Globalization;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace BetterRecall.UI.Views;

public sealed partial class ChatPage : Page, IDataPage
{
    private readonly DetailViewModel _detail;
    private bool _selecting;

    public ChatPage()
    {
        InitializeComponent();
        var s = Services.Current;
        _detail = new DetailViewModel(s.MediaApi);
        Detail.ViewModel = _detail;
        NewChatText.Text = Loc.Get("Chat_New");
        DeleteAllText.Text = Loc.Get("Chat_DeleteAll");
        var close = Loc.Get("Chat_CloseDetail");
        ToolTipService.SetToolTip(CloseDetailButton, close);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(CloseDetailButton, close);

        ChatControl.SourceInvoked += OnSourceInvoked;
        ChatControl.TurnCompleted += () => DispatcherQueue.TryEnqueue(() => _ = ReloadListAsync());

        Loaded += (_, _) =>
        {
            Services.Current.SettingsLoaded += UpdateAiState;
            UpdateAiState();
            _ = ReloadListAsync();
        };
        Unloaded += (_, _) => Services.Current.SettingsLoaded -= UpdateAiState;
    }

    /// <summary>With ai.enabled off the chat column shows how to turn it on; saved chats cannot be opened or started
    /// then, but Delete all stays so old chats can still be removed.</summary>
    private void UpdateAiState()
    {
        bool on = Services.Current.AiEnabled;
        NewChatButton.IsEnabled = on;
        SavedList.IsEnabled = on;
    }

    /// <summary>Narrow windows: a smaller chat list, and the source detail opens over the chat instead of beside it.</summary>
    private void Root_SizeChanged(object sender, SizeChangedEventArgs e)
    {
        bool narrow = e.NewSize.Width < 900;
        SidebarColumn.Width = new GridLength(narrow ? 200 : 260);
        Grid.SetColumn(DetailHost, narrow ? 1 : 2);
        Grid.SetColumnSpan(DetailHost, narrow ? 2 : 1);
        DetailHost.HorizontalAlignment = narrow ? HorizontalAlignment.Right : HorizontalAlignment.Stretch;
        DetailHost.Width = Math.Min(380, Math.Max(0, e.NewSize.Width - (narrow ? 200 : 260) - 24));
    }

    public static string FormatTime(DateTimeOffset ts) =>
        ts.ToLocalTime().ToString("g", CultureInfo.CurrentCulture);

    public void Reload()
    {
        _ = ReloadListAsync();
    }

    private async Task ReloadListAsync()
    {
        var r = await Services.Current.Api.ChatListAsync();
        if (r.Ok && r.Value is not null)
        {
            _selecting = true;
            SavedList.ItemsSource = r.Value;
            SavedList.SelectedItem = r.Value.FirstOrDefault(c => c.Id == ChatControl.ViewModel.ChatId);
            _selecting = false;
        }
    }

    private async void SavedList_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_selecting) return;
        if (SavedList.SelectedItem is ChatSummary summary)
        {
            var r = await Services.Current.Api.ChatGetAsync(summary.Id);
            if (r.Ok && r.Value is not null)
            {
                ChatControl.ViewModel.Load(r.Value);
            }
        }
    }

    private void NewChat_Click(object sender, RoutedEventArgs e)
    {
        SavedList.SelectedItem = null;
        ChatControl.ViewModel.Reset();
    }

    private void OnSourceInvoked(ChatSource source)
    {
        DetailHost.Visibility = Visibility.Visible;
        _ = _detail.LoadAsync(ChatViewModel.ToCaptureItem(source));
    }

    private void CloseDetail_Click(object sender, RoutedEventArgs e)
    {
        _detail.Clear();
        DetailHost.Visibility = Visibility.Collapsed;
    }

    /// <summary>The chat a context request is about: the row under the pointer, or the focused row for Shift+F10 and the
    /// menu key (raised on the ListViewItem, not on the template inside it).</summary>
    private ChatSummary? ChatAt(object? source)
    {
        for (var d = source as DependencyObject; d is not null && !ReferenceEquals(d, SavedList); d = Microsoft.UI.Xaml.Media.VisualTreeHelper.GetParent(d))
        {
            if (d is ListViewItem li) return SavedList.ItemFromContainer(li) as ChatSummary;
            if (d is FrameworkElement { DataContext: ChatSummary c }) return c;
        }
        return SavedList.SelectedItem as ChatSummary;
    }

    private void SavedList_KeyDown(object sender, Microsoft.UI.Xaml.Input.KeyRoutedEventArgs e)
    {
        if (e.Key is not (Windows.System.VirtualKey.F2 or Windows.System.VirtualKey.Delete) || ChatAt(e.OriginalSource) is not { } summary) return;
        e.Handled = true;
        _ = e.Key == Windows.System.VirtualKey.F2 ? RenameChatAsync(summary) : DeleteChatAsync(summary);
    }

    private void SavedList_ContextRequested(UIElement sender, Microsoft.UI.Xaml.Input.ContextRequestedEventArgs args)
    {
        if (ChatAt(args.OriginalSource) is not { } summary) return;
        args.Handled = true;
        var fe = (args.OriginalSource as FrameworkElement) ?? SavedList;
        var menu = new MenuFlyout();
        WindowHelpers.Themed(menu, SavedList);

        var renameItem = new MenuFlyoutItem { Text = Loc.Get("Chat_Rename"), KeyboardAcceleratorTextOverride = "F2" };
        renameItem.Click += async (_, _) => await RenameChatAsync(summary);
        menu.Items.Add(renameItem);

        var deleteItem = new MenuFlyoutItem { Text = Loc.Get("Chat_Delete"), KeyboardAcceleratorTextOverride = Loc.Get("Key_Delete") };
        deleteItem.Click += async (_, _) => await DeleteChatAsync(summary);
        menu.Items.Add(deleteItem);

        if (args.TryGetPosition(fe, out var pos))
            menu.ShowAt(fe, pos);
        else
            menu.ShowAt(fe);
    }

    private async Task RenameChatAsync(ChatSummary summary)
    {
        var tb = new TextBox { Text = summary.Title, MinWidth = 280 };
        tb.SelectAll();
        var dialog = WindowHelpers.Dialog(XamlRoot, Loc.Get("Chat_RenameTitle"), tb, Loc.Get("Chat_Rename"));
        if (await dialog.ShowAsync() == ContentDialogResult.Primary && !string.IsNullOrWhiteSpace(tb.Text))
        {
            await Services.Current.Api.ChatRenameAsync(summary.Id, tb.Text.Trim());
            if (ChatControl.ViewModel.ChatId == summary.Id)
                ChatControl.ViewModel.Title = tb.Text.Trim();
            await ReloadListAsync();
        }
    }

    private async Task DeleteChatAsync(ChatSummary summary)
    {
        var body = new TextBlock { Text = Loc.Get("Chat_DeleteConfirm"), TextWrapping = TextWrapping.Wrap };
        var dialog = WindowHelpers.Dialog(XamlRoot, Loc.Get("Chat_Delete"), body, Loc.Get("Chat_Delete"), danger: true);
        if (await dialog.ShowAsync() == ContentDialogResult.Primary)
        {
            await Services.Current.Api.ChatDeleteAsync(summary.Id);
            if (ChatControl.ViewModel.ChatId == summary.Id)
                ChatControl.ViewModel.Reset();
            await ReloadListAsync();
        }
    }

    private async void DeleteAll_Click(object sender, RoutedEventArgs e)
    {
        var body = new TextBlock { Text = Loc.Get("Chat_DeleteAllConfirm"), TextWrapping = TextWrapping.Wrap };
        var dialog = WindowHelpers.Dialog(XamlRoot, Loc.Get("Chat_DeleteAll"), body, Loc.Get("Chat_DeleteAll"), danger: true);
        if (await dialog.ShowAsync() == ContentDialogResult.Primary)
        {
            await Services.Current.Api.ChatClearAsync();
            ChatControl.ViewModel.Reset();
            await ReloadListAsync();
        }
    }
}
