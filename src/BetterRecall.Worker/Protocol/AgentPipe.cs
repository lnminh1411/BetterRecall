// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.ComponentModel;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Text;
using System.Text.RegularExpressions;
using BetterRecall.Contracts;
using Microsoft.Win32.SafeHandles;

namespace BetterRecall.Worker.Protocol;

/// <summary>Client side of the private worker pipe. Connects anonymously and checks that the server process is
/// &lt;install&gt;\BetterRecall.Agent.exe, where &lt;install&gt; is the parent of the worker's own directory.
/// Comparison uses final (link-resolved) paths.</summary>
public static partial class AgentPipe
{
    [GeneratedRegex("^BetterRecall\\.worker\\.[0-9a-f]{32}$")]
    private static partial Regex WorkerPipeName();

    /// <summary>Accepts "BetterRecall.worker.&lt;32 hex&gt;" with or without the \\.\pipe\ prefix. Null when invalid.</summary>
    public static string? NormalizeName(string? configured)
    {
        if (string.IsNullOrWhiteSpace(configured)) return null;
        var b = PipeNames.Base(configured.Trim());
        return WorkerPipeName().IsMatch(b) ? b : null;
    }

    /// <summary>Install dir = parent of the directory holding the worker exe.</summary>
    public static string InstallDir(string workerBaseDir)
        => Path.GetFullPath(Path.Combine(workerBaseDir.TrimEnd('\\', '/'), ".."));

    public static string ExpectedAgentImage(string workerBaseDir)
        => InstallLayout.Full(InstallDir(workerBaseDir), InstallLayout.AgentExe);

    public static async Task<NamedPipeClientStream> ConnectAsync(string name, TimeSpan timeout, CancellationToken ct)
    {
        var pipe = new NamedPipeClientStream(".", name, PipeDirection.InOut, PipeOptions.Asynchronous, TokenImpersonationLevel.Anonymous);
        try
        {
            await pipe.ConnectAsync(timeout, ct).ConfigureAwait(false);
            return pipe;
        }
        catch
        {
            await pipe.DisposeAsync().ConfigureAwait(false);
            throw;
        }
    }

    public static int ServerProcessId(SafePipeHandle handle)
    {
        if (!GetNamedPipeServerProcessId(handle, out uint pid)) throw new Win32Exception(Marshal.GetLastWin32Error());
        return checked((int)pid);
    }

    /// <summary>Full image path of a process (QueryFullProcessImageNameW), or null.</summary>
    public static string? ImagePath(int pid)
    {
        using var h = OpenProcess(ProcessQueryLimitedInformation, false, (uint)pid);
        if (h.IsInvalid) return null;
        var sb = new StringBuilder(32768);
        int size = sb.Capacity;
        return QueryFullProcessImageNameW(h, 0, sb, ref size) ? sb.ToString(0, size) : null;
    }

    internal static string FinalPath(string path)
    {
        using var handle = CreateFileW(
            path,
            FileReadAttributes,
            FileShareRead | FileShareWrite | FileShareDelete,
            IntPtr.Zero,
            OpenExisting,
            FileFlagBackupSemantics,
            IntPtr.Zero);
        if (handle.IsInvalid) return Path.GetFullPath(path);

        var sb = new StringBuilder(1024);
        uint ret = GetFinalPathNameByHandleW(handle, sb, (uint)sb.Capacity, 0);
        if (ret > sb.Capacity)
        {
            sb.EnsureCapacity((int)ret);
            ret = GetFinalPathNameByHandleW(handle, sb, (uint)sb.Capacity, 0);
        }
        if (ret == 0) return Path.GetFullPath(path);

        var result = sb.ToString(0, (int)ret);
        if (result.StartsWith(@"\\?\UNC\", StringComparison.OrdinalIgnoreCase))
            return @"\\" + result[8..];
        if (result.StartsWith(@"\\?\", StringComparison.OrdinalIgnoreCase))
            return result[4..];
        return result;
    }

    /// <summary>True when the pipe's server process image equals <paramref name="expectedImage"/> (case-insensitive, full path).</summary>
    public static bool VerifyServer(SafePipeHandle handle, string expectedImage, out int pid, out string? actual)
    {
        pid = ServerProcessId(handle);
        actual = ImagePath(pid);
        if (actual is null) return false;
        return string.Equals(FinalPath(actual), FinalPath(expectedImage), StringComparison.OrdinalIgnoreCase);
    }

    private const uint ProcessQueryLimitedInformation = 0x1000;
    private const uint FileReadAttributes = 0x80;
    private const uint FileShareRead = 1;
    private const uint FileShareWrite = 2;
    private const uint FileShareDelete = 4;
    private const uint OpenExisting = 3;
    private const uint FileFlagBackupSemantics = 0x02000000;

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static partial bool GetNamedPipeServerProcessId(SafePipeHandle pipe, out uint serverProcessId);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    private static partial SafeProcessHandle OpenProcess(uint access, [MarshalAs(UnmanagedType.Bool)] bool inherit, uint pid);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool QueryFullProcessImageNameW(SafeProcessHandle process, int flags, StringBuilder exeName, ref int size);

    [LibraryImport("kernel32.dll", EntryPoint = "CreateFileW", SetLastError = true, StringMarshalling = StringMarshalling.Utf16)]
    private static partial SafeFileHandle CreateFileW(
        string lpFileName,
        uint dwDesiredAccess,
        uint dwShareMode,
        IntPtr lpSecurityAttributes,
        uint dwCreationDisposition,
        uint dwFlagsAndAttributes,
        IntPtr hTemplateFile);

    [DllImport("kernel32.dll", EntryPoint = "GetFinalPathNameByHandleW", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern uint GetFinalPathNameByHandleW(SafeFileHandle hFile, StringBuilder lpszFilePath, uint cchFilePath, uint dwFlags);
}
