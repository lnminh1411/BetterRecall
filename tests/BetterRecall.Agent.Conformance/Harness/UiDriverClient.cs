// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Conformance.Harness;

internal sealed class UiDriverClient : IAsyncDisposable
{
    private readonly Process _proc;
    private readonly StreamWriter _stdin;
    private readonly StreamReader _stdout;
    private readonly SemaphoreSlim _rpcGate = new(1, 1);
    private long _nextId;

    public UiDriverClient(string uiExePath)
    {
        var psi = new ProcessStartInfo(uiExePath)
        {
            UseShellExecute = false,
            RedirectStandardInput = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            CreateNoWindow = true,
            WorkingDirectory = Path.GetDirectoryName(uiExePath)!,
            StandardInputEncoding = new UTF8Encoding(false),
            StandardOutputEncoding = new UTF8Encoding(false),
        };
        _proc = Process.Start(psi) ?? throw new InvalidOperationException("Failed to start UiDriver.");
        _stdin = _proc.StandardInput;
        _stdin.NewLine = "\n";
        _stdout = _proc.StandardOutput;
    }

    public async Task<(bool Ok, int ServerPid, string? ErrorKind)> ConnectAsync(string conn, string home, int timeoutMs = 1000)
    {
        using var doc = await CallAsync(new JsonObject
        {
            ["op"] = "connect",
            ["conn"] = conn,
            ["home"] = home,
            ["timeoutMs"] = timeoutMs,
        }, timeoutMs + 5000).ConfigureAwait(false);
        var root = doc.RootElement;
        bool ok = root.GetProperty("ok").GetBoolean();
        int pid = ok && root.TryGetProperty("serverPid", out var p) ? p.GetInt32() : 0;
        string? errKind = !ok && root.TryGetProperty("errorKind", out var e) ? e.GetString() : null;
        return (ok, pid, errKind);
    }

    public async Task<int> WaitForConnectAsync(string conn, string home, TimeSpan timeout)
    {
        var sw = Stopwatch.StartNew();
        while (true)
        {
            var (ok, pid, errKind) = await ConnectAsync(conn, home, 1000).ConfigureAwait(false);
            if (ok) return pid;
            if (errKind == "NotTrusted")
                throw new InvalidOperationException("UI pipe trust check failed against agent exe.");
            if (sw.Elapsed >= timeout)
                throw new TimeoutException($"UI pipe did not accept a connection within {timeout.TotalSeconds:F1} s (last errorKind={errKind}).");
            await Task.Delay(50).ConfigureAwait(false);
        }
    }

    public async Task<(bool Eof, string? Line)> RequestRawLineAsync(string conn, string line, int timeoutMs = 15000)
    {
        using var doc = await CallAsync(new JsonObject
        {
            ["op"] = "request",
            ["conn"] = conn,
            ["line"] = line,
            ["timeoutMs"] = timeoutMs,
        }, timeoutMs + 5000).ConfigureAwait(false);
        var root = doc.RootElement;
        if (!root.GetProperty("ok").GetBoolean())
            throw new InvalidOperationException("UiDriver request failed: " + root.GetProperty("error").GetString());
        if (root.TryGetProperty("eof", out var eof) && eof.GetBoolean())
            return (true, null);
        return (false, root.GetProperty("line").GetString());
    }

    public async Task<(bool Ok, JsonElement Data, string? Error)> RequestAsync(string conn, string line, int timeoutMs = 15000)
    {
        var (eof, resp) = await RequestRawLineAsync(conn, line, timeoutMs).ConfigureAwait(false);
        Assert.False(eof, $"Unexpected EOF on UI pipe for command: {line}");
        Assert.NotNull(resp);
        return Envelope.Parse(resp!);
    }

    public async Task<string?[]> ParallelRequestAsync(string[] conns, string line, int timeoutMs = 15000)
    {
        var arr = new JsonArray();
        foreach (var c in conns) arr.Add(c);
        using var doc = await CallAsync(new JsonObject
        {
            ["op"] = "parallelRequest",
            ["conns"] = arr,
            ["line"] = line,
            ["timeoutMs"] = timeoutMs,
        }, timeoutMs + 5000).ConfigureAwait(false);
        var root = doc.RootElement;
        if (!root.GetProperty("ok").GetBoolean())
            throw new InvalidOperationException("UiDriver parallelRequest failed: " + root.GetProperty("error").GetString());
        return root.GetProperty("lines").EnumerateArray().Select(e => e.ValueKind == JsonValueKind.Null ? null : e.GetString()).ToArray();
    }

    public async Task<(bool Ok, int ServerPid)> RawOpenAsync(string conn, string home, int timeoutMs = 1000)
    {
        using var doc = await CallAsync(new JsonObject
        {
            ["op"] = "rawOpen",
            ["conn"] = conn,
            ["home"] = home,
            ["timeoutMs"] = timeoutMs,
        }, timeoutMs + 5000).ConfigureAwait(false);
        var root = doc.RootElement;
        bool ok = root.GetProperty("ok").GetBoolean();
        int pid = ok && root.TryGetProperty("serverPid", out var p) ? p.GetInt32() : 0;
        return (ok, pid);
    }

    public async Task RawWriteAsync(string conn, byte[] bytes)
    {
        using var doc = await CallAsync(new JsonObject
        {
            ["op"] = "rawWrite",
            ["conn"] = conn,
            ["base64"] = Convert.ToBase64String(bytes),
        }, 10000).ConfigureAwait(false);
        if (!doc.RootElement.GetProperty("ok").GetBoolean())
            throw new InvalidOperationException("UiDriver rawWrite failed: " + doc.RootElement.GetProperty("error").GetString());
    }

    public async Task<(bool Eof, string? Line)> RawReadLineAsync(string conn, int maxBytes = 1048576, int timeoutMs = 5000)
    {
        using var doc = await CallAsync(new JsonObject
        {
            ["op"] = "rawReadLine",
            ["conn"] = conn,
            ["maxBytes"] = maxBytes,
            ["timeoutMs"] = timeoutMs,
        }, timeoutMs + 5000).ConfigureAwait(false);
        var root = doc.RootElement;
        if (!root.GetProperty("ok").GetBoolean())
            throw new InvalidOperationException("UiDriver rawReadLine failed: " + root.GetProperty("error").GetString());
        if (root.TryGetProperty("eof", out var eof) && eof.GetBoolean())
            return (true, null);
        return (false, root.GetProperty("line").GetString());
    }

    public async Task CloseAsync(string conn)
    {
        using var _ = await CallAsync(new JsonObject { ["op"] = "close", ["conn"] = conn }, 5000).ConfigureAwait(false);
    }

    private async Task<JsonDocument> CallAsync(JsonObject cmd, int timeoutMs)
    {
        await _rpcGate.WaitAsync().ConfigureAwait(false);
        try
        {
            cmd["id"] = Interlocked.Increment(ref _nextId);
            await _stdin.WriteLineAsync(cmd.ToJsonString()).ConfigureAwait(false);
            await _stdin.FlushAsync().ConfigureAwait(false);
            using var cts = new CancellationTokenSource(timeoutMs);
            var line = await _stdout.ReadLineAsync(cts.Token).ConfigureAwait(false);
            if (line is null)
            {
                var stderr = await _proc.StandardError.ReadToEndAsync().ConfigureAwait(false);
                throw new EndOfStreamException($"UiDriver exited unexpectedly (exit={(_proc.HasExited ? _proc.ExitCode : -1)}): {stderr}");
            }
            return JsonDocument.Parse(line);
        }
        finally
        {
            _rpcGate.Release();
        }
    }

    public async ValueTask DisposeAsync()
    {
        try
        {
            if (!_proc.HasExited)
            {
                using var _ = await CallAsync(new JsonObject { ["op"] = "quit" }, 2000).ConfigureAwait(false);
                _stdin.Close();
                if (!_proc.WaitForExit(3000)) _proc.Kill();
            }
        }
        catch
        {
            try { if (!_proc.HasExited) _proc.Kill(); } catch { }
        }
        finally
        {
            _proc.Dispose();
            _rpcGate.Dispose();
        }
    }
}
