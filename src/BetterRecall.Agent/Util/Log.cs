// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Collections.Concurrent;
using System.Text;

namespace BetterRecall.Agent.Util;

/// <summary>
/// Rolling log under logs\. Never write captured content, titles or query text here:
/// callers log event names, counts, timings and error codes only.
/// </summary>
internal static class Log
{
    public enum Level { Debug = 0, Info = 1, Warn = 2, Error = 3 }

    private static readonly BlockingCollection<string> Queue = new(new ConcurrentQueue<string>(), 4096);
    private static string? _dir;
    private static Thread? _thread;
    public static Level MinLevel { get; set; } = Level.Info;
    public static bool AlsoConsole { get; set; }

    public static void Init(string logsDir, string level)
    {
        _dir = logsDir;
        Directory.CreateDirectory(logsDir);
        SetLevel(level);
        _thread = new Thread(Pump) { IsBackground = true, Name = "log", Priority = ThreadPriority.BelowNormal };
        _thread.Start();
        Prune();
    }

    public static void SetLevel(string level) => MinLevel = level switch
    {
        "debug" => Level.Debug,
        "warn" => Level.Warn,
        "error" => Level.Error,
        _ => Level.Info,
    };

    public static void Debug(string msg) => Write(Level.Debug, msg);
    public static void Info(string msg) => Write(Level.Info, msg);
    public static void Warn(string msg) => Write(Level.Warn, msg);
    public static void Error(string msg) => Write(Level.Error, msg);
    public static void Error(string msg, Exception ex) => Write(Level.Error, $"{msg}: {ex.GetType().Name} 0x{ex.HResult:X8} {ex.Message}");
    public static void Warn(string msg, Exception ex) => Write(Level.Warn, $"{msg}: {ex.GetType().Name} 0x{ex.HResult:X8} {ex.Message}");

    private static void Write(Level level, string msg)
    {
        if (level < MinLevel) return;
        var line = $"{DateTimeOffset.Now:yyyy-MM-dd HH:mm:ss.fff} {level.ToString().ToUpperInvariant(),-5} [{Environment.CurrentManagedThreadId}] {msg}";
        if (AlsoConsole) Console.Error.WriteLine(line);
        if (_dir is null) return;
        Queue.TryAdd(line);
    }

    private static void Pump()
    {
        var sb = new StringBuilder();
        foreach (var line in Queue.GetConsumingEnumerable())
        {
            sb.Clear().AppendLine(line);
            while (Queue.TryTake(out var more)) sb.AppendLine(more);
            try
            {
                File.AppendAllText(Path.Combine(_dir!, $"agent-{DateTime.Now:yyyyMMdd}.log"), sb.ToString());
            }
            catch { /* logging never throws */ }
        }
    }

    public static void Flush()
    {
        for (int i = 0; i < 50 && Queue.Count > 0; i++) Thread.Sleep(10);
    }

    private static void Prune()
    {
        try
        {
            foreach (var f in Directory.EnumerateFiles(_dir!, "agent-*.log"))
                if (File.GetLastWriteTimeUtc(f) < DateTime.UtcNow.AddDays(-14)) File.Delete(f);
        }
        catch { }
    }
}
