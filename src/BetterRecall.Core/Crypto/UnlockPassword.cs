// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.Security.Cryptography;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Crypto;

public enum PasswordCheck { Ok, Wrong, Throttled, NotSet }

/// <summary>
/// Unlock password for security.unlockMethod = password: a read gate like Windows Hello, not an encryption key.
/// keys\unlock.bin holds "BRU1", Argon2id parameters (64 MiB, 3 passes, 4 lanes), a 16-byte salt, the 32-byte hash,
/// the count of wrong tries in a row and the time until which tries are refused. The counter lives in the file,
/// so restarting the agent does not reset it. The password itself is never stored or logged.
/// Rate limit: 5 free tries, then 30 s, doubling per further wrong try, at most 15 minutes.
/// </summary>
public sealed class UnlockPassword
{
    public const int MinLength = 6;
    public const int MaxLength = 256;
    public const int FreeTries = 5;
    private static readonly byte[] Magic = "BRU1"u8.ToArray();
    private const int FileLength = 4 + 12 + 16 + 32 + 4 + 8;

    private readonly string _file;
    private readonly Func<DateTimeOffset> _now;
    private readonly Lock _gate = new();
    private readonly int _memKiB, _passes, _lanes;

    public UnlockPassword(string home, Func<DateTimeOffset>? now = null, int memKiB = 65536, int passes = 3, int lanes = 4)
    {
        _file = DataPaths.UnlockPasswordFile(home);
        _now = now ?? (() => DateTimeOffset.UtcNow);
        _memKiB = memKiB;
        _passes = passes;
        _lanes = lanes;
    }

    public bool IsSet { get { lock (_gate) return Read() != null; } }

    /// <summary>Seconds until the next try is accepted, 0 when tries are open.</summary>
    public int RetryAfterSeconds
    {
        get
        {
            lock (_gate)
            {
                var r = Read();
                if (r == null) return 0;
                var wait = DateTimeOffset.FromUnixTimeMilliseconds(r.Value.LockedUntilMs) - _now();
                return wait > TimeSpan.Zero ? (int)Math.Ceiling(wait.TotalSeconds) : 0;
            }
        }
    }

    public static bool ValidNew(string? password) => password is { Length: >= MinLength and <= MaxLength } && !string.IsNullOrWhiteSpace(password);

    /// <summary>Checks a password, counting wrong tries. Throttled tries are refused without running Argon2id.</summary>
    public PasswordCheck Verify(string? password)
    {
        lock (_gate)
        {
            var r = Read();
            if (r == null) return PasswordCheck.NotSet;
            var rec = r.Value;
            if (_now().ToUnixTimeMilliseconds() < rec.LockedUntilMs) return PasswordCheck.Throttled;
            bool ok = !string.IsNullOrEmpty(password) && password.Length <= MaxLength && Matches(rec, password);
            if (ok)
            {
                if (rec.Failures != 0 || rec.LockedUntilMs != 0) Write(rec with { Failures = 0, LockedUntilMs = 0 });
                return PasswordCheck.Ok;
            }
            int failures = rec.Failures + 1;
            long until = 0;
            if (failures >= FreeTries)
            {
                int extra = Math.Min(failures - FreeTries, 5); // 30 s, 1, 2, 4, 8 min, then 15 min
                double seconds = Math.Min(30 * Math.Pow(2, extra), 900);
                until = (_now() + TimeSpan.FromSeconds(seconds)).ToUnixTimeMilliseconds();
            }
            Write(rec with { Failures = failures, LockedUntilMs = until });
            Log.Info($"unlock password wrong ({failures} in a row)");
            return PasswordCheck.Wrong;
        }
    }

    /// <summary>Sets a new password (fresh salt, counter cleared). The caller checks the current password first.</summary>
    public void Set(string password)
    {
        if (!ValidNew(password)) throw new ArgumentException("Password too short or too long.");
        var salt = RandomNumberGenerator.GetBytes(16);
        var hash = KeyVault.Argon(password, salt, _memKiB, _passes, _lanes);
        lock (_gate) Write(new Record(_memKiB, _passes, _lanes, salt, hash, 0, 0));
        CryptographicOperations.ZeroMemory(hash);
        Log.Info("unlock password set");
    }

    public void Clear()
    {
        lock (_gate)
        {
            try { if (File.Exists(_file)) File.Delete(_file); }
            catch (IOException ex) { Log.Warn("unlock password file delete failed", ex); throw; }
        }
        Log.Info("unlock password removed");
    }

    // ---- file ----

    private readonly record struct Record(int MemKiB, int Passes, int Lanes, byte[] Salt, byte[] Hash, int Failures, long LockedUntilMs);

    private static bool Matches(Record r, string password)
    {
        var h = KeyVault.Argon(password, r.Salt, r.MemKiB, r.Passes, r.Lanes);
        try { return CryptographicOperations.FixedTimeEquals(h, r.Hash); }
        finally { CryptographicOperations.ZeroMemory(h); }
    }

    private Record? Read()
    {
        byte[] b;
        try
        {
            if (!File.Exists(_file)) return null;
            b = File.ReadAllBytes(_file);
        }
        catch (IOException ex) { Log.Warn("unlock password file unreadable", ex); return null; }
        if (b.Length != FileLength || !b.AsSpan(0, 4).SequenceEqual(Magic)) { Log.Warn("unlock password file damaged"); return null; }
        var p = b.AsSpan(4);
        int mem = BinaryPrimitives.ReadInt32LittleEndian(p), passes = BinaryPrimitives.ReadInt32LittleEndian(p[4..]), lanes = BinaryPrimitives.ReadInt32LittleEndian(p[8..]);
        if (mem is < 8 or > 4 * 1024 * 1024 || passes is < 1 or > 64 || lanes is < 1 or > 64) { Log.Warn("unlock password file damaged"); return null; }
        return new Record(mem, passes, lanes, p.Slice(12, 16).ToArray(), p.Slice(28, 32).ToArray(),
            BinaryPrimitives.ReadInt32LittleEndian(p[60..]), BinaryPrimitives.ReadInt64LittleEndian(p[64..]));
    }

    private void Write(Record r)
    {
        var b = new byte[FileLength];
        Magic.CopyTo(b, 0);
        var p = b.AsSpan(4);
        BinaryPrimitives.WriteInt32LittleEndian(p, r.MemKiB);
        BinaryPrimitives.WriteInt32LittleEndian(p[4..], r.Passes);
        BinaryPrimitives.WriteInt32LittleEndian(p[8..], r.Lanes);
        r.Salt.CopyTo(p[12..]);
        r.Hash.CopyTo(p[28..]);
        BinaryPrimitives.WriteInt32LittleEndian(p[60..], r.Failures);
        BinaryPrimitives.WriteInt64LittleEndian(p[64..], r.LockedUntilMs);
        Directory.CreateDirectory(Path.GetDirectoryName(_file)!);
        var tmp = _file + ".tmp";
        using (var fs = new FileStream(tmp, FileMode.Create, FileAccess.Write, FileShare.None))
        {
            fs.Write(b);
            fs.Flush(flushToDisk: true);
        }
        File.Move(tmp, _file, overwrite: true);
    }
}
