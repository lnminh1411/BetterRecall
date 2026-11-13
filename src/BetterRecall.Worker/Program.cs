// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using BetterRecall.Contracts;
using BetterRecall.Worker.Engine;
using BetterRecall.Worker.Hosting;
using BetterRecall.Worker.Protocol;

namespace BetterRecall.Worker;

/// <summary>BetterRecall.Worker.exe --pipe &lt;name&gt; --home &lt;data dir&gt;. Exit codes: 0 normal, 2 bad arguments,
/// 3 pipe connect failed, 4 server is not the agent, 5 protocol error, 1 unexpected.</summary>
public static class Program
{
    public static async Task<int> Main(string[] args)
    {
        var parsed = WorkerArgs.Parse(args);
        if (parsed is null) return 2;
        var home = parsed.Home;

        var settings = WorkerSettings.Load(home);
        Log.Init(DataPaths.LogsDir(home), settings.LogLevel);
        Log.Info($"start fake={settings.Fake} device={settings.Device} gpuAdapter={settings.GpuAdapter} dim={settings.Dim} tokens={settings.ImageTokens} nsfwDevice={settings.NsfwDevice}");
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

public sealed record WorkerArgs(string Pipe, string Home)
{
    public static WorkerArgs? Parse(string[] args)
    {
        string? pipe = null, home = null;
        for (int i = 0; i + 1 < args.Length; i += 2)
        {
            switch (args[i])
            {
                case "--pipe": pipe = args[i + 1]; break;
                case "--home": home = args[i + 1]; break;
                default: return null;
            }
        }
        if (args.Length % 2 != 0) return null;
        var name = AgentPipe.NormalizeName(pipe);
        if (name is null || string.IsNullOrWhiteSpace(home)) return null;
        try { home = Path.GetFullPath(home); } catch { return null; }
        return new WorkerArgs(name, home);
    }
}
