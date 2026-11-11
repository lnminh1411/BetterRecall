// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using BetterRecall.Core.Native;

namespace BetterRecall.Core.Crypto;

/// <summary>
/// Windows Hello through UserConsentVerifier with raw WinRT COM calls (no CsWinRT projection, AOT-safe).
/// Unpackaged desktop apps must use IUserConsentVerifierInterop::RequestVerificationForWindowAsync with an HWND.
/// </summary>
internal static unsafe class HelloInterop
{
    private const string ClassName = "Windows.Security.Credentials.UI.UserConsentVerifier";
    private static readonly Guid IidInterop = new("39E050C3-4E74-441A-8DC0-B81104DF949C");
    // IIDs from windows.security.credentials.ui.h and userconsentverifierinterop.h, SDK 10.0.26100.0.
    private static readonly Guid IidStatics = new("AF4F3F91-564C-4DDC-B8B5-973447627C65");
    private static readonly Guid IidAsyncInfo = new("00000036-0000-0000-C000-000000000046");
    private const string IAsyncOperationPiid = "{9fc2b0bb-e446-44e2-aa61-9cab8f636af2}";

    /// <summary>IID of IAsyncOperation&lt;UserConsentVerificationResult&gt; (parameterized interface IID, WinRT spec).</summary>
    internal static readonly Guid IidAsyncOpResult =
        ParameterizedIid($"pinterface({IAsyncOperationPiid};enum(Windows.Security.Credentials.UI.UserConsentVerificationResult;i4))");

    /// <summary>WinRT parameterized IID: SHA-1 over the namespace GUID and the type signature, version 5 UUID.</summary>
    internal static Guid ParameterizedIid(string signature)
    {
        Span<byte> ns = stackalloc byte[16];
        new Guid("11f47ad5-7b73-42c0-abae-878b1e16adee").TryWriteBytes(ns, bigEndian: true, out _);
        var sig = Encoding.UTF8.GetBytes(signature);
        var data = new byte[16 + sig.Length];
        ns.CopyTo(data);
        sig.CopyTo(data, 16);
        var hash = SHA1.HashData(data);
        hash[6] = (byte)((hash[6] & 0x0F) | 0x50);
        hash[8] = (byte)((hash[8] & 0x3F) | 0x80);
        return new Guid(hash.AsSpan(0, 16), bigEndian: true);
    }

    /// <summary>True when UserConsentVerifier reports Available. False on any failure.</summary>
    public static Task<bool> CheckAvailableAsync(CancellationToken ct) => RunMta(() =>
    {
        nint factory = GetFactory(IidStatics);
        if (factory == 0) return false;
        try
        {
            nint op;
            int hr = ((delegate* unmanaged[Stdcall]<nint, nint*, int>)VTable(factory, 6))(factory, &op);
            if (hr < 0 || op == 0) return false;
            try { return WaitResult(op, ct) == 0; }
            finally { Release(op); }
        }
        finally { Release(factory); }
    });

    /// <summary>Shows the Hello prompt owned by hwnd. True only for Verified. False when unavailable, cancelled or on error.</summary>
    public static Task<bool> VerifyAsync(nint hwnd, string message, CancellationToken ct) => RunMta(() =>
    {
        nint hs = 0;
        if (Win32.WindowsCreateString(message, message.Length, out hs) < 0) return false;
        try
        {
            nint op;
            if (hwnd != 0)
            {
                nint interop = GetFactory(IidInterop);
                if (interop == 0) return false;
                try
                {
                    Guid riid = IidAsyncOpResult;
                    int hr = ((delegate* unmanaged[Stdcall]<nint, nint, nint, Guid*, nint*, int>)VTable(interop, 6))(interop, hwnd, hs, &riid, &op);
                    if (hr < 0 || op == 0) { Log.Warn($"Hello RequestVerificationForWindowAsync failed 0x{hr:X8}"); return false; }
                }
                finally { Release(interop); }
            }
            else
            {
                nint statics = GetFactory(IidStatics);
                if (statics == 0) return false;
                try
                {
                    int hr = ((delegate* unmanaged[Stdcall]<nint, nint, nint*, int>)VTable(statics, 7))(statics, hs, &op);
                    if (hr < 0 || op == 0) { Log.Warn($"Hello RequestVerificationAsync failed 0x{hr:X8}"); return false; }
                }
                finally { Release(statics); }
            }
            try { return WaitResult(op, ct) == 0; }
            finally { Release(op); }
        }
        finally { Win32.WindowsDeleteString(hs); }
    });

    private static Task<bool> RunMta(Func<bool> body)
    {
        var tcs = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        var t = new Thread(() =>
        {
            try
            {
                Win32.RoInitialize(1); // RO_INIT_MULTITHREADED; S_FALSE when already initialised
                tcs.TrySetResult(body());
            }
            catch (Exception ex)
            {
                Log.Warn("Hello call failed", ex);
                tcs.TrySetResult(false);
            }
        }) { IsBackground = true, Name = "BetterRecall.Hello" };
        t.SetApartmentState(ApartmentState.MTA);
        t.Start();
        return tcs.Task;
    }

    private static nint GetFactory(Guid iid)
    {
        if (Win32.WindowsCreateString(ClassName, ClassName.Length, out var hs) < 0) return 0;
        try
        {
            int hr = Win32.RoGetActivationFactory(hs, iid, out var f);
            if (hr < 0) { Log.Debug($"RoGetActivationFactory 0x{hr:X8}"); return 0; }
            return f;
        }
        finally { Win32.WindowsDeleteString(hs); }
    }

    /// <summary>Polls IAsyncInfo until done, then reads an enum (i4) result through IAsyncOperation.GetResults. -1 on failure.</summary>
    private static int WaitResult(nint op, CancellationToken ct)
    {
        nint info;
        Guid iid = IidAsyncInfo;
        int hr = Marshal.QueryInterface(op, in iid, out info);
        if (hr < 0) return -1;
        try
        {
            while (true)
            {
                int status;
                hr = ((delegate* unmanaged[Stdcall]<nint, int*, int>)VTable(info, 7))(info, &status);
                if (hr < 0) return -1;
                if (status == 1) break;              // Completed
                if (status is 2 or 3) return -1;     // Canceled, Error
                if (ct.IsCancellationRequested)
                {
                    ((delegate* unmanaged[Stdcall]<nint, int>)VTable(info, 9))(info);
                    return -1;
                }
                Thread.Sleep(50);
            }
            int result;
            hr = ((delegate* unmanaged[Stdcall]<nint, int*, int>)VTable(op, 8))(op, &result);
            return hr < 0 ? -1 : result;
        }
        finally { Release(info); }
    }

    private static nint VTable(nint obj, int slot) => (*(nint**)obj)[slot];
    private static void Release(nint obj) { if (obj != 0) Marshal.Release(obj); }
}
