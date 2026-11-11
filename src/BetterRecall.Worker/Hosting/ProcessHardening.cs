// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;

namespace BetterRecall.Worker.Hosting;

/// <summary>security.memoryIsolation and governance.ecoQos for the worker process (docs/v1-design.md sections 6, 7).
/// Every step is best effort: a failure is logged and reported, never fatal.</summary>
public static partial class ProcessHardening
{
    public sealed record Report(List<string> Applied, List<string> Failed);

    public static Report Apply(bool memoryIsolation, bool ecoQos)
    {
        var r = new Report([], []);
        if (memoryIsolation)
        {
            Step(r, "dll-search-app+system32", () => SetDefaultDllDirectories(LoadLibrarySearchApplicationDir | LoadLibrarySearchSystem32 | LoadLibrarySearchUserDirs));
            // No child processes.
            Step(r, "no-child-process", () => SetPolicy(ProcessChildProcessPolicy, 0x1));
            // Strict handle checks: raise on invalid handle use, permanently.
            Step(r, "strict-handle-checks", () => SetPolicy(ProcessStrictHandleCheckPolicy, 0x1 | 0x2));
            // Image loads: no remote (UNC) images, no low-integrity images. PreferSystem32Images stays off on purpose:
            // System32 has its own onnxruntime.dll and DirectML.dll, which would shadow the bundled newer ones.
            Step(r, "image-load-no-remote-no-low-il", () => SetPolicy(ProcessImageLoadPolicy, 0x1 | 0x2));
            Step(r, "process-dacl", ApplyProcessDacl);
            Step(r, "wer-excluded", () =>
            {
                int hr = WerAddExcludedApplication(Environment.ProcessPath ?? "BetterRecall.Worker.exe", false);
                return hr >= 0;
            });
        }
        if (ecoQos)
        {
            Step(r, "ecoqos", EnableEcoQos);
            Step(r, "background-mode", () => SetPriorityClass(GetCurrentProcess(), ProcessModeBackgroundBegin));
        }
        Log.Info($"hardening applied=[{string.Join(',', r.Applied)}] failed=[{string.Join(',', r.Failed)}]");
        return r;
    }

    private static void Step(Report r, string name, Func<bool> f)
    {
        bool ok;
        try { ok = f(); } catch { ok = false; }
        (ok ? r.Applied : r.Failed).Add(ok ? name : $"{name}:{Marshal.GetLastPInvokeError()}");
    }

    private static unsafe bool SetPolicy(int policy, uint flags)
    {
        uint v = flags;
        return SetProcessMitigationPolicy(policy, &v, sizeof(uint));
    }

    private static unsafe bool EnableEcoQos()
    {
        var state = new ProcessPowerThrottlingState
        {
            Version = 1,
            ControlMask = ProcessPowerThrottlingExecutionSpeed,
            StateMask = ProcessPowerThrottlingExecutionSpeed,
        };
        return SetProcessInformation(GetCurrentProcess(), ProcessPowerThrottling, &state, (uint)sizeof(ProcessPowerThrottlingState));
    }

    /// <summary>Process DACL: SYSTEM full; the user (and OWNER RIGHTS, so the owner gets no implicit WRITE_DAC) only
    /// query, synchronize, terminate. Denies PROCESS_VM_READ/WRITE, CREATE_THREAD, DUP_HANDLE to everyone but SYSTEM.</summary>
    private static bool ApplyProcessDacl()
    {
        var sid = System.Security.Principal.WindowsIdentity.GetCurrent().User!.Value;
        // 0x00121401 = SYNCHRONIZE | READ_CONTROL | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_QUERY_INFORMATION | PROCESS_TERMINATE
        string sddl = $"D:P(A;;GA;;;SY)(A;;0x00121401;;;{sid})(A;;0x00121401;;;OW)";
        if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, 1, out var sd, out _)) return false;
        try
        {
            if (!GetSecurityDescriptorDacl(sd, out _, out var dacl, out _)) return false;
            uint err = SetSecurityInfo(GetCurrentProcess(), SeKernelObject,
                DaclSecurityInformation | ProtectedDaclSecurityInformation, 0, 0, dacl, 0);
            return err == 0;
        }
        finally { LocalFree(sd); }
    }

    private const int ProcessStrictHandleCheckPolicy = 3;
    private const int ProcessImageLoadPolicy = 10;
    private const int ProcessChildProcessPolicy = 13;
    private const uint LoadLibrarySearchApplicationDir = 0x200;
    private const uint LoadLibrarySearchUserDirs = 0x400;
    private const uint LoadLibrarySearchSystem32 = 0x800;
    private const uint ProcessModeBackgroundBegin = 0x00100000;
    private const int ProcessPowerThrottling = 4;
    private const uint ProcessPowerThrottlingExecutionSpeed = 0x1;
    private const int SeKernelObject = 6;
    private const uint DaclSecurityInformation = 0x4;
    private const uint ProtectedDaclSecurityInformation = 0x80000000;

    [StructLayout(LayoutKind.Sequential)]
    private struct ProcessPowerThrottlingState { public uint Version, ControlMask, StateMask; }

    [LibraryImport("kernel32.dll")] private static partial nint GetCurrentProcess();

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static unsafe partial bool SetProcessMitigationPolicy(int policy, void* buffer, nuint length);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool SetDefaultDllDirectories(uint flags);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static unsafe partial bool SetProcessInformation(nint process, int infoClass, void* info, uint size);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool SetPriorityClass(nint process, uint priorityClass);

    [LibraryImport("wer.dll", StringMarshalling = StringMarshalling.Utf16)]
    private static partial int WerAddExcludedApplication(string exeName, [MarshalAs(UnmanagedType.Bool)] bool allUsers);

    [LibraryImport("advapi32.dll", SetLastError = true, StringMarshalling = StringMarshalling.Utf16)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool ConvertStringSecurityDescriptorToSecurityDescriptorW(string sddl, uint revision, out nint sd, out uint size);

    [LibraryImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetSecurityDescriptorDacl(nint sd, [MarshalAs(UnmanagedType.Bool)] out bool present, out nint dacl, [MarshalAs(UnmanagedType.Bool)] out bool defaulted);

    [LibraryImport("advapi32.dll")]
    private static partial uint SetSecurityInfo(nint handle, int objectType, uint securityInfo, nint owner, nint group, nint dacl, nint sacl);

    [LibraryImport("kernel32.dll")] private static partial nint LocalFree(nint mem);
}
