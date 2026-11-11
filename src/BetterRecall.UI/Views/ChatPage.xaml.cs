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

        Loaded += (_, _) => _ = ReloadListAsync();
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

    private void Chat_ContextRequested(UIElement sender, Microsoft.UI.Xaml.Input.ContextRequestedEventArgs args)
    {
        if (sender is not FrameworkElement fe || fe.Tag is not ChatSummary summary) return;
        var menu = new MenuFlyout();

        var renameItem = new MenuFlyoutItem { Text = Loc.Get("Chat_Rename") };
        renameItem.Click += async (_, _) => await RenameChatAsync(summary);
        menu.Items.Add(renameItem);

        var deleteItem = new MenuFlyoutItem { Text = Loc.Get("Chat_Delete") };
        deleteItem.Click += async (_, _) => await DeleteChatAsync(summary);
        menu.Items.Add(deleteItem);

        if (args.TryGetPosition(sender, out var pos))
            menu.ShowAt(sender, pos);
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
