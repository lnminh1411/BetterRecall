// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker;

/// <summary>BetterRecall.Worker.exe --pipe &lt;name&gt; --home &lt;data dir&gt; [--settings-stdin]. With --settings-stdin the
/// agent writes its worker keys to stdin (sweep 4 C5): the agent's accepted settings, not settings.json, which a hand edit
/// can change behind the agent's back. Stdin, not the command line, which process-creation logs keep. Missing, too large
/// or unreadable input exits with 2; it never runs on defaults. Without the flag (manual runs, tests) the worker reads
/// settings.json. Exit codes: 0 normal, 2 bad arguments or settings,
/// 3 pipe connect failed, 4 server is not the agent, 5 protocol error, 1 unexpected.</summary>
public static class Program
{
    public static async Task<int> Main(string[] args)
    {
        var parsed = WorkerArgs.Parse(args);
        if (parsed is null) return 2;
        var home = parsed.Home;

        // Logging first, so a settings.json parse warning is written; then again at the configured level.
        Log.Init(DataPaths.LogsDir(home), "info");
        WorkerSettings settings;
        if (parsed.SettingsFromStdin)
        {
            var fromAgent = WorkerSettings.ReadAgentInput(Console.OpenStandardInput(), out var why);
            if (fromAgent is null) { Log.Error($"settings from the agent: {why}; exiting"); return 2; }
            if (WorkerSettings.TryFromJson(fromAgent, home, out why) is not { } agentSettings)
            {
                Log.Error($"settings from the agent unreadable ({why}); exiting");
                return 2;
            }
            settings = agentSettings;
        }
        else settings = WorkerSettings.Load(home);
        Log.Init(DataPaths.LogsDir(home), settings.LogLevel);
        Log.Info($"start settings={(parsed.SettingsFromStdin ? "agent" : "file")} modelDir={settings.ModelDir} fake={settings.Fake} device={settings.Device} gpuAdapter={settings.GpuAdapter} dim={settings.Dim} tokens={settings.ImageTokens} nsfwDevice={settings.NsfwDevice}");
        ProcessHardening.Apply(settings.MemoryIsolation, settings.EcoQos);

        System.IO.Pipes.NamedPipeClientStream pipe;
        try
        {
            pipe = await AgentPipe.ConnectAsync(parsed.Pipe, TimeSpan.FromSeconds(10), CancellationToken.None);
        }
        catch (Exception e)
        {
            Log.Error($"pipe connect failed: {e.GetType().Name}");
            return 3;
        }

        await using (pipe)
        {
            var expected = AgentPipe.ExpectedAgentImage(AppContext.BaseDirectory);
            bool trusted;
            try { trusted = AgentPipe.VerifyServer(pipe.SafePipeHandle, expected, out var pid, out var actual) ; if (!trusted) Log.Error($"pipe server pid {pid} is not the agent"); }
            catch (Exception e) { Log.Error($"server check failed: {e.GetType().Name}"); trusted = false; }
            if (!trusted) return 4;

            using var engine = EngineFactory.Create(settings, home);
            var current = settings;
            var handler = new RequestHandler(engine, () => current);
            var exit = await WorkerLoop.RunAsync(pipe, handler, TimeSpan.FromSeconds(settings.IdleExitS), CancellationToken.None,
                () => { try { return AgentPipe.HasPendingInput(pipe.SafePipeHandle); } catch { return false; } });
            Log.Info($"exit {exit}");
            return exit == LoopExit.ProtocolError ? 5 : 0;
        }
    }
}

public sealed record WorkerArgs(string Pipe, string Home, bool SettingsFromStdin = false)
{
    public static WorkerArgs? Parse(string[] args)
    {
        string? pipe = null, home = null;
        bool stdin = false;
        for (int i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "--pipe" when i + 1 < args.Length: pipe = args[++i]; break;
                case "--home" when i + 1 < args.Length: home = args[++i]; break;
                case "--settings-stdin": stdin = true; break;
                default: return null;
            }
        }
        var name = AgentPipe.NormalizeName(pipe);
        if (name is null || string.IsNullOrWhiteSpace(home)) return null;
        try { home = Path.GetFullPath(home); } catch { return null; }
        return new WorkerArgs(name, home, stdin);
    }
}
