// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;

namespace BetterRecall.Agent.Worker;

/// <summary>The worker's native libraries (OpenVINO, ONNX Runtime) need the Microsoft Visual C++ 2015-2022
/// Redistributable (x64); the Native AOT exes themselves link the CRT statically. Without it the worker failed hello
/// with DllNotFoundException, left an empty log and was restarted for ever with no message (T2). Checked before each
/// worker start, from the normal DLL search path (the install folder first, so an app-local copy counts).</summary>
internal static class VcRuntime
{
    public const string MissingMessage =
        "The Microsoft Visual C++ 2015-2022 Redistributable (x64) is not installed: search, text recognition and transcription need it.";

    private static readonly string[] Dlls = ["vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll"];
    private static long _checkedAt = long.MinValue;
    private static bool _present = true;

    /// <summary>Cached for a minute, so status polling stays cheap and an install made meanwhile is seen.</summary>
    public static bool Present
    {
        get
        {
            long now = Environment.TickCount64;
            if (now - Volatile.Read(ref _checkedAt) < 60_000) return _present;
            _present = Probe();
            Volatile.Write(ref _checkedAt, now);
            return _present;
        }
    }

    internal static bool Probe(IEnumerable<string>? dlls = null)
    {
        foreach (var dll in dlls ?? Dlls)
        {
            if (!NativeLibrary.TryLoad(dll, out var h)) return false;
            NativeLibrary.Free(h);
        }
        return true;
    }
}
