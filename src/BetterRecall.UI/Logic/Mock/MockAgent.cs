// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic.Mock;

public sealed record MockOptions(
    bool StartLocked = false,
    bool Passphrase = false,
    bool FirstRun = false,
    bool Capturable = false,
    int Seed = 42,
    bool PasswordUnlock = false,   // security.unlockMethod password (the mock password is anything but "wrong")
    bool StoragePaused = false);   // --mock-storage: capture paused for storage (banner)

/// <summary>
/// Fake command processor for --mock and tests. Answers every UI pipe verb with plausible data,
/// including the locked state, validation errors and a replay list. Nothing it holds is real.
/// </summary>
public sealed class MockAgent
{
    private sealed record Row(CaptureMeta Meta, string Text);
    private readonly List<HistoryEntry> _history = [];

    private readonly object _lock = new();
    private readonly List<Row> _rows = [];
    private readonly List<ReplayClip> _replays = [];
    private readonly List<AuditEntry> _audit = [];
    private readonly Dictionary<string, (PurgeScope? Scope, DateTimeOffset Expires)> _tokens = [];
    private readonly Dictionary<string, byte[]> _imageCache = [];
    private readonly TimeProvider _clock;
    private JsonObject _settings;
    private bool _locked;
    private DateTimeOffset? _unlockedUntil;
    private DateTimeOffset _lastUnlock = DateTimeOffset.MinValue;
    private bool _paused;
    private string? _pauseReason;
    private DateTimeOffset? _pausedUntil;
    private int _pending = 3;
    private bool _passwordSet = true;
    private int _passwordFailures;
    private DateTimeOffset _passwordBlockedUntil;
    private bool _aiKeySet = true;
    private readonly Dictionary<string, (string Id, DateTimeOffset Expires)> _describeTokens = [];

    public const string WrongPassphrase = "wrong";

    /// <summary>Raised on pause, resume, lock, unlock, consent and settings changes.</summary>
    public event Action? StatusChanged;

    /// <summary>Delay for verbs that take time in the real agent (Hello prompt, bench, replay save). Zero in tests.</summary>
    public TimeSpan SlowVerbDelay { get; set; } = TimeSpan.FromMilliseconds(400);

    public MockAgent(MockOptions? options = null, TimeProvider? clock = null)
    {
        var o = options ?? new MockOptions();
        _clock = clock ?? TimeProvider.System;
        var s = new Settings();
        s.Capture.Consented = !o.FirstRun;
        s.Security.KeyProtection = o.Passphrase ? "passphrase" : "auto";
        s.Security.ExcludeOwnWindows = !o.Capturable;
        if (o.PasswordUnlock) s.Security.UnlockMethod = "password";
        s.Ai.Enabled = true;   // demo: Describe shows in the detail pane (off by default in the real agent)
        _settings = (JsonObject)JsonNode.Parse(JsonSerializer.Serialize(s, ContractsJson.Default.Settings))!;
        _locked = o.StartLocked;
        if (o.StoragePaused) { _paused = true; _pauseReason = "storage"; }
        if (!_locked) _unlockedUntil = _clock.GetUtcNow().AddMinutes(15);
        Generate(o.Seed);
        if (!o.FirstRun) _lastBench = MockBench.Build(_settings, _clock.GetUtcNow().AddDays(-2), _rows.Count);
    }

    private DateTimeOffset Now => _clock.GetUtcNow();

    // ---------------- data ----------------

    private static readonly (string App, string[] Titles)[] Apps =
    [
        ("chrome.exe", ["Acme invoice 4471 - Gmail", "Quarterly planning - Google Docs", "NavigationView class - Microsoft Learn", "Flights Hanoi to Tokyo - Google Flights", "Pull request #12: replay export - GitHub"]),
        ("Code.exe", ["AgentClient.cs - BetterRecall - Visual Studio Code", "settings.json - BetterRecall - Visual Studio Code", "README.md - notes - Visual Studio Code"]),
        ("ms-teams.exe", ["Daily standup | Microsoft Teams", "Chat | Design review | Microsoft Teams"]),
        ("explorer.exe", ["Downloads - File Explorer", "Pictures - File Explorer"]),
        ("WINWORD.EXE", ["Thesis draft v3.docx - Word", "Meeting notes 2026-11.docx - Word"]),
        ("EXCEL.EXE", ["Budget 2026.xlsx - Excel"]),
        ("WindowsTerminal.exe", ["dotnet build - Windows Terminal", "git log - Windows Terminal"]),
        ("Spotify.exe", ["Spotify Premium"]),
    ];

    /// <summary>Browser host for a demo browser title (what the agent reads from the address bar).</summary>
    private static string SiteOf(string title) =>
        title.EndsWith("Gmail", StringComparison.Ordinal) ? "mail.google.com"
        : title.EndsWith("Google Docs", StringComparison.Ordinal) ? "docs.google.com"
        : title.EndsWith("Microsoft Learn", StringComparison.Ordinal) ? "learn.microsoft.com"
        : title.EndsWith("Google Flights", StringComparison.Ordinal) ? "google.com"
        : title.EndsWith("GitHub", StringComparison.Ordinal) ? "github.com" : "";

    /// <summary>Demo OCR text for some screens, as stored: redacted values are already gone (the card number after
    /// "Pay by card:"). Other screens have no text, so get returns the title.</summary>
    private static readonly Dictionary<string, string> ScreenTexts = new()
    {
        ["Acme invoice 4471 - Gmail"] = "Inbox\nAcme Billing\nInvoice 4471\nAmount due: 1,240.00 EUR\nDue date: 30 November 2026\nPay by card:\nReply Forward",
        ["Quarterly planning - Google Docs"] = "Quarterly planning\nGoals: ship replay export, settings search and text recognition\nOwner: design team\nReview on Friday",
        ["Flights Hanoi to Tokyo - Google Flights"] = "Hanoi (HAN) to Tokyo (NRT)\nDeparture 14 Dec\nVietnam Airlines 6 h 05 min 412 USD\nJapan Airlines 5 h 50 min 455 USD",
        ["Budget 2026.xlsx - Excel"] = "Budget 2026\nCategory Q1 Q2 Q3 Q4\nHosting 1200 1200 1300 1300\nTravel 800 400 950 600\nTotal",
        ["Daily standup | Microsoft Teams"] = "Daily standup\nYesterday: fixed the timeline paging test\nToday: text recognition settings page\nBlockers: none",
    };

    private static readonly string[] Transcripts =
    [
        "Let's move the release to Friday and ship the replay buffer first.",
        "The invoice from Acme is due at the end of the month, can you check the amount?",
        "Can you share the slides from the quarterly planning meeting?",
        "The build is green again, the flaky test was the timeline paging.",
        "Remember to book the flight to Tokyo before the price goes up.",
        "Next sprint we focus on the settings page and the unlock flow.",
    ];

    private void Generate(int seed)
    {
        var rnd = new Random(seed);
        var now = Now;
        var t = now.AddDays(-3);
        int n = 0;
        while (t < now)
        {
            var local = t.ToLocalTime();
            if (local.Hour is >= 8 and < 24)
            {
                if (rnd.Next(12) == 0)
                {
                    var text = Transcripts[rnd.Next(Transcripts.Length)];
                    var src = rnd.Next(2) == 0 ? "ms-teams.exe" : "chrome.exe";
                    int dur = rnd.Next(5, 30) * 1000;
                    _rows.Add(new Row(new CaptureMeta($"a_{n++:D6}", t, CaptureKind.Audio, src, "", 0, dur, false, true), text));
                }
                else
                {
                    var (app, titles) = Apps[rnd.Next(Apps.Length)];
                    var title = titles[rnd.Next(titles.Length)];
                    _rows.Add(new Row(new CaptureMeta($"f_{n++:D6}", t, CaptureKind.Screen, app, title, rnd.Next(2), 0, rnd.Next(20) != 0, rnd.Next(30) != 0,
                        app == "chrome.exe" ? SiteOf(title) : ""), ScreenTexts.GetValueOrDefault(title, title)));
                }
            }
            t = t.AddSeconds(rnd.Next(60, 420));
        }
        _rows.Sort((a, b) => b.Meta.Ts.CompareTo(a.Meta.Ts));
        foreach (var (q, h) in new[] { ("acme invoice", 2), ("flight to tokyo", 5), ("standup notes", 26), ("budget 2026", 49) })
            _history.Add(new HistoryEntry(q, now.AddHours(-h)));

        for (int i = 0; i < 4; i++)
        {
            var ts = now.AddHours(-(i * 7 + 1)).AddMinutes(-rnd.Next(60));
            var clip = new ReplayClip($"r_{i:D3}", ts, 120_000 - i * 15_000, 60_000_000L + rnd.Next(30_000_000), i != 2,
                $@"C:\Users\you\AppData\Local\BetterRecall\replays\{ts:yyyyMMdd-HHmmss}.mp4{(i != 2 ? ".enc" : "")}");
            _replays.Add(clip);
            // Frames sampled every 5 s of the saved clip (replay.searchable): Replay matches in search.
            var (rapp, rtitles) = Apps[rnd.Next(Apps.Length)];
            for (int off = 0; off < clip.DurationMs; off += 5000)
            {
                var title = rtitles[(off / 5000) % rtitles.Length];
                _rows.Add(new Row(new CaptureMeta($"p_{n++:D6}", clip.Ts.AddMilliseconds(off - clip.DurationMs), CaptureKind.Replay, rapp, title, 0, 0, true, true,
                    rapp == "chrome.exe" ? SiteOf(title) : "", clip.Id, off), title));
            }
        }
        _rows.Sort((a, b) => b.Meta.Ts.CompareTo(a.Meta.Ts));

        for (int i = 0; i < 25; i++)
        {
            var verbs = new[] { "search", "get", "thumb", "status", "timeline", "connect" };
            var pipe = i % 7 == 0 ? "mcp" : i % 5 == 0 ? "ipc" : "ui";
            var verb = verbs[rnd.Next(verbs.Length)];
            var image = pipe switch { "mcp" => @"C:\Program Files\BetterRecall\betterrecall-mcp.exe", "ipc" => @"C:\Program Files\BetterRecall\betterrecall.exe", _ => @"C:\Program Files\BetterRecall\ui\BetterRecall.UI.exe" };
            _audit.Add(new AuditEntry(now.AddMinutes(-i * 37), pipe, 4000 + rnd.Next(30000), image, verb, verb == "search" ? rnd.Next(0, 11) : 1,
                verb == "search" ? Convert.ToHexString(SHA256.HashData(BitConverter.GetBytes(i))).ToLowerInvariant() : ""));
        }
    }

    // ---------------- dispatcher ----------------

    private static readonly string[] Verbs =
        ["ping", "version", "help", "status", "search", "get", "thumb", "subscribe", "pause", "resume", "purge",
         "unlock", "lock", "consent", "settings", "timeline", "keyframe", "replay", "panic", "audit", "bench", "facets", "history",
         "password", "ai", "describe", "export"];

    private static readonly HashSet<string> DataVerbs = ["search", "get", "thumb", "keyframe", "timeline", "audit", "replay list", "replay export", "facets", "history", "describe", "export"];

    public bool IsLocked { get { lock (_lock) return _locked; } }

    /// <summary>One request line in, one response line out. Never throws.</summary>
    public async Task<string> HandleAsync(string line, CancellationToken ct = default)
    {
        try
        {
            return await DispatchAsync(line.Trim(), ct);
        }
        catch (OperationCanceledException) { throw; }
        catch (Exception)
        {
            return Envelope.Error(AgentErrors.Internal);
        }
    }

    private async Task<string> DispatchAsync(string line, CancellationToken ct)
    {
        if (line.Length == 0) return Envelope.Error("Unknown command ''.");
        int sp = line.IndexOf(' ');
        var verb = (sp < 0 ? line : line[..sp]).ToLowerInvariant();
        var rest = sp < 0 ? "" : line[(sp + 1)..].Trim();
        var args = rest.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        var full = verb is "replay" && args.Length > 0 ? verb + " " + args[0].ToLowerInvariant() : verb;

        lock (_lock)
        {
            ExpireUnlock();
            if (DataVerbs.Contains(full) && _locked && Bool("security.requireUnlock"))
                return Envelope.Error(AgentErrors.StoreLocked);
        }

        switch (verb)
        {
            case "ping": return Envelope.Ok("{\"pong\":true}");
            case "version": return Envelope.Ok(new VersionReply("0.1.0-mock", 1), ContractsJson.Default.VersionReply);
            case "help":
                return Envelope.Ok(new JsonObject { ["verbs"] = new JsonArray(Verbs.Select(v => (JsonNode)v).ToArray()) }.ToJsonString());
            case "status": return Envelope.Ok(StatusJson());
            case "search": return Search(rest);
            case "get": return args.Length == 1 ? Get(args[0]) : Envelope.Error("Usage: get <id>");
            case "thumb":
            {
                if (args.Length is < 1 or > 2) return Envelope.Error("Usage: thumb <id> [width]");
                int w = 320;
                if (args.Length == 2 && (!int.TryParse(args[1], NumberStyles.None, CultureInfo.InvariantCulture, out w) || w is < 64 or > 1280))
                    return Envelope.Error("Invalid width.");
                return Image(args[0], w);
            }
            case "keyframe": return args.Length == 1 ? Image(args[0], 1280) : Envelope.Error("Usage: keyframe <id>");
            case "timeline": return Timeline(args);
            case "pause":
            {
                int? secs = null;
                if (args.Length == 1)
                {
                    if (!int.TryParse(args[0], NumberStyles.None, CultureInfo.InvariantCulture, out var s) || s < 1) return Envelope.Error("Usage: pause [seconds]");
                    secs = s;
                }
                lock (_lock) { _paused = true; _pauseReason = "user"; _pausedUntil = secs is { } x ? Now.AddSeconds(x) : null; }
                StatusChanged?.Invoke();
                return Envelope.Ok("null");
            }
            case "resume":
                lock (_lock) { _paused = false; _pauseReason = null; _pausedUntil = null; }
                StatusChanged?.Invoke();
                return Envelope.Ok("null");
            case "unlock": return await UnlockAsync(rest, ct);
            case "lock":
                lock (_lock) { _locked = true; _unlockedUntil = null; }
                StatusChanged?.Invoke();
                return Envelope.Ok("null");
            case "consent":
                lock (_lock) SettingsSchema.Set(_settings, "capture.consented", true);
                StatusChanged?.Invoke();
                return Envelope.Ok("null");
            case "settings": return SettingsVerb(args, rest);
            case "replay": return await ReplayAsync(args, rest, ct);
            case "panic":
                lock (_lock) { _imageCache.Clear(); _pending = 0; }
                StatusChanged?.Invoke();
                return Envelope.Ok("null");
            case "purge": return Purge(args);
            case "facets": return Facets(args);
            case "history": return History(rest);
            case "password": return PasswordVerb(rest);
            case "ai": return AiVerb(rest);
            case "describe": return await DescribeAsync(args, ct);
            case "export": return await ExportAsync(rest, ct);
            case "audit":
            {
                int limit = 100;
                if (args.Length == 1 && (!int.TryParse(args[0], NumberStyles.None, CultureInfo.InvariantCulture, out limit) || limit < 1))
                    return Envelope.Error("Usage: audit [limit]");
                List<AuditEntry> items;
                lock (_lock) items = _audit.Take(limit).ToList();
                return Items(items, ContractsJson.Default.ItemsReplyAuditEntry);
            }
            case "bench":
                return await BenchAsync(args, ct);
            case "subscribe":
                return args.Length == 0 ? Envelope.Ok("{\"subscribed\":true}")
                    : args is ["captures"] ? Envelope.Ok("{\"subscribed\":\"captures\"}") : Envelope.Error("Usage: subscribe [captures]");
            default:
                return Envelope.Error($"Unknown command '{verb}'.");
        }
    }

    private static string Items<T>(List<T> list, System.Text.Json.Serialization.Metadata.JsonTypeInfo<ItemsReply<T>> info) =>
        Envelope.Ok(new ItemsReply<T>(list), info);

    private bool Bool(string key) => SettingsSchema.Get(_settings, key) is JsonValue v && v.TryGetValue<bool>(out var b) && b;

    private void ExpireUnlock()
    {
        if (!_locked && _unlockedUntil is { } u && u <= Now) { _locked = true; _unlockedUntil = null; }
    }

    public AgentStatus Status()
    {
        lock (_lock)
        {
            ExpireUnlock();
            bool consented = Bool("capture.consented");
            bool paused = _paused || !consented;
            var oldest = _rows.Count > 0 ? _rows[^1].Meta.Ts : (DateTimeOffset?)null;
            var newest = _rows.Count > 0 ? _rows[0].Meta.Ts : (DateTimeOffset?)null;
            return new AgentStatus(
                Capturing: !paused && Bool("capture.enabled"),
                Paused: paused,
                PausedUntil: _pausedUntil,
                PauseReason: !consented ? "consent" : _pauseReason,
                Locked: _locked,
                UnlockedUntil: _locked ? null : _unlockedUntil,
                Audio: new AudioState(Bool("audio.system"), Bool("audio.mic")),
                Queue: new QueueState(_pending),
                Device: "NPU",
                Store: new StoreStats(_rows.Count, _rows.Count * 41_000L, 0, oldest, newest, _pending, _rows.Count(r => r.Meta.Embedded)),
                RetentionDays: SettingsSchema.Get(_settings, "retention.days")?.GetValue<int>() ?? 30);
        }
    }

    private string StatusJson() => JsonSerializer.Serialize(Status(), ContractsJson.Default.AgentStatus);

    private void Audit(string verb, int count, string? query = null)
    {
        _audit.Insert(0, new AuditEntry(Now, "ui", Environment.ProcessId, Environment.ProcessPath ?? "", verb, count,
            query is null ? "" : Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(query))).ToLowerInvariant()));
        if (_audit.Count > 500) _audit.RemoveAt(_audit.Count - 1);
    }

    // ---------------- verbs ----------------

    private BenchReport? _lastBench;

    private async Task<string> BenchAsync(string[] args, CancellationToken ct)
    {
        switch (args.Length == 0 ? "" : args[0].ToLowerInvariant())
        {
            case "":
                await Task.Delay(SlowVerbDelay * 2, ct);
                return Envelope.Ok(MockBench.Inference, ContractsJson.Default.BenchResult);
            case "last":
                lock (_lock)
                    return _lastBench is null ? Envelope.Error(AgentErrors.NotFound) : Envelope.Ok(_lastBench, ContractsJson.Default.BenchReport);
            case "full":
                await Task.Delay(SlowVerbDelay * 15, ct);   // minutes in the real agent
                lock (_lock)
                {
                    _lastBench = MockBench.Build(_settings, Now, _rows.Count);
                    return Envelope.Ok(_lastBench, ContractsJson.Default.BenchReport);
                }
            default:
                return Envelope.Error("Usage: bench | bench full | bench last");
        }
    }

    private string Search(string rest)
    {
        if (rest.Length == 0) return Envelope.Error("Usage: search <text> | search <json>");
        string q;
        int limit = 10;
        DateTimeOffset? from = null, to = null;
        string? app = null, site = null;
        HashSet<CaptureKind>? kinds = null;
        if (rest[0] == '{')
        {
            JsonObject o;
            try { o = JsonNode.Parse(rest) as JsonObject ?? throw new JsonException(); }
            catch (JsonException) { return Envelope.Error(AgentErrors.InvalidJson); }
            q = o["q"]?.GetValue<string>()?.Trim() ?? "";
            if (q.Length is 0 or > 1024) return Envelope.Error("Invalid q.");
            if (o["limit"] is { } l) limit = Math.Clamp(l.GetValue<int>(), 1, 200);
            if (o["from"] is { } f) { if (!DateTimeOffset.TryParse(f.GetValue<string>(), CultureInfo.InvariantCulture, out var v)) return Envelope.Error("Invalid from."); from = v; }
            if (o["to"] is { } t) { if (!DateTimeOffset.TryParse(t.GetValue<string>(), CultureInfo.InvariantCulture, out var v)) return Envelope.Error("Invalid to."); to = v; }
            if (from is not null && to is not null && from >= to) return Envelope.Error("Invalid from.");
            app = o["app"]?.GetValue<string>();
            site = o["site"]?.GetValue<string>();
            if (o["kind"] is JsonArray ka)
            {
                kinds = [];
                foreach (var k in ka)
                {
                    var s = k?.GetValue<string>();
                    if (s == "screen") kinds.Add(CaptureKind.Screen);
                    else if (s == "audio") kinds.Add(CaptureKind.Audio);
                    else if (s == "replay") kinds.Add(CaptureKind.Replay);
                    else return Envelope.Error("Invalid kind.");
                }
            }
            if (o["mode"] is { } m && m.GetValue<string>() is not ("hybrid" or "text" or "vector")) return Envelope.Error("Invalid mode.");
        }
        else q = rest;

        if (q.Equals("error", StringComparison.OrdinalIgnoreCase)) return Envelope.Error(AgentErrors.Internal);

        var tokens = q.ToLowerInvariant().Split(' ', StringSplitOptions.RemoveEmptyEntries);
        var hits = new List<SearchHit>();
        lock (_lock)
        {
            foreach (var r in _rows)
            {
                var m = r.Meta;
                if (from is { } f && m.Ts < f) continue;
                if (to is { } t && m.Ts >= t) continue;
                if (app is not null && !m.App.Equals(app, StringComparison.OrdinalIgnoreCase)) continue;
                if (site is not null && !m.Site.Equals(site, StringComparison.OrdinalIgnoreCase)) continue;
                if (kinds is not null && !kinds.Contains(m.Kind)) continue;
                if (!m.Embedded) continue;
                var hay = (m.Title + " " + m.App + " " + r.Text).ToLowerInvariant();
                int matched = tokens.Count(hay.Contains);
                // Demo stand-in for vector-only hits: a stable few screens per query show up as Visual matches.
                bool visual = matched == 0 && m.Kind == CaptureKind.Screen && m.HasThumb && (uint)StableHash(m.Id + "|" + q.ToLowerInvariant()) % 23 == 0;
                if (matched == 0 && !visual) continue;
                float score = visual ? 0.4f + (uint)StableHash(m.Id) % 20 / 100f
                    : (float)Math.Round(0.45 + 0.5 * matched / tokens.Length - (Now - m.Ts).TotalDays * 0.02, 3);
                // Screens snippet the title unless a word is only in the OCR text (as the real store does).
                var title = m.Title.ToLowerInvariant();
                var shown = m.Kind == CaptureKind.Screen && (r.Text == m.Title || tokens.Any(title.Contains) || !tokens.Any(r.Text.ToLowerInvariant().Contains)) ? m.Title : r.Text;
                var snippet = shown.Length > 300 ? shown[..300] : shown;
                hits.Add(new SearchHit(m.Id, m.Ts, m.Kind, m.App, m.Title, Math.Clamp(score, 0, 1), snippet, m.HasThumb,
                    m.Kind == CaptureKind.Audio ? m.DurationMs / 1000 : null,
                    Math.Clamp(score, 0, 1), matched > 0, matched == 0 && score < 0.5f, m.Site, m.ReplayId, m.ReplayOffsetMs));
            }
            hits = hits.OrderByDescending(h => h.Score).ThenByDescending(h => h.Ts).ToList();
            Audit("search", Math.Min(hits.Count, limit), q);
        }
        bool truncated = hits.Count > limit;
        var res = new SearchResponse(Math.Min(hits.Count, limit), truncated, 12 + hits.Count % 30, hits.Take(limit).ToList());
        return Envelope.Ok(res, ContractsJson.Default.SearchResponse);
    }

    private string Get(string id)
    {
        lock (_lock)
        {
            var r = _rows.FirstOrDefault(x => x.Meta.Id == id);
            if (r is null) return Envelope.Error(AgentErrors.NotFound);
            Audit("get", 1);
            var m = r.Meta;
            return Envelope.Ok(new CaptureDetail(m.Id, m.Ts, m.Kind, m.App, m.Title, r.Text, m.HasThumb,
                m.Kind == CaptureKind.Audio ? m.DurationMs / 1000 : null, m.Site, m.ReplayId, m.ReplayOffsetMs), ContractsJson.Default.CaptureDetail);
        }
    }

    private string Image(string id, int width)
    {
        Row? r;
        lock (_lock) r = _rows.FirstOrDefault(x => x.Meta.Id == id);
        if (r is null || !r.Meta.HasThumb) return Envelope.Error(AgentErrors.NotFound);
        int height = width * 9 / 16;
        var key = id + "@" + width;
        byte[]? bytes;
        lock (_lock) _imageCache.TryGetValue(key, out bytes);
        if (bytes is null)
        {
            bytes = Render(r.Meta, width, height);
            lock (_lock)
            {
                if (_imageCache.Count > 400) _imageCache.Clear();
                _imageCache[key] = bytes;
            }
        }
        return Envelope.Ok(new ThumbData(id, "image/png", width, height, bytes), ContractsJson.Default.ThumbData);
    }

    /// <summary>A flat "window" picture: app colour, title bar, text lines that vary with the id.</summary>
    private static byte[] Render(CaptureMeta m, int w, int h)
    {
        int hash = StableHash(m.App);
        double hue = (hash & 0xFFFF) / 65535.0;
        var (br, bg, bb) = Hsv(hue, 0.35, 0.92);
        var (tr, tg, tb) = Hsv(hue, 0.55, 0.55);
        int seed = StableHash(m.Id);
        int bar = Math.Max(4, h / 12);
        int margin = Math.Max(3, w / 24);
        int lineH = Math.Max(2, h / 28);
        return PngWriter.Encode(w, h, (x, y) =>
        {
            if (y < bar) return (tr, tg, tb);
            if (x < margin || x > w - margin) return (br, bg, bb);
            int row = (y - bar - margin) / (lineH * 2);
            if (row < 0 || (y - bar - margin) % (lineH * 2) >= lineH) return (250, 250, 250);
            int len = (int)(w * (0.35 + 0.55 * (((seed >> (row % 24)) ^ (row * 2654435761)) & 0xFF) / 255.0));
            return x - margin < len ? ((byte)(br / 2), (byte)(bg / 2), (byte)(bb / 2)) : ((byte)250, (byte)250, (byte)250);
        });
    }

    private static int StableHash(string s)
    {
        unchecked
        {
            int h = (int)2166136261;
            foreach (var c in s) h = (h ^ c) * 16777619;
            return h & 0x7FFFFFFF;
        }
    }

    private static (byte, byte, byte) Hsv(double h, double s, double v)
    {
        int i = (int)(h * 6) % 6;
        double f = h * 6 - Math.Floor(h * 6), p = v * (1 - s), q = v * (1 - f * s), t = v * (1 - (1 - f) * s);
        var (r, g, b) = i switch { 0 => (v, t, p), 1 => (q, v, p), 2 => (p, v, t), 3 => (p, q, v), 4 => (t, p, v), _ => (v, p, q) };
        return ((byte)(r * 255), (byte)(g * 255), (byte)(b * 255));
    }

    private string Timeline(string[] args)
    {
        if (args.Length is < 2 or > 3
            || !DateTimeOffset.TryParse(args[0], CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var from)
            || !DateTimeOffset.TryParse(args[1], CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var to))
            return Envelope.Error("Usage: timeline <from> <to> [limit]");
        int limit = 500;
        if (args.Length == 3 && (!int.TryParse(args[2], NumberStyles.None, CultureInfo.InvariantCulture, out limit) || limit is < 1 or > 5000))
            return Envelope.Error("Invalid limit.");
        if (from >= to) return Envelope.Error("Invalid from.");
        List<CaptureMeta> items;
        lock (_lock)
        {
            // Like the store: replay frames are search-only, the timeline shows screen and audio rows.
            items = _rows.Where(r => r.Meta.Ts >= from && r.Meta.Ts < to && r.Meta.Kind != CaptureKind.Replay).Take(limit).Select(r => r.Meta).ToList();
            Audit("timeline", items.Count);
        }
        return Items(items, ContractsJson.Default.ItemsReplyCaptureMeta);
    }

    private async Task<string> UnlockAsync(string passphrase, CancellationToken ct)
    {
        bool passMode, passwordMode;
        lock (_lock)
        {
            passMode = UnlockLogic.UsesPassphrase(SettingsSchema.Get(_settings, "security.keyProtection")?.GetValue<string>());
            passwordMode = UnlockLogic.UsesPassword(SettingsSchema.Get(_settings, "security.unlockMethod")?.GetValue<string>());
        }
        await Task.Delay(SlowVerbDelay, ct);   // Windows Hello prompt (or Argon2id) in the real agent
        if (passwordMode)
        {
            if (CheckPassword(passphrase) is { } err) return err;
        }
        else if (passMode && (passphrase.Length == 0 || passphrase == WrongPassphrase)) return Envelope.Error(AgentErrors.WrongPassphrase);
        DateTimeOffset until;
        lock (_lock)
        {
            int minutes = SettingsSchema.Get(_settings, "security.unlockTimeoutMin")?.GetValue<int>() ?? 15;
            _locked = false;
            _lastUnlock = Now;
            until = Now.AddMinutes(minutes);
            _unlockedUntil = until;
        }
        StatusChanged?.Invoke();
        return Envelope.Ok(new UnlockReply(true, until), ContractsJson.Default.UnlockReply);
    }

    // ---------------- unlock password and cloud describe (mock) ----------------

    /// <summary>Null when the password is accepted; any password except "wrong" is right. 5 free tries, then 30 s.</summary>
    private string? CheckPassword(string password)
    {
        lock (_lock)
        {
            if (!_passwordSet) return Envelope.Error(AgentErrors.NoPassword);
            var wait = _passwordBlockedUntil - Now;
            if (wait > TimeSpan.Zero) return Envelope.Error($"Too many attempts. Try again in {(int)Math.Ceiling(wait.TotalSeconds)} s.");
            if (password.Length > 0 && password != WrongPassphrase) { _passwordFailures = 0; return null; }
            if (++_passwordFailures >= 5)
            {
                _passwordBlockedUntil = Now.AddSeconds(30);
                return Envelope.Error("Too many attempts. Try again in 30 s.");
            }
            return Envelope.Error(AgentErrors.WrongPassword);
        }
    }

    private string PasswordVerb(string rest)
    {
        int sp = rest.IndexOf(' ');
        var sub = (sp < 0 ? rest : rest[..sp]).ToLowerInvariant();
        var json = sp < 0 ? "" : rest[(sp + 1)..];
        if (sub.Length == 0)
            lock (_lock)
            {
                int wait = Math.Max(0, (int)Math.Ceiling((_passwordBlockedUntil - Now).TotalSeconds));
                return Envelope.Ok(new PasswordStatus(_passwordSet, true, wait), ContractsJson.Default.PasswordStatus);
            }
        if (sub is not ("set" or "clear") || json.Length == 0) return Envelope.Error("Usage: password | password set <json> | password clear <json>");
        JsonObject o;
        try { o = JsonNode.Parse(json) as JsonObject ?? throw new JsonException(); }
        catch (JsonException) { return Envelope.Error(AgentErrors.InvalidJson); }
        bool wasSet;
        lock (_lock) wasSet = _passwordSet;
        if (wasSet && CheckPassword(o["current"]?.GetValue<string>() ?? "") is { } err) return err;
        lock (_lock)
        {
            if (sub == "clear")
            {
                if (!wasSet) return Envelope.Error(AgentErrors.NoPassword);
                if (UnlockLogic.UsesPassword(SettingsSchema.Get(_settings, "security.unlockMethod")?.GetValue<string>())) return Envelope.Error(AgentErrors.PasswordInUse);
                _passwordSet = false;
                Audit("password clear", 1);
                return Envelope.Ok("null");
            }
            var next = o["new"]?.GetValue<string>() ?? "";
            if (next.Length is < 6 or > 256 || string.IsNullOrWhiteSpace(next)) return Envelope.Error(AgentErrors.PasswordRule);
            _passwordSet = true;
            Audit("password set", 1);
            return Envelope.Ok("null");
        }
    }

    private AiStatus AiState()
    {
        var provider = SettingsSchema.Get(_settings, "ai.provider")?.GetValue<string>() ?? AiDefaults.Gemini;
        var model = SettingsSchema.Get(_settings, "ai.model")?.GetValue<string>() ?? "";
        var endpoint = SettingsSchema.Get(_settings, "ai.endpoint")?.GetValue<string>() ?? "";
        var ai = new AiSettings { Provider = provider, Model = model, Endpoint = endpoint };
        return new AiStatus(Bool("ai.enabled"), _aiKeySet, provider, AiDefaults.EffectiveModel(ai), AiDefaults.EffectiveEndpoint(ai));
    }

    private string AiVerb(string rest)
    {
        int sp = rest.IndexOf(' ');
        var sub = (sp < 0 ? rest : rest[..sp]).ToLowerInvariant();
        var tail = sp < 0 ? "" : rest[(sp + 1)..];
        lock (_lock)
        {
            switch (sub)
            {
                case "" when tail.Length == 0: break;
                case "setkey" when tail.Length > 0:
                    if (tail.Any(c => c <= ' ')) return Envelope.Error(AgentErrors.ApiKeyRule);
                    _aiKeySet = true;
                    Audit("ai setkey", 1);
                    break;
                case "clearkey" when tail.Length == 0:
                    _aiKeySet = false;
                    Audit("ai clearkey", 1);
                    break;
                default:
                    return Envelope.Error("Usage: ai | ai setkey <key> | ai clearkey");
            }
            return Envelope.Ok(AiState(), ContractsJson.Default.AiStatus);
        }
    }

    /// <summary>describe preview returns the keyframe and a token; send waits a little and returns canned text. Nothing leaves the PC.</summary>
    private async Task<string> DescribeAsync(string[] args, CancellationToken ct)
    {
        if (args.Length != 2) return Envelope.Error("Usage: describe preview <id> | describe send <token>");
        AiStatus st;
        lock (_lock) st = AiState();
        if (!st.Enabled) return Envelope.Error(AgentErrors.AiOff);
        if (!st.KeySet && st.Provider != AiDefaults.OpenAi) return Envelope.Error(AgentErrors.NoApiKey);
        switch (args[0].ToLowerInvariant())
        {
            case "preview":
            {
                Row? r;
                lock (_lock) r = _rows.FirstOrDefault(x => x.Meta.Id == args[1]);
                if (r is null || !r.Meta.HasThumb || r.Meta.Kind == CaptureKind.Audio) return Envelope.Error(AgentErrors.NoImage);
                var bytes = Render(r.Meta, 960, 540);
                var token = Convert.ToHexString(RandomNumberGenerator.GetBytes(16)).ToLowerInvariant();
                lock (_lock)
                {
                    _describeTokens[token] = (r.Meta.Id, Now.AddSeconds(120));
                    Audit("describe preview", 1);
                }
                return Envelope.Ok(new DescribePreview(token, st.Provider, st.Model, st.Endpoint, "image/png", 960, 540, false, bytes), ContractsJson.Default.DescribePreview);
            }
            case "send":
            {
                (string Id, DateTimeOffset Expires) t;
                lock (_lock)
                {
                    if (!_describeTokens.Remove(args[1], out t) || t.Expires <= Now) return Envelope.Error(AgentErrors.InvalidToken);
                }
                await Task.Delay(SlowVerbDelay * 3, ct);   // the provider round trip
                Row? r;
                lock (_lock)
                {
                    r = _rows.FirstOrDefault(x => x.Meta.Id == t.Id);
                    Audit("describe " + st.Provider, 1, t.Id);
                }
                var app = CaptureItem.AppName(r?.Meta.App ?? "an app");
                var text = $"(Mock description) The screen shows {app} with the window \"{r?.Meta.Title}\". The user appears to be reading and editing its content.";
                return Envelope.Ok(new DescribeResult(text, st.Provider, st.Model, 1200), ContractsJson.Default.DescribeResult);
            }
        }
        return Envelope.Error("Usage: describe preview <id> | describe send <token>");
    }

    private string SettingsVerb(string[] args, string rest)
    {
        if (args.Length >= 1 && args[0].Equals("get", StringComparison.OrdinalIgnoreCase) && args.Length == 1)
            lock (_lock) return Envelope.Ok(_settings.ToJsonString());
        if (args.Length >= 2 && args[0].Equals("set", StringComparison.OrdinalIgnoreCase))
        {
            var json = rest[3..].Trim();
            JsonObject partial;
            try { partial = JsonNode.Parse(json) as JsonObject ?? throw new JsonException(); }
            catch (JsonException) { return Envelope.Error(AgentErrors.InvalidJson); }
            string result;
            lock (_lock)
            {
                if (partial["security"] is JsonObject sec && sec["unlockMethod"] is JsonValue um && um.TryGetValue<string>(out var method)
                    && method == "password" && !_passwordSet)
                    return Envelope.Error(AgentErrors.NoPassword);
                var merged = (JsonObject)_settings.DeepClone();
                foreach (var (section, value) in partial)
                {
                    if (merged[section] is not JsonObject target || value is not JsonObject src) return Envelope.Error($"Invalid {section}.");
                    foreach (var (key, v) in src)
                    {
                        if (!target.ContainsKey(key) || SettingsSchema.Find(section + "." + key) is not { } d) return Envelope.Error($"Invalid {section}.{key}.");
                        if (d.Type == SettingType.ReadOnlyBool) return Envelope.Error(d.InvalidError);
                        target[key] = v?.DeepClone();
                        if (SettingsSchema.Validate(d, merged) is { } err) return Envelope.Error(err);
                    }
                }
                _settings = merged;
                result = _settings.ToJsonString();
            }
            StatusChanged?.Invoke();
            return Envelope.Ok(result);
        }
        return Envelope.Error("Usage: settings get | settings set <json>");
    }

    private async Task<string> ReplayAsync(string[] args, string rest, CancellationToken ct)
    {
        var sub = args.Length > 0 ? args[0].ToLowerInvariant() : "";
        switch (sub)
        {
            case "save":
            {
                await Task.Delay(SlowVerbDelay, ct);
                ReplayClip clip;
                lock (_lock)
                {
                    if (!Bool("replay.enabled")) return Envelope.Error("Replay is off.");
                    var ts = Now;
                    clip = new ReplayClip($"r_{_replays.Count + 100:D3}", ts, (SettingsSchema.Get(_settings, "replay.seconds")?.GetValue<int>() ?? 120) * 1000,
                        74_000_000, Bool("replay.encryptExports"), $@"C:\Users\you\AppData\Local\BetterRecall\replays\{ts:yyyyMMdd-HHmmss}.mp4.enc");
                    _replays.Insert(0, clip);
                }
                return Envelope.Ok(clip, ContractsJson.Default.ReplayClip);
            }
            case "list":
                lock (_lock) return Items(_replays.ToList(), ContractsJson.Default.ItemsReplyReplayClip);
            case "export":
            {
                // id is the first word, the path is the rest of the line (it may contain spaces)
                var afterSub = rest[(rest.IndexOf(' ') + 1)..].Trim();
                int sp = afterSub.IndexOf(' ');
                if (args.Length < 3 || sp < 0) return Envelope.Error("Usage: replay export <id> <path>");
                var id = afterSub[..sp];
                var path = afterSub[(sp + 1)..].Trim();
                if (!SettingsSchema.IsAbsolutePath(path)) return Envelope.Error("Invalid path.");
                lock (_lock)
                {
                    if (_replays.All(r => r.Id != id)) return Envelope.Error(AgentErrors.NotFound);
                    Audit("replay export", 1);
                }
                await Task.Delay(SlowVerbDelay, ct);
                return Envelope.Ok("null");   // the mock writes no file
            }
            case "delete":
                if (args.Length != 2) return Envelope.Error("Usage: replay delete <id>");
                lock (_lock)
                {
                    if (_replays.RemoveAll(r => r.Id == args[1]) == 0) return Envelope.Error(AgentErrors.NotFound);
                    _rows.RemoveAll(r => r.Meta.ReplayId == args[1]);
                    return Envelope.Ok("null");
                }
            default:
                return Envelope.Error("Usage: replay save | replay list | replay export <id> <path> | replay delete <id>");
        }
    }

    /// <summary>export: dry run counts; otherwise writes a real zip (rendered PNGs, at most 200, plus captures.json).</summary>
    private async Task<string> ExportAsync(string rest, CancellationToken ct)
    {
        ExportRequest? r;
        try { r = JsonSerializer.Deserialize(rest, ContractsJson.Default.ExportRequest); }
        catch (JsonException) { return Envelope.Error(AgentErrors.InvalidJson); }
        if (r is null) return Envelope.Error("Usage: export <json>");
        if (r.From >= r.To) return Envelope.Error("Invalid from.");
        if (!r.DryRun && (!SettingsSchema.IsAbsolutePath(r.Path) || !r.Path.EndsWith(".zip", StringComparison.OrdinalIgnoreCase)))
            return Envelope.Error("Invalid path.");
        string? app = string.IsNullOrWhiteSpace(r.App) ? null : r.App.Trim().Replace(".exe", "", StringComparison.OrdinalIgnoreCase);
        List<Row> rows;
        lock (_lock)
            rows = _rows.Where(x => x.Meta.Ts >= r.From && x.Meta.Ts < r.To
                                    && (app is null || string.Equals(x.Meta.App, app, StringComparison.OrdinalIgnoreCase))).ToList();
        int images = rows.Count(x => x.Meta.HasThumb);
        if (r.DryRun) return Envelope.Ok(new ExportResult(rows.Count, images, 0), ContractsJson.Default.ExportResult);
        await Task.Delay(SlowVerbDelay, ct);
        var entries = new JsonArray();
        using (var fs = new FileStream(r.Path, FileMode.Create, FileAccess.Write, FileShare.None))
        using (var zip = new System.IO.Compression.ZipArchive(fs, System.IO.Compression.ZipArchiveMode.Create))
        {
            int written = 0;
            foreach (var row in rows.OrderBy(x => x.Meta.Ts))
            {
                string? file = null;
                if (row.Meta.HasThumb && written < 200)
                {
                    file = $"images/{row.Meta.Ts.ToLocalTime():yyyyMMdd-HHmmss-fff}_{row.Meta.Id}.png";
                    using var es = zip.CreateEntry(file).Open();
                    es.Write(Render(row.Meta, 640, 360));
                    written++;
                }
                entries.Add(new JsonObject
                {
                    ["id"] = row.Meta.Id, ["ts"] = row.Meta.Ts.UtcDateTime.ToString("yyyy-MM-ddTHH:mm:ss.fffZ", CultureInfo.InvariantCulture),
                    ["kind"] = row.Meta.Kind == CaptureKind.Audio ? "audio" : "screen", ["app"] = row.Meta.App, ["title"] = row.Meta.Title,
                    ["site"] = row.Meta.Site, ["text"] = row.Text, ["file"] = file,
                });
            }
            using var js = zip.CreateEntry("captures.json").Open();
            js.Write(System.Text.Encoding.UTF8.GetBytes(entries.ToJsonString()));
        }
        lock (_lock) Audit("export", rows.Count);
        return Envelope.Ok(new ExportResult(rows.Count, images, new FileInfo(r.Path).Length), ContractsJson.Default.ExportResult);
    }

    private string Purge(string[] args)
    {
        const string usage = "Usage: purge last <seconds> | purge range <from> <to> | purge app <name> | purge confirm <token> | purge all";
        if (args.Length == 0) return Envelope.Error(usage);
        var sub = args[0].ToLowerInvariant();
        lock (_lock)
        {
            ExpireUnlock();
            PurgeScope? scope;
            switch (sub)
            {
                case "last" when args.Length == 2 && int.TryParse(args[1], NumberStyles.None, CultureInfo.InvariantCulture, out var secs) && secs > 0:
                    scope = PurgeScope.Last(TimeSpan.FromSeconds(secs));
                    break;
                case "range" when args.Length == 3
                    && DateTimeOffset.TryParse(args[1], CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var f)
                    && DateTimeOffset.TryParse(args[2], CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var t):
                    if (f >= t) return Envelope.Error("Invalid from.");
                    scope = PurgeScope.Range(f, t);
                    break;
                case "app" when args.Length >= 2:
                    scope = PurgeScope.ForApp(string.Join(' ', args.Skip(1)));
                    break;
                case "site" when args.Length == 2:
                    scope = PurgeScope.ForSite(args[1]);
                    break;
                case "id" when args.Length == 2:
                    scope = PurgeScope.ForId(args[1]);
                    break;
                case "all" when args.Length == 1:
                    if (Now - _lastUnlock > TimeSpan.FromSeconds(60)) return Envelope.Error(AgentErrors.UnlockRequired);
                    scope = null;
                    break;
                case "confirm" when args.Length == 2:
                {
                    if (!_tokens.Remove(args[1], out var entry) || entry.Expires < Now) return Envelope.Error(AgentErrors.InvalidToken);
                    int purged = entry.Scope is null ? _rows.Count : _rows.RemoveAll(r => Matches(entry.Scope, r.Meta));
                    if (entry.Scope is null) { _rows.Clear(); _replays.Clear(); _imageCache.Clear(); }
                    _imageCache.Clear();
                    return Envelope.Ok($"{{\"purged\":{purged}}}");
                }
                default:
                    return Envelope.Error(usage);
            }
            if (_locked && Bool("security.requireUnlock")) return Envelope.Error(AgentErrors.StoreLocked);
            var token = Convert.ToHexString(RandomNumberGenerator.GetBytes(8)).ToLowerInvariant();
            _tokens[token] = (scope, Now.AddSeconds(30));
            if (scope is null) return Envelope.Ok(new PurgeConfirmReply(token, null), ContractsJson.Default.PurgeConfirmReply);
            int rows = _rows.Count(r => Matches(scope, r.Meta));
            return Envelope.Ok(new PurgeConfirmReply(token, rows), ContractsJson.Default.PurgeConfirmReply);
        }
    }

    private string Facets(string[] args)
    {
        DateTimeOffset? from = null, to = null;
        if (args.Length == 2)
        {
            if (!DateTimeOffset.TryParse(args[0], CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var f)
                || !DateTimeOffset.TryParse(args[1], CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var t)) return Envelope.Error("Usage: facets [<from> <to>]");
            from = f; to = t;
        }
        else if (args.Length != 0) return Envelope.Error("Usage: facets [<from> <to>]");
        lock (_lock)
        {
            var rows = _rows.Where(r => r.Meta.Embedded && (from is null || r.Meta.Ts >= from) && (to is null || r.Meta.Ts < to)).Select(r => r.Meta).ToList();
            var apps = rows.Where(m => m.App.Length > 0).GroupBy(m => m.App, StringComparer.OrdinalIgnoreCase)
                .Select(g => new FacetCount(g.Key, g.Count())).OrderByDescending(f => f.Count).ThenBy(f => f.Name).Take(30).ToList();
            var sites = rows.Where(m => m.Site.Length > 0).GroupBy(m => m.Site, StringComparer.OrdinalIgnoreCase)
                .Select(g => new FacetCount(g.Key, g.Count())).OrderByDescending(f => f.Count).ThenBy(f => f.Name).Take(30).ToList();
            return Envelope.Ok(new FacetsReply(apps, sites), ContractsJson.Default.FacetsReply);
        }
    }

    private string History(string rest)
    {
        int sp = rest.IndexOf(' ');
        var sub = (sp < 0 ? rest : rest[..sp]).ToLowerInvariant();
        var text = sp < 0 ? "" : rest[(sp + 1)..].Trim();
        lock (_lock)
        {
            bool on = Bool("privacy.searchHistory");
            if (!on) _history.Clear();
            switch (sub)
            {
                case "":
                    return Items(_history.OrderByDescending(h => h.Ts).ToList(), ContractsJson.Default.ItemsReplyHistoryEntry);
                case "add" when text.Length > 0:
                    if (on)
                    {
                        _history.RemoveAll(h => h.Q == text);
                        _history.Add(new HistoryEntry(text, Now));
                    }
                    return Envelope.Ok("null");
                case "delete" when text.Length > 0:
                    _history.RemoveAll(h => h.Q == text);
                    return Envelope.Ok("null");
                case "clear" when text.Length == 0:
                    _history.Clear();
                    return Envelope.Ok("null");
            }
        }
        return Envelope.Error("Usage: history | history add <text> | history delete <text> | history clear");
    }

    private static bool Matches(PurgeScope s, CaptureMeta m) => s.Kind switch
    {
        "app" => m.App.Equals(s.App, StringComparison.OrdinalIgnoreCase),
        "site" => m.Site.Length > 0 && m.Site.Equals(s.Site, StringComparison.OrdinalIgnoreCase),
        "id" => m.Id == s.Id,
        _ => m.Ts >= s.From && m.Ts < s.To,
    };
}
