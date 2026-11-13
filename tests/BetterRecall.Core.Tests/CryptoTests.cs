// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Security.Cryptography;
using BetterRecall.Contracts;
using BetterRecall.Core.Crypto;

namespace BetterRecall.Core.Tests;

public class BlobCipherTests
{
    private static BlobCipher Cipher(byte[] key) => new(() => (byte[])key.Clone());

    private static string Write(BlobCipher c, string dir, byte[] data)
    {
        var p = Path.Combine(dir, Guid.NewGuid().ToString("N") + ".enc");
        using (var s = c.OpenWrite(p))
        {
            // odd write sizes cross chunk boundaries
            int off = 0, step = 7919;
            while (off < data.Length) { int n = Math.Min(step, data.Length - off); s.Write(data, off, n); off += n; }
        }
        return p;
    }

    private static byte[] Read(BlobCipher c, string p)
    {
        using var s = c.OpenRead(p);
        using var ms = new MemoryStream();
        s.CopyTo(ms);
        return ms.ToArray();
    }

    [Theory]
    [InlineData(0)]
    [InlineData(1)]
    [InlineData(65535)]
    [InlineData(65536)]
    [InlineData(65537)]
    [InlineData(3 * 65536)]
    [InlineData(200_001)]
    public void Round_trip(int size)
    {
        using var h = new TempHome(null);
        var key = RandomNumberGenerator.GetBytes(32);
        var data = RandomNumberGenerator.GetBytes(size);
        var p = Write(Cipher(key), h.Path, data);
        Assert.Equal(data, Read(Cipher(key), p));
        int chunks = Math.Max(1, (size + 65535) / 65536);
        Assert.Equal(20 + size + chunks * 16, new FileInfo(p).Length);
    }

    [Fact]
    public void Ciphertext_does_not_contain_plaintext()
    {
        using var h = new TempHome(null);
        var data = System.Text.Encoding.ASCII.GetBytes(string.Concat(Enumerable.Repeat("SECRET-PLAINTEXT ", 1000)));
        var p = Write(Cipher(RandomNumberGenerator.GetBytes(32)), h.Path, data);
        Assert.DoesNotContain("SECRET", System.Text.Encoding.ASCII.GetString(File.ReadAllBytes(p)));
    }

    [Theory]
    [InlineData(5)]          // header
    [InlineData(25)]         // first chunk body
    [InlineData(65536 + 20 + 3)] // first chunk tag
    [InlineData(-1)]         // last byte
    public void Tamper_throws(int offset)
    {
        using var h = new TempHome(null);
        var key = RandomNumberGenerator.GetBytes(32);
        var p = Write(Cipher(key), h.Path, RandomNumberGenerator.GetBytes(150_000));
        var bytes = File.ReadAllBytes(p);
        bytes[offset < 0 ? bytes.Length - 1 : offset] ^= 0x01;
        File.WriteAllBytes(p, bytes);
        Assert.ThrowsAny<CryptographicException>(() => Read(Cipher(key), p));
    }

    [Fact]
    public void Truncation_at_chunk_boundary_throws()
    {
        using var h = new TempHome(null);
        var key = RandomNumberGenerator.GetBytes(32);
        var p = Write(Cipher(key), h.Path, RandomNumberGenerator.GetBytes(3 * 65536 + 100));
        var bytes = File.ReadAllBytes(p);
        File.WriteAllBytes(p, bytes[..(20 + 2 * (65536 + 16))]); // keep the first two full chunks only
        Assert.ThrowsAny<CryptographicException>(() => Read(Cipher(key), p));
    }

    [Fact]
    public void Truncation_mid_chunk_and_wrong_key_throw()
    {
        using var h = new TempHome(null);
        var key = RandomNumberGenerator.GetBytes(32);
        var p = Write(Cipher(key), h.Path, RandomNumberGenerator.GetBytes(100_000));
        Assert.ThrowsAny<CryptographicException>(() => Read(Cipher(RandomNumberGenerator.GetBytes(32)), p));
        var bytes = File.ReadAllBytes(p);
        File.WriteAllBytes(p, bytes[..(bytes.Length - 10)]);
        Assert.ThrowsAny<CryptographicException>(() => Read(Cipher(key), p));
    }

    [Fact]
    public void Chunk_swap_throws()
    {
        using var h = new TempHome(null);
        var key = RandomNumberGenerator.GetBytes(32);
        var p = Write(Cipher(key), h.Path, RandomNumberGenerator.GetBytes(3 * 65536 + 5));
        var b = File.ReadAllBytes(p);
        int c = 65536 + 16;
        var first = b[20..(20 + c)];
        Array.Copy(b, 20 + c, b, 20, c);
        Array.Copy(first, 0, b, 20 + c, c);
        File.WriteAllBytes(p, b);
        Assert.ThrowsAny<CryptographicException>(() => Read(Cipher(key), p));
    }
}

public class KeyVaultTests
{
    private static Settings With(string mode) { var s = new Settings(); s.Security.KeyProtection = mode; return s; }

    [Fact]
    public void Dpapi_creates_and_reloads_same_key()
    {
        using var h = new TempHome(null);
        var s = With("dpapi");
        byte[] a, b;
        using (var v = new KeyVault(h.Path, () => s, probeHello: false))
        {
            Assert.Equal("dpapi", v.Protection);
            Assert.True(v.TryLoadSilent());
            Assert.True(v.KeyLoaded);
            a = v.Derive("x");
        }
        using (var v = new KeyVault(h.Path, () => s, probeHello: false))
        {
            Assert.True(v.TryLoadSilent());
            b = v.Derive("x");
            Assert.NotEqual(a, v.Derive("y"));
        }
        Assert.Equal(a, b);
        var file = File.ReadAllBytes(DataPaths.KeyFile(h.Path));
        Assert.Equal("BRK1"u8.ToArray(), file[..4]);
        Assert.Equal(3, file[4]);
    }

    [Fact]
    public void Passphrase_mode_waits_then_unlocks_and_rejects_wrong_passphrase()
    {
        using var h = new TempHome(null);
        var s = With("passphrase");
        byte[] a;
        using (var v = new KeyVault(h.Path, () => s, probeHello: false))
        {
            Assert.False(v.TryLoadSilent());
            Assert.True(v.NeedsPassphrase);
            Assert.False(v.WhenLoaded.IsCompleted);
            Assert.True(v.UnlockWithPassphrase("correct horse"));   // first passphrase creates the key
            Assert.True(v.WhenLoaded.IsCompleted);
            a = v.Derive("x");
            Assert.False(v.UnlockWithPassphrase("other"));           // never replaces an existing passphrase
        }
        using (var v = new KeyVault(h.Path, () => s, probeHello: false))
        {
            Assert.Equal("passphrase", v.Protection);
            Assert.False(v.TryLoadSilent());
            Assert.False(v.UnlockWithPassphrase("wrong"));
            Assert.False(v.KeyLoaded);
            Assert.True(v.UnlockWithPassphrase("correct horse"));
            Assert.Equal(a, v.Derive("x"));
        }
    }

    [Fact]
    public void Switching_dpapi_to_passphrase_keeps_the_key()
    {
        using var h = new TempHome(null);
        var s = With("dpapi");
        using var v = new KeyVault(h.Path, () => s, probeHello: false);
        Assert.True(v.TryLoadSilent());
        var a = v.Derive("x");
        s.Security.KeyProtection = "passphrase";
        Assert.True(v.UnlockWithPassphrase("pw"));
        Assert.Equal("passphrase", v.Protection);
        using var v2 = new KeyVault(h.Path, () => s, probeHello: false);
        Assert.True(v2.UnlockWithPassphrase("pw"));
        Assert.Equal(a, v2.Derive("x"));
    }

    [Fact]
    public void Destroy_removes_key_file_and_memory()
    {
        using var h = new TempHome(null);
        var s = With("dpapi");
        using var v = new KeyVault(h.Path, () => s, probeHello: false);
        v.TryLoadSilent();
        v.DestroyKey();
        Assert.False(v.KeyLoaded);
        Assert.False(File.Exists(DataPaths.KeyFile(h.Path)));
        Assert.Throws<InvalidOperationException>(() => v.Derive("x"));
    }

    [Fact]
    public void Argon2id_matches_rfc9106_shape_and_is_deterministic()
    {
        var salt = new byte[16];
        var a = KeyVault.Argon("pw", salt, 8192, 1, 1);
        var b = KeyVault.Argon("pw", salt, 8192, 1, 1);
        Assert.Equal(a, b);
        Assert.NotEqual(a, KeyVault.Argon("pw2", salt, 8192, 1, 1));
        Assert.Equal(32, a.Length);
    }

    [Fact]
    public void Winrt_parameterized_iid_matches_known_value()
    {
        // IAsyncOperation<Boolean> is {cdb5efb3-5788-509d-9be1-71ccb8a3362a} in the Windows SDK headers.
        Assert.Equal(new Guid("cdb5efb3-5788-509d-9be1-71ccb8a3362a"),
            HelloInterop.ParameterizedIid("pinterface({9fc2b0bb-e446-44e2-aa61-9cab8f636af2};b1)"));
        // IAsyncOperation<UserConsentVerificationResult>, from windows.security.credentials.ui.h (SDK 10.0.26100.0).
        Assert.Equal(new Guid("fd596ffd-2318-558f-9dbe-d21df43764a5"), HelloInterop.IidAsyncOpResult);
    }

    [Fact]
    public async Task Hello_availability_check_returns_without_prompt()
    {
        var available = await HelloInterop.CheckAvailableAsync(CancellationToken.None);
        File.WriteAllText(Path.Combine(TestEnv.Root, "hello-available.txt"), available.ToString());
    }

    [Fact]
    public void Tpm_key_name_is_per_data_folder_and_never_the_legacy_shared_name()
    {
        var a = KeyVault.TpmKeyNameFor(@"C:\Users\x\AppData\Local\BetterRecall");
        Assert.StartsWith(KeyVault.LegacyTpmKeyName + ".", a);
        Assert.Equal(KeyVault.LegacyTpmKeyName.Length + 17, a.Length);
        Assert.Equal(a, KeyVault.TpmKeyNameFor(@"c:\users\x\appdata\local\betterrecall\"));
        Assert.NotEqual(a, KeyVault.TpmKeyNameFor(@"C:\Users\x\AppData\Local\Temp\h-1"));
    }

    [Fact]
    public void Destroy_deletes_only_this_folders_own_tpm_key()
    {
        var own = KeyVault.TpmKeyNameFor(@"K:\t\h-1");
        Assert.Equal(own, KeyVault.OwnedTpmKey(own));
        Assert.Null(KeyVault.OwnedTpmKey(KeyVault.LegacyTpmKeyName));   // shared by every pre-0.2 folder
    }

    [Fact]
    public void Tpm_name_is_read_from_a_tpm_key_file_only()
    {
        var name = "BetterRecall.DataKeyWrap"u8.ToArray();
        var file = new byte[7 + name.Length + 4 + 3];
        "BRK1"u8.CopyTo(file);
        file[4] = 1;
        BitConverter.GetBytes((ushort)name.Length).CopyTo(file, 5);
        name.CopyTo(file, 7);
        Assert.Equal(KeyVault.LegacyTpmKeyName, KeyVault.TpmNameInFile(file));
        file[4] = 3;
        Assert.Null(KeyVault.TpmNameInFile(file));
        Assert.Null(KeyVault.TpmNameInFile([1, 2]));
    }

    /// <summary>A dpapi vault's purge leaves another TPM key alone. Opt in with BR_TEST_TPM=1.</summary>
    [Fact]
    public void Dpapi_destroy_leaves_a_tpm_key_it_does_not_own()
    {
        if (Environment.GetEnvironmentVariable("BR_TEST_TPM") != "1") return;
        var other = "BetterRecall.Test." + Guid.NewGuid().ToString("N")[..8];
        var p = new CngKeyCreationParameters { Provider = new CngProvider(KeyVault.TpmProvider), KeyUsage = CngKeyUsages.Decryption };
        CngKey.Create(CngAlgorithm.Rsa, other, p).Dispose();
        try
        {
            using var h = new TempHome(null);
            var s = With("dpapi");
            using var v = new KeyVault(h.Path, () => s, probeHello: false);
            v.TryLoadSilent();
            v.DestroyKey();
            Assert.True(CngKey.Exists(other, new CngProvider(KeyVault.TpmProvider)));
        }
        finally { CngKey.Open(other, new CngProvider(KeyVault.TpmProvider)).Delete(); }
    }

    /// <summary>Creates and deletes a throwaway TPM key. Opt in with BR_TEST_TPM=1.</summary>
    [Fact]
    public void Tpm_wrap_round_trip_when_enabled()
    {
        if (Environment.GetEnvironmentVariable("BR_TEST_TPM") != "1") return;
        using var h = new TempHome(null);
        var s = With("tpm");
        var name = "BetterRecall.Test." + Guid.NewGuid().ToString("N")[..8];
        byte[] a;
        using (var v = new KeyVault(h.Path, () => s, name, probeHello: false))
        {
            Assert.True(v.TryLoadSilent());
            Assert.Equal("tpm", v.Protection);
            a = v.Derive("x");
        }
        using (var v = new KeyVault(h.Path, () => s, name, probeHello: false))
        {
            Assert.True(v.TryLoadSilent());
            Assert.Equal(a, v.Derive("x"));
            v.DestroyKey();
        }
        Assert.False(CngKey.Exists(name, new CngProvider(KeyVault.TpmProvider)));
    }
}
