// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Conformance.Harness;

internal static class TestEnv
{
    public static string Root
    {
        get
        {
            var env = Environment.GetEnvironmentVariable("BR_TEST_TMP");
            var baseDir = !string.IsNullOrWhiteSpace(env) ? env
                : Directory.Exists(@"K:\scratch") ? @"K:\scratch"
                : Path.GetTempPath();
            return Path.Combine(baseDir, "br-conf");
        }
    }

    private static int _initialized;

    public static string NewDir(string tag)
    {
        if (Interlocked.Exchange(ref _initialized, 1) == 0) SweepStale(TimeSpan.FromMinutes(30));
        var d = Path.Combine(Root, $"{tag}-{Guid.NewGuid():N}"[..(tag.Length + 13)]);
        Directory.CreateDirectory(d);
        return d;
    }

    internal static bool IsTestDirName(string name)
    {
        int dash = name.LastIndexOf('-');
        return dash > 0 && name.Length - dash - 1 == 12 && name[(dash + 1)..].All(char.IsAsciiHexDigitLower);
    }

    public static void TryDeleteTree(string dir)
    {
        for (int attempt = 0; attempt < 20; attempt++)
        {
            try
            {
                if (!Directory.Exists(dir)) return;
                Directory.Delete(dir, recursive: true);
                return;
            }
            catch (IOException) { Thread.Sleep(100); }
            catch (UnauthorizedAccessException) { Thread.Sleep(100); }
        }
    }

    private static void SweepStale(TimeSpan age)
    {
        try
        {
            if (!Directory.Exists(Root)) return;
            var cutoff = DateTime.UtcNow - age;
            foreach (var d in Directory.EnumerateDirectories(Root))
            {
                if (!IsTestDirName(Path.GetFileName(d)) || Directory.GetLastWriteTimeUtc(d) > cutoff) continue;
                TryDeleteTree(d);
            }
        }
        catch (IOException) { }
    }
}
