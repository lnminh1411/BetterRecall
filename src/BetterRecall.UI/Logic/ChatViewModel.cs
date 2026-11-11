// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Collections.ObjectModel;
using System.Globalization;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic;

public sealed class ChatBubble : ObservableObject
{
    private string _text;
    private string _status = "";

    public string Role { get; }
    public string Text { get => _text; set => Set(ref _text, value); }
    public ObservableCollection<ChatSource> Sources { get; } = [];
    public string Status
    {
        get => _status;
        set
        {
            if (Set(ref _status, value))
                Raise(nameof(HasStatus));
        }
    }
    public bool IsUser => Role == "user";
    public bool IsAssistant => Role == "assistant";
    public bool HasSources => Sources.Count > 0;
    public bool HasStatus => !string.IsNullOrEmpty(Status);

    public ChatBubble(string role, string text = "", IEnumerable<ChatSource>? sources = null)
    {
        Role = role;
        _text = text;
        if (sources is not null)
        {
            foreach (var s in sources) Sources.Add(s);
        }
        Sources.CollectionChanged += (_, _) => Raise(nameof(HasSources));
    }
}

public sealed class ApprovalItemView : ObservableObject
{
    private bool _include = true;

    public ChatApprovalItem Item { get; }
    public bool Include { get => _include; set => Set(ref _include, value); }
    public string Id => Item.Id;
    public DateTimeOffset Ts => Item.Ts;
    public CaptureKind Kind => Item.Kind;
    public string App => Item.App;
    public string Title => Item.Title;
    public string Site => Item.Site;
    public string Text => Item.Text;
    public byte[]? Image => Item.Image;
    public bool HasImage => Item.Image is { Length: > 0 };

    public ApprovalItemView(ChatApprovalItem item, bool include = true)
    {
        Item = item;
        _include = include;
    }
}

public sealed class ChatViewModel : ObservableObject
{
    private string? _chatId;
    private string _title = "";
    private bool _busy;
    private string? _approvalToken;
    private string? _approvalShare;
    private string? _approvalNotice;
    private string? _warning;
    private string? _errorText;
    private string? _errorCode;
    private DateTimeOffset? _retryAt;
    private AiUsage? _usage;

    public string? ChatId { get => _chatId; set => Set(ref _chatId, value); }
    public string Title { get => _title; set => Set(ref _title, value); }
    public ObservableCollection<ChatBubble> Messages { get; } = [];
    public bool Busy
    {
        get => _busy;
        set { if (Set(ref _busy, value)) Raise(nameof(CanSend)); }
    }
    public ObservableCollection<ApprovalItemView> Approval { get; } = [];
    public string? ApprovalToken { get => _approvalToken; set => Set(ref _approvalToken, value); }
    public string? ApprovalShare { get => _approvalShare; set => Set(ref _approvalShare, value); }
    public string? ApprovalNotice { get => _approvalNotice; set => Set(ref _approvalNotice, value); }
    public bool HasApproval => Approval.Count > 0;
    public string? Warning { get => _warning; set => Set(ref _warning, value); }
    public string? ErrorText { get => _errorText; set => Set(ref _errorText, value); }
    public string? ErrorCode { get => _errorCode; set => Set(ref _errorCode, value); }
    public DateTimeOffset? RetryAt { get => _retryAt; set => Set(ref _retryAt, value); }
    public AiUsage? Usage { get => _usage; set => Set(ref _usage, value); }
    public bool CanSend => !Busy && !HasApproval;
    public string? LastQuestion { get; private set; }
    private bool _turnOpen;   // a question is in flight: its bubbles go if the agent rolls the turn back

    public ChatViewModel()
    {
        Approval.CollectionChanged += (_, _) =>
        {
            Raise(nameof(HasApproval));
            Raise(nameof(CanSend));
        };
    }

    public ChatBubble BeginSend(string question)
    {
        Warning = null;
        ErrorText = null;
        ErrorCode = null;
        RetryAt = null;
        LastQuestion = question;
        Messages.Add(new ChatBubble("user", question));
        var assistant = new ChatBubble("assistant", "");
        Messages.Add(assistant);
        _turnOpen = true;
        Busy = true;
        return assistant;
    }

    public void Handle(ChatEvent e)
    {
        switch (e.Type)
        {
            case "start":
                if (e.ChatId is not null) ChatId = e.ChatId;
                break;

            case "search":
                if (Messages.LastOrDefault(m => m.IsAssistant) is { } searchAssistant)
                    searchAssistant.Status = e.Query ?? "";
                break;

            case "text":
                if (Messages.LastOrDefault(m => m.IsAssistant) is { } textAssistant)
                {
                    textAssistant.Text += e.Text ?? "";
                    textAssistant.Status = "";
                }
                break;

            case "warning":
                Warning = e.Text;
                break;

            case "approval":
                Approval.Clear();
                if (e.Items is not null)
                {
                    foreach (var item in e.Items)
                        Approval.Add(new ApprovalItemView(item));
                }
                ApprovalToken = e.Token;
                ApprovalShare = e.Share;
                ApprovalNotice = e.Text;
                Busy = false;
                break;

            case "sources":
                if (Messages.LastOrDefault(m => m.IsAssistant) is { } srcAssistant && e.Sources is not null)
                {
                    foreach (var s in e.Sources) srcAssistant.Sources.Add(s);
                }
                break;

            case "done":
                _turnOpen = false;
                Busy = false;
                Usage = e.Usage;
                break;

            case "error":
                Busy = false;
                ErrorText = e.Text;
                ErrorCode = e.Code;
                RetryAt = e.RetryAt;
                // A failed question leaves the saved chat as it was, so its bubbles go too; LastQuestion keeps the text.
                if (_turnOpen && Messages.Count >= 2 && Messages[^1].IsAssistant && Messages[^2].IsUser)
                {
                    Messages.RemoveAt(Messages.Count - 1);
                    Messages.RemoveAt(Messages.Count - 1);
                }
                _turnOpen = false;
                break;
        }
    }

    /// <summary>The user pressed Stop: nothing more is shown for this question.</summary>
    public void Stopped()
    {
        Busy = false;
        _turnOpen = false;
        if (Messages.LastOrDefault() is { IsAssistant: true } a) a.Status = "";
    }

    public IReadOnlyList<string> RemovedIds() =>
        Approval.Where(i => !i.Include).Select(i => i.Id).ToList();

    public void ClearApproval()
    {
        Approval.Clear();
        ApprovalToken = null;
        ApprovalShare = null;
        ApprovalNotice = null;
    }

    public void Load(ChatDetail d)
    {
        ChatId = d.Chat.Id;
        Title = d.Chat.Title;
        Messages.Clear();
        foreach (var m in d.Messages)
            Messages.Add(new ChatBubble(m.Role, m.Text, m.Sources));
        ClearApproval();
        Warning = null;
        ErrorText = null;
        ErrorCode = null;
        RetryAt = null;
        Busy = false;
        _turnOpen = false;
    }

    public void Reset()
    {
        ChatId = null;
        Title = "";
        Messages.Clear();
        ClearApproval();
        Warning = null;
        ErrorText = null;
        ErrorCode = null;
        RetryAt = null;
        Busy = false;
        _turnOpen = false;
        LastQuestion = null;
    }

    /// <summary>A source card as a capture the detail pane can load.</summary>
    public static CaptureItem ToCaptureItem(ChatSource s) => new()
    {
        Id = s.Id,
        Ts = s.Ts,
        Kind = s.Kind,
        App = s.App,
        Title = s.Title,
        Site = s.Site,
        ReplayId = s.ReplayId,
        ReplayOffsetMs = s.ReplayOffsetMs,
        HasThumb = s.Kind != CaptureKind.Audio,
    };

    public static string ErrorTitleKey(string? code) => (code?.ToLowerInvariant()) switch
    {
        "auth" => "Chat_ErrAuth",
        "quota" => "Chat_ErrQuota",
        "rate" => "Chat_ErrRate",
        "overloaded" => "Chat_ErrOverloaded",
        "context" => "Chat_ErrContext",
        "model" => "Chat_ErrModel",
        "offline" => "Chat_ErrOffline",
        "timeout" => "Chat_ErrTimeout",
        "budget" => "Chat_ErrBudget",
        "refused" => "Chat_ErrRefused",
        _ => "Chat_ErrGeneric",
    };

    public static (string Requests, string RequestLimit, string Tokens, string TokenLimit)? UsageParts(AiUsage? u)
    {
        if (u is null) return null;
        return (
            u.Requests.ToString("N0", CultureInfo.InvariantCulture),
            u.RequestLimit == 0 ? "Chat_NoLimit" : u.RequestLimit.ToString("N0", CultureInfo.InvariantCulture),
            u.Tokens.ToString("N0", CultureInfo.InvariantCulture),
            u.TokenLimit == 0 ? "Chat_NoLimit" : u.TokenLimit.ToString("N0", CultureInfo.InvariantCulture)
        );
    }
}
