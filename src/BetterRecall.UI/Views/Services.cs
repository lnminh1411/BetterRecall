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
    public event Action? SettingsLoaded;

    private Services(AppArgs args, DispatcherQueue ui)
    {
        Args = args;
        Ui = ui;
        string pipe;
        IServerTrust trust;
        if (args.Mock)
        {
            var agent = new MockAgent(new MockOptions(args.MockLocked, args.MockPassphrase, args.MockFirstRun, args.MockCapturable, PasswordUnlock: args.MockPassword));
            pipe = MockPipeServer.NameFor(Environment.ProcessId);
            _mock = new MockPipeServer(agent, pipe);
            _mock.Start();
            trust = new ServerTrustVerifier(Environment.ProcessPath!);   // the mock server is this process
        }
        else
        {
            pipe = PipeNames.Ui(PipeNames.UserSid());
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
        Subscription.Disconnected += k => ui.TryEnqueue(() => SetConnection(k));
    }

    /// <summary>A fresh connection for one long request (bench full). The caller disposes it; disposing closes the pipe.</summary>
    public AgentClient CreateRunClient() => new(_pipe, _trust);

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

    public async Task RefreshStatusAsync()
    {
        var r = await Api.StatusAsync();
        if (r.Ok) ApplyStatus(r.Value!);
    }

    public bool Consented => SettingsSchema.Get(Settings.Original, "capture.consented")?.GetValue<bool>() ?? true;
    public string KeyProtection => SettingsSchema.Get(Settings.Original, "security.keyProtection")?.GetValue<string>() ?? "auto";
    public string UnlockMethod => SettingsSchema.Get(Settings.Original, "security.unlockMethod")?.GetValue<string>() ?? "hello";
    public bool AiEnabled => SettingsSchema.Get(Settings.Original, "ai.enabled")?.GetValue<bool>() ?? false;

    /// <summary>Unlock screen mode; the data key still waits for its passphrase while the agent reports pause reason keylocked.</summary>
    public UnlockMode UnlockMode => UnlockLogic.Mode(KeyProtection, UnlockMethod, Status?.PauseReason == "keylocked");

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

    private void ApplyStatus(AgentStatus s)
    {
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
            case AgentErrorKind.NotRunning:
            case AgentErrorKind.NotTrusted:
                SetConnection(kind);
                break;
            default:
                SetConnection(AgentErrorKind.None);
                break;
        }
    }

    private void SetConnection(AgentErrorKind kind)
    {
        if (Connection == kind) return;
        Connection = kind;
        ConnectionChanged?.Invoke();
    }

    private void SetLocked(bool locked)
    {
        if (Locked == locked) return;
        Locked = locked;
        LockedChanged?.Invoke();
        if (!locked) Unlocked?.Invoke();
    }

    /// <summary>After a successful unlock verb.</summary>
    public void MarkUnlocked()
    {
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
        await Subscription.DisposeAsync();
        await MainClient.DisposeAsync();
        await MediaClient.DisposeAsync();
        if (_mock is not null) await _mock.DisposeAsync();
    }
}
