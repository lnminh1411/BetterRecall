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

/// <param name="Span">"purge last N": the window is recomputed at confirm, so captures stored between preview and
/// confirm (the newest, often most sensitive) are deleted too.</param>
internal sealed record PurgeTicket(PurgeScope? Scope, int Rows, DateTimeOffset Expires, TimeSpan? Span = null);

/// <summary>describe preview: the exact image the confirmed send will use, and the provider, endpoint and model the
/// confirmation dialog named (a send after any of them changed is refused).</summary>
internal sealed record DescribeTicket(string CaptureId, Ai.OutgoingImage Image, DateTimeOffset Expires, string Provider, string Endpoint, string Model)
{
    public bool Matches(AiSettings ai) =>
        Provider == ai.Provider && Endpoint == AiDefaults.EffectiveEndpoint(ai) && Model == AiDefaults.EffectiveModel(ai);
}

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
    /// <summary>purge app: captures store the app as the file name without ".exe", so "claude.exe" or a full path matched
    /// nothing (D13). Pure.</summary>
    internal static string PurgeAppName(string name)
    {
        var n = name.Trim();
        if (n.IndexOfAny(['\\', '/']) >= 0) n = n[(n.LastIndexOfAny(['\\', '/']) + 1)..];
        if (n.EndsWith(".exe", StringComparison.OrdinalIgnoreCase)) n = n[..^4];
        return n.Trim();
    }

    /// <summary>purge site: the host of a pasted URL ("https://www.google.com/path" is google.com after NormalizeSite),
    /// else the argument as typed (D13). Pure.</summary>
    internal static string PurgeSiteHost(string arg)
    {
        var a = arg.Trim();
        if (a.Contains("://", StringComparison.Ordinal) ? Uri.TryCreate(a, UriKind.Absolute, out var u)
            : a.IndexOfAny(['/', ':', '?', '#']) >= 0 && Uri.TryCreate("http://" + a, UriKind.Absolute, out u))
            return u!.Host.Length > 0 ? u.Host : a;
        return a;
    }

    internal const string UsagePurge = "Usage: purge last <seconds> | purge range <from> <to> | purge app <name> | purge site <host> | purge id <id> | purge confirm <token>";
    internal const string UsagePurgeUi = "Usage: purge last <seconds> | purge range <from> <to> | purge app <name> | purge site <host> | purge id <id> | purge all | purge confirm <token>";
    internal const string UsageFacets = "Usage: facets [<from> <to>]";
    internal const string UsageHistory = "Usage: history | history add <text> | history delete <text> | history clear";
    internal const string UsageSubscribe = "Usage: subscribe [captures]";
    internal const string UsageThumb = "Usage: thumb <id> [width]";
    internal const string UsageKeyframe = "Usage: keyframe <id>";
    internal const string UsageTimeline = "Usage: timeline <from> <to> [limit]";
    internal const string UsageSettings = "Usage: settings get | settings set <json> | settings reset";
    internal const string ErrNothingToReset = "settings.json loaded; there is nothing to reset.";
    internal const string UsageReplay = "Usage: replay save | replay list | replay export <id> <path> | replay delete <id>";
    internal const string UsageAudit = "Usage: audit [limit]";
    internal const string UsageBench = "Usage: bench | bench full | bench last";
    internal const string UsageExport = "Usage: export <json>";
    internal const string ErrLocked = "Store is locked.";
    /// <summary>The read gate is open (or not required) but the store did not open: key lost or storage error. Not a lock,
    /// so clients show the pause reason instead of asking for an unlock.</summary>
    internal const string ErrNotOpen = "Store is not open.";
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
    /// <summary>T5: security.unlockMethod hello on a PC without Windows Hello, with an unlock password set.</summary>
    internal const string ErrHelloMissing = "Windows Hello is not available: unlock with the password (config security.unlockMethod).";
    internal const string ErrPassphraseRule = "A new passphrase must be 8 to 1024 characters.";
    internal const string ErrPassphraseFirst = "Set the passphrase first: with security.unlockMethod hello, unlock with the new passphrase, then switch to password.";
    internal const string ErrPassphraseNeeded = "Unlock with the data-key passphrase to change security.keyProtection.";
    internal const string ErrTooManyPrefix = "Too many attempts. Try again in ";
    internal const string ErrAiOff = "Cloud describe is off (config ai.enabled).";
    internal const string ErrNoApiKey = "API key not set.";
    internal const string ErrApiKeyRule = "Invalid API key.";
    internal const string ErrNoImage = "No image to describe.";
    internal const string ErrDescribeChanged = "The AI settings changed after the preview. Open the preview again.";
    internal const string ErrProviderUnreachable = "Provider unreachable.";
    internal const string ErrProviderTimeout = "Provider timed out.";
    internal static string ErrTooMany(int seconds) => ErrTooManyPrefix + seconds.ToString(CultureInfo.InvariantCulture) + " s.";

    private static readonly string[] PublicVerbs = ["ping", "version", "help", "status", "search", "get", "thumb", "subscribe", "pause", "resume", "purge"];
    private static readonly string[] McpVerbs = ["ping", "version", "status", "search", "get"];
    private static readonly string[] UiOnlyVerbs = ["unlock", "lock", "consent", "settings", "timeline", "keyframe", "replay", "panic", "audit", "bench", "facets", "history", "password", "ai", "describe", "export", "chat"];

    private readonly CommandDeps _d;
    private readonly AuditThrottle _connectAudit = new(TimeSpan.FromMinutes(1));
    private DateTimeOffset _lastUnlock = DateTimeOffset.MinValue;

    internal Func<DateTimeOffset> Now { get; set; } = () => DateTimeOffset.UtcNow;

    /// <summary>A status subscriber gets a status at least this often (research 7.7: 60 s). Tests shorten it.</summary>
    internal TimeSpan StatusHeartbeat { get; set; } = TimeSpan.FromSeconds(60);

    private readonly Ai.ChatService? _chat;
    private readonly Queue<DateTimeOffset> _publicSearches = new(), _mcpSearches = new();

    /// <summary>Panic: forget chat approvals and unsaved chats, and delete saved chats. The read gate is already closed,
    /// so this goes to the store directly.</summary>
    /// <summary>Rows left the store for any reason (ICoreHost.Purged): unsaved chats, approvals and running turns that
    /// quote them go too. purge confirm already does this with its window; this covers the panic flush, retention,
    /// thinning, the space guard, NSFW drops and replay deletes (N5).</summary>
    internal void OnStorePurged(PurgeNotice n)
    {
        if (n.Reason == "all") Interlocked.Increment(ref _purgeAllEpoch);
        if (n.Ids.Count > 0 || n.From is not null)
            try { Removed?.Invoke(n); } catch (Exception ex) { Log.Warn("purge: stream notice failed", ex); }
        if (_chat is null || (n.Ids.Count == 0 && n.From is null)) return;
        var gone = n.Ids.ToHashSet(StringComparer.Ordinal);
        _chat.ForgetCaptures(gone.Contains, n.From, n.To);
    }

    /// <summary>Bumped by every purge all notice: a replay save or export that fails while it ran answers the purge
    /// message instead of "Internal error." or "Not found." (sweep 4, batch P's purge-all fence).</summary>
    private int _purgeAllEpoch;

    /// <summary>The message a capture export gets when a purge ran during it; replay saves and exports use it too.</summary>
    internal static readonly string ErrPurgedDuring = new Store.ExportPurgedException().Message;

    /// <summary>Rows left the store: subscribe streams tell their clients (B10, A-L5).</summary>
    private event Action<PurgeNotice>? Removed;

    /// <summary>Most ids one removed event carries; more go out as a truncated event (the client reloads).</summary>
    internal const int MaxRemovedIds = 500;

    internal static RemovedEvent RemovedEventFor(PurgeNotice n) => n.Ids.Count > MaxRemovedIds
        ? new RemovedEvent("removed", n.Reason, [], n.From, n.To, Truncated: true)
        : new RemovedEvent("removed", n.Reason, n.Ids, n.From, n.To);

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
                ApiKey = () => keys.Read(_d.Settings.Current.Ai),
                Http = deps.ChatHttp ?? new Ai.ProviderHttp(),
                Providers = deps.ChatProviders ?? Ai.ChatProviders.For,
                ImageRedactor = deps.Redactor,
            });
            deps.Agent.Panicked += OnPanic;
            // Running chat turns stop on lock and when AI is turned off (panic clears them in OnPanic). Lock also forgets
            // "Allow for this chat": after an unlock every chat asks again before it shares captures.
            deps.Agent.StatusChanged += st => { if (st.Locked) { _chat.StopAll(); _chat.ForgetApprovals(); } };
            deps.Settings.Changed += s => { if (!s.Ai.Enabled) _chat.StopAll(); };
        }
    }

    private Settings S => _d.Settings.Current;

    // ------------------------------------------------------------ connection loop

    /// <summary>Request loop for one trust-checked connection, including subscribe streams and the idle timeout.</summary>
    public async Task ServeAsync(PipeConnection conn, CancellationToken ct)
    {
        var session = new ClientSession(conn.Kind, conn.Peer.Pid, conn.Peer.Image ?? "");
        // One connect row per client image and minute (count = connections merged), so a reconnect loop cannot grow store.db.
        if (_connectAudit.Admit(session.PipeLabel + "|" + session.Image, Now()) is > 0 and var merged) Audit(session, "connect", merged, "");
        var stream = conn.Stream;
        var reader = new LineReader(stream, 8192);
        Task<string?>? next = null; // read started while a long request ran; it is the next request line
        int maxLine = conn.Kind == PipeKind.Ui ? LineProtocol.MaxUiRequestBytes : LineProtocol.MaxRequestBytes;
        while (!ct.IsCancellationRequested)
        {
            string? line;
            using (var idle = CancellationTokenSource.CreateLinkedTokenSource(ct))
            {
                if (conn.Kind != PipeKind.Ui) idle.CancelAfter(TimeSpan.FromSeconds(S.Ipc.IdleTimeoutS));
                try
                {
                    if (next != null) { var pending = next; next = null; line = await pending.ConfigureAwait(false); }
                    else line = await reader.ReadLineAsync(maxLine, idle.Token).ConfigureAwait(false);
                }
                catch (LineTooLongException ex)
                {
                    await WriteAsync(stream, Envelope.Error(ex.Message), ct).ConfigureAwait(false);
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
                next = reader.ReadLineAsync(maxLine, ct);
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
                await StreamAsync(stream, reply.Subscribe, reply.Line, ct, conn.Kind).ConfigureAwait(false);
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

    /// <summary>Verbs that may run for minutes (bench full, up to about 10; chat turns; export and replay export of
    /// large ranges). UI pipe only. Closing the pipe cancels them.</summary>
    internal static bool IsLongRunning(PipeKind kind, string line)
    {
        if (kind != PipeKind.Ui) return false;
        var parts = line.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        if (parts.Length >= 2 && parts[0].Equals("export", StringComparison.OrdinalIgnoreCase)) return true;
        if (parts.Length >= 3 && parts[0].Equals("replay", StringComparison.OrdinalIgnoreCase) && parts[1].Equals("export", StringComparison.OrdinalIgnoreCase)) return true;
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
    /// <param name="kind">Pipe of the subscriber: capture events follow that pipe's gate (B18).</param>
    internal async Task StreamAsync(Stream stream, Subscription sub, string ack, CancellationToken ct, PipeKind kind = PipeKind.Ui)
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
        // While the gate is closed the public and MCP copy hides the queue and store counts; a status that then reads the
        // same as the last one sent is not sent again, as a push for each change of them would still time every capture.
        string? lastSent = null;
        Action<AgentStatus> onStatus = st =>
        {
            var line = Envelope.Ok(JsonSerializer.Serialize(StatusFor(kind, st), CoreJson.Default.AgentStatus));
            if (kind != PipeKind.Ui && line == Volatile.Read(ref lastSent) && IsLocked(kind)) return;
            Volatile.Write(ref lastSent, line);
            Push(line);
        };
        Action<CaptureMeta> onCapture = m =>
        {
            if (IsLocked(kind)) return;   // the subscriber's own pipe, not "both locked" (B18)
            Push(Envelope.Ok(JsonSerializer.Serialize(new CaptureEvent(m.Id, m.Ts, m.Kind, m.App), CoreJson.Default.CaptureEvent)));
        };
        // Rows gone (any client's purge, retention, storage limit, NSFW drop): same gate as capture events (B10, A-L5).
        Action<PurgeNotice> onRemoved = n =>
        {
            if (IsLocked(kind)) return;
            Push(Envelope.Ok(JsonSerializer.Serialize(RemovedEventFor(n), CoreJson.Default.RemovedEvent)));
        };
        if (sub == Subscription.Status) _d.Agent.StatusChanged += onStatus;
        else _d.Agent.CaptureStored += onCapture;
        Removed += onRemoved;
        if (!await WriteAsync(stream, ack, ct).ConfigureAwait(false))
        {
            if (sub == Subscription.Status) _d.Agent.StatusChanged -= onStatus;
            else _d.Agent.CaptureStored -= onCapture;
            Removed -= onRemoved;
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
                tick.CancelAfter(TimeSpan.FromSeconds(sub == Subscription.Status ? Math.Max(StatusHeartbeat.TotalSeconds / 60, StatusHeartbeat.TotalSeconds - lastStatus.Elapsed.TotalSeconds) : 3600));
                string? msg = null;
                try { msg = await ch.Reader.ReadAsync(tick.Token).ConfigureAwait(false); }
                catch (OperationCanceledException) when (!cts.IsCancellationRequested) { }
                catch (OperationCanceledException) { break; }
                if (msg == null)
                {
                    if (sub != Subscription.Status) continue;
                    // The heartbeat follows the same pipe rules as a pushed status (B7: it used to send raw store counts).
                    msg = Envelope.Ok(JsonSerializer.Serialize(StatusFor(kind, _d.Agent.GetStatus()), CoreJson.Default.AgentStatus));
                }
                if (sub == Subscription.Status) lastStatus.Restart();
                if (!await WriteAsync(stream, msg, cts.Token).ConfigureAwait(false)) break;
            }
        }
        finally
        {
            if (sub == Subscription.Status) _d.Agent.StatusChanged -= onStatus;
            else _d.Agent.CaptureStored -= onCapture;
            Removed -= onRemoved;
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
                return Ok(JsonSerializer.Serialize(StatusFor(s.Kind, _d.Agent.GetStatus()), CoreJson.Default.AgentStatus));
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
                _unlockedWithPassphrase = false;
                return Ok("null");
            case "consent":
                // settings.json did not load: consent would write defaults over every other choice (B3, D4).
                if (SettingsUnreadable) return Err(SettingsStore.ErrUnreadable);
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
    internal bool IsLocked(PipeKind kind) => LockError(kind) != null;

    private static readonly StoreStats NoStoreStats = new(0, 0, 0, null, null, 0, 0);

    /// <summary>Status as a pipe may see it: the public and MCP pipes get no store rows, bytes, oldest/newest or queue
    /// count while their gate is closed, since those change with every capture and reveal activity (D12, B7). Locked is
    /// also true while the data key waits for its passphrase, as every data verb then answers "Store is locked." (B6).
    /// The UI pipe learns which secret the unlock screen must ask for (B1, B8, B13).</summary>
    internal AgentStatus StatusFor(PipeKind kind, AgentStatus st)
    {
        bool keyLocked = KeyNeedsPassphrase;
        if (keyLocked && !st.Locked) st = st with { Locked = true, UnlockedUntil = null };
        if (kind == PipeKind.Ui) return st with { UnlockNeeds = UnlockNeeds(keyLocked) };
        return LockError(kind) == ErrLocked ? st with { Store = NoStoreStats, Queue = new QueueState(0) } : st;
    }

    /// <summary>
    /// What an unlock needs now, from the key file and the unlock method rather than settings alone (B1): passphrase while
    /// the data key waits for it (whatever security.keyProtection says; auto keeps a passphrase-wrapped key, D3), or
    /// with the hello method and keyProtection passphrase (checked, or used to re-wrap, before Hello); password with the
    /// password method and a password set, or with the hello method on a PC without Hello (T5 fallback, B13); hello
    /// otherwise (an empty unlock: Windows Hello, or lifting a user Lock with security.requireUnlock off); none while open.
    /// </summary>
    private string UnlockNeeds(bool keyLocked)
    {
        if (keyLocked) return "passphrase";
        if (_d.Agent.IsUnlocked) return "none";
        bool pwSet = _d.Password?.IsSet == true;
        if (UsesPassword) return pwSet ? "password" : "hello";   // no password file: Hello stands in (B15)
        if (S.Security.KeyProtection == Crypto.KeyVault.ModePassphrase) return "passphrase";
        if (S.Security.RequireUnlock && HelloMissing && pwSet) return "password";
        return "hello";
    }

    /// <summary>Null when data verbs may read. "Store is locked." while the gate is closed or the key waits for its
    /// passphrase; "Store is not open." when the gate is open but the store did not open (keylost, storeerror).</summary>
    internal string? LockError(PipeKind kind)
    {
        var s = S;
        // The agent's gate also reports closed with security.requireUnlock off after the user pressed Lock (D4).
        if ((kind == PipeKind.Ui || s.Ipc.RequireUnlocked) && !_d.Agent.IsUnlocked) return ErrLocked;
        return _d.Store() == null ? NotOpenError : null;
    }

    private bool KeyNeedsPassphrase => _d.Keys is Crypto.KeyVault kv ? kv.NeedsPassphrase : (!_d.Keys.KeyLoaded && _d.Keys.Protection == Crypto.KeyVault.ModePassphrase);

    /// <summary>Error for a store that is not open: locked while the key waits for its passphrase, else not open.</summary>
    private string NotOpenError => KeyNeedsPassphrase ? ErrLocked : ErrNotOpen;

    private void Audit(ClientSession s, string verb, int count, string sha)
    {
        try { _d.Store()?.AppendAudit(new AuditEntry(Now(), s.PipeLabel, s.Pid, s.Image, verb, count, sha)); }
        catch (Exception ex) { Log.Warn("audit append failed", ex); }
    }

    internal static string Sha256(string text) => Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(text)));

    /// <summary>Audit hash of a search query: HMAC-SHA256 keyed by an HKDF subkey of the data key, so a dictionary of
    /// common words or sites does not recover the query from the audit log after a purge (B9). Plain SHA-256 only when
    /// no data key is loaded (then no store is open and nothing is audited; tests with a separate store).</summary>
    internal string QueryHash(string q)
    {
        if (_d.Keys is not Crypto.KeyVault { KeyLoaded: true } kv) return Sha256(q);
        byte[] key;
        try { key = kv.Derive("betterrecall-audit-query"); }
        catch (InvalidOperationException) { return ""; }   // key dropped meanwhile: no hash rather than a plain one
        try { return Convert.ToHexStringLower(HMACSHA256.HashData(key, Encoding.UTF8.GetBytes(q))); }
        finally { CryptographicOperations.ZeroMemory(key); }
    }

    /// <summary>Queries whose audit hash an app or site purge blanks (B9): the purge terms themselves (as typed and lower
    /// case) and the saved searches that name them, read before the purge deletes those. Searching the term again would
    /// otherwise match the old hash on the Audit page. Searches neither saved nor equal to a term cannot be found.</summary>
    private static List<string> AuditQueriesFor(IStore store, PurgeScope scope)
    {
        var needles = Store.EncryptedStore.HistoryNeedles(scope);
        var queries = new HashSet<string>(StringComparer.Ordinal);
        foreach (var n in needles) { queries.Add(n); queries.Add(n.ToLowerInvariant()); }
        if (needles.Count > 0)
            try
            {
                foreach (var h in store.History(1000))
                    if (needles.Any(n => h.Q.Contains(n, StringComparison.OrdinalIgnoreCase))) queries.Add(h.Q);
            }
            catch (Exception ex) { Log.Warn("purge: history read failed", ex); }
        return [.. queries];
    }

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
        // Per pipe kind, not per connection: reconnecting used to reset the window.
        var times = s.Kind == PipeKind.Mcp ? _mcpSearches : _publicSearches;
        lock (times)
        {
            while (times.Count > 0 && now - times.Peek() >= TimeSpan.FromMinutes(1)) times.Dequeue();
            if (times.Count >= S.Ipc.MaxSearchPerMinute) return true;
            times.Enqueue(now);
            return false;
        }
    }

    /// <summary>How long a search waits for the query embedding while the worker is not running.</summary>
    internal static readonly TimeSpan ColdEmbedWait = TimeSpan.FromSeconds(3);

    private async Task<Reply> SearchAsync(ClientSession s, string rest, CancellationToken ct)
    {
        var sw = Stopwatch.StartNew();
        var ipc = S.Ipc;
        int max = s.Kind == PipeKind.Ui ? Math.Max(ipc.MaxResults, UiMaxResults) : ipc.MaxResults;
        var (req, error) = ParseSearch(rest, Math.Min(ipc.DefaultResults, max), max);
        if (req == null) return Err(error!);
        if (LockError(s.Kind) is { } locked) return Err(locked);
        if (RateLimited(s)) return Err(ErrRate);
        var store = _d.Store();
        if (store == null) return Err(NotOpenError);

        float[] qv = [];
        string? degraded = null;
        if (req.Mode != SearchMode.Text)
        {
            bool workerCold = !_d.Embedder.Warm;
            bool cold = workerCold && req.Mode != SearchMode.Vector;
            try
            {
                using var to = CancellationTokenSource.CreateLinkedTokenSource(ct);
                // A cold worker takes 10-15 s to start: wait only briefly and answer from text (flagged); the start goes
                // on, so the next search ranks visually again (D5). Vector-only mode has no text answer to give.
                to.CancelAfter(cold ? ColdEmbedWait : TimeSpan.FromSeconds(15));
                qv = await _d.Embedder.EmbedQueryAsync(req.Q, to.Token).ConfigureAwait(false) ?? [];
            }
            catch (Exception ex) when (ex is not OperationCanceledException || !ct.IsCancellationRequested)
            {
                Log.Warn(req.Mode == SearchMode.Vector ? "query embedding failed, vector search has no answer" : "query embedding failed, text-only search", ex);
                qv = [];
                // K review S2: only the cold wait running out means "starting, try again". A failure (model not
                // installed, VC++ runtime missing, restart backoff, a busy worker past 15 s) says nothing of the sort.
                // Vector mode always waits the full time; with a cold worker its timeout is still "starting" (T1).
                degraded = (cold || req.Mode == SearchMode.Vector && workerCold) && ex is OperationCanceledException ? "vector" : "vector-unavailable";
            }
            // The gate may have closed during the wait (up to 15 s): check again before reading the store (B9).
            if (LockError(s.Kind) is { } lockedAfterWait) return Err(lockedAfterWait);
            store = _d.Store();
            if (store == null) return Err(NotOpenError);
        }
        // Vector mode without a query vector has nothing to rank: an empty, flagged answer instead of "Internal error." (T1).
        var resp = req.Mode == SearchMode.Vector && qv.Length == 0
            ? new SearchResponse(0, false, 0, [])
            : store.Search(req, qv, max, MaxResponseBytes(s.Kind));
        if (degraded != null) resp = resp with { Degraded = degraded };
        double weakBelow = S.Index.WeakMatchBelow;
        if (weakBelow > 0)
            resp = resp with { Results = resp.Results.Select(h => h with { Weak = !h.TextMatch && (h.Cosine ?? 0) < weakBelow }).ToList() };
        resp = resp with { TookMs = sw.ElapsedMilliseconds };
        // A lock while the store searched: the answer is not sent (B9).
        if (LockError(s.Kind) is { } lockedAfterSearch) return Err(lockedAfterSearch);
        Audit(s, "search", resp.Count, QueryHash(req.Q));
        return Ok(JsonSerializer.Serialize(resp, CoreJson.Default.SearchResponse));
    }

    // ------------------------------------------------------------ get, thumb, keyframe, timeline

    private Reply Get(ClientSession s, string[] args)
    {
        if (args.Length != 1) return Err(UsageGet);
        if (LockError(s.Kind) is { } locked) return Err(locked);
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
        if (LockError(s.Kind) is { } locked) return Err(locked);
        var t = await _d.Thumbnailer.GetThumbAsync(args[0], width, ct).ConfigureAwait(false);
        // The UI fetches one thumbnail per card; its timeline and search rows already stand for those reads, and a row
        // per thumbnail pushed script and MCP entries out of view (sweep 3 D9). Public pipe thumbs stay audited.
        if (s.Kind != PipeKind.Ui) Audit(s, "thumb", t == null ? 0 : 1, "");
        if (t == null) return Err(ErrNotFound);
        return Ok(JsonSerializer.Serialize(t, CoreJson.Default.ThumbData));
    }

    private Reply Keyframe(ClientSession s, string[] args)
    {
        if (args.Length != 1) return Err(UsageKeyframe);
        if (LockError(s.Kind) is { } locked) return Err(locked);
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
        if (LockError(s.Kind) is { } locked) return Err(locked);
        var store = _d.Store()!;
        if (r.DryRun) return Ok(JsonSerializer.Serialize(Store.CaptureExporter.Count(store, r), CoreJson.Default.ExportResult));
        try
        {
            var result = await Task.Run(() => Store.CaptureExporter.Write(store, r, _d.Thumbnailer.Transcode, ct), ct).ConfigureAwait(false);
            Audit(s, "export", result.Count, "");
            return Ok(JsonSerializer.Serialize(result, CoreJson.Default.ExportResult));
        }
        catch (Store.ExportPurgedException ex) { Log.Info("export aborted: a purge ran during it"); return Err(ex.Message); }
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
        if (LockError(s.Kind) is { } locked) return Err(locked);
        var items = _d.Store()!.Timeline(from, to, limit);
        Audit(s, "timeline", items.Count, "");
        return Ok(JsonSerializer.Serialize(new ItemsReply<CaptureMeta>(items), CoreJson.Default.ItemsReplyCaptureMeta));
    }

    private Reply AuditVerb(ClientSession s, string[] args)
    {
        int limit = 200;
        if (args.Length > 1 || (args.Length == 1 && !int.TryParse(args[0], NumberStyles.None, CultureInfo.InvariantCulture, out limit))) return Err(UsageAudit);
        if (limit is < 1 or > 10000) return Err("Invalid limit.");
        if (LockError(s.Kind) is { } locked) return Err(locked);
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
        if (LockError(s.Kind) is { } locked) return Err(locked);
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
        if (LockError(s.Kind) is { } locked) return Err(locked);
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
            if (LockError(s.Kind) is { } locked) return Err(locked);
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
        s.PurgeTokens[token] = new PurgeTicket(scope, rows, now.AddSeconds(30), scope is { Kind: "last", From: { } f, To: { } t } ? t - f : null);
        return token;
    }

    private async Task<Reply> PurgeAsync(ClientSession s, string[] args, CancellationToken ct)
    {
        bool ui = s.Kind == PipeKind.Ui;
        if (!ui)
        {
            if (!S.Ipc.AllowControl) return Err(ErrControlOff);
            if (!S.Ipc.AllowPurge) return Err(ErrPurgeOff);
            // Row counts reveal visited sites and apps: scripts need the read gate open, like search.
            if (LockError(s.Kind) is { } locked) return Err(locked);
        }
        string usage = ui ? UsagePurgeUi : UsagePurge;
        if (args.Length == 0) return Err(usage);
        var sub = args[0].ToLowerInvariant();
        // UI pipe: scoped previews (row counts) and their confirms need the read gate open too. purge all has its own
        // recent-unlock check and stays possible while locked (panic).
        if (ui && sub is not ("all" or "confirm") && LockError(s.Kind) is { } uiLocked) return Err(uiLocked);
        PurgeScope scope;
        switch (sub)
        {
            case "confirm":
            {
                if (args.Length != 2) return Err(usage);
                var now = Now();
                if (!s.PurgeTokens.Remove(args[1], out var ticket) || ticket.Expires <= now) return Err(ErrToken);
                if (ui && ticket.Scope != null && LockError(s.Kind) is { } confirmLocked) return Err(confirmLocked);
                if (ticket.Scope == null)
                {
                    int all = await _d.PurgeAll(ct).ConfigureAwait(false);
                    _chat?.Clear();
                    return Ok(JsonSerializer.Serialize(new PurgeResult(all), CoreJson.Default.PurgeResult));
                }
                var st = _d.Store();
                if (st == null) return Err(NotOpenError);
                var scope2 = ticket.Span is { } span ? PurgeScope.Last(span) : ticket.Scope;
                var auditQueries = scope2.Kind is "app" or "site" ? AuditQueriesFor(st, scope2) : null;
                int n = st.Purge(scope2);
                if (auditQueries is { Count: > 0 } && st is Store.EncryptedStore es)
                    es.ClearAuditQueryHashes(auditQueries.SelectMany(q => new[] { QueryHash(q), Sha256(q) }).Where(h => h.Length > 0).Distinct().ToList());
                // Unsaved chats, approval cards and running turns that quote a deleted capture go too.
                bool window = scope2.Kind is "last" or "range";
                _chat?.ForgetCaptures(id => st.Get(id) == null, window ? scope2.From : null, window ? scope2.To : null);
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
                if (args.Length == 2 && args[1].Length > 0 && args[1].All(char.IsAsciiDigit) && !long.TryParse(args[1], NumberStyles.None, CultureInfo.InvariantCulture, out _))
                    return Err("Invalid seconds.");   // digits that overflow are a range error, not a usage error (D13)
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
                var appName = PurgeAppName(string.Join(' ', args.Skip(1)));
                if (appName.Length == 0) return Err(usage);
                scope = PurgeScope.ForApp(appName);
                break;
            case "site":
                if (args.Length != 2) return Err(usage);
                var host = PurgeSiteHost(args[1]);
                if (Store.EncryptedStore.NormalizeSite(host).Length == 0) return Err(usage);
                scope = PurgeScope.ForSite(host);
                break;
            case "id":
                if (args.Length != 2 || args[1].Length > 64) return Err(usage);
                scope = PurgeScope.ForId(args[1]);
                break;
            default:
                return Err(usage);
        }
        var store = _d.Store();
        if (store == null) return Err(NotOpenError);
        int rows = store.CountPurge(scope);
        return Ok(JsonSerializer.Serialize(new PurgeConfirmReply(NewToken(s, scope, rows), rows), CoreJson.Default.PurgeConfirmReply));
    }

    // ------------------------------------------------------------ UI verbs

    private bool UsesPassword => S.Security.UnlockMethod == "password";

    /// <summary>settings.json did not parse at start: defaults are in use and saves are refused until a reset (D4).</summary>
    private bool SettingsUnreadable => _d.Settings is SettingsStore { LoadError: not null };

    /// <summary>The Windows Hello check finished and found none on this PC.</summary>
    private bool HelloMissing => !_d.Keys.HelloAvailable && _d.Keys is not Crypto.KeyVault { HelloChecked: false };

    /// <summary>The configured unlock method cannot open the gate: hello without Windows Hello on this PC (T5), or password
    /// with keys\unlock.bin missing or damaged and no Hello to fall back on (B15).</summary>
    private bool NoUnlockPath => S.Security.RequireUnlock && HelloMissing && (!UsesPassword || _d.Password?.IsSet != true);

    /// <summary>
    /// unlock [secret]. Data key still passphrase-wrapped: the secret is the passphrase; an empty one runs Windows Hello
    /// only, which is enough for purge all (a forgotten passphrase can start over) but opens no data. Otherwise with
    /// security.unlockMethod password the secret is the unlock password (rate-limited, no Hello prompt); with hello an
    /// optional passphrase check comes first, then Windows Hello. With the password method a passphrase that loads the
    /// key also opens the gate: it is the stronger secret of the two. A key that is not loaded (keylost, storeerror)
    /// ignores the secret.
    /// </summary>
    private async Task<Reply> UnlockAsync(ClientSession s, string rest, CancellationToken ct)
    {
        var keys = _d.Keys;
        bool rewrap = false, helloFallback = false, passphraseChecked = false;
        // Passphrase guesses are rate-limited like the unlock password (each one is a 64 MiB Argon2id run).
        bool passphraseTry = rest.Length > 0 && (KeyNeedsPassphrase || !UsesPassword && keys.KeyLoaded && keys.Protection == Crypto.KeyVault.ModePassphrase);
        if (passphraseTry && keys.PassphraseRetryAfterSeconds is > 0 and var passWait) return Err(ErrTooMany(passWait));
        // A passphrase that would create the key or re-wrap it must meet the rule (B14); say so instead of "Wrong passphrase.".
        // Only where the text is a passphrase: with the password method and a loaded key it is the unlock password (6+).
        if (rest.Length > 0 && (KeyNeedsPassphrase || !UsesPassword) && keys is Crypto.KeyVault { WouldSetPassphrase: true }
            && !Crypto.KeyVault.ValidNewPassphrase(rest)) return Err(ErrPassphraseRule);
        if (KeyNeedsPassphrase)
        {
            if (rest.Length > 0)
            {
                if (!keys.UnlockWithPassphrase(rest)) return Err(ErrWrongPassphrase);
                passphraseChecked = true;
                if (UsesPassword) { _unlockedWithPassphrase = true; return OpenVerified(s, "unlock passphrase"); }
            }
        }
        else if (UsesPassword)
        {
            var pw = _d.Password;
            var check = pw == null ? Crypto.PasswordCheck.NotSet : pw.Verify(rest);
            switch (check)
            {
                case Crypto.PasswordCheck.NotSet:
                    // B15: keys\unlock.bin missing or damaged. Windows Hello opens the gate instead (below); without Hello
                    // the answer stays "no password", and password set is allowed while locked.
                    Log.Warn("unlock password file missing or damaged: trying Windows Hello instead");
                    helloFallback = true;
                    break;
                case Crypto.PasswordCheck.Throttled: return Err(ErrTooMany(Math.Max(1, pw!.RetryAfterSeconds)));
                case Crypto.PasswordCheck.Wrong:
                    Audit(s, "unlock password", 0, "");
                    int wait = pw!.RetryAfterSeconds;
                    return Err(wait > 0 ? ErrTooMany(wait) : ErrWrongPassword);
            }
            if (!helloFallback) { _unlockedWithPassphrase = false; return OpenVerified(s, "unlock password"); }
        }
        else if (rest.Length > 0 && keys.KeyLoaded)
        {
            // Already passphrase-wrapped: check the passphrase before Hello. security.keyProtection = passphrase while the
            // key file is still tpm/dpapi (keys.Protection reports the file's mode): the passphrase re-wraps the key, but
            // only after Hello passed, so a cancelled Hello leaves the key file as it was.
            if (keys.Protection == Crypto.KeyVault.ModePassphrase) { if (!keys.UnlockWithPassphrase(rest)) return Err(ErrWrongPassphrase); passphraseChecked = true; }
            else rewrap = S.Security.KeyProtection == Crypto.KeyVault.ModePassphrase;
        }
        if (!await _d.Agent.UnlockAsync(ct).ConfigureAwait(false))
        {
            if (helloFallback) return Err(ErrNoPassword);
            // T5: hello configured on a PC without Windows Hello. The prompt cannot work, so the passphrase just checked or
            // the unlock password stands in for it; otherwise say what is missing instead of "cancelled".
            if (S.Security.RequireUnlock && HelloMissing) return HelloMissingUnlock(s, rest, passphraseChecked, rewrap);
            return Err(ErrUnlockCancelled);
        }
        if (rewrap && !keys.UnlockWithPassphrase(rest)) return Err(ErrWrongPassphrase);
        if (helloFallback) Audit(s, "unlock hello (no password file)", 1, "");
        _unlockedWithPassphrase = passphraseChecked || rewrap;
        _lastUnlock = Now();
        var until = _d.Agent.GetStatus().UnlockedUntil;
        return Ok(JsonSerializer.Serialize(new UnlockReply(true, until), CoreJson.Default.UnlockReply));
    }

    /// <summary>T5: unlock with security.unlockMethod hello after the Hello prompt failed on a PC without Windows Hello.</summary>
    private Reply HelloMissingUnlock(ClientSession s, string rest, bool passphraseChecked, bool rewrap)
    {
        if (passphraseChecked) { _unlockedWithPassphrase = true; return OpenVerified(s, "unlock passphrase"); }
        var pw = _d.Password;
        if (pw?.IsSet != true) return Err(ErrNoPassword);
        // A re-wrap needs a proof of the user before it runs, and an empty secret is no password.
        if (rest.Length == 0 || rewrap) return Err(ErrHelloMissing);
        switch (pw.Verify(rest))
        {
            case Crypto.PasswordCheck.NotSet: return Err(ErrNoPassword);
            case Crypto.PasswordCheck.Throttled: return Err(ErrTooMany(Math.Max(1, pw.RetryAfterSeconds)));
            case Crypto.PasswordCheck.Wrong:
                Audit(s, "unlock password", 0, "");
                int wait = pw.RetryAfterSeconds;
                return Err(wait > 0 ? ErrTooMany(wait) : ErrWrongPassword);
        }
        _unlockedWithPassphrase = false;
        return OpenVerified(s, "unlock password (no Hello)");
    }

    /// <summary>The gate was last opened with the data-key passphrase checked (B16).</summary>
    private bool _unlockedWithPassphrase;

    /// <summary>Last value of security.name in a settings set (TryUpdate applies them in order), else null.</summary>
    private static string? SecurityValue(JsonElement root, string name)
    {
        string? v = null;
        if (root.ValueKind != JsonValueKind.Object) return null;
        foreach (var g in root.EnumerateObject())
            if (g.Name == "security" && g.Value.ValueKind == JsonValueKind.Object)
                foreach (var k in g.Value.EnumerateObject())
                    if (k.Name == name && k.Value.ValueKind == JsonValueKind.String) v = k.Value.GetString();
        return v;
    }

    /// <summary>
    /// security.keyProtection changes the settings set refuses:
    /// - passphrase with unlockMethod password while the key file is not passphrase-wrapped: unlock then checks only the
    ///   password, so the re-wrap never runs and the key stays dpapi/tpm while settings say passphrase (B7);
    /// - off passphrase while the key file is passphrase-wrapped, unless the gate was opened with that passphrase: the
    ///   unlock password alone would re-wrap the key to dpapi (B16).
    /// </summary>
    private string? KeyProtectionError(JsonElement root)
    {
        var protection = SecurityValue(root, "keyProtection");
        var method = SecurityValue(root, "unlockMethod");
        if (protection is null && method is null) return null;
        string fileMode = _d.Keys.Protection;
        string nextProtection = protection ?? S.Security.KeyProtection, nextMethod = method ?? S.Security.UnlockMethod;
        if (nextProtection == Crypto.KeyVault.ModePassphrase && nextMethod == "password" && fileMode != Crypto.KeyVault.ModePassphrase)
            return ErrPassphraseFirst;
        if (protection is not null && protection != Crypto.KeyVault.ModePassphrase && fileMode == Crypto.KeyVault.ModePassphrase
            && _d.Keys.KeyLoaded && !_unlockedWithPassphrase)
            return ErrPassphraseNeeded;
        return null;
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
                    // First password while locked: only when Windows Hello is known to be missing (nobody could unlock
                    // otherwise). Before the Hello check finished, "unavailable" just meant "not checked yet".
                    else if (S.Security.RequireUnlock && !_d.Agent.IsUnlocked && (_d.Keys.HelloAvailable || _d.Keys is Crypto.KeyVault { HelloChecked: false }))
                        return Err(ErrUnlockRequired);
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
        return new AiStatus(a.Enabled, _d.AiKey?.Read(a) != null, a.Provider, AiDefaults.EffectiveModel(a), AiDefaults.EffectiveEndpoint(a),
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
                if (LockError(s.Kind) is { } setLocked) return Err(setLocked);
                if (!Ai.ApiKeyStore.Valid(tail)) return Err(ErrApiKeyRule);
                store.Set(tail, S.Ai);
                Audit(s, "ai setkey", 1, "");
                return Ok(JsonSerializer.Serialize(AiState(), CoreJson.Default.AiStatus));
            case "clearkey" when tail.Length == 0:
                if (LockError(s.Kind) is { } clearLocked) return Err(clearLocked);
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
                if (keyNeeded && _d.AiKey.Read(ai) == null) return Err(ErrNoApiKey);
                if (LockError(s.Kind) is { } locked) return Err(locked);
                var id = args[1];
                ThumbData? img = _d.Store()!.GetKeyframe(id);
                if (img != null && !SendableMimes.Contains(img.Mime)) img = await _d.Thumbnailer.GetThumbAsync(id, 1280, ct).ConfigureAwait(false);
                if (img == null) return Err(ErrNoImage);
                var outgoing = new Ai.OutgoingImage(img.Bytes, img.Mime, img.Width, img.Height, false);
                try { outgoing = await (_d.Redactor ?? new Ai.PassThroughRedactor()).RedactAsync(id, outgoing, ct).ConfigureAwait(false); }
                catch (Ai.DescribeException ex) { Audit(s, "describe preview", 0, ""); return Err(ex.Message); }
                while (s.DescribeTokens.Count >= 4) s.DescribeTokens.Remove(s.DescribeTokens.MinBy(kv => kv.Value.Expires).Key);
                var token = Convert.ToHexStringLower(RandomNumberGenerator.GetBytes(16));
                s.DescribeTokens[token] = new DescribeTicket(id, outgoing, now.AddSeconds(120), ai.Provider, AiDefaults.EffectiveEndpoint(ai), AiDefaults.EffectiveModel(ai));
                Audit(s, "describe preview", 1, "");
                return Ok(JsonSerializer.Serialize(new DescribePreview(token, ai.Provider, AiDefaults.EffectiveModel(ai), AiDefaults.EffectiveEndpoint(ai),
                    outgoing.Mime, outgoing.Width, outgoing.Height, outgoing.Redacted, outgoing.Bytes), CoreJson.Default.DescribePreview));
            }
            case "send":
            {
                if (!s.DescribeTokens.Remove(args[1], out var ticket) || ticket.Expires <= now) return Err(ErrToken);
                if (LockError(s.Kind) is { } locked) return Err(locked);
                // The user confirmed a send to the provider, endpoint and model the preview showed, nowhere else.
                if (!ticket.Matches(ai)) return Err(ErrDescribeChanged);
                var key = _d.AiKey.Read(ai);
                if (keyNeeded && key == null) return Err(ErrNoApiKey);
                var verb = "describe " + ai.Provider;
                var sha = Convert.ToHexStringLower(SHA256.HashData(ticket.Image.Bytes));
                var store = _d.Store()!;
                // The capture was purged after the preview (panic, purge, lock and unlock in between): its image is gone (B8).
                if (store.Get(ticket.CaptureId) == null) return Err(ErrNotFound);
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

    /// <summary>Groups that change what is protected, kept or redacted, or what leaves the PC (retention.days and
    /// storage.maxGb delete history; privacy and ocr off stop redaction; mcp opens a pipe; replay.encryptExports and
    /// exportDir write plaintext clips anywhere; audio.mic records the room; index.dim truncates every vector;
    /// keyframes and inference.modelDir change what is stored and which models read it, B6).</summary>
    internal static readonly string[] GuardedSettingGroups = ["security", "ai", "ipc", "mcp", "privacy", "ocr", "retention", "storage", "replay", "index", "audio", "keyframes"];

    /// <summary>Single keys guarded outside the guarded groups (group, key).</summary>
    internal static readonly (string Group, string Key)[] GuardedSettingKeys = [("inference", "modelDir")];

    /// <summary>Every property is looked at (a repeated group name cannot hide a guarded key).</summary>
    internal static bool TouchesGuardedSettings(JsonElement root)
    {
        if (root.ValueKind != JsonValueKind.Object) return false;
        foreach (var g in root.EnumerateObject())
        {
            if (GuardedSettingGroups.Contains(g.Name)) return true;
            if (g.Value.ValueKind != JsonValueKind.Object) continue;
            foreach (var k in g.Value.EnumerateObject())
                if (GuardedSettingKeys.Contains((g.Name, k.Name))) return true;
        }
        return false;
    }

    private Reply Settings(string rest)
    {
        var (sub, json) = SplitVerb(rest);
        switch (sub.ToLowerInvariant())
        {
            case "get" when json.Length == 0:
                return Ok(JsonSerializer.Serialize(S, CoreJson.Default.Settings));
            case "reset" when json.Length == 0:
                // Only while settings.json is unreadable: the defaults already in use are written out (the bad file stays
                // as settings.json.bad). Nothing changes in effect, so the gate does not apply (B3, D4).
                if (_d.Settings is not SettingsStore store || store.LoadError is null) return Err(ErrNothingToReset);
                store.ResetUnreadable();
                return Ok(JsonSerializer.Serialize(S, CoreJson.Default.Settings));
            case "set" when json.Length > 0:
                JsonDocument doc;
                try { doc = JsonDocument.Parse(json); }
                catch (JsonException) { return Err(ErrJson); }
                using (doc)
                {
                    // Guarded groups change what is protected, kept or redacted and what leaves the PC: not while the gate
                    // is closed (a passphrase-wrapped key would otherwise be re-wrapped to dpapi without the passphrase).
                    if (LockError(PipeKind.Ui) is { } locked && TouchesGuardedSettings(doc.RootElement)
                        && !(NoUnlockPath && OnlyUnlockMethod(doc.RootElement)))
                        return Err(locked);
                    // The password method needs a password first, or nobody could unlock.
                    if (doc.RootElement.ValueKind == JsonValueKind.Object && doc.RootElement.TryGetProperty("security", out var sec)
                        && JsonString(sec, "unlockMethod") == "password" && _d.Password?.IsSet != true)
                        return Err(ErrNoPassword);
                    if (KeyProtectionError(doc.RootElement) is { } keyErr) return Err(keyErr);
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

    /// <summary>{"security":{"unlockMethod":...}} and nothing else. Allowed while locked when no unlock path exists (T5):
    /// otherwise only hand-editing settings.json gets the user back in. Password still needs a password set (checked
    /// below), so this opens nothing by itself.</summary>
    private static bool OnlyUnlockMethod(JsonElement root)
    {
        int groups = 0, keys = 0;
        foreach (var g in root.EnumerateObject()) groups++;
        if (groups != 1 || !root.TryGetProperty("security", out var sec) || sec.ValueKind != JsonValueKind.Object) return false;
        foreach (var k in sec.EnumerateObject()) keys++;
        return keys == 1 && sec.TryGetProperty("unlockMethod", out var m) && m.ValueKind == JsonValueKind.String;
    }

    private async Task<Reply> ReplayAsync(ClientSession s, string rest, CancellationToken ct)
    {
        var (sub, tail) = SplitVerb(rest);
        var args = Args(tail);
        int purgeEpoch = Volatile.Read(ref _purgeAllEpoch);
        try
        {
            switch (sub.ToLowerInvariant())
            {
                case "save" when args.Length == 0:
                    return Ok(JsonSerializer.Serialize(await _d.Agent.SaveReplayAsync(ct).ConfigureAwait(false), CoreJson.Default.ReplayClip));
                case "list" when args.Length == 0:
                    if (LockError(s.Kind) is { } locked) return Err(locked);
                    var items = _d.Agent.ListReplays();
                    Audit(s, "replay list", items.Count, "");
                    return Ok(JsonSerializer.Serialize(new ItemsReply<ReplayClip>(items), CoreJson.Default.ItemsReplyReplayClip));
                case "export" when args.Length >= 2:
                {
                    var (id, path) = SplitVerb(tail);
                    path = path.Trim(); // rest of the line, spaces included, no quoting
                    if (!Path.IsPathFullyQualified(path)) return Err(UsageReplay);
                    if (LockError(s.Kind) is { } exportLocked) return Err(exportLocked);
                    await _d.Agent.ExportReplayAsync(id, path, ct).ConfigureAwait(false);
                    Audit(s, "replay export", 1, "");
                    return Ok("null");
                }
                case "delete" when args.Length == 1:
                    if (LockError(s.Kind) is { } deleteLocked) return Err(deleteLocked);
                    _d.Agent.DeleteReplay(args[0]);
                    return Ok("null");
            }
        }
        catch (Exception) when (Volatile.Read(ref _purgeAllEpoch) != purgeEpoch)
        {
            // Purge all discarded the clip being saved, or cancelled the export and deleted its copy.
            Log.Info("replay " + sub.ToLowerInvariant() + ": purge all ran during it");
            return Err(ErrPurgedDuring);
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
