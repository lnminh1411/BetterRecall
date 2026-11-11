// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Globalization;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Windows.System;

namespace BetterRecall.UI.Views;

public sealed partial class ChatView : UserControl
{
    private CancellationTokenSource? _cts;

    public ChatViewModel ViewModel { get; } = new();

    public event Action<ChatSource>? SourceInvoked;
    public event Action? TurnCompleted;

    public ChatView()
    {
        InitializeComponent();
        OffText.Text = Loc.Get("Chat_Off");
        OpenSettingsButton.Content = Loc.Get("Chat_OpenSettings");
        LocalBar.Message = Loc.Get("Chat_LocalNote");
        TryAgainButton.Content = Loc.Get("Chat_TryAgain");
        EmptyHint.Text = Loc.Get("Chat_Empty");
        AllowButton.Content = Loc.Get("Chat_AllowChat");
        DenyButton.Content = Loc.Get("Chat_DontShare");
        InputBox.PlaceholderText = Loc.Get("Chat_InputPlaceholder");
        Label(SendButton, "Chat_Send");
        Label(StopButton, "Chat_Stop");
        MessagesList.ItemsSource = ViewModel.Messages;
        ApprovalList.ItemsSource = ViewModel.Approval;

        ViewModel.PropertyChanged += (_, _) => DispatcherQueue.TryEnqueue(UpdateBindings);
        ViewModel.Approval.CollectionChanged += (_, _) => DispatcherQueue.TryEnqueue(UpdateBindings);
        ViewModel.Messages.CollectionChanged += (_, _) => DispatcherQueue.TryEnqueue(() =>
        {
            UpdateBindings();
            ScrollToBottom();
        });

        Loaded += (_, _) =>
        {
            UpdateOffState();
            UpdateBindings();
            _ = RefreshStatusAsync();
        };

        var services = Services.Current;
        services.SettingsLoaded += () => DispatcherQueue.TryEnqueue(UpdateOffState);
    }

    private static void Label(Button b, string key)
    {
        var text = Loc.Get(key);
        ToolTipService.SetToolTip(b, text);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(b, text);
    }

    /// <summary>Status line under an answer while the agent searches: the query it is looking for.</summary>
    public static string StatusText(string query) => string.IsNullOrEmpty(query) ? "" : Loc.Format("Chat_Searching", query);

    public static string SourceHeader(string app, string site) =>
        string.IsNullOrEmpty(site) ? CaptureItem.AppName(app) : site;

    public static string FormatShortTime(DateTimeOffset ts) =>
        ts.ToLocalTime().ToString("t", CultureInfo.CurrentCulture);

    public static string FormatLongTime(DateTimeOffset ts) =>
        ts.ToLocalTime().ToString("g", CultureInfo.CurrentCulture);

    public static string FormatUsage(AiUsage? u)
    {
        var p = ChatViewModel.UsageParts(u);
        if (p is null) return "";
        var (req, reqLim, tok, tokLim) = p.Value;
        return Loc.Format("Chat_Usage",
            req,
            reqLim == "Chat_NoLimit" ? Loc.Get("Chat_NoLimit") : reqLim,
            tok,
            tokLim == "Chat_NoLimit" ? Loc.Get("Chat_NoLimit") : tokLim);
    }

    private void UpdateOffState()
    {
        bool off = !Services.Current.AiEnabled;
        OffPanel.Visibility = off ? Visibility.Visible : Visibility.Collapsed;
        ChatScroll.Visibility = off ? Visibility.Collapsed : Visibility.Visible;
        InputBox.IsEnabled = !off;
        SendButton.IsEnabled = !off && ViewModel.CanSend && !string.IsNullOrWhiteSpace(InputBox.Text);
    }

    private void UpdateBindings()
    {
        EmptyHint.Visibility = ViewModel.Messages.Count == 0 ? Visibility.Visible : Visibility.Collapsed;

        WarningBar.IsOpen = !string.IsNullOrEmpty(ViewModel.Warning);
        WarningBar.Message = ViewModel.Warning ?? "";

        bool hasError = !string.IsNullOrEmpty(ViewModel.ErrorText);
        ErrorBar.IsOpen = hasError;
        if (hasError)
        {
            ErrorBar.Title = Loc.Get(ChatViewModel.ErrorTitleKey(ViewModel.ErrorCode));
            var msg = ViewModel.ErrorText ?? "";
            if (ViewModel.RetryAt is { } r)
                msg += " " + Loc.Format("Chat_RetryAt", r.ToLocalTime().ToString("t", CultureInfo.CurrentCulture));
            ErrorBar.Message = msg;
        }
        TryAgainButton.Visibility = !string.IsNullOrEmpty(ViewModel.LastQuestion) ? Visibility.Visible : Visibility.Collapsed;

        ApprovalPanel.Visibility = ViewModel.HasApproval ? Visibility.Visible : Visibility.Collapsed;
        if (ViewModel.HasApproval)
        {
            var providerId = SettingsSchema.Get(Services.Current.Settings.Original, "ai.provider")?.GetValue<string>() ?? "gemini";
            var providerName = Loc.Get("S_ai_provider_" + providerId);
            ApprovalTitleBlock.Text = Loc.Format("Chat_ApprovalTitle", providerName);
            var body = Loc.Get("Chat_ApprovalBody");
            if (!string.IsNullOrEmpty(ViewModel.ApprovalNotice))
                body += "\n\n" + ViewModel.ApprovalNotice;
            ApprovalBodyBlock.Text = body;
        }

        StopButton.Visibility = ViewModel.Busy ? Visibility.Visible : Visibility.Collapsed;
        SendButton.IsEnabled = Services.Current.AiEnabled && ViewModel.CanSend && !string.IsNullOrWhiteSpace(InputBox.Text);

        UsageBlock.Text = FormatUsage(ViewModel.Usage);
        UsageBlock.Visibility = string.IsNullOrEmpty(UsageBlock.Text) ? Visibility.Collapsed : Visibility.Visible;
    }

    private void ScrollToBottom()
    {
        ChatScroll.ChangeView(null, ChatScroll.ScrollableHeight, null, false);
    }

    private async Task RefreshStatusAsync()
    {
        try
        {
            var r = await Services.Current.Api.AiStatusAsync();
            if (r.Ok && r.Value is not null)
            {
                LocalBar.IsOpen = r.Value.Local;
                if (r.Value.Usage is not null)
                {
                    ViewModel.Usage = r.Value.Usage;
                    UpdateBindings();
                }
            }
        }
        catch { }
    }

    public async Task SendAsync(string text)
    {
        if (!Services.Current.AiEnabled) return;
        text = text.Trim();
        if (text.Length == 0 || !ViewModel.CanSend) return;
        ViewModel.BeginSend(text);
        InputBox.Text = "";
        UpdateBindings();
        ScrollToBottom();
        await RunAsync(AgentApi.ChatSendLine(ViewModel.ChatId, text));
    }

    private async void AllowChat_Click(object sender, RoutedEventArgs e)
    {
        if (ViewModel.ApprovalToken is not { Length: > 0 } token) return;
        var line = AgentApi.ChatApproveLine(token, ViewModel.RemovedIds());
        ViewModel.ClearApproval();
        ViewModel.Busy = true;
        await RunAsync(line);
    }

    private async void DontShare_Click(object sender, RoutedEventArgs e)
    {
        if (ViewModel.ApprovalToken is not { Length: > 0 } token) return;
        ViewModel.ClearApproval();
        ViewModel.Busy = true;
        await RunAsync(AgentApi.ChatDenyLine(token));
    }

    /// <summary>Streams one chat request on its own connection. Stop cancels it; the agent then finishes or drops the
    /// turn on its side.</summary>
    private async Task RunAsync(string line)
    {
        _cts?.Cancel();
        var cts = _cts = new CancellationTokenSource();
        UpdateBindings();
        AgentResult res = default;
        bool got = false;
        try
        {
            await using var run = Services.Current.CreateRunClient();
            res = await AgentApi.ChatStreamAsync(run, line, ev => DispatcherQueue.TryEnqueue(() => OnEvent(ev)), cts.Token);
            got = true;
        }
        catch (OperationCanceledException) { }
        if (cts.IsCancellationRequested || !got)
        {
            ViewModel.Stopped();
        }
        else if (!res.Ok)
        {
            var msg = Loc.Text(SearchViewModel.ServerError(res.Error));
            if (string.IsNullOrWhiteSpace(msg)) msg = res.Error ?? Loc.Get("Chat_ErrGeneric");
            OnEvent(new ChatEvent("error", Code: "generic", Text: msg));
        }
        else _ = RefreshStatusAsync();
        UpdateBindings();
    }

    private void OnEvent(ChatEvent e)
    {
        ViewModel.Handle(e);
        if (e.Type == "error" && InputBox.Text.Length == 0 && ViewModel.LastQuestion is { } q) InputBox.Text = q;
        UpdateBindings();
        ScrollToBottom();
        if (e.Type is "done" or "approval") TurnCompleted?.Invoke();
    }

    private void TryAgain_Click(object sender, RoutedEventArgs e)
    {
        if (ViewModel.LastQuestion is { Length: > 0 } q)
        {
            InputBox.Text = "";
            _ = SendAsync(q);
        }
    }

    private void OpenSettings_Click(object sender, RoutedEventArgs e)
    {
        App.Current.ShowMain("settings:ai");
    }

    private void Source_Click(object sender, RoutedEventArgs e)
    {
        if (sender is Button { Tag: ChatSource s })
            SourceInvoked?.Invoke(s);
    }

    private async void ApprovalThumb_Loaded(object sender, RoutedEventArgs e)
    {
        if (sender is Image img && img.DataContext is ApprovalItemView item && item.Image is { Length: > 0 })
        {
            img.Source = await Images.DecodeAsync(item.Image, 160);
        }
    }

    private void SendButton_Click(object sender, RoutedEventArgs e)
    {
        if (ViewModel.CanSend && !string.IsNullOrWhiteSpace(InputBox.Text))
            _ = SendAsync(InputBox.Text);
    }

    private void StopButton_Click(object sender, RoutedEventArgs e) => Stop();

    /// <summary>Stops showing the running answer (Stop button, launcher hidden, Ask turned off).</summary>
    public void Stop() => _cts?.Cancel();

    public void FocusInput() => InputBox.Focus(FocusState.Programmatic);

    private void InputBox_TextChanged(object sender, TextChangedEventArgs e)
    {
        SendButton.IsEnabled = Services.Current.AiEnabled && ViewModel.CanSend && !string.IsNullOrWhiteSpace(InputBox.Text);
    }

    private void InputBox_PreviewKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key == VirtualKey.Enter)
        {
            var shift = Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift);
            bool isShiftDown = (shift & Windows.UI.Core.CoreVirtualKeyStates.Down) == Windows.UI.Core.CoreVirtualKeyStates.Down;
            if (!isShiftDown)
            {
                e.Handled = true;
                if (ViewModel.CanSend && !string.IsNullOrWhiteSpace(InputBox.Text))
                    _ = SendAsync(InputBox.Text);
            }
        }
    }
}
