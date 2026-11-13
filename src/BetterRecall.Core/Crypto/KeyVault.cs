// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using BetterRecall.Contracts;
using BetterRecall.Core.Native;
using Konscious.Security.Cryptography;

namespace BetterRecall.Core.Crypto;

/// <summary>
/// Random 256-bit data key, wrapped in keys\datakey.bin by one of three modes (v1-design section 6):
/// tpm (RSA-2048, non-exportable, Microsoft Platform Crypto Provider, OAEP-SHA1), passphrase (Argon2id
/// 64 MiB, 3 passes, AES-256-GCM wrap) or dpapi (CurrentUser). File layout: "BRK1", mode byte, mode payload.
/// </summary>
public sealed class KeyVault : IKeyVault, IDisposable
{
    public const string ModeTpm = "tpm";
    public const string ModePassphrase = "passphrase";
    public const string ModeDpapi = "dpapi";

    internal const string TpmProvider = "Microsoft Platform Crypto Provider";
    /// <summary>Name every key file before 0.2 used. Persisted TPM keys are per Windows user, not per data folder,
    /// so one fixed name let any other data folder (a test run, a second home) open and delete the real key.</summary>
    internal const string LegacyTpmKeyName = "BetterRecall.DataKeyWrap";
    private static readonly byte[] Magic = "BRK1"u8.ToArray();
    private const int ArgonMemoryKiB = 65536;
    private const int ArgonPasses = 3;
    private const int ArgonLanes = 4;

    private readonly string _keyFile;
    private readonly Func<Settings> _settings;
    private readonly string _tpmKeyName;
    private readonly Lock _gate = new();
    private SecureKey? _key;
    private string? _fileMode;
    private TaskCompletionSource _loaded = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private volatile bool _helloAvailable;

    public KeyVault(string home, Func<Settings> settings, string? tpmKeyName = null, bool probeHello = true)
    {
        _keyFile = DataPaths.KeyFile(home);
        _settings = settings;
        _tpmKeyName = tpmKeyName ?? TpmKeyNameFor(home);
        _fileMode = ReadFileMode();
        if (probeHello)
            _ = HelloInterop.CheckAvailableAsync(CancellationToken.None).ContinueWith(t => _helloAvailable = t.IsCompletedSuccessfully && t.Result, TaskScheduler.Default);
    }

    /// <summary>Mode of the key file when it exists, else the mode a new key would use.</summary>
    public string Protection
    {
        get
        {
            lock (_gate) return _fileMode ?? ResolveNewMode(_settings().Security.KeyProtection, probe: false);
        }
    }

    public bool HelloAvailable => _helloAvailable;

    /// <summary>TPM key name for a data folder: "BetterRecall.DataKeyWrap." + 16 hex of SHA-256 over the full path
    /// (upper case, no trailing separator). Key files store the name they were wrapped with, so older files keep
    /// opening the legacy name.</summary>
    internal static string TpmKeyNameFor(string home)
    {
        var full = Path.GetFullPath(home).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar).ToUpperInvariant();
        return LegacyTpmKeyName + "." + Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(full)))[..16];
    }

    /// <summary>The TPM key this folder may delete: only its own name. A name read from a key file is never deleted, since a
    /// copied or restored folder (or the legacy shared name) names a key another folder still needs.</summary>
    internal static string? OwnedTpmKey(string ownName) => ownName == LegacyTpmKeyName ? null : ownName;

    private void DeleteOwnTpmKey()
    {
        if (OwnedTpmKey(_tpmKeyName) is not { } name) return;
        try { if (CngKey.Exists(name, new CngProvider(TpmProvider))) CngKey.Open(name, new CngProvider(TpmProvider)).Delete(); }
        catch (Exception ex) { Log.Debug($"TPM key delete skipped: {ex.GetType().Name}"); }
    }

    /// <summary>TPM key name stored in a tpm-mode key file; null for other modes or a short file.</summary>
    internal static string? TpmNameInFile(byte[] file)
    {
        if (file.Length < 7 || !file.AsSpan(0, 4).SequenceEqual(Magic) || file[4] != 1) return null;
        int nameLen = BinaryPrimitives.ReadUInt16LittleEndian(file.AsSpan(5));
        return file.Length < 7 + nameLen ? null : Encoding.UTF8.GetString(file, 7, nameLen);
    }

    public bool KeyLoaded { get { lock (_gate) return _key != null; } }

    /// <summary>Completes when the data key is in memory.</summary>
    public Task WhenLoaded => _loaded.Task;

    /// <summary>True when the key file is passphrase-wrapped, or no key exists and passphrase mode is configured.</summary>
    public bool NeedsPassphrase
    {
        get
        {
            lock (_gate)
            {
                if (_key != null) return false;
                return (_fileMode ?? (_settings().Security.KeyProtection == ModePassphrase ? ModePassphrase : null)) == ModePassphrase;
            }
        }
    }

    /// <summary>
    /// Loads the key without any prompt: unwraps a tpm or dpapi file, or creates a new key when none exists
    /// and the configured mode is not passphrase. Returns false when a passphrase is needed.
    /// </summary>
    public bool TryLoadSilent()
    {
        lock (_gate)
        {
            if (_key != null) return true;
            var s = _settings();
            bool iso = s.Security.MemoryIsolation;
            if (_fileMode == null)
            {
                var mode = ResolveNewMode(s.Security.KeyProtection, probe: true);
                if (mode == ModePassphrase) return false;
                var key = SecureKey.Random(iso);
                try { WriteWrapped(key, mode, passphrase: null); }
                catch { key.Dispose(); throw; }
                SetLoaded(key, mode);
                Log.Info($"data key created, protection {mode}");
                return true;
            }
            if (_fileMode == ModePassphrase) return false;
            var bytes = File.ReadAllBytes(_keyFile);
            var loaded = _fileMode == ModeTpm ? UnwrapTpm(bytes, iso) : UnwrapDpapi(bytes, iso);
            SetLoaded(loaded, _fileMode);
            Log.Info($"data key loaded, protection {_fileMode}");
            if (_fileMode == ModeTpm && TpmNameInFile(bytes) != _tpmKeyName)
            {
                // Wrapped under another folder's or the legacy shared TPM key: move it to this folder's own key, so a
                // purge here never needs to delete a key that may still serve another folder. The old key is left alone.
                try { WriteWrapped(loaded, ModeTpm, null); Log.Info("data key re-wrapped under this folder's TPM key"); }
                catch (Exception ex) { Log.Warn("TPM key rename failed, old wrap kept", ex); }
            }
            MigrateIfNeededLocked();
            return true;
        }
    }

    public bool UnlockWithPassphrase(string passphrase)
    {
        if (string.IsNullOrEmpty(passphrase)) return false;
        lock (_gate)
        {
            var s = _settings();
            if (_key != null)
            {
                if (_fileMode == ModePassphrase)
                {
                    // Already passphrase-wrapped: only check it, never replace it here.
                    using var check = UnwrapPassphrase(File.ReadAllBytes(_keyFile), passphrase, false);
                    return check != null;
                }
                if (s.Security.KeyProtection == ModePassphrase)
                {
                    // First passphrase for a key wrapped by tpm or dpapi: switch the wrap.
                    bool wasTpm = _fileMode == ModeTpm;
                    WriteWrapped(_key, ModePassphrase, passphrase);
                    _fileMode = ModePassphrase;
                    if (wasTpm) DeleteOwnTpmKey();
                    Log.Info("data key re-wrapped with passphrase");
                    return true;
                }
                return false;
            }
            if (_fileMode == null)
            {
                var key = SecureKey.Random(s.Security.MemoryIsolation);
                WriteWrapped(key, ModePassphrase, passphrase);
                SetLoaded(key, ModePassphrase);
                Log.Info("data key created, protection passphrase");
                return true;
            }
            if (_fileMode != ModePassphrase) return false;
            var unwrapped = UnwrapPassphrase(File.ReadAllBytes(_keyFile), passphrase, s.Security.MemoryIsolation);
            if (unwrapped == null) { Log.Info("passphrase unlock failed"); return false; }
            SetLoaded(unwrapped, ModePassphrase);
            Log.Info("data key unlocked with passphrase");
            MigrateIfNeededLocked();
            return true;
        }
    }

    /// <summary>Re-wraps a loaded key when security.keyProtection names tpm or dpapi and the file uses another mode.</summary>
    public void MigrateIfNeeded()
    {
        lock (_gate) MigrateIfNeededLocked();
    }

    private void MigrateIfNeededLocked()
    {
        if (_key == null) return;
        var want = _settings().Security.KeyProtection;
        if (want is not (ModeTpm or ModeDpapi) || want == _fileMode) return;
        try
        {
            WriteWrapped(_key, want, null);
            Log.Info($"data key re-wrapped from {_fileMode} to {want}");
            if (_fileMode == ModeTpm) DeleteOwnTpmKey();   // the old wrap could still unwrap a backed-up key file
            _fileMode = want;
        }
        catch (Exception ex) { Log.Warn($"re-wrap to {want} failed, {_fileMode} kept", ex); }
    }

    public Task<bool> VerifyUserAsync(nint hwnd, string reason, CancellationToken ct)
        => HelloInterop.VerifyAsync(hwnd, string.IsNullOrWhiteSpace(reason) ? "Unlock BetterRecall" : reason, ct);

    public void DestroyKey()
    {
        lock (_gate)
        {
            try
            {
                if (File.Exists(_keyFile))
                {
                    var len = (int)new FileInfo(_keyFile).Length;
                    using (var fs = new FileStream(_keyFile, FileMode.Open, FileAccess.Write, FileShare.None))
                    {
                        fs.Write(RandomNumberGenerator.GetBytes(Math.Max(len, 64)));
                        fs.Flush(flushToDisk: true);
                    }
                    File.Delete(_keyFile);
                }
            }
            catch (Exception ex) { Log.Error("key file destroy failed", ex); throw; }
            DeleteOwnTpmKey();   // never a name read from the file: see OwnedTpmKey
            _key?.Dispose();
            _key = null;
            _fileMode = null;
            _loaded = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
            Log.Info("data key destroyed");
        }
    }

    /// <summary>HKDF subkey of the data key. Throws when the key is not loaded.</summary>
    internal byte[] Derive(string label)
    {
        SecureKey k;
        lock (_gate) k = _key ?? throw new InvalidOperationException("Data key not loaded.");
        return k.Derive(label);
    }

    public void Dispose()
    {
        lock (_gate) { _key?.Dispose(); _key = null; }
    }

    private void SetLoaded(SecureKey key, string mode)
    {
        _key = key;
        _fileMode = mode;
        _loaded.TrySetResult();
    }

    // ---- mode selection ----

    private string ResolveNewMode(string configured, bool probe)
    {
        switch (configured)
        {
            case ModeTpm: return ModeTpm;
            case ModePassphrase: return ModePassphrase;
            case ModeDpapi: return ModeDpapi;
        }
        // auto: tpm when a TPM 2.0 provider works, else dpapi. A passphrase only applies once the user set one,
        // which shows up as a passphrase-wrapped key file.
        if (!probe) return TpmLikelyAvailable() ? ModeTpm : ModeDpapi;
        return TpmUsable() ? ModeTpm : ModeDpapi;
    }

    private static bool TpmLikelyAvailable()
    {
        try { CngKey.Exists("BetterRecall.probe", new CngProvider(TpmProvider)); return true; }
        catch { return false; }
    }

    private bool TpmUsable()
    {
        try
        {
            using var rsa = OpenOrCreateTpmKey();
            return rsa != null;
        }
        catch (Exception ex)
        {
            Log.Info($"TPM not usable ({ex.GetType().Name}), dpapi chosen");
            return false;
        }
    }

    private string? ReadFileMode()
    {
        try
        {
            if (!File.Exists(_keyFile)) return null;
            var head = new byte[5];
            using var fs = File.OpenRead(_keyFile);
            if (fs.Read(head) != 5 || !head.AsSpan(0, 4).SequenceEqual(Magic)) throw new InvalidDataException("Bad key file.");
            return head[4] switch { 1 => ModeTpm, 2 => ModePassphrase, 3 => ModeDpapi, _ => throw new InvalidDataException("Bad key file mode.") };
        }
        catch (IOException ex) { Log.Error("key file unreadable", ex); throw; }
    }

    // ---- wrap / unwrap ----

    private void WriteWrapped(SecureKey key, string mode, string? passphrase)
    {
        byte[] payload = mode switch
        {
            ModeTpm => WrapTpm(key),
            ModeDpapi => key.Use(k => DpapiProtect(k)),
            ModePassphrase => WrapPassphrase(key, passphrase ?? throw new ArgumentNullException(nameof(passphrase))),
            _ => throw new ArgumentException("Unknown mode."),
        };
        var file = new byte[5 + payload.Length];
        Magic.CopyTo(file, 0);
        file[4] = mode switch { ModeTpm => 1, ModePassphrase => 2, _ => 3 };
        payload.CopyTo(file, 5);
        Directory.CreateDirectory(Path.GetDirectoryName(_keyFile)!);
        var tmp = _keyFile + ".tmp";
        using (var fs = new FileStream(tmp, FileMode.Create, FileAccess.Write, FileShare.None))
        {
            fs.Write(file);
            fs.Flush(flushToDisk: true);
        }
        File.Move(tmp, _keyFile, overwrite: true);
    }

    private RSACng? OpenOrCreateTpmKey()
    {
        var provider = new CngProvider(TpmProvider);
        CngKey key;
        if (CngKey.Exists(_tpmKeyName, provider)) key = CngKey.Open(_tpmKeyName, provider);
        else
        {
            var p = new CngKeyCreationParameters
            {
                Provider = provider,
                ExportPolicy = CngExportPolicies.None,
                KeyUsage = CngKeyUsages.Decryption,
                KeyCreationOptions = CngKeyCreationOptions.None,
            };
            p.Parameters.Add(new CngProperty("Length", BitConverter.GetBytes(2048), CngPropertyOptions.None));
            key = CngKey.Create(CngAlgorithm.Rsa, _tpmKeyName, p);
        }
        return new RSACng(key);
    }

    private byte[] WrapTpm(SecureKey key)
    {
        using var rsa = OpenOrCreateTpmKey() ?? throw new CryptographicException("TPM key unavailable.");
        var wrapped = key.Use(k =>
        {
            var copy = k.ToArray();
            try { return rsa.Encrypt(copy, RSAEncryptionPadding.OaepSHA1); }
            finally { CryptographicOperations.ZeroMemory(copy); }
        });
        var name = Encoding.UTF8.GetBytes(_tpmKeyName);
        var payload = new byte[2 + name.Length + 4 + wrapped.Length];
        BinaryPrimitives.WriteUInt16LittleEndian(payload, (ushort)name.Length);
        name.CopyTo(payload, 2);
        BinaryPrimitives.WriteInt32LittleEndian(payload.AsSpan(2 + name.Length), wrapped.Length);
        wrapped.CopyTo(payload, 6 + name.Length);
        return payload;
    }

    private static SecureKey UnwrapTpm(byte[] file, bool iso)
    {
        var p = file.AsSpan(5);
        int nameLen = BinaryPrimitives.ReadUInt16LittleEndian(p);
        var name = Encoding.UTF8.GetString(p.Slice(2, nameLen));
        int len = BinaryPrimitives.ReadInt32LittleEndian(p.Slice(2 + nameLen));
        var wrapped = p.Slice(6 + nameLen, len).ToArray();
        using var key = CngKey.Open(name, new CngProvider(TpmProvider));
        using var rsa = new RSACng(key);
        var plain = rsa.Decrypt(wrapped, RSAEncryptionPadding.OaepSHA1);
        try { return new SecureKey(plain, iso); }
        finally { CryptographicOperations.ZeroMemory(plain); }
    }

    private static byte[] WrapPassphrase(SecureKey key, string passphrase)
    {
        var salt = RandomNumberGenerator.GetBytes(16);
        var kek = Argon(passphrase, salt, ArgonMemoryKiB, ArgonPasses, ArgonLanes);
        try
        {
            var nonce = RandomNumberGenerator.GetBytes(12);
            var ct = new byte[32];
            var tag = new byte[16];
            using (var gcm = new AesGcm(kek, 16)) key.Use(k => { gcm.Encrypt(nonce, k, ct, tag, "BRK1-pass"u8); return 0; });
            var payload = new byte[12 + 16 + 12 + 32 + 16];
            BinaryPrimitives.WriteInt32LittleEndian(payload, ArgonMemoryKiB);
            BinaryPrimitives.WriteInt32LittleEndian(payload.AsSpan(4), ArgonPasses);
            BinaryPrimitives.WriteInt32LittleEndian(payload.AsSpan(8), ArgonLanes);
            salt.CopyTo(payload, 12);
            nonce.CopyTo(payload, 28);
            ct.CopyTo(payload, 40);
            tag.CopyTo(payload, 72);
            return payload;
        }
        finally { CryptographicOperations.ZeroMemory(kek); }
    }

    private static SecureKey? UnwrapPassphrase(byte[] file, string passphrase, bool iso)
    {
        var p = file.AsSpan(5);
        int mem = BinaryPrimitives.ReadInt32LittleEndian(p);
        int passes = BinaryPrimitives.ReadInt32LittleEndian(p.Slice(4));
        int lanes = BinaryPrimitives.ReadInt32LittleEndian(p.Slice(8));
        if (mem is < 8192 or > 4 * 1024 * 1024 || passes is < 1 or > 64 || lanes is < 1 or > 64) throw new InvalidDataException("Bad key file.");
        var salt = p.Slice(12, 16).ToArray();
        var kek = Argon(passphrase, salt, mem, passes, lanes);
        var plain = new byte[32];
        try
        {
            using var gcm = new AesGcm(kek, 16);
            gcm.Decrypt(p.Slice(28, 12), p.Slice(40, 32), p.Slice(72, 16), plain, "BRK1-pass"u8);
            return new SecureKey(plain, iso);
        }
        catch (AuthenticationTagMismatchException) { return null; }
        finally
        {
            CryptographicOperations.ZeroMemory(kek);
            CryptographicOperations.ZeroMemory(plain);
        }
    }

    internal static byte[] Argon(string passphrase, byte[] salt, int memKiB, int passes, int lanes)
    {
        var pw = Encoding.UTF8.GetBytes(passphrase);
        try
        {
            using var a = new Argon2id(pw) { Salt = salt, MemorySize = memKiB, Iterations = passes, DegreeOfParallelism = lanes };
            return a.GetBytes(32);
        }
        finally { CryptographicOperations.ZeroMemory(pw); }
    }

    private static SecureKey UnwrapDpapi(byte[] file, bool iso)
    {
        var p = file.AsSpan(5);
        int len = BinaryPrimitives.ReadInt32LittleEndian(p);
        var plain = DpapiUnprotect(p.Slice(4, len));
        try { return new SecureKey(plain, iso); }
        finally { CryptographicOperations.ZeroMemory(plain); }
    }

    private static unsafe byte[] DpapiProtect(ReadOnlySpan<byte> data)
    {
        fixed (byte* d = data)
        {
            var input = new Win32.DATA_BLOB { cbData = data.Length, pbData = (nint)d };
            if (!Win32.CryptProtectData(ref input, "BetterRecall data key", 0, 0, 0, Win32.CRYPTPROTECT_UI_FORBIDDEN, out var output))
                throw new CryptographicException(Marshal.GetLastPInvokeError());
            try
            {
                var blob = new ReadOnlySpan<byte>((void*)output.pbData, output.cbData);
                var payload = new byte[4 + blob.Length];
                BinaryPrimitives.WriteInt32LittleEndian(payload, blob.Length);
                blob.CopyTo(payload.AsSpan(4));
                return payload;
            }
            finally { Win32.LocalFree(output.pbData); }
        }
    }

    private static unsafe byte[] DpapiUnprotect(ReadOnlySpan<byte> blob)
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
