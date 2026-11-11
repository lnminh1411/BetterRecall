// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace BetterRecall.Core.Native;

internal static partial class Win32
{
    // ---- memory ----
    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool VirtualLock(nint address, nuint size);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool VirtualUnlock(nint address, nuint size);

    internal const uint CRYPTPROTECTMEMORY_BLOCK_SIZE = 16;
    internal const uint CRYPTPROTECTMEMORY_SAME_PROCESS = 0;

    [LibraryImport("crypt32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool CryptProtectMemory(nint data, uint size, uint flags);

    [LibraryImport("crypt32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool CryptUnprotectMemory(nint data, uint size, uint flags);

    // ---- DPAPI ----
    [StructLayout(LayoutKind.Sequential)]
    internal struct DATA_BLOB
    {
        public int cbData;
        public nint pbData;
    }

    internal const int CRYPTPROTECT_UI_FORBIDDEN = 0x1;

    [LibraryImport("crypt32.dll", SetLastError = true, StringMarshalling = StringMarshalling.Utf16)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool CryptProtectData(ref DATA_BLOB dataIn, string? description, nint entropy, nint reserved, nint prompt, int flags, out DATA_BLOB dataOut);

    [LibraryImport("crypt32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool CryptUnprotectData(ref DATA_BLOB dataIn, nint description, nint entropy, nint reserved, nint prompt, int flags, out DATA_BLOB dataOut);

    [LibraryImport("kernel32.dll")]
    internal static partial nint LocalFree(nint mem);

    // ---- pipes ----
    internal const uint PIPE_ACCESS_DUPLEX = 0x00000003;
    internal const uint FILE_FLAG_OVERLAPPED = 0x40000000;
    internal const uint FILE_FLAG_FIRST_PIPE_INSTANCE = 0x00080000;
    internal const uint PIPE_TYPE_BYTE = 0x0;
    internal const uint PIPE_READMODE_BYTE = 0x0;
    internal const uint PIPE_WAIT = 0x0;
    internal const uint PIPE_REJECT_REMOTE_CLIENTS = 0x00000008;

    [StructLayout(LayoutKind.Sequential)]
    internal struct SECURITY_ATTRIBUTES
    {
        public int nLength;
        public nint lpSecurityDescriptor;
        public int bInheritHandle;
    }

    [LibraryImport("kernel32.dll", SetLastError = true, StringMarshalling = StringMarshalling.Utf16, EntryPoint = "CreateNamedPipeW")]
    internal static partial SafePipeHandle CreateNamedPipe(string name, uint openMode, uint pipeMode, uint maxInstances, uint outBuffer, uint inBuffer, uint defaultTimeout, ref SECURITY_ATTRIBUTES sa);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool GetNamedPipeClientProcessId(SafePipeHandle pipe, out uint pid);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool GetNamedPipeClientSessionId(SafePipeHandle pipe, out uint session);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool GetNamedPipeServerProcessId(SafePipeHandle pipe, out uint pid);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool GetNamedPipeServerSessionId(SafePipeHandle pipe, out uint session);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool ProcessIdToSessionId(uint pid, out uint session);

    [LibraryImport("advapi32.dll", SetLastError = true, StringMarshalling = StringMarshalling.Utf16, EntryPoint = "ConvertStringSecurityDescriptorToSecurityDescriptorW")]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool ConvertStringSecurityDescriptorToSecurityDescriptor(string sddl, uint revision, out nint sd, out uint size);

    [LibraryImport("advapi32.dll", SetLastError = true, EntryPoint = "ConvertSecurityDescriptorToStringSecurityDescriptorW")]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool ConvertSecurityDescriptorToStringSecurityDescriptor(nint sd, uint revision, uint info, out nint sddl, out uint len);

    internal const uint SE_KERNEL_OBJECT = 6;
    internal const uint OWNER_SECURITY_INFORMATION = 0x1;
    internal const uint DACL_SECURITY_INFORMATION = 0x4;
    internal const uint LABEL_SECURITY_INFORMATION = 0x10;

    [LibraryImport("advapi32.dll")]
    internal static partial uint GetSecurityInfo(SafeHandle handle, uint objectType, uint info, nint owner, nint group, nint dacl, nint sacl, out nint sd);

    // ---- process and token ----
    internal const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;
    internal const uint TOKEN_QUERY = 0x0008;

    [LibraryImport("kernel32.dll", SetLastError = true)]
    internal static partial SafeProcessHandle OpenProcess(uint access, [MarshalAs(UnmanagedType.Bool)] bool inherit, uint pid);

    [LibraryImport("kernel32.dll", SetLastError = true, StringMarshalling = StringMarshalling.Utf16, EntryPoint = "QueryFullProcessImageNameW")]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool QueryFullProcessImageName(SafeProcessHandle process, uint flags, [Out] char[] buffer, ref uint size);

    [LibraryImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool OpenProcessToken(SafeProcessHandle process, uint access, out SafeAccessTokenHandle token);

    internal const int TokenUser = 1;
    internal const int TokenIntegrityLevel = 25;
    internal const int TokenIsAppContainer = 29;

    [LibraryImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    internal static partial bool GetTokenInformation(SafeAccessTokenHandle token, int infoClass, nint info, int length, out int returned);

    [LibraryImport("advapi32.dll")]
    internal static partial nint GetSidSubAuthorityCount(nint sid);

    [LibraryImport("advapi32.dll")]
    internal static partial nint GetSidSubAuthority(nint sid, uint index);

    [LibraryImport("kernel32.dll")]
    internal static partial uint GetCurrentProcessId();

    internal const uint SECURITY_MANDATORY_MEDIUM_RID = 0x2000;

    // ---- WinRT activation ----
    [LibraryImport("combase.dll", StringMarshalling = StringMarshalling.Utf16)]
    internal static partial int WindowsCreateString(string source, int length, out nint hstring);

    [LibraryImport("combase.dll")]
    internal static partial int WindowsDeleteString(nint hstring);

    [LibraryImport("combase.dll")]
    internal static partial int RoGetActivationFactory(nint classId, in Guid iid, out nint factory);

    [LibraryImport("combase.dll")]
    internal static partial int RoInitialize(int initType);
}
