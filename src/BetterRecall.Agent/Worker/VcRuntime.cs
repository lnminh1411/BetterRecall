// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Worker;

/// <summary>The worker's native libraries (OpenVINO, ONNX Runtime) need the Microsoft Visual C++ 2015-2022
/// Redistributable (x64); the Native AOT exes themselves link the CRT statically. Without it the worker failed hello
/// with DllNotFoundException, left an empty log and was restarted for ever with no message (T2). Checked before each
/// worker start, where the hardened worker loads DLLs from: its own folder (an app-local copy counts) and System32. Not
/// PATH, which the worker never searches (K review N2), and nothing is loaded into the agent, which holds the data key.</summary>
internal static class VcRuntime
{
    public const string MissingMessage =
        "The Microsoft Visual C++ 2015-2022 Redistributable (x64) is not installed: search, text recognition and transcription need it.";

    private static readonly string[] Dlls = ["vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll"];
    private static long _checkedAt = long.MinValue;
    private static bool _present = true;

    /// <summary>The worker exe's folder; the agent sets it once at start.</summary>
    public static string? WorkerDir { get; set; }

    /// <summary>Cached for a minute, so status polling stays cheap and an install made meanwhile is seen.</summary>
    public static bool Present
    {
        get
        {
            long now = Environment.TickCount64;
            if (now - Volatile.Read(ref _checkedAt) < 60_000) return _present;
            _present = Probe(WorkerDir, Environment.SystemDirectory);
            Volatile.Write(ref _checkedAt, now);
            return _present;
        }
    }

    /// <summary>Every DLL is a file in one of the folders. A file check, not a load.</summary>
    internal static bool Probe(string? workerDir, string systemDir, IEnumerable<string>? dlls = null)
    {
        foreach (var dll in dlls ?? Dlls)
        {
            bool found = (workerDir != null && File.Exists(Path.Combine(workerDir, dll))) || File.Exists(Path.Combine(systemDir, dll));
            if (!found) return false;
        }
        return true;
    }
}
