// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;
using BetterRecall.UI.Logic.Mock;
using BetterRecall.UI.Native;
using Microsoft.UI.Dispatching;

namespace BetterRecall.UI.Views;

/// <summary>Process-wide state: agent connections, status, lock state, settings working copy.</summary>
internal sealed class Services
{
    public static Services Current { get; private set; } = null!;

    public AppArgs Args { get; }
    public DispatcherQueue Ui { get; }
    public AgentClient MainClient { get; }
    public AgentClient MediaClient { get; }
    public AgentApi Api { get; }
    /// <summary>Second request connection for thumbnails and keyframes, so previews never queue behind a search.</summary>
    public AgentApi MediaApi { get; }
    public StatusSubscription Subscription { get; }
    public ThumbnailCache Thumbs { get; } = new(48L * 1024 * 1024);
    public SettingsEditor Settings { get; } = new();
    public AgentStatus? Status { get; private set; }
    public bool Locked { get; private set; }
    /// <summary>None when the last exchange worked, else NotRunning or NotTrusted.</summary>
    public AgentErrorKind Connection { get; private set; }

    private readonly MockPipeServer? _mock;
    private readonly string _pipe;
    private readonly IServerTrust _trust;
    private Task<BenchReport?>? _lastBench;

    public event Action? LockedChanged;
    public event Action? Unlocked;
    public event Action? StatusChanged;
    public event Action? ConnectionChanged;
    /// <summary>The agent answers again after the connection was marked down (agent restart): data pages reload, since
    /// what they show is from before the restart and their last request may have failed. Raised on the UI thread.</summary>
    public event Action? Reconnected;
    public event Action? SettingsLoaded;
    /// <summary>The agent process this UI talked to exited (tray Quit, quit event, crash). Raised on the UI thread.</summary>
    public event Action? AgentExited;

    private readonly AgentExitWatch _agentWatch = new();

    private Services(AppArgs args, DispatcherQueue ui)
    {
        Args = args;
        Ui = ui;
        string pipe;
        IServerTrust trust;
        if (args.Mock)
        {
            var agent = new MockAgent(new MockOptions(args.MockLocked, args.MockPassphrase, args.MockFirstRun, args.MockCapturable, PasswordUnlock: args.MockPassword, StoragePaused: args.MockStorage));
            pipe = MockPipeServer.NameFor(Environment.ProcessId);
            _mock = new MockPipeServer(agent, pipe);
            _mock.Start();
            trust = new ServerTrustVerifier(Environment.ProcessPath!);   // the mock server is this process
        }
        else
        {
            pipe = PipeNames.Ui(PipeNames.UserSid(), DataPaths.Home());   // the agent of this UI's data folder only
            trust = new ServerTrustVerifier(ServerTrustVerifier.InstalledAgentPath(AppContext.BaseDirectory));
        }
        _pipe = pipe;
        _trust = trust;
        MainClient = new AgentClient(pipe, trust);
        MediaClient = new AgentClient(pipe, trust);
        Api = new AgentApi(MainClient);
        MediaApi = new AgentApi(MediaClient);
        Subscription = new StatusSubscription(pipe, trust);

        MainClient.ResponseClassified += k => ui.TryEnqueue(() => OnClassified(k));
        MediaClient.ResponseClassified += k => ui.TryEnqueue(() => OnClassified(k));
        Subscription.StatusReceived += s => ui.TryEnqueue(() => { SetConnection(AgentErrorKind.None); ApplyStatus(s); });
        Subscription.Removed += e => ui.TryEnqueue(() => OnRemoved(e));
        Subscription.Disconnected += k => ui.TryEnqueue(() => SetConnection(k));
        Subscription.Connected += () => ui.TryEnqueue(() =>
        {
            // The first connection follows InitAsync's fetch; every later one may face a restarted agent whose settings
            // changed (reset to defaults, file edited): window exclusion, isolation and consent follow them (D2). A UI
            // started before the agent never got them from InitAsync: the first connection loads them then (B8).
            if (SettingsReload.OnConnect(++_subscriptionConnects, Settings.Loaded)) _ = ReloadSettingsAsync();
        });
        _agentWatch.Exited += () => ui.TryEnqueue(() => AgentExited?.Invoke());
        Reconnected += () => PasswordFallback = false;   // a restarted agent says again what it needs (review N2)
    }

    /// <summary>Watches the agent behind the main connection (mock mode: none, the server is this process).</summary>
    private void WatchAgent()
    {
        if (_mock is null) _agentWatch.Watch(MainClient.ServerPid);
    }

    /// <summary>A fresh connection for one long request (bench full). The caller disposes it; disposing closes the pipe.</summary>
    public AgentClient CreateRunClient() => new(_pipe, _trust);

    /// <summary>Requests that may wait long on the agent (launcher search: up to 15 s for the query vector) each on their
    /// own short connection, so settings, timeline and unlock on the main connection do not queue behind them (AB-S1).
    /// Answers are classified like the main connection's (lock screen, offline banner).</summary>
    public OneShotClient CreateOneShotClient()
    {
        var c = new OneShotClient(CreateRunClient, MainClient);
        c.ResponseClassified += k => Ui.TryEnqueue(() => OnClassified(k));
        return c;
    }

    /// <summary>One describe dialog's connection: preview and send on it, closed with the dialog (B2, AB-S1).</summary>
    public DescribeSession CreateDescribeSession()
    {
        var c = CreateRunClient();
        c.ResponseClassified += k => Ui.TryEnqueue(() => OnClassified(k));
        return new DescribeSession(c);
    }

    /// <summary>Captures left the store (another client's purge, retention, storage limit, NSFW drop): their thumbnails
    /// leave the cache and pages drop them (B10, A-L5). Raised on the UI thread.</summary>
    public event Action<RemovedEvent>? Removed;

    private void OnRemoved(RemovedEvent e)
    {
        if (e.Truncated || e.From is not null) Thumbs.Clear();
        else Thumbs.Remove(e.Ids);
        Removed?.Invoke(e);
    }

    /// <summary>"settings reset" while settings.json is unreadable (D4), then fresh settings and status.</summary>
    public async Task<AgentResult<System.Text.Json.Nodes.JsonObject>> ResetSettingsAsync()
    {
        var r = await Api.SettingsResetAsync();
        if (r.Ok)
        {
            Settings.Refresh(r.Value!);
            ApplySecurity();
            SettingsLoaded?.Invoke();
            await RefreshStatusAsync();
        }
        return r;
    }

    /// <summary>Last full benchmark ("bench last"), fetched once and cached. Null when none exists or the agent is unreachable.</summary>
    public Task<BenchReport?> GetLastBenchAsync() => _lastBench ??= FetchLastBenchAsync();

    public void SetLastBench(BenchReport report) => _lastBench = Task.FromResult<BenchReport?>(report);

    private async Task<BenchReport?> FetchLastBenchAsync()
    {
        var r = await Api.BenchLastAsync();
        if (!r.Ok) _lastBench = null;   // retry next time
        return r.Ok ? r.Value : null;
    }

    public static Services Create(AppArgs args, DispatcherQueue ui) => Current = new Services(args, ui);

    public async Task InitAsync()
    {
        await RefreshSettingsAsync();
        await RefreshStatusAsync();
        Subscription.Start();
    }

    public async Task RefreshSettingsAsync()
    {
        var r = await Api.SettingsGetAsync();
        if (!r.Ok) return;
        Settings.Load(r.Value!);
        ApplySecurity();
        SettingsLoaded?.Invoke();
    }

    private int _subscriptionConnects;

    /// <summary>The agent asks for consent (its settings say capture.consented is false) after a reconnect.</summary>
    public event Action? ConsentNeeded;

    /// <summary>Re-reads settings after a reconnect, keeping pending edits, and re-applies window exclusion, memory
    /// isolation and theme (D2).</summary>
    public async Task ReloadSettingsAsync()
    {
        var r = await Api.SettingsGetAsync();
        if (!r.Ok) return;
        Settings.Refresh(r.Value!);
        ApplySecurity();
        SettingsLoaded?.Invoke();
        if (!Consented) ConsentNeeded?.Invoke();
    }

    public async Task RefreshStatusAsync()
    {
        var r = await Api.StatusAsync();
        if (r.Ok) ApplyStatus(r.Value!);
    }

    public bool Consented => SettingsSchema.Get(Settings.Original, "capture.consented")?.GetValue<bool>() ?? true;
    public string KeyProtection => SettingsSchema.Get(Settings.Original, "security.keyProtection")?.GetValue<string>() ?? "auto";
    public string UnlockMethod => SettingsSchema.Get(Settings.Original, "security.unlockMethod")?.GetValue<string>() ?? "hello";
    public bool AiEnabled => SettingsSchema.Get(Settings.Original, "ai.enabled")?.GetValue<bool>() ?? false;

    /// <summary>Unlock screen mode: what the agent waits for (status.unlockNeeds), not what settings say (B1). The password
    /// box also after the agent answered that Hello is missing and the password stands in (B13), until the next unlock.</summary>
    public UnlockMode UnlockMode => PasswordFallback ? UnlockMode.Password
        : UnlockLogic.Mode(Status?.UnlockNeeds, KeyProtection, UnlockMethod, Status?.PauseReason == "keylocked");

    /// <summary>Set by the unlock panel on the "Windows Hello is not available" answer (B13).</summary>
    public bool PasswordFallback { get; set; }

    /// <summary>security.excludeOwnWindows and security.memoryIsolation, from the saved settings.</summary>
    public void ApplySecurity()
    {
        bool exclude = SettingsSchema.Get(Settings.Original, "security.excludeOwnWindows")?.GetValue<bool>() ?? true;
        bool isolate = SettingsSchema.Get(Settings.Original, "security.memoryIsolation")?.GetValue<bool>() ?? true;
        WindowSecurity.SetExclude(exclude);
        ProcessHardening.Apply(isolate);
        ThemeChanged?.Invoke();   // ui.theme may have changed with the same save
    }

    /// <summary>ui.theme as saved: Default follows Windows.</summary>
    public Microsoft.UI.Xaml.ElementTheme Theme => (SettingsSchema.Get(Settings.Original, "ui.theme")?.GetValue<string>()) switch
    {
        "light" => Microsoft.UI.Xaml.ElementTheme.Light,
        "dark" => Microsoft.UI.Xaml.ElementTheme.Dark,
        _ => Microsoft.UI.Xaml.ElementTheme.Default,
    };

    public event Action? ThemeChanged;

    /// <summary>ui.searchAlwaysOnTop as saved (default on).</summary>
    public bool SearchAlwaysOnTop => SettingsSchema.Get(Settings.Original, "ui.searchAlwaysOnTop") is not System.Text.Json.Nodes.JsonValue v
        || !v.TryGetValue<bool>(out var b) || b;

    private void ApplyStatus(AgentStatus s)
    {
        WatchAgent();
        Status = s;
        SetLocked(s.Locked);
        StatusChanged?.Invoke();
    }

    private void OnClassified(AgentErrorKind kind)
    {
        switch (kind)
        {
            case AgentErrorKind.Locked:
                SetConnection(AgentErrorKind.None);
                SetLocked(true);
                break;
            case AgentErrorKind.NotOpen:
                // Key lost or storage error: not a lock. The status (pause reason) drives the banner; ask for a fresh one.
                SetConnection(AgentErrorKind.None);
                _ = RefreshStatusAsync();
                break;
            case AgentErrorKind.NotRunning:
            case AgentErrorKind.NotTrusted:
                SetConnection(kind);
                break;
            default:
                SetConnection(AgentErrorKind.None);
                break;
        }
        if (kind is not (AgentErrorKind.NotRunning or AgentErrorKind.NotTrusted)) WatchAgent();
    }

    private void SetConnection(AgentErrorKind kind)
    {
        if (Connection == kind) return;
        bool wasDown = Connection is AgentErrorKind.NotRunning or AgentErrorKind.NotTrusted;
        Connection = kind;
        ConnectionChanged?.Invoke();
        if (wasDown && kind == AgentErrorKind.None) Reconnected?.Invoke();
    }

    /// <summary>Open dialogs that show decrypted content: hidden on lock and when their window hides (C2).</summary>
    public OpenDialogs<Microsoft.UI.Xaml.Controls.ContentDialog> SensitiveDialogs { get; } = new();

    private void SetLocked(bool locked)
    {
        if (Locked == locked) return;
        Locked = locked;
        if (locked) SensitiveDialogs.HideAll(d => d.Hide());
        LockedChanged?.Invoke();
        if (!locked) Unlocked?.Invoke();
    }

    /// <summary>After a successful unlock verb.</summary>
    public void MarkUnlocked()
    {
        PasswordFallback = false;
        SetLocked(false);
        _ = RefreshStatusAsync();
    }

    /// <summary>The agent shows Windows Hello; let it take the foreground.</summary>
    public void AllowAgentForeground()
    {
        var pid = MainClient.ServerPid;
        if (pid > 0) Win32.AllowSetForegroundWindow((uint)pid);
    }

    public async Task ShutdownAsync()
    {
        _agentWatch.Dispose();
        await Subscription.DisposeAsync();
        await MainClient.DisposeAsync();
        await MediaClient.DisposeAsync();
        if (_mock is not null) await _mock.DisposeAsync();
    }
}
