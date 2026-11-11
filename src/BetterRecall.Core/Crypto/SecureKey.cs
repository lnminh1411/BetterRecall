// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using BetterRecall.Core.Native;

namespace BetterRecall.Core.Crypto;

/// <summary>
/// 32-byte key in native memory: page-aligned, VirtualLock'ed, and CryptProtectMemory-wrapped while idle
/// when memory isolation is on. Never copied onto the managed heap except through Use/Derive.
/// </summary>
internal sealed unsafe class SecureKey : IDisposable
{
    public const int KeyBytes = 32;
    private const int PageBytes = 4096;

    private readonly Lock _gate = new();
    private byte* _ptr;
    private bool _protected;
    private readonly bool _isolate;

    public SecureKey(ReadOnlySpan<byte> key, bool memoryIsolation)
    {
        if (key.Length != KeyBytes) throw new ArgumentException("Key must be 32 bytes.");
        _isolate = memoryIsolation;
        _ptr = (byte*)NativeMemory.AlignedAlloc(PageBytes, PageBytes);
        NativeMemory.Clear(_ptr, PageBytes);
        if (!Win32.VirtualLock((nint)_ptr, PageBytes))
            Log.Debug($"VirtualLock on key page failed, error {Marshal.GetLastPInvokeError()}");
        key.CopyTo(new Span<byte>(_ptr, KeyBytes));
        Protect();
    }

    public static SecureKey Random(bool memoryIsolation)
    {
        Span<byte> k = stackalloc byte[KeyBytes];
        RandomNumberGenerator.Fill(k);
        try { return new SecureKey(k, memoryIsolation); }
        finally { CryptographicOperations.ZeroMemory(k); }
    }

    private void Protect()
    {
        if (!_isolate || _protected) return;
        if (Win32.CryptProtectMemory((nint)_ptr, KeyBytes, Win32.CRYPTPROTECTMEMORY_SAME_PROCESS)) _protected = true;
        else Log.Warn($"CryptProtectMemory failed, error {Marshal.GetLastPInvokeError()}");
    }

    private void Unprotect()
    {
        if (!_protected) return;
        if (!Win32.CryptUnprotectMemory((nint)_ptr, KeyBytes, Win32.CRYPTPROTECTMEMORY_SAME_PROCESS))
            throw new CryptographicException("CryptUnprotectMemory failed.");
        _protected = false;
    }

    public delegate T SpanFunc<T>(ReadOnlySpan<byte> key);

    /// <summary>Runs f with the plain key, then wraps the key again.</summary>
    public T Use<T>(SpanFunc<T> f)
    {
        lock (_gate)
        {
            ObjectDisposedException.ThrowIf(_ptr == null, this);
            Unprotect();
            try { return f(new ReadOnlySpan<byte>(_ptr, KeyBytes)); }
            finally { Protect(); }
        }
    }

    /// <summary>HKDF-SHA256 subkey for a label. Caller zeroes the result after use.</summary>
    public byte[] Derive(string label, int length = 32)
        => Use(k =>
        {
            var info = Encoding.UTF8.GetBytes(label);
            var output = new byte[length];
            HKDF.DeriveKey(HashAlgorithmName.SHA256, k, output, ReadOnlySpan<byte>.Empty, info);
            return output;
        });

    public void Dispose()
    {
        lock (_gate)
        {
            if (_ptr == null) return;
            NativeMemory.Clear(_ptr, PageBytes);
            Win32.VirtualUnlock((nint)_ptr, PageBytes);
            NativeMemory.AlignedFree(_ptr);
            _ptr = null;
        }
    }
}
