// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using System.Text;
using BetterRecall.Contracts;
using Microsoft.Win32.SafeHandles;

namespace BetterRecall.UI.Logic;

public sealed record TrustCheck(bool Trusted, int ServerPid);

/// <summary>Client side of research.md 7.4: the server process must be the installed agent in this session.</summary>
public interface IServerTrust
{
    TrustCheck Verify(SafePipeHandle pipe);
}

public sealed class ServerTrustVerifier : IServerTrust
{
    private readonly string _expectedImage;
    private readonly Func<SafePipeHandle, int?> _serverPid;
    private readonly Func<int, string?> _imageOf;
    private readonly Func<int, int?> _sessionOf;
    private readonly int _ownSession;

    public ServerTrustVerifier(
        string expectedImage,
        Func<SafePipeHandle, int?>? serverPid = null,
        Func<int, string?>? imageOf = null,
        Func<int, int?>? sessionOf = null,
        int? ownSession = null)
    {
        _expectedImage = Normalize(expectedImage);
        _serverPid = serverPid ?? Win32Trust.ServerPid;
        _imageOf = imageOf ?? Win32Trust.ImagePath;
        _sessionOf = sessionOf ?? Win32Trust.SessionOf;
        _ownSession = ownSession ?? Win32Trust.SessionOf(Environment.ProcessId) ?? -1;
    }

    /// <summary>The agent path for the installed layout: install dir is the parent of the UI exe's folder.</summary>
    public static string InstalledAgentPath(string uiBaseDirectory)
    {
        var uiDir = Path.GetFullPath(uiBaseDirectory).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
        var install = Path.GetDirectoryName(uiDir) ?? uiDir;
        return InstallLayout.Full(install, InstallLayout.AgentExe);
    }

    public TrustCheck Verify(SafePipeHandle pipe)
    {
        var pid = _serverPid(pipe);
        if (pid is null or <= 0) return new TrustCheck(false, 0);
        var image = _imageOf(pid.Value);
        if (image is null || !string.Equals(Normalize(image), _expectedImage, StringComparison.OrdinalIgnoreCase))
            return new TrustCheck(false, pid.Value);
        var session = _sessionOf(pid.Value);
        if (session is null || session.Value != _ownSession) return new TrustCheck(false, pid.Value);
        return new TrustCheck(true, pid.Value);
    }

    private static string Normalize(string path)
    {
        try { return Path.GetFullPath(path); } catch { return path; }
    }
}

internal static partial class Win32Trust
{
    private const uint ProcessQueryLimitedInformation = 0x1000;

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetNamedPipeServerProcessId(SafePipeHandle pipe, out uint pid);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    private static partial nint OpenProcess(uint access, [MarshalAs(UnmanagedType.Bool)] bool inherit, uint pid);

    [LibraryImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool CloseHandle(nint h);

    [LibraryImport("kernel32.dll", EntryPoint = "QueryFullProcessImageNameW", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static unsafe partial bool QueryFullProcessImageName(nint process, uint flags, char* buffer, ref uint size);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool ProcessIdToSessionId(uint pid, out uint session);

    public static int? ServerPid(SafePipeHandle pipe) => GetNamedPipeServerProcessId(pipe, out var pid) ? (int)pid : null;

    public static unsafe string? ImagePath(int pid)
    {
        var h = OpenProcess(ProcessQueryLimitedInformation, false, (uint)pid);
        if (h == 0) return null;
        try
        {
            var buf = new char[32768];
            uint size = (uint)buf.Length;
            fixed (char* p = buf)
                return QueryFullProcessImageName(h, 0, p, ref size) ? new string(buf, 0, (int)size) : null;
        }
        finally { CloseHandle(h); }
    }

    public static int? SessionOf(int pid) => ProcessIdToSessionId((uint)pid, out var s) ? (int)s : null;
}
