// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Threading.Channels;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Ipc;

/// <summary>Per-connection state: who is connected, rate-limit window, purge tokens.</summary>
public sealed class ClientSession(PipeKind kind, int pid, string image)
{
    public PipeKind Kind { get; } = kind;
    public int Pid { get; } = pid;
    public string Image { get; } = image;
    internal readonly Queue<DateTimeOffset> SearchTimes = new();
    internal readonly Dictionary<string, PurgeTicket> PurgeTokens = new(StringComparer.Ordinal);
    internal readonly Dictionary<string, DescribeTicket> DescribeTokens = new(StringComparer.Ordinal);
    public string PipeLabel => Kind switch { PipeKind.Ui => "ui", PipeKind.Mcp => "mcp", _ => "ipc" };
}

internal sealed record PurgeTicket(PurgeScope? Scope, int Rows, DateTimeOffset Expires);

/// <summary>describe preview: the exact image the confirmed send will use.</summary>
internal sealed record DescribeTicket(string CaptureId, Ai.OutgoingImage Image, DateTimeOffset Expires);

public enum Subscription { None, Status, Captures }

/// <summary>One response line, plus a request to switch the connection to stream mode or close it. Stream: a reply of
/// several lines (chat); the producer emits each line, the connection stays open for further requests afterwards.</summary>
public readonly record struct Reply(string Line, Subscription Subscribe = Subscription.None, bool Close = false, Func<Action<string>, Task>? Stream = null);

/// <summary>Everything the command processor reads or calls.</summary>
public sealed class CommandDeps
{
    public required ISettingsStore Settings { get; init; }
    public required Func<IStore?> Store { get; init; }
    public required IKeyVault Keys { get; init; }
    public required IAgentControl Agent { get; init; }
    public required IQueryEmbedder Embedder { get; init; }
    public required IThumbnailer Thumbnailer { get; init; }
    /// <summary>purge all: destroys the key and deletes every store, keyframe and replay file. Returns rows deleted.</summary>
    public required Func<CancellationToken, Task<int>> PurgeAll { get; init; }
    /// <summary>Encrypts the saved bench report. Null disables saving (bench last then answers Not found.).</summary>
    public IBlobCipher? Blobs { get; init; }
    /// <summary>Data dir, for DataPaths.BenchReport.</summary>
    public string? Home { get; init; }
    /// <summary>security.unlockMethod password. Null: password verbs answer "No unlock password is set.".</summary>
    public Crypto.UnlockPassword? Password { get; init; }
    /// <summary>Cloud describe key file. Null disables the ai and describe verbs.</summary>
    public Ai.ApiKeyStore? AiKey { get; init; }
    public Ai.CloudDescriber? Describer { get; init; }
    /// <summary>Applied to every image before preview and send. Null means pass-through (no redaction yet).</summary>
    public Ai.IDescribeRedactor? Redactor { get; init; }
    /// <summary>Cloud chat transport (tests pass a handler for a local fake server). Null: a default ProviderHttp.</summary>
    public Ai.ProviderHttp? ChatHttp { get; init; }
    /// <summary>Provider wire formats (tests may override). Null: Ai.ChatProviders.For.</summary>
    public Func<string, Ai.IChatProvider>? ChatProviders { get; init; }
}

/// <summary>
/// Verbs, usage strings and errors of research 7.6 and v1-design section 5 for the UI, public and MCP pipes.
/// HandleAsync never throws: unexpected exceptions become "Internal error." and go to the log without detail text.
/// </summary>
public sealed partial class CommandProcessor
{
    public static readonly string AppVersion =
        System.Reflection.CustomAttributeExtensions.GetCustomAttribute<System.Reflection.AssemblyInformationalVersionAttribute>(typeof(CommandProcessor).Assembly)?.InformationalVersion ?? "0.0.0";
    public const int Protocol = 1;
    public const int UiMaxResponseBytes = 16 * 1024 * 1024;
    public const int UiMaxResults = 500;

    internal const string UsageSearch = "Usage: search <text> | search <json>";
    internal const string UsageGet = "Usage: get <id>";
    internal const string UsagePause = "Usage: pause [seconds]";
    internal const string UsagePurge = "Usage: purge last <seconds> | purge range <from> <to> | purge app <name> | purge site <host> | purge id <id> | purge confirm <token>";
    internal const string UsagePurgeUi = "Usage: purge last <seconds> | purge range <from> <to> | purge app <name> | purge site <host> | purge id <id> | purge all | purge confirm <token>";
    internal const string UsageFacets = "Usage: facets [<from> <to>]";
    internal const string UsageHistory = "Usage: history | history add <text> | history delete <text> | history clear";
    internal const string UsageSubscribe = "Usage: subscribe [captures]";
    internal const string UsageThumb = "Usage: thumb <id> [width]";
    internal const string UsageKeyframe = "Usage: keyframe <id>";
    internal const string UsageTimeline = "Usage: timeline <from> <to> [limit]";
    internal const string UsageSettings = "Usage: settings get | settings set <json>";
    internal const string UsageReplay = "Usage: replay save | replay list | replay export <id> <path> | replay delete <id>";
    internal const string UsageAudit = "Usage: audit [limit]";
    internal const string UsageBench = "Usage: bench | bench full | bench last";
    internal const string UsageExport = "Usage: export <json>";
    internal const string ErrLocked = "Store is locked.";
    internal const string ErrNotFound = "Not found.";
    internal const string ErrInternal = "Internal error.";
    internal const string ErrRate = "Rate limit exceeded.";
    internal const string ErrToken = "Invalid or expired token.";
    internal const string ErrJson = "Invalid JSON.";
    internal const string ErrControlOff = "Control is off (config ipc.allowControl).";
    internal const string ErrPurgeOff = "Purge is off (config ipc.allowPurge).";
    internal const string ErrThumbsOff = "Thumbnails are off (config ipc.allowThumbnails).";
    internal const string ErrUnlockCancelled = "Unlock cancelled.";
    internal const string ErrWrongPassphrase = "Wrong passphrase.";
    internal const string ErrUnlockRequired = "Unlock required.";
    internal const string UsagePassword = "Usage: password | password set <json> | password clear <json>";
    internal const string UsageAi = "Usage: ai | ai setkey <key> | ai clearkey";
    internal const string UsageDescribe = "Usage: describe preview <id> | describe send <token>";
    internal const string UsageChat = "Usage: chat list | chat get <id> | chat send <json> | chat approve <json> | chat deny <token> | chat rename <json> | chat delete <id> | chat clear";
    internal const string ErrWrongPassword = "Wrong password.";
    internal const string ErrNoPassword = "No unlock password is set.";
    internal const string ErrPasswordRule = "Password must be 6 to 256 characters.";
    internal const string ErrPasswordInUse = "Password in use (config security.unlockMethod).";
    internal const string ErrTooManyPrefix = "Too many attempts. Try again in ";
    internal const string ErrAiOff = "Cloud describe is off (config ai.enabled).";
    internal const string ErrNoApiKey = "API key not set.";
    internal const string ErrApiKeyRule = "Invalid API key.";
    internal const string ErrNoImage = "No image to describe.";
    internal const string ErrProviderUnreachable = "Provider unreachable.";
    internal const string ErrProviderTimeout = "Provider timed out.";
    internal static string ErrTooMany(int seconds) => ErrTooManyPrefix + seconds.ToString(CultureInfo.InvariantCulture) + " s.";

    private static readonly string[] PublicVerbs = ["ping", "version", "help", "status", "search", "get", "thumb", "subscribe", "pause", "resume", "purge"];
    private static readonly string[] McpVerbs = ["ping", "version", "status", "search", "get"];
    private static readonly string[] UiOnlyVerbs = ["unlock", "lock", "consent", "settings", "timeline", "keyframe", "replay", "panic", "audit", "bench", "facets", "history", "password", "ai", "describe", "export", "chat"];

    private readonly CommandDeps _d;
    private DateTimeOffset _lastUnlock = DateTimeOffset.MinValue;

    internal Func<DateTimeOffset> Now { get; set; } = () => DateTimeOffset.UtcNow;

    private readonly Ai.ChatService? _chat;

    /// <summary>Panic: forget chat approvals and unsaved chats, and delete saved chats. The read gate is already closed,
    /// so this goes to the store directly.</summary>
    private void OnPanic()
    {
        _chat?.Clear();
        try { _d.Store()?.DeleteChats(null); } catch (Exception ex) { Log.Warn("panic chat delete", ex); }
    }

    public CommandProcessor(CommandDeps deps)
    {
        _d = deps;
        if (deps.AiKey is { } keys)
        {
            _chat = new Ai.ChatService(new Ai.ChatDeps
            {
                Settings = () => _d.Settings.Current,
                Store = () => IsLocked(PipeKind.Ui) ? null : _d.Store(),
                Embedder = deps.Embedder,
                Thumbnailer = deps.Thumbnailer,
                ApiKey = keys.Read,
                Http = deps.ChatHttp ?? new Ai.ProviderHttp(),
                Providers = deps.ChatProviders ?? Ai.ChatProviders.For,
                ImageRedactor = deps.Redactor,
            });
            deps.Agent.Panicked += OnPanic;
        }
    }

    private Settings S => _d.Settings.Current;

    // ------------------------------------------------------------ connection loop

    /// <summary>Request loop for one trust-checked connection, including subscribe streams and the idle timeout.</summary>
    public async Task ServeAsync(PipeConnection conn, CancellationToken ct)
    {
        var session = new ClientSession(conn.Kind, conn.Peer.Pid, conn.Peer.Image ?? "");
        Audit(session, "connect", 0, "");
        var stream = conn.Stream;
        var reader = new LineReader(stream, 8192);
        Task<string?>? next = null; // read started while a long request ran; it is the next request line
        while (!ct.IsCancellationRequested)
        {
            string? line;
            using (var idle = CancellationTokenSource.CreateLinkedTokenSource(ct))
            {
                if (conn.Kind != PipeKind.Ui) idle.CancelAfter(TimeSpan.FromSeconds(S.Ipc.IdleTimeoutS));
                try
                {
                    if (next != null) { var pending = next; next = null; line = await pending.ConfigureAwait(false); }
                    else line = await reader.ReadLineAsync(LineProtocol.MaxRequestBytes, idle.Token).ConfigureAwait(false);
                }
                catch (LineTooLongException)
                {
                    await WriteAsync(stream, Envelope.Error(LineProtocol.TooLongError), ct).ConfigureAwait(false);
                    return;
                }
                catch (OperationCanceledException) { return; }
                catch (IOException) { return; }
            }
            if (line == null) return;
            Reply reply;
            if (IsLongRunning(conn.Kind, line))
            {
                // No per-request timeout. Closing the pipe cancels the request: keep a read pending and cancel on end of stream.
                using var req = CancellationTokenSource.CreateLinkedTokenSource(ct);
                next = reader.ReadLineAsync(LineProtocol.MaxRequestBytes, ct);
                _ = next.ContinueWith(t =>
                {
                    if (t.IsFaulted || t.IsCanceled || t.Result == null) { try { req.Cancel(); } catch (ObjectDisposedException) { } }
                }, TaskScheduler.Default);
                reply = await HandleAsync(session, line, req.Token).ConfigureAwait(false);
                if (reply.Stream is { } producer)
                {
                    if (!await RunStreamAsync(stream, producer, req.Token).ConfigureAwait(false)) return;
                    continue;
                }
            }
            else reply = await HandleAsync(session, line, ct).ConfigureAwait(false);
            if (reply.Subscribe != Subscription.None)
            {
                // The ack goes out only after the event handlers are attached, so no event between the two is lost.
                await StreamAsync(stream, reply.Subscribe, reply.Line, ct).ConfigureAwait(false);
                return;
            }
            if (!await WriteAsync(stream, reply.Line, ct).ConfigureAwait(false)) return;
            if (reply.Close) return;
        }
    }

    /// <summary>Writes a multi-line reply as the producer emits it. False when the client went away.</summary>
    internal static async Task<bool> RunStreamAsync(Stream stream, Func<Action<string>, Task> producer, CancellationToken ct)
    {
        var ch = Channel.CreateUnbounded<string>(new UnboundedChannelOptions { SingleReader = true });
        var run = Task.Run(async () =>
        {
            try { await producer(l => ch.Writer.TryWrite(l)).ConfigureAwait(false); }
            catch (OperationCanceledException) { }
            catch (Exception ex) { Log.Error("stream producer failed", ex); ch.Writer.TryWrite(Envelope.Error(ErrInternal)); }
            finally { ch.Writer.TryComplete(); }
        }, CancellationToken.None);
        bool ok = true;
        try
        {
            await foreach (var l in ch.Reader.ReadAllAsync(ct).ConfigureAwait(false))
                if (!await WriteAsync(stream, l, ct).ConfigureAwait(false)) { ok = false; break; }
        }
        catch (OperationCanceledException) { ok = false; }
        try { await run.ConfigureAwait(false); } catch { }
        return ok;
    }

    /// <summary>Verbs that may run for minutes (bench full, up to about 10; chat turns). UI pipe only.</summary>
    internal static bool IsLongRunning(PipeKind kind, string line)
    {
        if (kind != PipeKind.Ui) return false;
        var parts = line.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        if (parts.Length >= 2 && parts[0].Equals("chat", StringComparison.OrdinalIgnoreCase)
            && parts[1].ToLowerInvariant() is "send" or "approve" or "deny") return true;
        if (parts.Length == 3 && parts[0].Equals("describe", StringComparison.OrdinalIgnoreCase) && parts[1].Equals("send", StringComparison.OrdinalIgnoreCase)) return true;
        return parts.Length == 2 && parts[0].Equals("bench", StringComparison.OrdinalIgnoreCase) && parts[1].Equals("full", StringComparison.OrdinalIgnoreCase);
    }

    private static async Task<bool> WriteAsync(Stream s, string line, CancellationToken ct)
    {
        try { await LineProtocol.WriteLineAsync(s, line, ct).ConfigureAwait(false); return true; }
        catch (Exception ex) when (ex is IOException or OperationCanceledException or ObjectDisposedException) { return false; }
    }

    /// <summary>One-way stream after subscribe (research 7.7). Input is ignored; 64 queued messages disconnect the client.</summary>
    internal async Task StreamAsync(Stream stream, Subscription sub, string ack, CancellationToken ct)
    {
        var ch = Channel.CreateBounded<string>(new BoundedChannelOptions(64) { FullMode = BoundedChannelFullMode.Wait, SingleReader = true });
        using var cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        // Runs on the agent's event-raising thread: must never throw, even after this stream has ended
        // and cts is disposed (race between unsubscribe and a concurrent raise).
        void Push(string line)
        {
            if (ch.Writer.TryWrite(line)) return;
            try { if (!cts.IsCancellationRequested) { Log.Info("subscriber too slow, disconnected"); cts.Cancel(); } }
            catch (ObjectDisposedException) { }
        }
        Action<AgentStatus> onStatus = st => Push(Envelope.Ok(JsonSerializer.Serialize(st, CoreJson.Default.AgentStatus)));
        Action<CaptureMeta> onCapture = m =>
        {
            if (IsLocked(PipeKind.Public) && IsLocked(PipeKind.Ui)) return;
            Push(Envelope.Ok(JsonSerializer.Serialize(new CaptureEvent(m.Id, m.Ts, m.Kind, m.App), CoreJson.Default.CaptureEvent)));
        };
        if (sub == Subscription.Status) _d.Agent.StatusChanged += onStatus;
        else _d.Agent.CaptureStored += onCapture;
        if (!await WriteAsync(stream, ack, ct).ConfigureAwait(false))
        {
            if (sub == Subscription.Status) _d.Agent.StatusChanged -= onStatus;
            else _d.Agent.CaptureStored -= onCapture;
            return;
        }
        // Drain input until the client closes the pipe.
        var drain = Task.Run(async () =>
        {
            var buf = new byte[256];
            try { while (await stream.ReadAsync(buf, cts.Token).ConfigureAwait(false) > 0) { } }
            catch { }
            cts.Cancel();
        });
        try
        {
            var lastStatus = Stopwatch.StartNew();
            while (!cts.IsCancellationRequested)
            {
                using var tick = CancellationTokenSource.CreateLinkedTokenSource(cts.Token);
                tick.CancelAfter(TimeSpan.FromSeconds(sub == Subscription.Status ? Math.Max(1, 60 - lastStatus.Elapsed.TotalSeconds) : 3600));
                string? msg = null;
                try { msg = await ch.Reader.ReadAsync(tick.Token).ConfigureAwait(false); }
                catch (OperationCanceledException) when (!cts.IsCancellationRequested) { }
                catch (OperationCanceledException) { break; }
                if (msg == null)
                {
                    if (sub != Subscription.Status) continue;
                    msg = Envelope.Ok(JsonSerializer.Serialize(_d.Agent.GetStatus(), CoreJson.Default.AgentStatus));
                }
                if (sub == Subscription.Status) lastStatus.Restart();
                if (!await WriteAsync(stream, msg, cts.Token).ConfigureAwait(false)) break;
            }
        }
        finally
        {
            if (sub == Subscription.Status) _d.Agent.StatusChanged -= onStatus;
            else _d.Agent.CaptureStored -= onCapture;
            cts.Cancel();
            try { await drain.ConfigureAwait(false); } catch { }
        }
    }

    // ------------------------------------------------------------ dispatch

    /// <summary>Handles one request line. Never throws.</summary>
    public async Task<Reply> HandleAsync(ClientSession session, string line, CancellationToken ct)
    {
        try
        {
            return await DispatchAsync(session, line, ct).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (ct.IsCancellationRequested)
        {
            return new Reply(Envelope.Error(ErrInternal), Close: true);
        }
        catch (Exception ex)
        {
            Log.Error("command failed", ex);
            return new Reply(Envelope.Error(ErrInternal));
        }
    }

    private static (string Verb, string Tail) SplitVerb(string line)
    {
        var t = line.Trim();
        int sp = t.IndexOf(' ');
        return sp < 0 ? (t, "") : (t[..sp], t[(sp + 1)..].TrimStart(' '));
    }

    /// <summary>Everything after the verb and one space, untrimmed (passphrases may start or end with spaces).</summary>
    private static string RawRest(string line)
    {
        var t = line.TrimStart();
        int sp = t.IndexOf(' ');
        return sp < 0 ? "" : t[(sp + 1)..];
    }

    private static string[] Args(string rest) => rest.Split(' ', StringSplitOptions.RemoveEmptyEntries);

    private static Reply Err(string e) => new(Envelope.Error(e));
    private static Reply Ok(string json) => new(Envelope.Ok(json));

    private bool Allowed(PipeKind kind, string verb) => kind switch
    {
        PipeKind.Mcp => McpVerbs.Contains(verb),
        PipeKind.Public => PublicVerbs.Contains(verb),
        _ => PublicVerbs.Contains(verb) || UiOnlyVerbs.Contains(verb),
    };

    private async Task<Reply> DispatchAsync(ClientSession s, string line, CancellationToken ct)
    {
        var (verbRaw, rest) = SplitVerb(line);
        var verb = verbRaw.ToLowerInvariant();
        if (!Allowed(s.Kind, verb)) return Err($"Unknown command '{verbRaw}'.");
        var args = Args(rest);
        switch (verb)
        {
            case "ping":
                return Ok(JsonSerializer.Serialize(new PongData(true), CoreJson.Default.PongData));
            case "version":
                return Ok(JsonSerializer.Serialize(new VersionReply(AppVersion, Protocol), CoreJson.Default.VersionReply));
            case "help":
                return Ok(JsonSerializer.Serialize(new HelpData(HelpVerbs(s.Kind)), CoreJson.Default.HelpData));
            case "status":
                return Ok(JsonSerializer.Serialize(_d.Agent.GetStatus(), CoreJson.Default.AgentStatus));
            case "search":
                return await SearchAsync(s, rest, ct).ConfigureAwait(false);
            case "get":
                return Get(s, args);
            case "thumb":
                return await ThumbAsync(s, args, ct).ConfigureAwait(false);
            case "subscribe":
                return Subscribe(s, args);
            case "pause":
                return Pause(s, args);
            case "resume":
                if (s.Kind == PipeKind.Public && !S.Ipc.AllowControl) return Err(ErrControlOff);
                if (args.Length != 0) return Err("Usage: resume");
                _d.Agent.Resume(s.Kind == PipeKind.Ui ? "user" : "ipc");
                return Ok("null");
            case "purge":
                return await PurgeAsync(s, args, ct).ConfigureAwait(false);
            case "unlock":
                return await UnlockAsync(s, RawRest(line), ct).ConfigureAwait(false);
            case "lock":
                _d.Agent.Lock();
                return Ok("null");
            case "consent":
                _d.Agent.Consent();
                return Ok("null");
            case "panic":
                _d.Agent.Panic();
                return Ok("null");
            case "bench":
                return await BenchAsync(s, args, ct).ConfigureAwait(false);
            case "settings":
                return Settings(rest);
            case "timeline":
                return Timeline(s, args);
            case "keyframe":
                return Keyframe(s, args);
            case "replay":
                return await ReplayAsync(s, rest, ct).ConfigureAwait(false);
            case "audit":
                return AuditVerb(s, args);
            case "facets":
                return Facets(s, args);
            case "history":
                return History(s, rest);
            case "password":
                return PasswordVerb(s, RawRest(line));
            case "ai":
                return AiVerb(s, RawRest(line));
            case "describe":
                return await DescribeAsync(s, args, ct).ConfigureAwait(false);
            case "export":
                return await ExportAsync(s, rest, ct).ConfigureAwait(false);
            case "chat":
                return ChatVerb(s, rest, ct);
        }
        return Err($"Unknown command '{verbRaw}'.");
    }

    internal List<string> HelpVerbs(PipeKind kind)
    {
        if (kind == PipeKind.Mcp) return [.. McpVerbs];
        var list = new List<string> { "ping", "version", "help", "status", "search", "get" };
        var ipc = S.Ipc;
        bool ui = kind == PipeKind.Ui;
        if (ui || ipc.AllowThumbnails) list.Add("thumb");
        list.Add("subscribe");
        if (ui || ipc.AllowControl) { list.Add("pause"); list.Add("resume"); }
        if (ui || (ipc.AllowControl && ipc.AllowPurge)) list.Add("purge");
        if (ui)
        {
            list.AddRange(UiOnlyVerbs);
            list.Insert(list.IndexOf("bench") + 1, "bench full");
            list.Insert(list.IndexOf("bench full") + 1, "bench last");
        }
        return list;
    }

    // ------------------------------------------------------------ gate, audit

    /// <summary>Read gate: store not open, or security.requireUnlock with the gate closed (ipc.requireUnlocked for IPC and MCP).</summary>
    internal bool IsLocked(PipeKind kind)
    {
        if (_d.Store() == null) return true;
        var s = S;
        if (!s.Security.RequireUnlock) return false;
        if (kind != PipeKind.Ui && !s.Ipc.RequireUnlocked) return false;
        return !_d.Agent.IsUnlocked;
    }

    private void Audit(ClientSession s, string verb, int count, string sha)
    {
        try { _d.Store()?.AppendAudit(new AuditEntry(Now(), s.PipeLabel, s.Pid, s.Image, verb, count, sha)); }
        catch (Exception ex) { Log.Warn("audit append failed", ex); }
    }

    internal static string Sha256(string text) => Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(text)));

    private int MaxResponseBytes(PipeKind k) => k == PipeKind.Ui ? UiMaxResponseBytes : S.Ipc.MaxResponseBytes;

    // ------------------------------------------------------------ search

    internal static (SearchRequest? Req, string? Error) ParseSearch(string rest, int defaultLimit, int maxLimit)
    {
        var text = rest.Trim();
        if (text.Length == 0) return (null, UsageSearch);
        if (text[0] != '{')
        {
            if (text.Length > 1024) return (null, "Invalid q.");
            return (new SearchRequest(text, defaultLimit), null);
        }
        JsonDocument doc;
        try { doc = JsonDocument.Parse(text); }
        catch (JsonException) { return (null, ErrJson); }
        using (doc)
        {
            var root = doc.RootElement;
            if (root.ValueKind != JsonValueKind.Object) return (null, ErrJson);
            string? q = null; int limit = defaultLimit; DateTimeOffset? from = null, to = null; string? app = null;
            CaptureKind[]? kinds = null; var mode = SearchMode.Hybrid; string? site = null;
            foreach (var p in root.EnumerateObject())
            {
                switch (p.Name)
                {
                    case "q":
                        if (p.Value.ValueKind != JsonValueKind.String) return (null, "Invalid q.");
                        q = p.Value.GetString()!.Trim();
                        break;
                    case "limit":
                        if (p.Value.ValueKind != JsonValueKind.Number || !p.Value.TryGetInt64(out var l) || l < 1) return (null, "Invalid limit.");
                        limit = (int)Math.Min(l, maxLimit);
                        break;
                    case "from":
                        if (!TryTime(p.Value, out var f)) return (null, "Invalid from.");
                        from = f;
                        break;
                    case "to":
                        if (!TryTime(p.Value, out var t)) return (null, "Invalid to.");
                        to = t;
                        break;
                    case "app":
                        if (p.Value.ValueKind == JsonValueKind.Null) break;
                        if (p.Value.ValueKind != JsonValueKind.String || string.IsNullOrWhiteSpace(p.Value.GetString()) || p.Value.GetString()!.Length > 260) return (null, "Invalid app.");
                        app = p.Value.GetString()!.Trim();
                        break;
                    case "site":
                        if (p.Value.ValueKind == JsonValueKind.Null) break;
                        if (p.Value.ValueKind != JsonValueKind.String || Store.EncryptedStore.NormalizeSite(p.Value.GetString()).Length == 0) return (null, "Invalid site.");
                        site = Store.EncryptedStore.NormalizeSite(p.Value.GetString());
                        break;
                    case "kind":
                        if (p.Value.ValueKind != JsonValueKind.Array) return (null, "Invalid kind.");
                        var ks = new List<CaptureKind>();
                        foreach (var k in p.Value.EnumerateArray())
                        {
                            var v = k.ValueKind == JsonValueKind.String ? k.GetString()!.ToLowerInvariant() : null;
                            if (v == "screen") ks.Add(CaptureKind.Screen);
                            else if (v == "audio") ks.Add(CaptureKind.Audio);
                            else if (v == "replay") ks.Add(CaptureKind.Replay);
                            else return (null, "Invalid kind.");
                        }
                        if (ks.Count == 0) return (null, "Invalid kind.");
                        kinds = [.. ks.Distinct()];
                        break;
                    case "mode":
                        var m = p.Value.ValueKind == JsonValueKind.String ? p.Value.GetString()!.ToLowerInvariant() : null;
                        if (m == "hybrid") mode = SearchMode.Hybrid;
                        else if (m == "text") mode = SearchMode.Text;
                        else if (m == "vector") mode = SearchMode.Vector;
                        else return (null, "Invalid mode.");
                        break;
                }
            }
            if (q == null || q.Length is < 1 or > 1024) return (null, "Invalid q.");
            if (from != null && to != null && from >= to) return (null, "Invalid from.");
            return (new SearchRequest(q, limit, from, to, app, kinds, mode, site), null);
        }
    }

    private static bool TryTime(JsonElement e, out DateTimeOffset v)
    {
        v = default;
        return e.ValueKind == JsonValueKind.String && TryIso(e.GetString(), out v);
    }

    internal static bool TryIso(string? s, out DateTimeOffset v)
    {
        v = default;
        if (string.IsNullOrWhiteSpace(s) || !char.IsAsciiDigit(s[0])) return false;
        if (!DateTimeOffset.TryParse(s, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal, out v)) return false;
        v = v.ToUniversalTime();
        return true;
    }

    private bool RateLimited(ClientSession s)
    {
        if (s.Kind == PipeKind.Ui) return false;
        var now = Now();
        while (s.SearchTimes.Count > 0 && now - s.SearchTimes.Peek() >= TimeSpan.FromMinutes(1)) s.SearchTimes.Dequeue();
        if (s.SearchTimes.Count >= S.Ipc.MaxSearchPerMinute) return true;
        s.SearchTimes.Enqueue(now);
        return false;
    }

    private async Task<Reply> SearchAsync(ClientSession s, string rest, CancellationToken ct)
    {
        var sw = Stopwatch.StartNew();
        var ipc = S.Ipc;
        int max = s.Kind == PipeKind.Ui ? Math.Max(ipc.MaxResults, UiMaxResults) : ipc.MaxResults;
        var (req, error) = ParseSearch(rest, Math.Min(ipc.DefaultResults, max), max);
        if (req == null) return Err(error!);
        if (IsLocked(s.Kind)) return Err(ErrLocked);
        if (RateLimited(s)) return Err(ErrRate);
        var store = _d.Store();
        if (store == null) return Err(ErrLocked);

        float[] qv = [];
        if (req.Mode != SearchMode.Text)
        {
            try
            {
                using var to = CancellationTokenSource.CreateLinkedTokenSource(ct);
                to.CancelAfter(TimeSpan.FromSeconds(15));
                qv = await _d.Embedder.EmbedQueryAsync(req.Q, to.Token).ConfigureAwait(false) ?? [];
            }
            catch (Exception ex) when (ex is not OperationCanceledException || !ct.IsCancellationRequested)
            {
                if (req.Mode == SearchMode.Vector) throw;
                Log.Warn("query embedding failed, text-only search", ex);
                qv = [];
            }
        }
        var resp = store.Search(req, qv, max, MaxResponseBytes(s.Kind));
        double weakBelow = S.Index.WeakMatchBelow;
        if (weakBelow > 0)
            resp = resp with { Results = resp.Results.Select(h => h with { Weak = !h.TextMatch && (h.Cosine ?? 0) < weakBelow }).ToList() };
        resp = resp with { TookMs = sw.ElapsedMilliseconds };
        Audit(s, "search", resp.Count, Sha256(req.Q));
        return Ok(JsonSerializer.Serialize(resp, CoreJson.Default.SearchResponse));
    }

    // ------------------------------------------------------------ get, thumb, keyframe, timeline

    private Reply Get(ClientSession s, string[] args)
    {
        if (args.Length != 1) return Err(UsageGet);
        if (IsLocked(s.Kind)) return Err(ErrLocked);
        var d = _d.Store()!.Get(args[0]);
        Audit(s, "get", d == null ? 0 : 1, "");
        if (d == null) return Err(ErrNotFound);
        return Ok(FitDetail(d, MaxResponseBytes(s.Kind) - 32));
    }

    internal static string FitDetail(CaptureDetail d, int budget)
    {
        var json = JsonSerializer.Serialize(d, CoreJson.Default.CaptureDetail);
        if (Encoding.UTF8.GetByteCount(json) <= budget) return json;
        var text = d.Text;
        for (int i = 0; i < 24 && text.Length > 0; i++)
        {
            int over = Encoding.UTF8.GetByteCount(json) - budget;
            if (over <= 0) break;
            int cut = Math.Max(1, Math.Min(text.Length, over / 2 + 1));
            text = text[..(text.Length - cut)];
            if (text.Length > 0 && char.IsHighSurrogate(text[^1])) text = text[..^1];
            json = JsonSerializer.Serialize(d with { Text = text }, CoreJson.Default.CaptureDetail);
        }
        return json;
    }

    private async Task<Reply> ThumbAsync(ClientSession s, string[] args, CancellationToken ct)
    {
        if (s.Kind == PipeKind.Public && !S.Ipc.AllowThumbnails) return Err(ErrThumbsOff);
        if (args.Length is < 1 or > 2) return Err(UsageThumb);
        int width = 320;
        if (args.Length == 2)
        {
            if (!int.TryParse(args[1], NumberStyles.None, CultureInfo.InvariantCulture, out width)) return Err(UsageThumb);
            if (width is < 64 or > 1280) return Err("Invalid width.");
        }
        if (IsLocked(s.Kind)) return Err(ErrLocked);
        var t = await _d.Thumbnailer.GetThumbAsync(args[0], width, ct).ConfigureAwait(false);
        Audit(s, "thumb", t == null ? 0 : 1, "");
        if (t == null) return Err(ErrNotFound);
        return Ok(JsonSerializer.Serialize(t, CoreJson.Default.ThumbData));
    }

    private Reply Keyframe(ClientSession s, string[] args)
    {
        if (args.Length != 1) return Err(UsageKeyframe);
        if (IsLocked(s.Kind)) return Err(ErrLocked);
        var t = _d.Store()!.GetKeyframe(args[0]);
        Audit(s, "keyframe", t == null ? 0 : 1, "");
        if (t == null) return Err(ErrNotFound);
        // AVIF and JPEG XL keyframes reach the UI as WebP (the agent transcodes).
        var ui = _d.Thumbnailer.Transcode(t, "ui");
        try { return Ok(JsonSerializer.Serialize(ui, CoreJson.Default.ThumbData)); }
        finally
        {
            Array.Clear(t.Bytes);
            if (!ReferenceEquals(ui, t)) Array.Clear(ui.Bytes);
        }
    }

    /// <summary>export {"from","to","path","app"?,"jpeg"?,"dryRun"?}: zip of decrypted keyframes plus captures.json.</summary>
    private async Task<Reply> ExportAsync(ClientSession s, string rest, CancellationToken ct)
    {
        if (rest.Length == 0) return Err(UsageExport);
        ExportRequest? r;
        try { r = JsonSerializer.Deserialize(rest, CoreJson.Default.ExportRequest); }
        catch (JsonException) { return Err(ErrJson); }
        if (r is null) return Err(UsageExport);
        if (r.From >= r.To) return Err("Invalid from.");
        if (!r.DryRun && (string.IsNullOrWhiteSpace(r.Path) || !Path.IsPathFullyQualified(r.Path)
            || !r.Path.EndsWith(".zip", StringComparison.OrdinalIgnoreCase) || r.Path.IndexOfAny(Path.GetInvalidPathChars()) >= 0))
            return Err("Invalid path.");
        if (IsLocked(s.Kind)) return Err(ErrLocked);
        var store = _d.Store()!;
        if (r.DryRun) return Ok(JsonSerializer.Serialize(Store.CaptureExporter.Count(store, r), CoreJson.Default.ExportResult));
        try
        {
            var result = await Task.Run(() => Store.CaptureExporter.Write(store, r, _d.Thumbnailer.Transcode, ct), ct).ConfigureAwait(false);
            Audit(s, "export", result.Count, "");
            return Ok(JsonSerializer.Serialize(result, CoreJson.Default.ExportResult));
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            Log.Warn("export failed", ex);
            return Err("Export failed.");
        }
    }

    private Reply Timeline(ClientSession s, string[] args)
    {
        if (args.Length is < 2 or > 3 || !TryIso(args[0], out var from) || !TryIso(args[1], out var to)) return Err(UsageTimeline);
        int limit = 500;
        if (args.Length == 3)
        {
            if (!int.TryParse(args[2], NumberStyles.None, CultureInfo.InvariantCulture, out limit)) return Err(UsageTimeline);
            if (limit < 1) return Err("Invalid limit.");
            limit = Math.Min(limit, 5000);
        }
        if (from >= to) return Err("Invalid from.");
        if (IsLocked(s.Kind)) return Err(ErrLocked);
        var items = _d.Store()!.Timeline(from, to, limit);
        Audit(s, "timeline", items.Count, "");
        return Ok(JsonSerializer.Serialize(new ItemsReply<CaptureMeta>(items), CoreJson.Default.ItemsReplyCaptureMeta));
    }

    private Reply AuditVerb(ClientSession s, string[] args)
    {
        int limit = 200;
        if (args.Length > 1 || (args.Length == 1 && !int.TryParse(args[0], NumberStyles.None, CultureInfo.InvariantCulture, out limit))) return Err(UsageAudit);
        if (limit is < 1 or > 10000) return Err("Invalid limit.");
        if (IsLocked(s.Kind)) return Err(ErrLocked);
        var items = _d.Store()!.ReadAudit(limit);
        return Ok(JsonSerializer.Serialize(new ItemsReply<AuditEntry>(items), CoreJson.Default.ItemsReplyAuditEntry));
    }

    private Reply Facets(ClientSession s, string[] args)
    {
        DateTimeOffset? from = null, to = null;
        if (args.Length == 2)
        {
            if (!TryIso(args[0], out var f) || !TryIso(args[1], out var t)) return Err(UsageFacets);
            if (f >= t) return Err("Invalid from.");
            from = f; to = t;
        }
        else if (args.Length != 0) return Err(UsageFacets);
        if (IsLocked(s.Kind)) return Err(ErrLocked);
        var r = _d.Store()!.Facets(from, to, 30);
        return Ok(JsonSerializer.Serialize(r, CoreJson.Default.FacetsReply));
    }

    /// <summary>
    /// Saved searches (privacy.searchHistory), UI pipe only. The UI adds a search when the user commits to it
    /// (Enter, opening a result), not on every keystroke. Off: list is empty and add does nothing.
    /// </summary>
    private Reply History(ClientSession s, string rest)
    {
        var (sub, text) = SplitVerb(rest);
        if (IsLocked(s.Kind)) return Err(ErrLocked);
        var store = _d.Store()!;
        var p = S.Privacy;
        switch (sub.ToLowerInvariant())
        {
            case "" :
                IReadOnlyList<HistoryEntry> items = p.SearchHistory ? store.History(Math.Clamp(p.SearchHistoryMax, 10, 1000)) : [];
                return Ok(JsonSerializer.Serialize(new ItemsReply<HistoryEntry>(items), CoreJson.Default.ItemsReplyHistoryEntry));
            case "add" when text.Trim().Length is > 0 and <= 1024:
                if (p.SearchHistory) store.AddHistory(text.Trim(), Now(), Math.Clamp(p.SearchHistoryMax, 10, 1000));
                return Ok("null");
            case "delete" when text.Trim().Length > 0:
                store.DeleteHistory(text.Trim());
                return Ok("null");
            case "clear" when text.Length == 0:
                store.DeleteHistory(null);
                return Ok("null");
        }
        return Err(UsageHistory);
    }

    // ------------------------------------------------------------ subscribe, pause

    private Reply Subscribe(ClientSession s, string[] args)
    {
        if (args.Length == 0) return new Reply(Envelope.Ok(JsonSerializer.Serialize(new SubscribedStatus(true), CoreJson.Default.SubscribedStatus)), Subscription.Status);
        if (args.Length == 1 && args[0].Equals("captures", StringComparison.OrdinalIgnoreCase))
        {
            if (IsLocked(s.Kind)) return Err(ErrLocked);
            Audit(s, "subscribe captures", 0, "");
            return new Reply(Envelope.Ok(JsonSerializer.Serialize(new SubscribedCaptures("captures"), CoreJson.Default.SubscribedCaptures)), Subscription.Captures);
        }
        return Err(UsageSubscribe);
    }

    private Reply Pause(ClientSession s, string[] args)
    {
        if (s.Kind == PipeKind.Public && !S.Ipc.AllowControl) return Err(ErrControlOff);
        if (args.Length > 1) return Err(UsagePause);
        TimeSpan? duration = null;
        if (args.Length == 1)
        {
            if (!long.TryParse(args[0], NumberStyles.None, CultureInfo.InvariantCulture, out var sec)) return Err(UsagePause);
            int max = s.Kind == PipeKind.Ui ? 604800 : S.Ipc.MaxPauseS;
            if (sec < 1 || sec > max) return Err("Invalid seconds.");
            duration = TimeSpan.FromSeconds(sec);
        }
        _d.Agent.Pause(duration, s.Kind == PipeKind.Ui ? "user" : "ipc");
        return Ok("null");
    }

    // ------------------------------------------------------------ purge

    private string NewToken(ClientSession s, PurgeScope? scope, int rows)
    {
        var now = Now();
        foreach (var k in s.PurgeTokens.Where(kv => kv.Value.Expires <= now).Select(kv => kv.Key).ToList()) s.PurgeTokens.Remove(k);
        var token = Convert.ToHexStringLower(RandomNumberGenerator.GetBytes(16));
        s.PurgeTokens[token] = new PurgeTicket(scope, rows, now.AddSeconds(30));
        return token;
    }

    private async Task<Reply> PurgeAsync(ClientSession s, string[] args, CancellationToken ct)
    {
        bool ui = s.Kind == PipeKind.Ui;
        if (!ui)
        {
            if (!S.Ipc.AllowControl) return Err(ErrControlOff);
            if (!S.Ipc.AllowPurge) return Err(ErrPurgeOff);
        }
        string usage = ui ? UsagePurgeUi : UsagePurge;
        if (args.Length == 0) return Err(usage);
        var sub = args[0].ToLowerInvariant();
        PurgeScope scope;
        switch (sub)
        {
            case "confirm":
            {
                if (args.Length != 2) return Err(usage);
                var now = Now();
                if (!s.PurgeTokens.Remove(args[1], out var ticket) || ticket.Expires <= now) return Err(ErrToken);
                if (ticket.Scope == null)
                {
                    int all = await _d.PurgeAll(ct).ConfigureAwait(false);
                    _chat?.Clear();
                    return Ok(JsonSerializer.Serialize(new PurgeResult(all), CoreJson.Default.PurgeResult));
                }
                var st = _d.Store();
                if (st == null) return Err(ErrLocked);
                int n = st.Purge(ticket.Scope);
                Audit(s, "purge", n, "");
                return Ok(JsonSerializer.Serialize(new PurgeResult(n), CoreJson.Default.PurgeResult));
            }
            case "all" when ui:
            {
                if (args.Length != 1) return Err(usage);
                if (Now() - _lastUnlock > TimeSpan.FromSeconds(60)) return Err(ErrUnlockRequired);
                return Ok(JsonSerializer.Serialize(new PurgeConfirmReply(NewToken(s, null, 0), null), CoreJson.Default.PurgeConfirmReply));
            }
            case "last":
                if (args.Length != 2 || !long.TryParse(args[1], NumberStyles.None, CultureInfo.InvariantCulture, out var secs)) return Err(usage);
                if (secs < 1 || secs > 3650L * 86400) return Err("Invalid seconds.");
                scope = PurgeScope.Last(TimeSpan.FromSeconds(secs));
                break;
            case "range":
                if (args.Length != 3 || !TryIso(args[1], out var from) || !TryIso(args[2], out var to)) return Err(usage);
                if (from >= to) return Err("Invalid from.");
                scope = PurgeScope.Range(from, to);
                break;
            case "app":
                if (args.Length < 2) return Err(usage);
                scope = PurgeScope.ForApp(string.Join(' ', args.Skip(1)));
                break;
            case "site":
                if (args.Length != 2 || Store.EncryptedStore.NormalizeSite(args[1]).Length == 0) return Err(usage);
                scope = PurgeScope.ForSite(args[1]);
                break;
            case "id":
                if (args.Length != 2 || args[1].Length > 64) return Err(usage);
                scope = PurgeScope.ForId(args[1]);
                break;
            default:
                return Err(usage);
        }
        var store = _d.Store();
        if (store == null) return Err(ErrLocked);
        int rows = store.CountPurge(scope);
        return Ok(JsonSerializer.Serialize(new PurgeConfirmReply(NewToken(s, scope, rows), rows), CoreJson.Default.PurgeConfirmReply));
    }

    // ------------------------------------------------------------ UI verbs

    private bool UsesPassword => S.Security.UnlockMethod == "password";

    /// <summary>
    /// unlock [secret]. Data key still passphrase-wrapped: the secret is the passphrase. Otherwise with
    /// security.unlockMethod password the secret is the unlock password (rate-limited, no Hello prompt); with hello an
    /// optional passphrase check comes first, then Windows Hello. With the password method a passphrase that loads the
    /// key also opens the gate: it is the stronger secret of the two.
    /// </summary>
    private async Task<Reply> UnlockAsync(ClientSession s, string rest, CancellationToken ct)
    {
        var keys = _d.Keys;
        bool needPass = keys is Crypto.KeyVault kv ? kv.NeedsPassphrase : (!keys.KeyLoaded && keys.Protection == Crypto.KeyVault.ModePassphrase);
        if (needPass)
        {
            if (rest.Length == 0 || !keys.UnlockWithPassphrase(rest)) return Err(ErrWrongPassphrase);
            if (UsesPassword) return OpenVerified(s, "unlock passphrase");
        }
        else if (UsesPassword)
        {
            var pw = _d.Password;
            var check = pw == null ? Crypto.PasswordCheck.NotSet : pw.Verify(rest);
            switch (check)
            {
                case Crypto.PasswordCheck.NotSet: return Err(ErrNoPassword);
                case Crypto.PasswordCheck.Throttled: return Err(ErrTooMany(Math.Max(1, pw!.RetryAfterSeconds)));
                case Crypto.PasswordCheck.Wrong:
                    Audit(s, "unlock password", 0, "");
                    int wait = pw!.RetryAfterSeconds;
                    return Err(wait > 0 ? ErrTooMany(wait) : ErrWrongPassword);
            }
            return OpenVerified(s, "unlock password");
        }
        else if (rest.Length > 0 && keys.Protection == Crypto.KeyVault.ModePassphrase && !keys.UnlockWithPassphrase(rest))
        {
            return Err(ErrWrongPassphrase);
        }
        if (!await _d.Agent.UnlockAsync(ct).ConfigureAwait(false)) return Err(ErrUnlockCancelled);
        _lastUnlock = Now();
        var until = _d.Agent.GetStatus().UnlockedUntil;
        return Ok(JsonSerializer.Serialize(new UnlockReply(true, until), CoreJson.Default.UnlockReply));
    }

    private Reply OpenVerified(ClientSession s, string what)
    {
        _d.Agent.OpenReadGate();
        _lastUnlock = Now();
        Audit(s, what, 1, "");
        var until = _d.Agent.GetStatus().UnlockedUntil;
        return Ok(JsonSerializer.Serialize(new UnlockReply(true, until), CoreJson.Default.UnlockReply));
    }

    // ------------------------------------------------------------ unlock password

    private static string? JsonString(JsonElement root, string name) =>
        root.ValueKind == JsonValueKind.Object && root.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;

    /// <summary>
    /// password: status. password set {"current":"...","new":"..."}: current is needed once a password exists; the first
    /// password needs the read gate open (or security.requireUnlock off, or no Windows Hello on this PC, so a user without
    /// Hello is not locked out). password clear {"current":"..."}: refused while security.unlockMethod is password.
    /// Wrong current passwords count toward the same rate limit as unlock.
    /// </summary>
    private Reply PasswordVerb(ClientSession s, string rest)
    {
        var pw = _d.Password;
        var (sub, json) = SplitVerb(rest);
        switch (sub.ToLowerInvariant())
        {
            case "" when json.Length == 0:
                return Ok(JsonSerializer.Serialize(new PasswordStatus(pw?.IsSet == true, _d.Keys.HelloAvailable, pw?.RetryAfterSeconds ?? 0), CoreJson.Default.PasswordStatus));
            case "set" when json.Length > 0:
            case "clear" when json.Length > 0:
            {
                if (pw == null) return Err(ErrNoPassword);
                JsonDocument doc;
                try { doc = JsonDocument.Parse(json); }
                catch (JsonException) { return Err(ErrJson); }
                using (doc)
                {
                    var root = doc.RootElement;
                    if (root.ValueKind != JsonValueKind.Object) return Err(ErrJson);
                    bool clear = sub.Equals("clear", StringComparison.OrdinalIgnoreCase);
                    if (pw.IsSet)
                    {
                        switch (pw.Verify(JsonString(root, "current")))
                        {
                            case Crypto.PasswordCheck.Throttled: return Err(ErrTooMany(Math.Max(1, pw.RetryAfterSeconds)));
                            case Crypto.PasswordCheck.Wrong:
                                Audit(s, "password " + (clear ? "clear" : "set"), 0, "");
                                int wait = pw.RetryAfterSeconds;
                                return Err(wait > 0 ? ErrTooMany(wait) : ErrWrongPassword);
                        }
                    }
                    else if (clear) return Err(ErrNoPassword);
                    else if (S.Security.RequireUnlock && !_d.Agent.IsUnlocked && _d.Keys.HelloAvailable) return Err(ErrUnlockRequired);
                    if (clear)
                    {
                        if (UsesPassword) return Err(ErrPasswordInUse);
                        pw.Clear();
                        Audit(s, "password clear", 1, "");
                        return Ok("null");
                    }
                    var next = JsonString(root, "new");
                    if (!Crypto.UnlockPassword.ValidNew(next)) return Err(ErrPasswordRule);
                    pw.Set(next!);
                    Audit(s, "password set", 1, "");
                    return Ok("null");
                }
            }
        }
        return Err(UsagePassword);
    }

    // ------------------------------------------------------------ cloud describe

    private AiStatus AiState()
    {
        var a = S.Ai;
        AiUsage? usage = null;
        try { if (!IsLocked(PipeKind.Ui) && _d.Store() is { } st) usage = Ai.AiBudget.Usage(st, a, Now()); }
        catch (Exception ex) { Log.Warn("ai usage read failed", ex); }
        return new AiStatus(a.Enabled, _d.AiKey?.IsSet == true, a.Provider, AiDefaults.EffectiveModel(a), AiDefaults.EffectiveEndpoint(a),
            usage, Ai.ProviderHttp.IsLoopback(AiDefaults.EffectiveEndpoint(a)));
    }

    /// <summary>ai: status (never the key). ai setkey &lt;key&gt; stores the key DPAPI-encrypted; ai clearkey deletes it.</summary>
    private Reply AiVerb(ClientSession s, string rest)
    {
        var store = _d.AiKey;
        var (sub, tail) = SplitVerb(rest);
        switch (sub.ToLowerInvariant())
        {
            case "" when tail.Length == 0:
                return Ok(JsonSerializer.Serialize(AiState(), CoreJson.Default.AiStatus));
            case "setkey" when tail.Length > 0:
                if (store == null) return Err(ErrAiOff);
                if (!Ai.ApiKeyStore.Valid(tail)) return Err(ErrApiKeyRule);
                store.Set(tail);
                Audit(s, "ai setkey", 1, "");
                return Ok(JsonSerializer.Serialize(AiState(), CoreJson.Default.AiStatus));
            case "clearkey" when tail.Length == 0:
                store?.Clear();
                Audit(s, "ai clearkey", 1, "");
                return Ok(JsonSerializer.Serialize(AiState(), CoreJson.Default.AiStatus));
        }
        return Err(UsageAi);
    }

    private static readonly string[] SendableMimes = ["image/jpeg", "image/png", "image/webp", "image/gif"];

    /// <summary>
    /// describe preview &lt;id&gt;: the image after the redactor, plus a 120 s token. describe send &lt;token&gt;: sends exactly that
    /// image to the provider and returns its text. Nothing leaves the PC without the token, so the UI's confirmation
    /// dialog (provider, model, preview) always comes first. Every send gets an audit entry (verb "describe &lt;provider&gt;",
    /// SHA-256 of the image sent, count 1 on success, 0 on failure).
    /// </summary>
    private async Task<Reply> DescribeAsync(ClientSession s, string[] args, CancellationToken ct)
    {
        if (args.Length != 2) return Err(UsageDescribe);
        var ai = S.Ai;
        if (!ai.Enabled || _d.AiKey == null || _d.Describer == null) return Err(ErrAiOff);
        bool keyNeeded = ai.Provider != AiDefaults.OpenAi;
        var now = Now();
        foreach (var k in s.DescribeTokens.Where(kv => kv.Value.Expires <= now).Select(kv => kv.Key).ToList()) s.DescribeTokens.Remove(k);
        switch (args[0].ToLowerInvariant())
        {
            case "preview":
            {
                if (keyNeeded && !_d.AiKey.IsSet) return Err(ErrNoApiKey);
                if (IsLocked(s.Kind)) return Err(ErrLocked);
                var id = args[1];
                ThumbData? img = _d.Store()!.GetKeyframe(id);
                if (img != null && !SendableMimes.Contains(img.Mime)) img = await _d.Thumbnailer.GetThumbAsync(id, 1280, ct).ConfigureAwait(false);
                if (img == null) return Err(ErrNoImage);
                var outgoing = new Ai.OutgoingImage(img.Bytes, img.Mime, img.Width, img.Height, false);
                try { outgoing = await (_d.Redactor ?? new Ai.PassThroughRedactor()).RedactAsync(id, outgoing, ct).ConfigureAwait(false); }
                catch (Ai.DescribeException ex) { Audit(s, "describe preview", 0, ""); return Err(ex.Message); }
                while (s.DescribeTokens.Count >= 4) s.DescribeTokens.Remove(s.DescribeTokens.MinBy(kv => kv.Value.Expires).Key);
                var token = Convert.ToHexStringLower(RandomNumberGenerator.GetBytes(16));
                s.DescribeTokens[token] = new DescribeTicket(id, outgoing, now.AddSeconds(120));
                Audit(s, "describe preview", 1, "");
                return Ok(JsonSerializer.Serialize(new DescribePreview(token, ai.Provider, AiDefaults.EffectiveModel(ai), AiDefaults.EffectiveEndpoint(ai),
                    outgoing.Mime, outgoing.Width, outgoing.Height, outgoing.Redacted, outgoing.Bytes), CoreJson.Default.DescribePreview));
            }
            case "send":
            {
                if (!s.DescribeTokens.Remove(args[1], out var ticket) || ticket.Expires <= now) return Err(ErrToken);
                if (IsLocked(s.Kind)) return Err(ErrLocked);
                var key = _d.AiKey.Read();
                if (keyNeeded && key == null) return Err(ErrNoApiKey);
                var verb = "describe " + ai.Provider;
                var sha = Convert.ToHexStringLower(SHA256.HashData(ticket.Image.Bytes));
                var store = _d.Store()!;
                if (Ai.AiBudget.Check(Ai.AiBudget.Usage(store, ai, now)) is { } over) return Err(over.Message);
                try
                {
                    var result = await _d.Describer.DescribeAsync(ai, key, ticket.Image, ct).ConfigureAwait(false);
                    store.AddAiUsage(Ai.AiBudget.Today(now), 1, 0);
                    Audit(s, verb, 1, sha);
                    return Ok(JsonSerializer.Serialize(result, CoreJson.Default.DescribeResult));
                }
                catch (Ai.DescribeException ex) { Audit(s, verb, 0, sha); return Err(ex.Message); }
                catch (HttpRequestException ex) { Audit(s, verb, 0, sha); Log.Warn($"cloud describe: {ex.HttpRequestError}"); return Err(ErrProviderUnreachable); }
                catch (TaskCanceledException) when (!ct.IsCancellationRequested) { Audit(s, verb, 0, sha); return Err(ErrProviderTimeout); }
                finally { key = null; }
            }
        }
        return Err(UsageDescribe);
    }

    private Reply Settings(string rest)
    {
        var (sub, json) = SplitVerb(rest);
        switch (sub.ToLowerInvariant())
        {
            case "get" when json.Length == 0:
                return Ok(JsonSerializer.Serialize(S, CoreJson.Default.Settings));
            case "set" when json.Length > 0:
                JsonDocument doc;
                try { doc = JsonDocument.Parse(json); }
                catch (JsonException) { return Err(ErrJson); }
                using (doc)
                {
                    // The password method needs a password first, or nobody could unlock.
                    if (doc.RootElement.ValueKind == JsonValueKind.Object && doc.RootElement.TryGetProperty("security", out var sec)
                        && JsonString(sec, "unlockMethod") == "password" && _d.Password?.IsSet != true)
                        return Err(ErrNoPassword);
                    var error = _d.Settings.TryUpdate(doc.RootElement);
                    if (error != null) return Err(error);
                }
                // Turning search history off forgets what was saved.
                if (!S.Privacy.SearchHistory)
                    try { _d.Store()?.DeleteHistory(null); } catch (Exception ex) { Log.Warn("history clear failed", ex); }
                // Turning chat history off deletes the saved chats (chats of this session stay in memory only).
                if (!S.Ai.ChatHistory)
                    try { if (!IsLocked(PipeKind.Ui)) _d.Store()?.DeleteChats(null); } catch (Exception ex) { Log.Warn("chat clear failed", ex); }
                return Ok(JsonSerializer.Serialize(S, CoreJson.Default.Settings));
        }
        return Err(UsageSettings);
    }

    private async Task<Reply> ReplayAsync(ClientSession s, string rest, CancellationToken ct)
    {
        var (sub, tail) = SplitVerb(rest);
        var args = Args(tail);
        try
        {
            switch (sub.ToLowerInvariant())
            {
                case "save" when args.Length == 0:
                    return Ok(JsonSerializer.Serialize(await _d.Agent.SaveReplayAsync(ct).ConfigureAwait(false), CoreJson.Default.ReplayClip));
                case "list" when args.Length == 0:
                    if (IsLocked(s.Kind)) return Err(ErrLocked);
                    var items = _d.Agent.ListReplays();
                    Audit(s, "replay list", items.Count, "");
                    return Ok(JsonSerializer.Serialize(new ItemsReply<ReplayClip>(items), CoreJson.Default.ItemsReplyReplayClip));
                case "export" when args.Length >= 2:
                {
                    var (id, path) = SplitVerb(tail);
                    path = path.Trim(); // rest of the line, spaces included, no quoting
                    if (!Path.IsPathFullyQualified(path)) return Err(UsageReplay);
                    if (IsLocked(s.Kind)) return Err(ErrLocked);
                    await _d.Agent.ExportReplayAsync(id, path, ct).ConfigureAwait(false);
                    Audit(s, "replay export", 1, "");
                    return Ok("null");
                }
                case "delete" when args.Length == 1:
                    _d.Agent.DeleteReplay(args[0]);
                    return Ok("null");
            }
        }
        catch (Exception ex) when (ex is KeyNotFoundException or FileNotFoundException)
        {
            return Err(ErrNotFound);
        }
        return Err(UsageReplay);
    }

    // ------------------------------------------------------------ bench

    private async Task<Reply> BenchAsync(ClientSession s, string[] args, CancellationToken ct)
    {
        if (args.Length == 0)
            return Ok(JsonSerializer.Serialize(await _d.Agent.BenchAsync(ct).ConfigureAwait(false), CoreJson.Default.BenchResult));
        if (args.Length != 1) return Err(UsageBench);
        switch (args[0].ToLowerInvariant())
        {
            case "full":
            {
                var sw = Stopwatch.StartNew();
                var report = await _d.Agent.BenchFullAsync(ct).ConfigureAwait(false);
                var search = RunSearchBench(s.Kind);
                report = report with { Search = search ?? report.Search, TookMs = Math.Max(report.TookMs, sw.ElapsedMilliseconds) };
                report = report with { Recommendations = Recommender.Recommend(report, S) };
                SaveReport(report);
                return Ok(JsonSerializer.Serialize(report, CoreJson.Default.BenchReport));
            }
            case "last":
            {
                var report = LoadReport();
                return report == null ? Err(ErrNotFound) : Ok(JsonSerializer.Serialize(report, CoreJson.Default.BenchReport));
            }
        }
        return Err(UsageBench);
    }

    private static readonly string[] BenchWords = ["report", "invoice", "meeting", "email", "design", "photo", "budget", "release", "draft", "ticket"];

    /// <summary>20 random-vector queries on the live index, vector and hybrid, median ms. Null when locked or empty.</summary>
    internal SearchBench? RunSearchBench(PipeKind kind)
    {
        var store = _d.Store();
        if (store == null || IsLocked(kind)) return null;
        long rows;
        try { rows = store.Stats().Rows; } catch (Exception ex) { Log.Warn("search bench: stats failed", ex); return null; }
        if (rows == 0) return null;
        int dim = (store as Store.EncryptedStore)?.Space?.Dim ?? S.Index.Dim;
        var rnd = new Random(20261110);
        var vec = new List<double>(20);
        var hyb = new List<double>(20);
        for (int i = 0; i < 20; i++)
        {
            var q = new float[dim];
            for (int j = 0; j < dim; j++) q[j] = (float)(rnd.NextDouble() * 2 - 1);
            var t = Stopwatch.StartNew();
            store.Search(new SearchRequest("x", 10, Mode: SearchMode.Vector), q, 10, UiMaxResponseBytes);
            vec.Add(t.Elapsed.TotalMilliseconds);
            t.Restart();
            store.Search(new SearchRequest(BenchWords[i % BenchWords.Length], 10, Mode: SearchMode.Hybrid), q, 10, UiMaxResponseBytes);
            hyb.Add(t.Elapsed.TotalMilliseconds);
        }
        return new SearchBench(rows, Math.Round(Median(vec), 2), Math.Round(Median(hyb), 2));
    }

    private static double Median(List<double> v)
    {
        v.Sort();
        return v.Count % 2 == 1 ? v[v.Count / 2] : (v[v.Count / 2 - 1] + v[v.Count / 2]) / 2;
    }

    private void SaveReport(BenchReport report)
    {
        if (_d.Blobs == null || _d.Home == null || !_d.Keys.KeyLoaded) { Log.Info("bench report not saved (no key)"); return; }
        try
        {
            var path = DataPaths.BenchReport(_d.Home);
            var tmp = path + ".tmp";
            using (var w = _d.Blobs.OpenWrite(tmp)) JsonSerializer.Serialize(w, report, CoreJson.Default.BenchReport);
            File.Move(tmp, path, overwrite: true);
        }
        catch (Exception ex) { Log.Warn("bench report save failed", ex); }
    }

    private BenchReport? LoadReport()
    {
        if (_d.Blobs == null || _d.Home == null || !_d.Keys.KeyLoaded) return null;
        var path = DataPaths.BenchReport(_d.Home);
        if (!File.Exists(path)) return null;
        try
        {
            using var r = _d.Blobs.OpenRead(path);
            return JsonSerializer.Deserialize(r, CoreJson.Default.BenchReport);
        }
        catch (Exception ex)
        {
            // Damaged, or written under a key destroyed by purge all.
            Log.Warn("bench report unreadable", ex);
            return null;
        }
    }
}
