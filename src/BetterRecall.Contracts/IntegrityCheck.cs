// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace BetterRecall.Contracts;

/// <summary>Outcome of <see cref="IntegrityCheck.CheckInstall"/> or <see cref="IntegrityCheck.CheckDir"/>.</summary>
public enum IntegrityOutcome
{
    /// <summary>Built without <c>BetterRecallReleasePubKey</c> (dev and test builds).</summary>
    Skipped,
    /// <summary>Signature, manifest, every listed file, and the unlisted-file walk passed.</summary>
    Ok,
    /// <summary>Integrity verification failed.</summary>
    Failed,
}

/// <summary>Failure category matching Rust <c>br_integrity::IntegrityError</c>.</summary>
public enum IntegrityErrorKind
{
    Missing,
    Changed,
    Unlisted,
    BadSignature,
    BadManifest,
    Io,
}

/// <summary>Structured result from an integrity check.</summary>
public sealed record IntegrityResult(
    IntegrityOutcome Outcome,
    IntegrityErrorKind? ErrorKind = null,
    string? OffendingPath = null,
    string? Message = null)
{
    public static IntegrityResult Skipped { get; } = new(IntegrityOutcome.Skipped);
    public static IntegrityResult Ok { get; } = new(IntegrityOutcome.Ok);

    public static IntegrityResult Fail(IntegrityErrorKind kind, string offendingPath, string message)
        => new(IntegrityOutcome.Failed, kind, offendingPath, message);
}

/// <summary>
/// Install-folder integrity checker (R9, stage I7).
/// In release builds (<c>BR_RELEASE</c>, set when <c>BetterRecallReleasePubKey</c> is provided to MSBuild),
/// verifies <c>integrity.sig</c> (64-byte IEEE P1363 ECDSA P-256 / SHA-256) over <c>integrity.json</c>,
/// checks every listed file's size and streamed SHA-256, and rejects unlisted files under the install root.
/// Without <c>BR_RELEASE</c>, <see cref="CheckInstall"/> returns <see cref="IntegrityResult.Skipped"/>.
/// </summary>
public static partial class IntegrityCheck
{
    public const string IntegrityJsonName = "integrity.json";
    public const string IntegritySigName = "integrity.sig";
    public const string InstallMarkerName = ".install";
    public const int ManifestFormat = 1;
    public const int SignatureLength = 64;
    public const int ExitCode = 86;

    private const int HashBufferSize = 1024 * 1024;
    private const string EmbeddedKeyResourceName = "BetterRecall.Contracts.release_pubkey.pem";
    private const string NistP256Oid = "1.2.840.10045.3.1.7";

    private sealed record ManifestEntry(string Path, long Size, string Sha256);

    /// <summary>Checks <paramref name="root"/> using the compile-time embedded release key, or skips in dev builds.</summary>
    public static IntegrityResult CheckInstall(string root)
    {
#if BR_RELEASE
        var pem = ReadEmbeddedPublicKey();
        if (string.IsNullOrWhiteSpace(pem))
            return IntegrityResult.Fail(IntegrityErrorKind.BadSignature, IntegritySigName, "embedded release public key is missing");
        return CheckDir(root, pem);
#else
        _ = root;
        return IntegrityResult.Skipped;
#endif
    }

    /// <summary>
    /// Verifies <c>integrity.sig</c>, parses and validates <c>integrity.json</c>, checks sizes of all listed files,
    /// verifies streamed SHA-256 hashes (1 MiB buffer), and rejects any unlisted file under <paramref name="root"/>.
    /// </summary>
    public static IntegrityResult CheckDir(string root, string publicKeyPem)
    {
        var manifestRead = ReadRequiredFile(Path.Combine(root, IntegrityJsonName), IntegrityJsonName);
        if (manifestRead.Error is not null) return manifestRead.Error;
        var manifestBytes = manifestRead.Bytes!;

        var sigRead = ReadRequiredFile(Path.Combine(root, IntegritySigName), IntegritySigName);
        if (sigRead.Error is not null) return sigRead.Error;
        var sigBytes = sigRead.Bytes!;

        var sigErr = VerifySignature(manifestBytes, sigBytes, publicKeyPem);
        if (sigErr is not null) return sigErr;

        var parse = ParseAndValidateManifest(manifestBytes);
        if (parse.Error is not null) return parse.Error;
        var entries = parse.Entries!;

        // Pass 1: check existence and sizes first so missing or resized files fail fast.
        foreach (var entry in entries)
        {
            var full = Path.Combine(root, entry.Path);
            try
            {
                if (!File.Exists(full))
                    return IntegrityResult.Fail(IntegrityErrorKind.Missing, entry.Path, $"missing file: {entry.Path}");
                var info = new FileInfo(full);
                if (info.Length != entry.Size)
                    return IntegrityResult.Fail(IntegrityErrorKind.Changed, entry.Path, $"changed file: {entry.Path}");
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
            {
                return IntegrityResult.Fail(IntegrityErrorKind.Io, entry.Path, $"I/O error on {entry.Path}: {ex.Message}");
            }
        }

        // Pass 2: streamed SHA-256 with a 1 MiB buffer.
        var buffer = new byte[HashBufferSize];
        foreach (var entry in entries)
        {
            var full = Path.Combine(root, entry.Path);
            string actual;
            try
            {
                actual = HashFileHexLower(full, buffer);
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
            {
                return IntegrityResult.Fail(IntegrityErrorKind.Io, entry.Path, $"I/O error on {entry.Path}: {ex.Message}");
            }
            if (!string.Equals(actual, entry.Sha256, StringComparison.Ordinal))
                return IntegrityResult.Fail(IntegrityErrorKind.Changed, entry.Path, $"changed file: {entry.Path}");
        }

        // Pass 3: walk the install root and reject any unlisted file.
        var listed = new HashSet<string>(entries.Count, AsciiIgnoreCaseComparer.Instance);
        foreach (var entry in entries) listed.Add(entry.Path);

        return CheckUnlisted(root, "", listed);
    }

    /// <summary>
    /// Enforces install integrity at program start. On failure in a release build: appends one line to
    /// <c>&lt;home&gt;\logs\integrity.log</c> (ignoring log errors), shows one <c>MessageBoxW</c> if
    /// <paramref name="gui"/> is true or writes to stderr otherwise, and exits with code 86.
    /// </summary>
    public static void EnforceOrExit(string root, bool gui)
    {
        var result = CheckInstall(root);
        if (result.Outcome != IntegrityOutcome.Failed) return;

        var offending = result.OffendingPath ?? IntegrityJsonName;
        var msg = $"BetterRecall's files were changed or are damaged ({offending}). Reinstall BetterRecall.";
        AppendIntegrityLog(root, gui, result);

        if (gui)
        {
            MessageBoxW(0, msg, "BetterRecall", MB_OK | MB_ICONERROR);
        }
        else
        {
            try { Console.Error.WriteLine(msg); } catch (IOException) { }
        }
        Environment.Exit(ExitCode);
    }

    private static IntegrityResult? VerifySignature(byte[] manifestBytes, byte[] sigBytes, string publicKeyPem)
    {
        if (sigBytes.Length != SignatureLength)
            return IntegrityResult.Fail(IntegrityErrorKind.BadSignature, IntegritySigName,
                $"bad signature: expected {SignatureLength} bytes, got {sigBytes.Length}");

        try
        {
            using var ecdsa = ECDsa.Create();
            ecdsa.ImportFromPem(publicKeyPem.AsSpan().Trim());
            if (ecdsa.KeySize != 256)
                return IntegrityResult.Fail(IntegrityErrorKind.BadSignature, IntegritySigName, "bad signature: key is not P-256");

            var parameters = ecdsa.ExportParameters(includePrivateParameters: false);
            if (!string.Equals(parameters.Curve.Oid?.Value, NistP256Oid, StringComparison.Ordinal))
                return IntegrityResult.Fail(IntegrityErrorKind.BadSignature, IntegritySigName, "bad signature: key curve is not NIST P-256");

            bool ok = ecdsa.VerifyData(
                manifestBytes,
                sigBytes,
                HashAlgorithmName.SHA256,
                DSASignatureFormat.IeeeP1363FixedFieldConcatenation);
            return ok
                ? null
                : IntegrityResult.Fail(IntegrityErrorKind.BadSignature, IntegritySigName, "bad signature: signature verification failed");
        }
        catch (Exception ex) when (ex is CryptographicException or ArgumentException)
        {
            return IntegrityResult.Fail(IntegrityErrorKind.BadSignature, IntegritySigName, $"bad signature: {ex.Message}");
        }
    }

    private static (List<ManifestEntry>? Entries, IntegrityResult? Error) ParseAndValidateManifest(byte[] bytes)
    {
        if (bytes.Length >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF)
            return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, "bad manifest: UTF-8 BOM is not allowed"));

        JsonDocument doc;
        try
        {
            doc = JsonDocument.Parse(bytes);
        }
        catch (JsonException ex)
        {
            return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, $"bad manifest: invalid JSON ({ex.Message})"));
        }

        using (doc)
        {
            var root = doc.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
                return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, "bad manifest: root must be a JSON object"));

            int? format = null;
            string? version = null;
            JsonElement? filesEl = null;

            foreach (var prop in root.EnumerateObject())
            {
                switch (prop.Name)
                {
                    case "format":
                        if (prop.Value.ValueKind != JsonValueKind.Number || !prop.Value.TryGetInt32(out var f))
                            return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, "bad manifest: format must be an integer"));
                        format = f;
                        break;
                    case "version":
                        if (prop.Value.ValueKind != JsonValueKind.String)
                            return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, "bad manifest: version must be a string"));
                        version = prop.Value.GetString();
                        break;
                    case "files":
                        if (prop.Value.ValueKind != JsonValueKind.Array)
                            return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, "bad manifest: files must be an array"));
                        filesEl = prop.Value;
                        break;
                    default:
                        return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, $"bad manifest: unknown field '{prop.Name}'"));
                }
            }

            if (format != ManifestFormat)
                return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, $"bad manifest: unsupported format {format}"));
            if (string.IsNullOrWhiteSpace(version))
                return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, "bad manifest: version must not be empty"));
            if (filesEl is null)
                return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, "bad manifest: missing files array"));

            var entries = new List<ManifestEntry>();
            string? prevPath = null;

            foreach (var item in filesEl.Value.EnumerateArray())
            {
                if (item.ValueKind != JsonValueKind.Object)
                    return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, "bad manifest: file entry must be an object"));

                string? path = null;
                long? size = null;
                string? sha256 = null;

                foreach (var prop in item.EnumerateObject())
                {
                    switch (prop.Name)
                    {
                        case "path":
                            if (prop.Value.ValueKind != JsonValueKind.String)
                                return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, "bad manifest: path must be a string"));
                            path = prop.Value.GetString();
                            break;
                        case "size":
                            if (prop.Value.ValueKind != JsonValueKind.Number || !prop.Value.TryGetInt64(out var s) || s < 0)
                                return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, "bad manifest: size must be a non-negative integer"));
                            size = s;
                            break;
                        case "sha256":
                            if (prop.Value.ValueKind != JsonValueKind.String)
                                return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, "bad manifest: sha256 must be a string"));
                            sha256 = prop.Value.GetString();
                            break;
                        default:
                            return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, $"bad manifest: unknown entry field '{prop.Name}'"));
                    }
                }

                if (path is null || size is null || sha256 is null)
                    return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, "bad manifest: file entry missing required field"));
                if (!IsValidRelativePath(path))
                    return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, $"bad manifest: invalid relative path '{path}'"));
                if (IsAllowedRootFile(path))
                    return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, $"bad manifest: excluded file '{path}' must not be listed"));
                if (!IsValidSha256Hex(sha256))
                    return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, $"bad manifest: invalid sha256 for '{path}'"));

                if (prevPath is not null)
                {
                    int cmp = AsciiIgnoreCaseComparer.Instance.Compare(prevPath, path);
                    if (cmp == 0)
                        return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, $"bad manifest: duplicate path '{path}'"));
                    if (cmp > 0)
                        return (null, IntegrityResult.Fail(IntegrityErrorKind.BadManifest, IntegrityJsonName, $"bad manifest: paths not sorted ('{path}' after '{prevPath}')"));
                }
                prevPath = path;
                entries.Add(new ManifestEntry(path, size.Value, sha256));
            }

            return (entries, null);
        }
    }

    private static IntegrityResult CheckUnlisted(string root, string prefix, HashSet<string> listed)
    {
        var dir = prefix.Length == 0 ? root : Path.Combine(root, prefix);
        List<FileSystemInfo> children;
        try
        {
            children = [.. new DirectoryInfo(dir).EnumerateFileSystemInfos()];
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            var label = prefix.Length == 0 ? "." : prefix;
            return IntegrityResult.Fail(IntegrityErrorKind.Io, label, $"I/O error on {label}: {ex.Message}");
        }

        children.Sort((a, b) => AsciiIgnoreCaseComparer.Instance.Compare(a.Name, b.Name));

        foreach (var child in children)
        {
            var rel = prefix.Length == 0 ? child.Name : prefix + "\\" + child.Name;
            if ((child.Attributes & FileAttributes.Directory) != 0)
            {
                var sub = CheckUnlisted(root, rel, listed);
                if (sub.Outcome != IntegrityOutcome.Ok) return sub;
            }
            else
            {
                if (prefix.Length == 0 && IsAllowedRootFile(rel)) continue;
                if (!listed.Contains(rel))
                    return IntegrityResult.Fail(IntegrityErrorKind.Unlisted, rel, $"unlisted file: {rel}");
            }
        }

        return IntegrityResult.Ok;
    }

    private static (byte[]? Bytes, IntegrityResult? Error) ReadRequiredFile(string path, string label)
    {
        try
        {
            return (File.ReadAllBytes(path), null);
        }
        catch (Exception ex) when (ex is FileNotFoundException or DirectoryNotFoundException)
        {
            return (null, IntegrityResult.Fail(IntegrityErrorKind.Missing, label, $"missing file: {label}"));
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            return (null, IntegrityResult.Fail(IntegrityErrorKind.Io, label, $"I/O error on {label}: {ex.Message}"));
        }
    }

    private static string HashFileHexLower(string fullPath, byte[] buffer)
    {
        using var hasher = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        using var fs = new FileStream(fullPath, FileMode.Open, FileAccess.Read, FileShare.Read, buffer.Length, FileOptions.SequentialScan);
        int read;
        while ((read = fs.Read(buffer, 0, buffer.Length)) > 0)
            hasher.AppendData(buffer, 0, read);
        return Convert.ToHexString(hasher.GetHashAndReset()).ToLowerInvariant();
    }

    private static bool IsAllowedRootFile(string rel)
        => string.Equals(rel, IntegrityJsonName, StringComparison.OrdinalIgnoreCase)
        || string.Equals(rel, IntegritySigName, StringComparison.OrdinalIgnoreCase)
        || string.Equals(rel, InstallMarkerName, StringComparison.OrdinalIgnoreCase);

    private static bool IsValidRelativePath(string path)
    {
        if (path.Length == 0 || path.Contains('\0') || path.Contains('/') || path.Contains(':')) return false;
        if (path[0] == '\\' || path[^1] == '\\') return false;
        foreach (var seg in path.Split('\\'))
            if (seg.Length == 0 || seg == "." || seg == "..") return false;
        return true;
    }

    private static bool IsValidSha256Hex(string s)
    {
        if (s.Length != 64) return false;
        foreach (var c in s)
            if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) return false;
        return true;
    }

#if BR_RELEASE
    private static string? ReadEmbeddedPublicKey()
    {
        using var stream = typeof(IntegrityCheck).Assembly.GetManifestResourceStream(EmbeddedKeyResourceName);
        if (stream is null) return null;
        using var reader = new StreamReader(stream, Encoding.UTF8, detectEncodingFromByteOrderMarks: true);
        return reader.ReadToEnd();
    }
#endif

    private static void AppendIntegrityLog(string root, bool gui, IntegrityResult result)
    {
        try
        {
            var logsDir = DataPaths.LogsDir(DataPaths.Home());
            Directory.CreateDirectory(logsDir);
            var logPath = Path.Combine(logsDir, "integrity.log");
            var ts = DateTimeOffset.UtcNow.ToString("yyyy-MM-dd'T'HH:mm:ss.fff'Z'", CultureInfo.InvariantCulture);
            var kind = gui ? "gui" : "headless";
            File.AppendAllText(logPath, $"{ts} [{kind}] root={root} error={result.Message}\n", new UTF8Encoding(false));
        }
        catch
        {
            // Ignore log errors so integrity enforcement always completes.
        }
    }

    private const uint MB_OK = 0x00000000;
    private const uint MB_ICONERROR = 0x00000010;

    [LibraryImport("user32.dll", StringMarshalling = StringMarshalling.Utf16)]
    private static partial int MessageBoxW(nint hwnd, string text, string caption, uint type);
}

/// <summary>
/// Path order and equality shared with the Rust checker (<c>br-integrity</c> <c>cmp_ordinal_ignore_case</c>):
/// Unicode scalar values compared after ASCII-only lower-casing. <see cref="StringComparison.OrdinalIgnoreCase"/>
/// upper-cases instead, so it orders <c>[ \ ] ^ _</c> and the backtick differently from letters (sweep 7 F2).
/// </summary>
public sealed class AsciiIgnoreCaseComparer : IComparer<string>, IEqualityComparer<string>
{
    public static readonly AsciiIgnoreCaseComparer Instance = new();

    private AsciiIgnoreCaseComparer()
    {
    }

    public int Compare(string? x, string? y)
    {
        if (ReferenceEquals(x, y)) return 0;
        if (x is null) return -1;
        if (y is null) return 1;
        var ex = x.EnumerateRunes();
        var ey = y.EnumerateRunes();
        while (true)
        {
            bool hx = ex.MoveNext();
            bool hy = ey.MoveNext();
            if (!hx) return hy ? -1 : 0;
            if (!hy) return 1;
            int cx = Lower(ex.Current.Value);
            int cy = Lower(ey.Current.Value);
            if (cx != cy) return cx < cy ? -1 : 1;
        }
    }

    public bool Equals(string? x, string? y) => Compare(x, y) == 0;

    public int GetHashCode(string obj)
    {
        var h = new HashCode();
        foreach (var r in obj.EnumerateRunes()) h.Add(Lower(r.Value));
        return h.ToHashCode();
    }

    private static int Lower(int c) => c is >= 'A' and <= 'Z' ? c + 32 : c;
}
