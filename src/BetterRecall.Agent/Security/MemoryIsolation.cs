// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Security;

/// <summary>
/// security.memoryIsolation (v1-design section 6). Applied in two stages so our own later loads keep working:
/// Early (before devices and encoders load): process DACL, WER exclusion, strict handle checks, image load policy.
/// Late (after GPU devices, Media Foundation, audio and UI Automation are initialised): dynamic code prohibited.
/// Each step logs its result; failures never stop the agent.
/// </summary>
internal static unsafe class MemoryIsolation
{
    public static readonly List<string> Applied = new();
    public static readonly List<string> Skipped = new();

    public static void ApplyEarly(string installDir)
    {
        Step("process-dacl", SetProcessDacl);
        Step("wer-exclusion", () =>
        {
            foreach (var exe in new[] { InstallLayout.AgentExe, InstallLayout.CliExe, InstallLayout.McpExe, "BetterRecall.Worker.exe", "BetterRecall.UI.exe" })
                if (Win32.WerAddExcludedApplication(Path.GetFileName(exe), false) < 0) return false;
            return true;
        });
        Step("strict-handles", () =>
        {
            uint flags = 0x1 | 0x2; // RaiseExceptionOnInvalidHandleReference | HandleExceptionsPermanentlyEnabled
            return Win32.SetProcessMitigationPolicy(Win32.ProcessStrictHandleCheckPolicy, &flags, 4);
        });
        Step("image-load", () =>
        {
            // NoRemoteImages | NoLowMandatoryLabelImages | PreferSystem32Images.
            // There is no "install dir only" policy; signature-only policies would block SkiaSharp and GPU drivers.
            uint flags = 0x1 | 0x2 | 0x4;
            return Win32.SetProcessMitigationPolicy(Win32.ProcessImageLoadPolicy, &flags, 4);
        });
    }

    /// <summary>Arbitrary Code Guard: no new executable memory. Native AOT needs none; done last because drivers load first.</summary>
    public static void ApplyLate(bool dynamicCode)
    {
        if (!dynamicCode) { Skipped.Add("dynamic-code (disabled by caller)"); return; }
        Step("dynamic-code", () =>
        {
            uint flags = 0x1; // ProhibitDynamicCode
            return Win32.SetProcessMitigationPolicy(Win32.ProcessDynamicCodePolicy, &flags, 4);
        });
    }

    /// <summary>
    /// Protected DACL: SYSTEM full access; the user keeps terminate/query/synchronize/read-control only, so
    /// PROCESS_VM_READ, VM_WRITE, CREATE_THREAD and DUP_HANDLE are refused to other processes of the same user.
    /// OWNER RIGHTS limits the owner's implicit WRITE_DAC so the DACL cannot simply be rewritten.
    /// </summary>
    private static bool SetProcessDacl()
    {
        var sid = PipeNames.UserSid();
        // 0x121401 = TERMINATE | QUERY_INFORMATION | QUERY_LIMITED_INFORMATION | READ_CONTROL | SYNCHRONIZE
        string sddl = $"D:P(A;;GA;;;SY)(A;;0x121401;;;{sid})(A;;RC;;;OW)";
        nint sd;
        if (!Win32.ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, 1, &sd, null)) return false;
        try { return Win32.SetKernelObjectSecurity(Win32.GetCurrentProcess(), Win32.DACL_SECURITY_INFORMATION, sd); }
        finally { Win32.LocalFree(sd); }
    }

    /// <summary>Raises the working-set minimum so VirtualLock can pin the replay rings (soft limits).</summary>
    public static bool ReserveWorkingSet(long lockedBytes)
    {
        nint self = Win32.GetCurrentProcess();
        nuint min, max;
        uint flags;
        if (!Win32.GetProcessWorkingSetSizeEx(self, &min, &max, &flags)) return false;
        nuint wantMin = (nuint)Math.Max((long)min, lockedBytes + 96L * 1024 * 1024);
        nuint wantMax = (nuint)Math.Max((long)max, (long)wantMin + 512L * 1024 * 1024);
        const uint MinDisable = 0x2, MaxDisable = 0x8;
        bool ok = Win32.SetProcessWorkingSetSizeEx(self, wantMin, wantMax, MinDisable | MaxDisable);
        if (!ok) Log.Warn($"SetProcessWorkingSetSizeEx failed ({Marshal.GetLastPInvokeError()})");
        return ok;
    }

    private static void Step(string name, Func<bool> action)
    {
        try
        {
            if (action())
            {
                Applied.Add(name);
                Log.Info($"isolation {name} applied");
            }
            else
            {
                int err = Marshal.GetLastPInvokeError();
                Skipped.Add($"{name} (error {err})");
                Log.Warn($"isolation {name} failed ({err})");
            }
        }
        catch (Exception ex)
        {
            Skipped.Add($"{name} ({ex.GetType().Name})");
            Log.Warn($"isolation {name}", ex);
        }
    }
}
