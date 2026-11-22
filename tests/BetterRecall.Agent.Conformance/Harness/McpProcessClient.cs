// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Text;
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Agent.Conformance.Harness;

internal sealed class McpProcessClient : IAsyncDisposable
{
    private readonly Process _proc;
    private readonly StreamWriter _stdin;
    private readonly StreamReader _stdout;
    private int _nextId;

    public McpProcessClient(string mcpExePath, string home, params string[] extraArgs)
    {
        var utf8 = new UTF8Encoding(false);
        var psi = new ProcessStartInfo(mcpExePath)
        {
            UseShellExecute = false,
            RedirectStandardInput = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            CreateNoWindow = true,
            WorkingDirectory = Path.GetDirectoryName(mcpExePath)!,
            StandardInputEncoding = utf8,
            StandardOutputEncoding = utf8,
        };
        foreach (var a in extraArgs) psi.ArgumentList.Add(a);
        psi.Environment[DataPaths.HomeEnv] = home;
        _proc = Process.Start(psi) ?? throw new InvalidOperationException("Failed to start betterrecall-mcp.exe.");
        _stdin = _proc.StandardInput;
        _stdin.NewLine = "\n";
        _stdout = _proc.StandardOutput;
    }

    public async Task<JsonElement> InitializeAsync(string protocolVersion = "2025-11-25")
    {
        var init = await RequestAsync("initialize", new
        {
            protocolVersion,
            capabilities = new { },
            clientInfo = new { name = "conformance", version = "1.0" },
        }).ConfigureAwait(false);
        await NotifyAsync("notifications/initialized").ConfigureAwait(false);
        return init;
    }

    public async Task NotifyAsync(string method, object? @params = null)
    {
        var line = @params is null
            ? JsonSerializer.Serialize(new { jsonrpc = "2.0", method })
            : JsonSerializer.Serialize(new { jsonrpc = "2.0", method, @params });
        await _stdin.WriteLineAsync(line).ConfigureAwait(false);
        await _stdin.FlushAsync().ConfigureAwait(false);
    }

    public async Task<JsonElement> RequestAsync(string method, object? @params = null, int timeoutMs = 15_000)
    {
        int id = Interlocked.Increment(ref _nextId);
        var line = @params is null
            ? JsonSerializer.Serialize(new { jsonrpc = "2.0", id, method })
            : JsonSerializer.Serialize(new { jsonrpc = "2.0", id, method, @params });
        await _stdin.WriteLineAsync(line).ConfigureAwait(false);
        await _stdin.FlushAsync().ConfigureAwait(false);

        using var cts = new CancellationTokenSource(timeoutMs);
        var reply = await _stdout.ReadLineAsync(cts.Token).ConfigureAwait(false);
        if (reply is null)
        {
            var err = await _proc.StandardError.ReadToEndAsync().ConfigureAwait(false);
            throw new EndOfStreamException($"betterrecall-mcp exited unexpectedly: {err}");
        }
        using var doc = JsonDocument.Parse(reply);
        return doc.RootElement.Clone();
    }

    public async ValueTask DisposeAsync()
    {
        try
        {
            if (!_proc.HasExited)
            {
                _stdin.Close();
                using var cts = new CancellationTokenSource(5000);
                try { await _proc.WaitForExitAsync(cts.Token).ConfigureAwait(false); }
                catch (OperationCanceledException) { _proc.Kill(); }
            }
        }
        catch
        {
            try { if (!_proc.HasExited) _proc.Kill(); } catch { }
        }
        finally
        {
            _proc.Dispose();
        }
    }
}
