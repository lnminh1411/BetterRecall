// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Text.Json;
using BetterRecall.Contracts;
using BetterRecall.Core.Ipc;

namespace BetterRecall.Agent.Conformance.Harness;

internal sealed class AgentHarness : IAsyncDisposable
{
    private readonly InstallFixture _install;
    private readonly List<(Process Proc, int Pid, string Home)> _agents = [];
    private readonly List<string> _extraHomes = [];
    private UiDriverClient? _ui;

    public string Home { get; }
    public string Passphrase { get; }
    public string Password { get; }
    public InstallFixture Install => _install;

    public AgentHarness(InstallFixture install, Action<Settings>? configureSettings = null)
    {
        _install = install;
        Home = TestEnv.NewDir("home");
        Passphrase = "br-conf-pass-" + Guid.NewGuid().ToString("N")[..12];
        Password = "br-conf-pw-" + Guid.NewGuid().ToString("N")[..10];
        WriteSettings(Home, configureSettings);
    }

    public string NewExtraHome(Action<Settings>? configureSettings = null)
    {
        var extra = TestEnv.NewDir("home");
        _extraHomes.Add(extra);
        WriteSettings(extra, configureSettings);
        return extra;
    }

    public static void WriteSettings(string home, Action<Settings>? configure = null)
    {
        var s = new Settings();
        s.Capture.Consented = false;
        s.Capture.Enabled = false;
        s.Capture.EventTriggers = false;
        s.Keyframes.Enabled = false;
        s.Replay.Enabled = false;
        s.Replay.Audio = false;
        s.Replay.Notify = false;
        s.Audio.System = false;
        s.Audio.Mic = false;
        s.Ocr.Enabled = false;
        s.Privacy.NsfwFilter = false;
        s.Privacy.NsfwDetector = false;
        s.Privacy.RedactPasswords = false;
        s.Privacy.RedactCards = false;
        s.Privacy.RedactIds = false;
        s.Privacy.RedactBirthDates = false;
        s.Hotkeys.Search = "";
        s.Hotkeys.Pause = "";
        s.Hotkeys.Replay = "";
        s.Hotkeys.Panic = "";
        s.Hotkeys.Timeline = "";
        s.Hotkeys.Home = "";
        s.Hotkeys.Lock = "";
        s.Ui.StartMinimized = true;
        s.Ai.Enabled = false;
        s.Inference.Fake = true;
        s.Inference.ModelDir = Path.Combine(home, "models");
        s.Security.KeyProtection = "passphrase";
        s.Security.UnlockMethod = "password";
        s.Security.RequireUnlock = true;
        configure?.Invoke(s);

        Directory.CreateDirectory(home);
        File.WriteAllBytes(DataPaths.SettingsFile(home), JsonSerializer.SerializeToUtf8Bytes(s, ContractsJson.Default.Settings));
    }

    public UiDriverClient Ui => _ui ??= new UiDriverClient(_install.UiExe);

    public Process StartAgent(string? homeOverride = null, params string[] extraArgs)
    {
        var home = homeOverride ?? Home;
        var psi = new ProcessStartInfo(_install.AgentExe)
        {
            UseShellExecute = false,
            CreateNoWindow = true,
            WorkingDirectory = _install.InstallDir,
        };
        psi.ArgumentList.Add("--no-autostart");
        foreach (var a in extraArgs) psi.ArgumentList.Add(a);
        psi.Environment[DataPaths.HomeEnv] = home;

        var proc = Process.Start(psi) ?? throw new InvalidOperationException("Failed to start BetterRecall.Agent.exe.");
        _agents.Add((proc, proc.Id, home));
        return proc;
    }

    public async Task<(Process Agent, int ConnectedServerPid)> StartAndConnectUiAsync(string conn = "ui", string? homeOverride = null)
    {
        var home = homeOverride ?? Home;
        var agent = StartAgent(home);
        int pid = await Ui.WaitForConnectAsync(conn, home, TimeSpan.FromSeconds(15)).ConfigureAwait(false);
        return (agent, pid);
    }

    public async Task SetupPassphraseAndWaitStoreAsync(string conn = "ui", string? passphrase = null)
    {
        var (ok, d, err) = await Ui.RequestAsync(conn, "unlock " + (passphrase ?? Passphrase)).ConfigureAwait(false);
        Assert.True(ok, err);
        Assert.True(d.GetProperty("unlocked").GetBoolean());
        await WaitForStoreOpenAsync(conn).ConfigureAwait(false);
    }

    public async Task WaitForStoreOpenAsync(string conn = "ui", TimeSpan? timeout = null)
    {
        var limit = timeout ?? TimeSpan.FromSeconds(10);
        var sw = Stopwatch.StartNew();
        while (true)
        {
            var (ok, _, err) = await Ui.RequestAsync(conn, "search {\"q\":\"warmup\",\"mode\":\"text\"}").ConfigureAwait(false);
            if (ok) return;
            if (sw.Elapsed >= limit)
                throw new TimeoutException($"Store did not open within {limit.TotalSeconds:F1} s (last error={err}).");
            await Task.Delay(50).ConfigureAwait(false);
        }
    }

    public async Task WaitForWorkerWarmAsync(string conn = "ui", TimeSpan? timeout = null)
    {
        var limit = timeout ?? TimeSpan.FromSeconds(15);
        var sw = Stopwatch.StartNew();
        while (true)
        {
            var (ok, d, err) = await Ui.RequestAsync(conn, "status").ConfigureAwait(false);
            Assert.True(ok, err);
            var st = d.Deserialize(ContractsJson.Default.AgentStatus)!;
            if (!string.Equals(st.Device, "none", StringComparison.OrdinalIgnoreCase)) return;
            if (sw.Elapsed >= limit)
                throw new TimeoutException($"Worker did not finish prewarm within {limit.TotalSeconds:F1} s.");
            await Task.Delay(50).ConfigureAwait(false);
        }
    }

    public async Task SetUnlockPasswordAsync(string conn = "ui", string? password = null)
    {
        var json = JsonSerializer.Serialize(new { @new = password ?? Password });
        var (ok, _, err) = await Ui.RequestAsync(conn, "password set " + json).ConfigureAwait(false);
        Assert.True(ok, err);
    }

    public Task<(PipeConnectResult Result, PipeClient? Client)> ConnectPublicAsync(int timeoutMs = 2000, string pipeBase = "BetterRecall", string? homeOverride = null)
    {
        var pipeName = PipeNames.Public(pipeBase, PipeNames.UserSid(), homeOverride ?? Home);
        return PipeClient.ConnectAsync(pipeName, _install.AgentExe, timeoutMs);
    }

    public async Task<PipeClient> WaitForPublicConnectAsync(TimeSpan? timeout = null, string pipeBase = "BetterRecall")
    {
        var limit = timeout ?? TimeSpan.FromSeconds(5);
        var sw = Stopwatch.StartNew();
        while (true)
        {
            var (res, client) = await ConnectPublicAsync(500, pipeBase).ConfigureAwait(false);
            if (res == PipeConnectResult.Ok && client is not null) return client;
            if (res == PipeConnectResult.NotTrusted)
                throw new InvalidOperationException("Public pipe server trust check failed.");
            if (sw.Elapsed >= limit)
                throw new TimeoutException($"Public pipe did not open within {limit.TotalSeconds:F1} s.");
            await Task.Delay(50).ConfigureAwait(false);
        }
    }

    public async Task WaitForPublicClosedAsync(TimeSpan? timeout = null, string pipeBase = "BetterRecall")
    {
        var limit = timeout ?? TimeSpan.FromSeconds(5);
        var sw = Stopwatch.StartNew();
        while (true)
        {
            var (res, client) = await ConnectPublicAsync(300, pipeBase).ConfigureAwait(false);
            if (client is not null) await client.DisposeAsync().ConfigureAwait(false);
            if (res == PipeConnectResult.NotRunning) return;
            if (sw.Elapsed >= limit)
                throw new TimeoutException($"Public pipe did not close within {limit.TotalSeconds:F1} s.");
            await Task.Delay(50).ConfigureAwait(false);
        }
    }

    public async Task<(int ExitCode, string Stdout, string Stderr)> RunCliAsync(params string[] args)
    {
        var psi = new ProcessStartInfo(_install.CliExe)
        {
            UseShellExecute = false,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            CreateNoWindow = true,
            WorkingDirectory = _install.InstallDir,
        };
        psi.Environment[DataPaths.HomeEnv] = Home;
        foreach (var a in args) psi.ArgumentList.Add(a);

        using var proc = Process.Start(psi) ?? throw new InvalidOperationException("Failed to start betterrecall.exe.");
        var stdoutTask = proc.StandardOutput.ReadToEndAsync();
        var stderrTask = proc.StandardError.ReadToEndAsync();
        using var cts = new CancellationTokenSource(15_000);
        await proc.WaitForExitAsync(cts.Token).ConfigureAwait(false);
        return (proc.ExitCode, await stdoutTask.ConfigureAwait(false), await stderrTask.ConfigureAwait(false));
    }

    public McpProcessClient StartMcp() => new(_install.McpExe, Home);

    public bool SignalQuitEvent(string? homeOverride = null)
    {
        try
        {
            using var ev = EventWaitHandle.OpenExisting(PipeNames.AgentQuitEventFor(homeOverride ?? Home));
            return ev.Set();
        }
        catch (WaitHandleCannotBeOpenedException) { return false; }
    }

    public async Task QuitAgentAsync(Process agent, string? homeOverride = null)
    {
        var home = homeOverride ?? Home;
        var children = ProcessTree.GetDescendantPids(agent.Id);
        if (!agent.HasExited)
        {
            SignalQuitEvent(home);
            using var cts = new CancellationTokenSource(10_000);
            try { await agent.WaitForExitAsync(cts.Token).ConfigureAwait(false); }
            catch (OperationCanceledException) { }
        }
        if (!agent.HasExited)
        {
            foreach (var childPid in children) ProcessTree.KillPid(childPid);
            try { agent.Kill(); } catch { }
            agent.WaitForExit(5000);
        }
        foreach (var childPid in children)
        {
            if (!ProcessTree.WaitForExitByPid(childPid, 3000))
                ProcessTree.KillPid(childPid);
        }
    }

    private async Task QuitAgentByPidAsync(int pid, string home)
    {
        if (ProcessTree.WaitForExitByPid(pid, 0)) return;
        var children = ProcessTree.GetDescendantPids(pid);
        SignalQuitEvent(home);
        if (!await Task.Run(() => ProcessTree.WaitForExitByPid(pid, 10_000)).ConfigureAwait(false))
        {
            foreach (var childPid in children) ProcessTree.KillPid(childPid);
            ProcessTree.KillPid(pid);
        }
        foreach (var childPid in children)
        {
            if (!ProcessTree.WaitForExitByPid(childPid, 3000))
                ProcessTree.KillPid(childPid);
        }
    }

    public async ValueTask DisposeAsync()
    {
        if (_ui is not null)
        {
            await _ui.DisposeAsync().ConfigureAwait(false);
            _ui = null;
        }
        for (int i = _agents.Count - 1; i >= 0; i--)
        {
            var (proc, pid, home) = _agents[i];
            try { await QuitAgentAsync(proc, home).ConfigureAwait(false); }
            catch (InvalidOperationException)
            {
                // The test disposed the Process object; fall back to the pid recorded at start.
                await QuitAgentByPidAsync(pid, home).ConfigureAwait(false);
            }
            finally { proc.Dispose(); }
        }
        _agents.Clear();
        foreach (var extra in _extraHomes) TestEnv.TryDeleteTree(extra);
        TestEnv.TryDeleteTree(Home);
    }
}
