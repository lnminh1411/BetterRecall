// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Tests;

public sealed class IntegrityCheckTests : IDisposable
{
    private readonly string _tempRoot;

    public IntegrityCheckTests()
    {
        var baseDir = Environment.GetEnvironmentVariable("BR_TEST_TMP");
        if (string.IsNullOrWhiteSpace(baseDir))
            baseDir = Path.Combine(AppContext.BaseDirectory, "test-tmp");
        _tempRoot = Path.Combine(baseDir, $"i7-cs-{Environment.ProcessId}-{Guid.NewGuid():N}");
        Directory.CreateDirectory(_tempRoot);
    }

    public void Dispose()
    {
        try
        {
            if (Directory.Exists(_tempRoot))
                Directory.Delete(_tempRoot, recursive: true);
        }
        catch
        {
            // Best-effort cleanup.
        }
    }

    private static (string PublicPem, byte[] ManifestBytes, byte[] SigBytes) CreateSignedLayout(
        string dir,
        string version,
        (string RelPath, byte[] Content)[] files)
    {
        var entries = new List<(string Path, long Size, string Sha256)>();
        foreach (var (rel, content) in files)
        {
            var full = Path.Combine(dir, rel);
            var parent = Path.GetDirectoryName(full);
            if (!string.IsNullOrEmpty(parent)) Directory.CreateDirectory(parent);
            File.WriteAllBytes(full, content);
            var hash = Convert.ToHexString(SHA256.HashData(content)).ToLowerInvariant();
            entries.Add((rel, content.Length, hash));
        }

        entries.Sort((a, b) => string.Compare(a.Path, b.Path, StringComparison.OrdinalIgnoreCase));

        using var ms = new MemoryStream();
        using (var w = new Utf8JsonWriter(ms))
        {
            w.WriteStartObject();
            w.WriteNumber("format", IntegrityCheck.ManifestFormat);
            w.WriteString("version", version);
            w.WriteStartArray("files");
            foreach (var e in entries)
            {
                w.WriteStartObject();
                w.WriteString("path", e.Path);
                w.WriteNumber("size", e.Size);
                w.WriteString("sha256", e.Sha256);
                w.WriteEndObject();
            }
            w.WriteEndArray();
            w.WriteEndObject();
        }
        ms.WriteByte((byte)'\n');
        var manifestBytes = ms.ToArray();

        using var ecdsa = ECDsa.Create(ECCurve.NamedCurves.nistP256);
        var pubPem = ecdsa.ExportSubjectPublicKeyInfoPem();
        var sigBytes = ecdsa.SignData(
            manifestBytes,
            HashAlgorithmName.SHA256,
            DSASignatureFormat.IeeeP1363FixedFieldConcatenation);

        File.WriteAllBytes(Path.Combine(dir, IntegrityCheck.IntegrityJsonName), manifestBytes);
        File.WriteAllBytes(Path.Combine(dir, IntegrityCheck.IntegritySigName), sigBytes);

        return (pubPem, manifestBytes, sigBytes);
    }

    private static string FindGoldenFixtureRoot()
    {
        var cur = new DirectoryInfo(AppContext.BaseDirectory);
        while (cur is not null)
        {
            var candidate = Path.Combine(cur.FullName, "tests", "fixtures", "integrity");
            if (Directory.Exists(candidate)) return candidate;
            cur = cur.Parent;
        }
        throw new DirectoryNotFoundException("Could not locate tests/fixtures/integrity from " + AppContext.BaseDirectory);
    }

    private static void CopyDirectory(string src, string dst)
    {
        Directory.CreateDirectory(dst);
        foreach (var file in Directory.EnumerateFiles(src))
            File.Copy(file, Path.Combine(dst, Path.GetFileName(file)), overwrite: true);
        foreach (var dir in Directory.EnumerateDirectories(src))
            CopyDirectory(dir, Path.Combine(dst, Path.GetFileName(dir)));
    }

    [Fact]
    public void CheckInstall_SkipsWithoutReleaseKey()
    {
        var result = IntegrityCheck.CheckInstall(_tempRoot);
        Assert.Equal(IntegrityOutcome.Skipped, result.Outcome);
    }

    [Fact]
    public void CheckDir_PassesCleanLayout_AndAllowsInstallMarker()
    {
        var dir = Path.Combine(_tempRoot, "clean");
        Directory.CreateDirectory(dir);
        var (pubPem, _, _) = CreateSignedLayout(dir, "0.3.0-dev", [
            ("BetterRecall.Agent.exe", Encoding.UTF8.GetBytes("agent-bytes")),
            (@"worker\BetterRecall.Worker.exe", Encoding.UTF8.GetBytes("worker-bytes")),
            (@"ui\BetterRecall.UI.exe", Encoding.UTF8.GetBytes("ui-bytes")),
        ]);

        Assert.Equal(IntegrityOutcome.Ok, IntegrityCheck.CheckDir(dir, pubPem).Outcome);

        File.WriteAllText(Path.Combine(dir, IntegrityCheck.InstallMarkerName), "0.3.0-dev\nabc\n3\n");
        Assert.Equal(IntegrityOutcome.Ok, IntegrityCheck.CheckDir(dir, pubPem).Outcome);
    }

    [Fact]
    public void CheckDir_DetectsFlippedByte_MissingFile_UnlistedFiles_WrongKey_AndFlippedSig()
    {
        var dir = Path.Combine(_tempRoot, "tamper");
        Directory.CreateDirectory(dir);
        var (pubPem, _, origSig) = CreateSignedLayout(dir, "0.3.0-dev", [
            ("BetterRecall.Agent.exe", Encoding.UTF8.GetBytes("agent-bytes-1234")),
            (@"worker\BetterRecall.Worker.exe", Encoding.UTF8.GetBytes("worker-bytes-5678")),
        ]);

        // Flipped byte -> Changed(path)
        var agentPath = Path.Combine(dir, "BetterRecall.Agent.exe");
        var origAgent = File.ReadAllBytes(agentPath);
        var flipped = (byte[])origAgent.Clone();
        flipped[0] ^= 0x01;
        File.WriteAllBytes(agentPath, flipped);
        var res = IntegrityCheck.CheckDir(dir, pubPem);
        Assert.Equal(IntegrityOutcome.Failed, res.Outcome);
        Assert.Equal(IntegrityErrorKind.Changed, res.ErrorKind);
        Assert.Equal("BetterRecall.Agent.exe", res.OffendingPath);
        File.WriteAllBytes(agentPath, origAgent);

        // Removed file -> Missing(path)
        var workerPath = Path.Combine(dir, @"worker\BetterRecall.Worker.exe");
        var origWorker = File.ReadAllBytes(workerPath);
        File.Delete(workerPath);
        res = IntegrityCheck.CheckDir(dir, pubPem);
        Assert.Equal(IntegrityOutcome.Failed, res.Outcome);
        Assert.Equal(IntegrityErrorKind.Missing, res.ErrorKind);
        Assert.Equal(@"worker\BetterRecall.Worker.exe", res.OffendingPath);
        File.WriteAllBytes(workerPath, origWorker);

        // Unlisted file at root -> Unlisted("evil.dll")
        var evilRoot = Path.Combine(dir, "evil.dll");
        File.WriteAllBytes(evilRoot, [1, 2, 3]);
        res = IntegrityCheck.CheckDir(dir, pubPem);
        Assert.Equal(IntegrityOutcome.Failed, res.Outcome);
        Assert.Equal(IntegrityErrorKind.Unlisted, res.ErrorKind);
        Assert.Equal("evil.dll", res.OffendingPath);
        File.Delete(evilRoot);

        // Unlisted file in subfolder -> Unlisted("worker\\evil.dll")
        var evilSub = Path.Combine(dir, @"worker\evil.dll");
        File.WriteAllBytes(evilSub, [4, 5, 6]);
        res = IntegrityCheck.CheckDir(dir, pubPem);
        Assert.Equal(IntegrityOutcome.Failed, res.Outcome);
        Assert.Equal(IntegrityErrorKind.Unlisted, res.ErrorKind);
        Assert.Equal(@"worker\evil.dll", res.OffendingPath);
        File.Delete(evilSub);

        // Wrong public key -> BadSignature
        using var otherKey = ECDsa.Create(ECCurve.NamedCurves.nistP256);
        var otherPubPem = otherKey.ExportSubjectPublicKeyInfoPem();
        res = IntegrityCheck.CheckDir(dir, otherPubPem);
        Assert.Equal(IntegrityOutcome.Failed, res.Outcome);
        Assert.Equal(IntegrityErrorKind.BadSignature, res.ErrorKind);

        // Flipped signature byte -> BadSignature
        var badSig = (byte[])origSig.Clone();
        badSig[7] ^= 0x40;
        var sigPath = Path.Combine(dir, IntegrityCheck.IntegritySigName);
        File.WriteAllBytes(sigPath, badSig);
        res = IntegrityCheck.CheckDir(dir, pubPem);
        Assert.Equal(IntegrityOutcome.Failed, res.Outcome);
        Assert.Equal(IntegrityErrorKind.BadSignature, res.ErrorKind);
        File.WriteAllBytes(sigPath, origSig);
    }

    [Fact]
    public void GoldenFixture_VerifiesAndDetectsModifiedCopy()
    {
        var fixtureRoot = FindGoldenFixtureRoot();
        var pubPem = File.ReadAllText(Path.Combine(fixtureRoot, "public.pem"));
        var layoutDir = Path.Combine(fixtureRoot, "layout");

        var ok = IntegrityCheck.CheckDir(layoutDir, pubPem);
        Assert.Equal(IntegrityOutcome.Ok, ok.Outcome);

        var copyDir = Path.Combine(_tempRoot, "golden-copy");
        CopyDirectory(layoutDir, copyDir);
        Assert.Equal(IntegrityOutcome.Ok, IntegrityCheck.CheckDir(copyDir, pubPem).Outcome);

        var nested = Path.Combine(copyDir, @"sub\nested.txt");
        var bytes = File.ReadAllBytes(nested);
        bytes[0] ^= 0x01;
        File.WriteAllBytes(nested, bytes);

        var tampered = IntegrityCheck.CheckDir(copyDir, pubPem);
        Assert.Equal(IntegrityOutcome.Failed, tampered.Outcome);
        Assert.Equal(IntegrityErrorKind.Changed, tampered.ErrorKind);
        Assert.Equal(@"sub\nested.txt", tampered.OffendingPath);
    }
}
