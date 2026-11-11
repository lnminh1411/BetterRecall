// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Security.Principal;
using BetterRecall.Core.Native;
using Microsoft.Win32.SafeHandles;

namespace BetterRecall.Core.Ipc;

/// <summary>Facts about a peer process read from its token and image name.</summary>
public sealed record PeerInfo(int Pid, uint Session, string? Sid, int IntegrityRid, bool AppContainer, string? Image);

internal static class ProcessInfo
{
    private static readonly Lazy<uint> OwnSession = new(() =>
        Win32.ProcessIdToSessionId(Win32.GetCurrentProcessId(), out var s) ? s : uint.MaxValue);

    private static readonly Lazy<string> OwnSid = new(() => WindowsIdentity.GetCurrent().User!.Value);

    public static uint CurrentSession => OwnSession.Value;
    public static string CurrentSid => OwnSid.Value;

    public static string? ImagePath(SafeProcessHandle h)
    {
        var buf = new char[32768];
        uint size = (uint)buf.Length;
        return Win32.QueryFullProcessImageName(h, 0, buf, ref size) ? new string(buf, 0, (int)size) : null;
    }

    public static string? ImagePath(int pid)
    {
        using var h = Win32.OpenProcess(Win32.PROCESS_QUERY_LIMITED_INFORMATION, false, (uint)pid);
        return h.IsInvalid ? null : ImagePath(h);
    }

    public static uint SessionOf(int pid) => Win32.ProcessIdToSessionId((uint)pid, out var s) ? s : uint.MaxValue;

    /// <summary>Reads session, user SID, integrity RID, AppContainer flag and image path. Null fields on failure.</summary>
    public static unsafe PeerInfo Read(int pid, uint session)
    {
        string? sid = null, image = null;
        int rid = -1;
        bool appContainer = true;
        using var h = Win32.OpenProcess(Win32.PROCESS_QUERY_LIMITED_INFORMATION, false, (uint)pid);
        if (h.IsInvalid) return new PeerInfo(pid, session, null, -1, true, null);
        image = ImagePath(h);
        if (Win32.OpenProcessToken(h, Win32.TOKEN_QUERY, out var token))
        {
            using (token)
            {
                var buf = Marshal.AllocHGlobal(1024);
                try
                {
                    if (Win32.GetTokenInformation(token, Win32.TokenUser, buf, 1024, out _))
                        sid = new SecurityIdentifier(Marshal.ReadIntPtr(buf)).Value;
                    if (Win32.GetTokenInformation(token, Win32.TokenIntegrityLevel, buf, 1024, out _))
                    {
                        nint psid = Marshal.ReadIntPtr(buf);
                        byte count = *(byte*)Win32.GetSidSubAuthorityCount(psid);
                        if (count > 0) rid = *(int*)Win32.GetSidSubAuthority(psid, (uint)(count - 1));
                    }
                    if (Win32.GetTokenInformation(token, Win32.TokenIsAppContainer, buf, 4, out _))
                        appContainer = Marshal.ReadInt32(buf) != 0;
                }
                finally { Marshal.FreeHGlobal(buf); }
            }
        }
        return new PeerInfo(pid, session, sid, rid, appContainer, image);
    }

    public static string Normalize(string path)
    {
        try { return Path.GetFullPath(path).TrimEnd('\\'); }
        catch { return path; }
    }

    public static bool SamePath(string? a, string? b)
        => a != null && b != null && string.Equals(Normalize(a), Normalize(b), StringComparison.OrdinalIgnoreCase);

    public static int CurrentPid => Environment.ProcessId;

    public static string? CurrentImage => Environment.ProcessPath ?? Process.GetCurrentProcess().MainModule?.FileName;
}
