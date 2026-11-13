// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using BetterRecall.Contracts;
using BetterRecall.Core.Native;

namespace BetterRecall.Core.Ai;

/// <summary>
/// Cloud API key in keys\ai-key.bin: "BRA2" + DPAPI (CurrentUser) blob of UTF-8 "provider|endpoint\nkey". Never in
/// settings.json, never returned on a pipe, never logged. The key is read only for the moment of a send, and only for the
/// provider and endpoint it was entered for: changing ai.provider or ai.endpoint asks for the key again, so a key never
/// goes to a host the user did not enter it for. "BRA1" files (0.2, no binding) are used with default endpoints only.
/// </summary>
public sealed class ApiKeyStore(string home)
{
    public const int MaxLength = 4096;
    private static readonly byte[] Magic = "BRA2"u8.ToArray();
    private static readonly byte[] LegacyMagic = "BRA1"u8.ToArray();

    /// <summary>What a key is bound to: provider and effective endpoint (lower case).</summary>
    public static string Binding(AiSettings a) => (a.Provider + "|" + AiDefaults.EffectiveEndpoint(a)).ToLowerInvariant();
    private readonly string _file = DataPaths.AiKeyFile(home);
    private readonly Lock _gate = new();

    public bool IsSet { get { lock (_gate) return File.Exists(_file); } }

    public static bool Valid(string? key) =>
        key is { Length: > 0 and <= MaxLength } && key.Trim().Length == key.Length && key.All(c => c > ' ' && c < 127);

    public void Set(string key, AiSettings boundTo)
    {
        if (!Valid(key)) throw new ArgumentException("Invalid API key.");
        var plain = Encoding.UTF8.GetBytes(Binding(boundTo) + "\n" + key);
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

    /// <summary>The key for these AI settings, or null when none is stored, it was entered for another provider or
    /// endpoint, or the file cannot be decrypted (other user, damaged).</summary>
    public string? Read(AiSettings a)
    {
        byte[] file;
        lock (_gate)
        {
            if (!File.Exists(_file)) return null;
            file = File.ReadAllBytes(_file);
        }
        bool legacy = file.Length >= 5 && file.AsSpan(0, 4).SequenceEqual(LegacyMagic);
        if (file.Length < 5 || (!legacy && !file.AsSpan(0, 4).SequenceEqual(Magic))) { Log.Warn("API key file damaged"); return null; }
        if (legacy && !string.IsNullOrWhiteSpace(a.Endpoint)) { Log.Info("API key predates endpoint binding: enter it again for a custom endpoint"); return null; }
        byte[]? plain = null;
        try
        {
            plain = Unprotect(file.AsSpan(4));
            var text = Encoding.UTF8.GetString(plain);
            if (legacy) return text;
            int nl = text.IndexOf('\n');
            if (nl < 0 || text[..nl] != Binding(a)) { Log.Info("API key was entered for another provider or endpoint"); return null; }
            return text[(nl + 1)..];
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
