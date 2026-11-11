// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using BetterRecall.Contracts;
using BetterRecall.Core.Native;

namespace BetterRecall.Core.Ai;

/// <summary>
/// Cloud describe API key in keys\ai-key.bin: "BRA1" + DPAPI (CurrentUser) blob of the UTF-8 key. Never in settings.json,
/// never returned on a pipe, never logged. The key is read only for the moment of a send.
/// </summary>
public sealed class ApiKeyStore(string home)
{
    public const int MaxLength = 4096;
    private static readonly byte[] Magic = "BRA1"u8.ToArray();
    private readonly string _file = DataPaths.AiKeyFile(home);
    private readonly Lock _gate = new();

    public bool IsSet { get { lock (_gate) return File.Exists(_file); } }

    public static bool Valid(string? key) =>
        key is { Length: > 0 and <= MaxLength } && key.Trim().Length == key.Length && key.All(c => c > ' ' && c < 127);

    public void Set(string key)
    {
        if (!Valid(key)) throw new ArgumentException("Invalid API key.");
        var plain = Encoding.UTF8.GetBytes(key);
        try
        {
            var blob = Protect(plain);
            var file = new byte[4 + blob.Length];
            Magic.CopyTo(file, 0);
            blob.CopyTo(file, 4);
            lock (_gate)
            {
                Directory.CreateDirectory(Path.GetDirectoryName(_file)!);
                var tmp = _file + ".tmp";
                File.WriteAllBytes(tmp, file);
                File.Move(tmp, _file, overwrite: true);
            }
        }
        finally { CryptographicOperations.ZeroMemory(plain); }
        Log.Info("cloud describe API key stored");
    }

    public void Clear()
    {
        lock (_gate)
        {
            if (!File.Exists(_file)) return;
            var len = (int)new FileInfo(_file).Length;
            using (var fs = new FileStream(_file, FileMode.Open, FileAccess.Write, FileShare.None))
                fs.Write(RandomNumberGenerator.GetBytes(Math.Max(len, 16)));
            File.Delete(_file);
        }
        Log.Info("cloud describe API key removed");
    }

    /// <summary>The key, or null when none is stored or the file cannot be decrypted (other user, damaged).</summary>
    public string? Read()
    {
        byte[] file;
        lock (_gate)
        {
            if (!File.Exists(_file)) return null;
            file = File.ReadAllBytes(_file);
        }
        if (file.Length < 5 || !file.AsSpan(0, 4).SequenceEqual(Magic)) { Log.Warn("API key file damaged"); return null; }
        byte[]? plain = null;
        try
        {
            plain = Unprotect(file.AsSpan(4));
            return Encoding.UTF8.GetString(plain);
        }
        catch (CryptographicException) { Log.Warn("API key file cannot be decrypted"); return null; }
        finally { if (plain != null) CryptographicOperations.ZeroMemory(plain); }
    }

    private static unsafe byte[] Protect(ReadOnlySpan<byte> data)
    {
        fixed (byte* d = data)
        {
            var input = new Win32.DATA_BLOB { cbData = data.Length, pbData = (nint)d };
            if (!Win32.CryptProtectData(ref input, "BetterRecall cloud describe key", 0, 0, 0, Win32.CRYPTPROTECT_UI_FORBIDDEN, out var output))
                throw new CryptographicException(Marshal.GetLastPInvokeError());
            try { return new ReadOnlySpan<byte>((void*)output.pbData, output.cbData).ToArray(); }
            finally { Win32.LocalFree(output.pbData); }
        }
    }

    private static unsafe byte[] Unprotect(ReadOnlySpan<byte> blob)
    {
        fixed (byte* d = blob)
        {
            var input = new Win32.DATA_BLOB { cbData = blob.Length, pbData = (nint)d };
            if (!Win32.CryptUnprotectData(ref input, 0, 0, 0, 0, Win32.CRYPTPROTECT_UI_FORBIDDEN, out var output))
                throw new CryptographicException(Marshal.GetLastPInvokeError());
            try
            {
                var plain = new ReadOnlySpan<byte>((void*)output.pbData, output.cbData).ToArray();
                new Span<byte>((void*)output.pbData, output.cbData).Clear();
                return plain;
            }
            finally { Win32.LocalFree(output.pbData); }
        }
    }
}
