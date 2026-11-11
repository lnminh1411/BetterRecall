// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Collections.ObjectModel;
using System.Globalization;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic;

/// <summary>One search hit or timeline row. The view sets Thumb to a decoded image.</summary>
public sealed class CaptureItem : ObservableObject
{
    private object? _thumb;

    public required string Id { get; init; }
    public required DateTimeOffset Ts { get; init; }
    public required CaptureKind Kind { get; init; }
    public string App { get; init; } = "";
    public string Title { get; init; } = "";
    public string Snippet { get; init; } = "";
    public bool HasThumb { get; init; }
    public int? DurationS { get; init; }
    /// <summary>Search hit with no keyword match and a low score: shown with a "weak match" label.</summary>
    public bool IsWeak { get; init; }

    public bool IsAudio => Kind == CaptureKind.Audio;
    public string TimeText => Ts.ToLocalTime().ToString("g", CultureInfo.CurrentCulture);
    public string ShortTime => Ts.ToLocalTime().ToString("t", CultureInfo.CurrentCulture);
    public string DisplayTitle => string.IsNullOrWhiteSpace(Title) ? App : Title;
    /// <summary>Text shown in the tile when there is no image: transcript snippet for audio.</summary>
    public string TileText => IsAudio ? Snippet : "";
    public string Subtitle => string.IsNullOrWhiteSpace(Title) ? TimeText : App + " · " + TimeText;
    /// <summary>Narrator name for the tile: title, app, time (data only, no UI words to translate).</summary>
    public string AccessibleName => DisplayTitle + ", " + App + ", " + TimeText;

    public object? Thumb { get => _thumb; set => Set(ref _thumb, value); }

    public static CaptureItem From(SearchHit h) => new()
    {
        Id = h.Id, Ts = h.Ts, Kind = h.Kind, App = h.App, Title = h.Title, Snippet = h.Snippet, HasThumb = h.HasThumb, DurationS = h.DurationS, IsWeak = h.Weak,
    };

    public static CaptureItem From(CaptureMeta m) => new()
    {
        Id = m.Id, Ts = m.Ts, Kind = m.Kind, App = m.App, Title = m.Title, HasThumb = m.HasThumb,
        DurationS = m.DurationMs > 0 ? (int)Math.Round(m.DurationMs / 1000.0) : null,
    };
}

public enum TimeRange { Any, LastHour, Today, Last7Days, Last30Days }
public enum KindFilter { All, Screen, Audio }

/// <summary>Launcher search: debounce, filters, stale-response guard, keyboard selection.</summary>
public sealed class SearchViewModel : ObservableObject
{
    private readonly AgentApi _api;
    private readonly TimeProvider _clock;
    private readonly Debouncer _debouncer;
    private string _query = "";
    private TimeRange _range;
    private KindFilter _kind;
    private string _app = "";
    private bool _busy;
    private LocalizedText _message = new("Search_Hint");
    private int _selectedIndex = -1;
    private long _seq;

    public SearchViewModel(AgentApi api, TimeProvider? clock = null, TimeSpan? debounce = null)
    {
        _api = api;
        _clock = clock ?? TimeProvider.System;
        _debouncer = new Debouncer(debounce ?? TimeSpan.FromMilliseconds(180));
    }

    public ObservableCollection<CaptureItem> Results { get; } = [];

    public string Query { get => _query; set => Set(ref _query, value ?? ""); }
    public TimeRange Range { get => _range; set => Set(ref _range, value); }
    public KindFilter Kind { get => _kind; set => Set(ref _kind, value); }
    public string App { get => _app; set => Set(ref _app, value ?? ""); }
    public bool IsBusy { get => _busy; private set => Set(ref _busy, value); }
    public LocalizedText Message { get => _message; private set => Set(ref _message, value); }
    public int SelectedIndex { get => _selectedIndex; set => Set(ref _selectedIndex, value); }
    public int ResultLimit { get; set; } = 60;

    /// <summary>Raised when a search answered "Store is locked.".</summary>
    public event Action<AgentErrorKind>? Failed;

    /// <summary>Debounced search after a keystroke or filter change.</summary>
    public Task<bool> ScheduleAsync() => _debouncer.RunAsync(SearchNowAsync);

    public void CancelPending() => _debouncer.Cancel();

    public static (DateTimeOffset? From, DateTimeOffset? To) RangeBounds(TimeRange r, DateTimeOffset now)
    {
        var local = now.ToLocalTime();
        return r switch
        {
            TimeRange.LastHour => (now.AddHours(-1), null),
            TimeRange.Today => (new DateTimeOffset(local.Date, local.Offset), null),
            TimeRange.Last7Days => (now.AddDays(-7), null),
            TimeRange.Last30Days => (now.AddDays(-30), null),
            _ => (null, null),
        };
    }

    public SearchQuery? BuildQuery()
    {
        var text = Query.Trim();
        if (text.Length == 0) return null;
        if (text.Length > 1024) text = text[..1024];
        var (from, to) = RangeBounds(Range, _clock.GetUtcNow());
        CaptureKind? kind = Kind switch { KindFilter.Screen => CaptureKind.Screen, KindFilter.Audio => CaptureKind.Audio, _ => null };
        return new SearchQuery(text, ResultLimit, from, to, string.IsNullOrWhiteSpace(App) ? null : App.Trim(), kind);
    }

    public async Task SearchNowAsync(CancellationToken ct = default)
    {
        var seq = Interlocked.Increment(ref _seq);
        var q = BuildQuery();
        if (q is null)
        {
            Results.Clear();
            SelectedIndex = -1;
            Message = new LocalizedText("Search_Hint");
            IsBusy = false;
            return;
        }
        IsBusy = true;
        var r = await _api.SearchAsync(q, ct);
        if (seq != Interlocked.Read(ref _seq)) return;   // a newer search owns the list
        IsBusy = false;
        if (!r.Ok)
        {
            Results.Clear();
            SelectedIndex = -1;
            Message = ErrorText(r.Kind, r.Error);
            Failed?.Invoke(r.Kind);
            return;
        }
        SelectedIndex = -1;
        Results.Clear();
        foreach (var hit in r.Value!.Results) Results.Add(CaptureItem.From(hit));
        SelectedIndex = Results.Count > 0 ? 0 : -1;
        Message = Results.Count == 0 ? new LocalizedText("Search_NoResults") : new LocalizedText("Search_Count", Results.Count, r.Value.TookMs);
    }

    /// <summary>New index after an arrow key in a grid of the given column count. Clamped, -1 stays -1 unless moving forward.</summary>
    public static int Move(int current, int count, int delta)
    {
        if (count == 0) return -1;
        if (current < 0) return delta > 0 ? 0 : -1;
        return Math.Clamp(current + delta, 0, count - 1);
    }

    public static LocalizedText ErrorText(AgentErrorKind kind, string? error) => kind switch
    {
        AgentErrorKind.NotRunning => new LocalizedText("Err_NotRunning"),
        AgentErrorKind.NotTrusted => new LocalizedText("Err_NotTrusted"),
        AgentErrorKind.Locked => new LocalizedText("Err_Locked"),
        AgentErrorKind.Protocol => new LocalizedText("Err_Protocol"),
        _ => ServerError(error),
    };

    /// <summary>Known agent errors map to translated text; others are shown as sent.</summary>
    public static LocalizedText ServerError(string? error) => error switch
    {
        AgentErrors.NotFound => new LocalizedText("Err_NotFound"),
        AgentErrors.UnlockCancelled => new LocalizedText("Err_UnlockCancelled"),
        AgentErrors.WrongPassphrase => new LocalizedText("Err_WrongPassphrase"),
        AgentErrors.UnlockRequired => new LocalizedText("Err_UnlockRequired"),
        AgentErrors.InvalidToken => new LocalizedText("Err_InvalidToken"),
        AgentErrors.Internal or null => new LocalizedText("Err_Internal"),
        _ when AgentErrors.InvalidKey(error) is { } key => new LocalizedText("Err_InvalidKey", key),
        _ => new LocalizedText("Err_Raw", error),
    };
}

/// <summary>Detail pane: full keyframe, title, app, time, transcript for audio.</summary>
public sealed class DetailViewModel(AgentApi api) : ObservableObject
{
    private CaptureItem? _item;
    private string _text = "";
    private byte[]? _image;
    private bool _busy;
    private LocalizedText _message = LocalizedText.Empty;
    private long _seq;

    public CaptureItem? Item { get => _item; private set => Set(ref _item, value); }
    public string Text { get => _text; private set => Set(ref _text, value); }
    public byte[]? Image { get => _image; private set => Set(ref _image, value); }
    public bool IsBusy { get => _busy; private set => Set(ref _busy, value); }
    public LocalizedText Message { get => _message; private set => Set(ref _message, value); }

    public event Action<AgentErrorKind>? Failed;

    public async Task LoadAsync(CaptureItem? item, CancellationToken ct = default)
    {
        var seq = Interlocked.Increment(ref _seq);
        Item = item;
        Image = null;
        Text = "";
        Message = LocalizedText.Empty;
        if (item is null) return;
        IsBusy = true;
        var detail = await api.GetAsync(item.Id, ct);
        if (seq != _seq) return;
        if (detail.Ok) Text = detail.Value!.Text;
        else
        {
            Message = SearchViewModel.ErrorText(detail.Kind, detail.Error);
            Failed?.Invoke(detail.Kind);
        }
        if (item.Kind == CaptureKind.Screen && item.HasThumb && detail.Kind != AgentErrorKind.Locked)
        {
            var kf = await api.KeyframeAsync(item.Id, ct);
            if (seq != _seq) return;
            if (kf.Ok) Image = kf.Value!.Bytes;
            else if (Message.IsEmpty) Message = SearchViewModel.ErrorText(kf.Kind, kf.Error);
        }
        IsBusy = false;
    }

    public void Clear()
    {
        Interlocked.Increment(ref _seq);
        Item = null;
        Image = null;
        Text = "";
        Message = LocalizedText.Empty;
        IsBusy = false;
    }
}

/// <summary>Timeline: one local day, per-hour counts, optional hour filter.</summary>
public sealed class TimelineViewModel(AgentApi api, TimeZoneInfo? zone = null) : ObservableObject
{
    public const int DayLimit = 5000;
    private readonly TimeZoneInfo _zone = zone ?? TimeZoneInfo.Local;
    private DateOnly _day = DateOnly.FromDateTime(DateTime.Now);
    private int? _hour;
    private List<CaptureMeta> _all = [];
    private bool _busy;
    private LocalizedText _message = LocalizedText.Empty;

    public DateOnly Day { get => _day; set => Set(ref _day, value); }
    public int? Hour { get => _hour; private set => Set(ref _hour, value); }
    public int[] HourCounts { get; private set; } = new int[24];
    public ObservableCollection<CaptureItem> Items { get; } = [];
    public bool IsBusy { get => _busy; private set => Set(ref _busy, value); }
    public LocalizedText Message { get => _message; private set => Set(ref _message, value); }
    public bool Truncated { get; private set; }

    public event Action<AgentErrorKind>? Failed;

    public (DateTimeOffset From, DateTimeOffset To) DayBounds(DateOnly day)
    {
        var start = day.ToDateTime(TimeOnly.MinValue, DateTimeKind.Unspecified);
        var end = start.AddDays(1);
        return (new DateTimeOffset(start, _zone.GetUtcOffset(start)), new DateTimeOffset(end, _zone.GetUtcOffset(end)));
    }

    public async Task LoadAsync(CancellationToken ct = default)
    {
        IsBusy = true;
        var (from, to) = DayBounds(Day);
        var r = await api.TimelineAsync(from, to, DayLimit, ct);
        IsBusy = false;
        if (!r.Ok)
        {
            _all = [];
            HourCounts = new int[24];
            Raise(nameof(HourCounts));
            Items.Clear();
            Message = SearchViewModel.ErrorText(r.Kind, r.Error);
            Failed?.Invoke(r.Kind);
            return;
        }
        _all = r.Value!;
        Truncated = _all.Count >= DayLimit;
        var counts = new int[24];
        foreach (var m in _all) counts[TimeZoneInfo.ConvertTime(m.Ts, _zone).Hour]++;
        HourCounts = counts;
        Raise(nameof(HourCounts));
        Apply();
    }

    public void SelectHour(int? hour)
    {
        Hour = hour is >= 0 and < 24 ? hour : null;
        Apply();
    }

    public Task ShiftDayAsync(int days, CancellationToken ct = default)
    {
        Day = Day.AddDays(days);
        Hour = null;
        return LoadAsync(ct);
    }

    private void Apply()
    {
        Items.Clear();
        foreach (var m in _all)   // newest first, as sent
        {
            if (Hour is { } h && TimeZoneInfo.ConvertTime(m.Ts, _zone).Hour != h) continue;
            Items.Add(CaptureItem.From(m));
        }
        Message = Items.Count == 0 ? new LocalizedText("Timeline_Empty")
            : Truncated ? new LocalizedText("Timeline_CountTruncated", Items.Count) : new LocalizedText("Timeline_Count", Items.Count);
    }
}

/// <summary>Formats AgentStatus into short translated parts for the status bar.</summary>
public static class StatusText
{
    public static IReadOnlyList<LocalizedText> Describe(AgentStatus s, DateTimeOffset now)
    {
        var parts = new List<LocalizedText>();
        if (s.Paused)
        {
            var reason = string.IsNullOrEmpty(s.PauseReason) ? "user" : s.PauseReason;
            parts.Add(s.PausedUntil is { } until && until > now
                ? new LocalizedText("Status_PausedUntil", new LocalizedText("Reason_" + reason), until.ToLocalTime().ToString("t", CultureInfo.CurrentCulture))
                : new LocalizedText("Status_Paused", new LocalizedText("Reason_" + reason)));
        }
        else parts.Add(new LocalizedText(s.Capturing ? "Status_Capturing" : "Status_Idle"));

        parts.Add(s.Locked ? new LocalizedText("Status_Locked")
            : s.UnlockedUntil is { } u ? new LocalizedText("Status_UnlockedUntil", u.ToLocalTime().ToString("t", CultureInfo.CurrentCulture))
            : new LocalizedText("Status_Unlocked"));
        if (s.Audio.System || s.Audio.Mic)
            parts.Add(new LocalizedText(s.Audio.System && s.Audio.Mic ? "Status_AudioBoth" : s.Audio.System ? "Status_AudioSystem" : "Status_AudioMic"));
        parts.Add(new LocalizedText("Status_Device", s.Device));
        if (s.Queue.Pending > 0) parts.Add(new LocalizedText("Status_Queue", s.Queue.Pending));
        parts.Add(new LocalizedText("Status_Store", s.Store.Rows, FormatBytes(s.Store.Bytes)));
        return parts;
    }

    public static string FormatBytes(long bytes)
    {
        string[] units = ["B", "KB", "MB", "GB", "TB"];
        double v = bytes;
        int i = 0;
        while (v >= 1024 && i < units.Length - 1) { v /= 1024; i++; }
        return v.ToString(i == 0 ? "0" : "0.#", CultureInfo.CurrentCulture) + " " + units[i];
    }

    public static string FormatDuration(int ms)
    {
        var t = TimeSpan.FromMilliseconds(ms);
        return t.TotalHours >= 1 ? t.ToString(@"h\:mm\:ss", CultureInfo.InvariantCulture) : t.ToString(@"m\:ss", CultureInfo.InvariantCulture);
    }
}

/// <summary>Unlock screen logic. Passphrase mode comes from security.keyProtection.</summary>
public static class UnlockLogic
{
    public static bool UsesPassphrase(string? keyProtection) => string.Equals(keyProtection, "passphrase", StringComparison.OrdinalIgnoreCase);
}

/// <summary>Danger-zone flows.</summary>
public static class PurgeFlow
{
    /// <summary>Typed confirmation: trimmed, case-insensitive in the current culture.</summary>
    public static bool PhraseMatches(string? typed, string phrase) =>
        !string.IsNullOrWhiteSpace(typed) && string.Compare(typed.Trim(), phrase.Trim(), CultureInfo.CurrentCulture, CompareOptions.IgnoreCase) == 0;

    /// <summary>
    /// purge all: Windows Hello unlock (purge all needs an unlock in the last 60 s), then "purge all", then "purge confirm".
    /// Stops at the first failure.
    /// </summary>
    public static async Task<AgentResult<int>> PurgeAllAsync(AgentApi api, string? passphrase, CancellationToken ct = default)
    {
        var u = await api.UnlockAsync(passphrase, ct);
        if (!u.Ok) return new AgentResult<int>(false, 0, u.Kind, u.Error);
        var p = await api.PurgeAllAsync(ct);
        if (!p.Ok) return new AgentResult<int>(false, 0, p.Kind, p.Error);
        return await api.PurgeConfirmAsync(p.Value!.Confirm, ct);
    }
}
