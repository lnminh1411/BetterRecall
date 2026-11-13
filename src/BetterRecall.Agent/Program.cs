// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Text.Json;
using BetterRecall.Agent.Host;
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;

namespace BetterRecall.Agent;

internal static class Program
{
    [STAThread]
    private static int Main(string[] args)
    {
        if (args.Length > 0 && args[0] == "--selftest-replay") return DevSelfTest.Replay(args);
        if (args.Length > 0 && args[0] == "--selftest-keyframe") return DevSelfTest.Keyframe(args);
        if (args.Length > 0 && args[0] == "--selftest-bench") return DevSelfTest.Bench(args);
        if (args.Length > 0 && args[0] == "--selftest-codec") return DevSelfTest.Codec(args);
        if (args.Length > 0 && args[0] == "--selftest-keyframe-budget") return KeyframeBudgetRun.Run(args);

        string installDir = AppContext.BaseDirectory;
        bool autostart = true;
        int exitAfterS = 0;
        int devReplayAt = 0;
        string? devExport = null;
        string? devBench = null;
        for (int i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "--install-dir" when i + 1 < args.Length: installDir = args[++i]; break;
                case "--no-autostart": autostart = false; break;
                case "--unregister": StartupRegistration.Unregister(); return 0;
                case "--console": Log.AlsoConsole = true; break;
                case "--exit-after" when i + 1 < args.Length: exitAfterS = int.Parse(args[++i]); break;
                case "--dev-replay-at" when i + 1 < args.Length: devReplayAt = int.Parse(args[++i]); break;
                case "--dev-export" when i + 1 < args.Length: devExport = args[++i]; break;
                case "--dev-bench" when i + 1 < args.Length: devBench = args[++i]; break;
            }
        }
        installDir = Path.GetFullPath(installDir).TrimEnd('\\');

        var home = DataPaths.Home();
        using var mutex = new Mutex(true, PipeNames.AgentMutexFor(home), out bool first);
        if (!first) return 0; // single instance per data folder

        Directory.CreateDirectory(home);
        Log.Init(DataPaths.LogsDir(home), "info");
        Log.Info($"agent start, pid {Environment.ProcessId}");
        AppDomain.CurrentDomain.UnhandledException += (_, e) =>
        {
            if (e.ExceptionObject is Exception ex) Log.Error("unhandled", ex);
            Log.Flush();
        };

        ICoreHost core;
        try { core = BetterRecall.Core.CoreHost.Create(new CoreHostOptions(home, installDir)); }
        catch (Exception ex)
        {
            Log.Error("Core failed to start", ex);
            Log.Flush();
            return 1;
        }

        var host = new AgentHost(core, new AgentOptions(installDir, home, autostart));
        Timer? exitTimer = null;
        if (exitAfterS > 0)
            exitTimer = new Timer(_ => Diagnostics.DumpAndQuit(host), null, exitAfterS * 1000, Timeout.Infinite);
        Timer? devTimer = null;
        if (devReplayAt > 0)
            devTimer = new Timer(_ => DevSaveReplay(host, devExport), null, devReplayAt * 1000, Timeout.Infinite);
        else if (devBench is not null)
            devTimer = new Timer(_ => DevBench(host, devBench), null, 20_000, Timeout.Infinite);
        try
        {
            host.Run();
        }
        catch (Exception ex)
        {
            Log.Error("agent failed", ex);
        }
        finally
        {
            exitTimer?.Dispose();
            devTimer?.Dispose();
            var sw = Stopwatch.StartNew();
            try { host.DisposeAsync().AsTask().Wait(TimeSpan.FromSeconds(10)); } catch (Exception ex) { Log.Warn("shutdown", ex); }
            try { core.DisposeAsync().AsTask().Wait(TimeSpan.FromSeconds(10)); } catch (Exception ex) { Log.Warn("core shutdown", ex); }
            Log.Info($"agent stopped in {sw.ElapsedMilliseconds} ms");
            Log.Flush();
        }
        return 0;
    }

    /// <summary>Dev run: save a replay through the real path (IBlobCipher), then export it decrypted for checking.</summary>
    private static void DevSaveReplay(AgentHost host, string? exportPath)
    {
        try
        {
            var clip = host.SaveReplayAsync(CancellationToken.None).GetAwaiter().GetResult();
            Log.Info($"dev replay {clip.Id}: {clip.DurationMs} ms, {clip.Bytes} bytes, encrypted {clip.Encrypted}");
            if (exportPath is not null)
            {
                host.ExportReplayAsync(clip.Id, exportPath, CancellationToken.None).GetAwaiter().GetResult();
                var listed = host.ListReplays().FirstOrDefault(c => c.Id == clip.Id);
                Log.Info($"dev export done; listed duration {listed?.DurationMs} ms");
            }
        }
        catch (Exception ex) { Log.Error("dev replay", ex); }
    }

    private static void DevBench(AgentHost host, string path)
    {
        try
        {
            var report = host.BenchFullAsync(CancellationToken.None).GetAwaiter().GetResult();
            var json = JsonSerializer.Serialize(report, new ContractsJson(new JsonSerializerOptions(ContractsJson.Default.Options) { WriteIndented = true }).BenchReport);
            File.WriteAllText(path, json);
            Log.Info($"dev bench written to {path}: {report.Encoders.Length} encoder results in {report.TookMs} ms");
        }
        catch (Exception ex)
        {
            Log.Error("dev bench", ex);
        }
        finally
        {
            host.RequestQuit();
        }
    }
}
