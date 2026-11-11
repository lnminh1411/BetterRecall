// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using System.Security.Principal;

namespace BetterRecall.UI.Native;

/// <summary>
/// security.memoryIsolation for the UI (docs/v1-design.md section 6): a process DACL that denies
/// PROCESS_VM_READ, PROCESS_VM_WRITE, PROCESS_CREATE_THREAD and PROCESS_DUP_HANDLE to everyone but SYSTEM,
/// and exclusion from Windows Error Reporting dumps. Same-user code can still rewrite the DACL; this raises the bar only.
/// </summary>
internal static partial class ProcessHardening
{
    private const uint DACL_SECURITY_INFORMATION = 0x4;
    private const uint SDDL_REVISION_1 = 1;
    private const uint DeniedMask = 0x0002 /*CREATE_THREAD*/ | 0x0010 /*VM_READ*/ | 0x0020 /*VM_WRITE*/ | 0x0040 /*DUP_HANDLE*/;

    [LibraryImport("kernel32.dll")]
    private static partial nint GetCurrentProcess();

    [LibraryImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool SetKernelObjectSecurity(nint handle, uint info, nint sd);

    [LibraryImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetKernelObjectSecurity(nint handle, uint info, nint sd, uint length, out uint needed);

    [LibraryImport("advapi32.dll", EntryPoint = "ConvertStringSecurityDescriptorToSecurityDescriptorW", SetLastError = true, StringMarshalling = StringMarshalling.Utf16)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool ConvertSddl(string sddl, uint revision, out nint sd, out uint size);

    [LibraryImport("kernel32.dll")]
    private static partial nint LocalFree(nint mem);

    [LibraryImport("wer.dll", StringMarshalling = StringMarshalling.Utf16)]
    private static partial int WerAddExcludedApplication(string exe, [MarshalAs(UnmanagedType.Bool)] bool allUsers);

    [LibraryImport("wer.dll", StringMarshalling = StringMarshalling.Utf16)]
    private static partial int WerRemoveExcludedApplication(string exe, [MarshalAs(UnmanagedType.Bool)] bool allUsers);

    private static byte[]? _original;
    private static bool? _applied;

    public static bool Apply(bool on)
    {
        if (_applied == on) return true;
        var process = GetCurrentProcess();
        bool ok;
        if (on)
        {
            _original ??= Capture(process);
            var sid = WindowsIdentity.GetCurrent().User!.Value;
            // SYSTEM first so its full grant is decided before the deny; then the deny; then the user and admins.
            var sddl = $"D:(A;;0x1FFFFF;;;SY)(D;;0x{DeniedMask:X};;;WD)(A;;0x1FFFFF;;;{sid})(A;;0x101000;;;BA)";
            ok = ConvertSddl(sddl, SDDL_REVISION_1, out var sd, out _);
            if (ok)
            {
                try { ok = SetKernelObjectSecurity(process, DACL_SECURITY_INFORMATION, sd); }
                finally { LocalFree(sd); }
            }
        }
        else
        {
            ok = _original is null || Restore(process, _original);
        }

        var exe = Environment.ProcessPath;
        if (exe is not null)
        {
            try { _ = on ? WerAddExcludedApplication(exe, false) : WerRemoveExcludedApplication(exe, false); }
            catch (Exception ex) when (ex is DllNotFoundException or EntryPointNotFoundException) { }
        }
        if (ok) _applied = on;
        return ok;
    }

    private static byte[]? Capture(nint process)
    {
        GetKernelObjectSecurity(process, DACL_SECURITY_INFORMATION, 0, 0, out var needed);
        if (needed == 0) return null;
        var buf = Marshal.AllocHGlobal((int)needed);
        try
        {
            if (!GetKernelObjectSecurity(process, DACL_SECURITY_INFORMATION, buf, needed, out _)) return null;
            var copy = new byte[needed];
            Marshal.Copy(buf, copy, 0, (int)needed);
            return copy;
        }
        finally { Marshal.FreeHGlobal(buf); }
    }

    private static bool Restore(nint process, byte[] sd)
    {
        var buf = Marshal.AllocHGlobal(sd.Length);
        try
        {
            Marshal.Copy(sd, 0, buf, sd.Length);
            return SetKernelObjectSecurity(process, DACL_SECURITY_INFORMATION, buf);
        }
        finally { Marshal.FreeHGlobal(buf); }
    }
}
