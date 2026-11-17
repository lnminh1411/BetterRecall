// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Stub;

/// <summary>One MCP server process the relay talks to.</summary>
public interface IMcpChild : IDisposable
{
    /// <summary>The server's stdin.</summary>
    Stream Input { get; }

    /// <summary>The server's stdout.</summary>
    Stream Output { get; }

    Task<int> Exited { get; }
}

/// <summary>
/// bin\betterrecall-mcp.exe (B5/C4, D6): relays MCP stdio between the MCP client and the MCP server of the install that
/// bin\current names, and follows launcher upgrades within one session. A server exits with McpRestartExitCode once
/// the agent it trusts is gone (the agent now runs from another install); the relay then starts the current install's
/// server and sends it every request the old one left unanswered. While idle (no request in flight) it also moves to
/// a new current install on its own, so the old server stops holding the old install folder. Any other exit ends the
/// relay with that exit code, as before. The server keeps no session state a tool call needs (McpServer), so the new
/// one answers without the client's initialize.
/// </summary>
public sealed class McpRelay
{
    private const int MaxRestarts = 5;   // per minute; then the relay gives up and ends
    private static readonly TimeSpan RestartWindow = TimeSpan.FromMinutes(1);

    private readonly Stream _clientIn, _clientOut;
    private readonly Func<string?> _current;
    private readonly Func<string, IMcpChild> _start;
    private readonly TimeSpan _poll, _waitForCurrent;
    private readonly object _gate = new();          // the child and its stdin
    private readonly object _pendingGate = new();   // _pending only (the child pump takes it while _gate may be held)
    private readonly List<Pending> _pending = [];
    private readonly SemaphoreSlim _out = new(1, 1);
    private IMcpChild? _child;
    private string? _install;
    private bool _swapping;
    private volatile bool _clientEnded;

    private sealed record Pending(string Key, string RawId, byte[] Line);

    /// <param name="currentInstall">The install folder bin\current names now, or null.</param>
    /// <param name="start">Starts the MCP server of an install folder.</param>
    public McpRelay(Stream clientIn, Stream clientOut, Func<string?> currentInstall, Func<string, IMcpChild> start,
        TimeSpan? poll = null, TimeSpan? waitForCurrent = null)
    {
        _clientIn = clientIn;
        _clientOut = clientOut;
        _current = currentInstall;
        _start = start;
        _poll = poll ?? TimeSpan.FromSeconds(2);
        _waitForCurrent = waitForCurrent ?? TimeSpan.FromSeconds(20);
    }

    /// <summary>Servers started after the first one.</summary>
    public int Restarts { get; private set; }

    public async Task<int> RunAsync()
    {
        var install = _current() ?? throw new InvalidOperationException("no current install");
        lock (_gate)
        {
            _child = _start(install);
            _install = install;
        }
        _ = Task.Run(ReadClientAsync);
        var restarts = new Queue<DateTime>();
        while (true)
        {
            IMcpChild child;
            lock (_gate) child = _child!;
            var pump = Task.Run(() => PumpChildAsync(child));
            int code;
            while (true)
            {
                var exited = child.Exited;
                if (await Task.WhenAny(exited, Task.Delay(_poll)).ConfigureAwait(false) == exited) { code = await exited.ConfigureAwait(false); break; }
                MaybeSwap(child);
            }
            // Every answer the old server wrote is delivered (and leaves _pending) before anything is sent again.
            await Task.WhenAny(pump, Task.Delay(TimeSpan.FromSeconds(5))).ConfigureAwait(false);
            bool swapped;
            string? old;
            lock (_gate) { swapped = _swapping; old = _install; }
            if (_clientEnded || (code != InstallLayout.McpRestartExitCode && !swapped))
            {
                child.Dispose();
                return code;
            }

            var now = DateTime.UtcNow;
            while (restarts.Count > 0 && now - restarts.Peek() > RestartWindow) restarts.Dequeue();
            string? next = swapped ? _current() : await WaitForNewInstallAsync(old).ConfigureAwait(false);
            if (restarts.Count >= MaxRestarts || next is null)
            {
                await FailPendingAsync("BetterRecall was updated while this MCP session ran: restart your MCP client to reconnect.").ConfigureAwait(false);
                child.Dispose();
                return code;
            }
            try
            {
                lock (_gate)
                {
                    _child = _start(next);
                    _install = next;
                    _swapping = false;
                    List<Pending> again;
                    lock (_pendingGate) again = [.. _pending];
                    foreach (var p in again) Send(_child, p.Line);
                }
            }
            catch (Exception e) when (e is IOException or InvalidOperationException or System.ComponentModel.Win32Exception)
            {
                await FailPendingAsync("BetterRecall could not restart its MCP server: restart your MCP client.").ConfigureAwait(false);
                child.Dispose();
                return 1;
            }
            child.Dispose();
            restarts.Enqueue(now);
            Restarts++;
        }
    }

    /// <summary>A server that exited as stale waits for the launcher to point bin\current at the new install.</summary>
    private async Task<string?> WaitForNewInstallAsync(string? old)
    {
        var deadline = DateTime.UtcNow + _waitForCurrent;
        while (true)
        {
            var cur = _current();
            if (cur is not null && !SameFolder(cur, old)) return cur;
            if (DateTime.UtcNow >= deadline) return null;
            await Task.Delay(250).ConfigureAwait(false);
        }
    }

    /// <summary>Idle and bin\current moved on: end the old server (end of its input) so the main loop starts the new one.</summary>
    private void MaybeSwap(IMcpChild child)
    {
        if (_clientEnded) return;
        var cur = _current();
        lock (_gate)
        {
            if (_swapping || cur is null || SameFolder(cur, _install) || !ReferenceEquals(child, _child)) return;
            lock (_pendingGate) if (_pending.Count > 0) return;
            _swapping = true;
            try { child.Input.Dispose(); } catch (IOException) { }
        }
    }

    private static bool SameFolder(string? a, string? b)
        => a is not null && b is not null && string.Equals(Path.GetFileName(a.TrimEnd('\\')), Path.GetFileName(b.TrimEnd('\\')), StringComparison.OrdinalIgnoreCase);

    private async Task ReadClientAsync()
    {
        var lines = new LineSplitter(_clientIn);
        while (await lines.ReadAsync().ConfigureAwait(false) is { } line)
        {
            var id = MessageId(line, request: true);
            lock (_gate)
            {
                // Recorded and sent under one lock: a restart either resends it or comes after it, never both.
                if (id is { } i) lock (_pendingGate) _pending.Add(new Pending(i.Key, i.Raw, line));
                if (_child is { } c) Send(c, line);
            }
        }
        _clientEnded = true;
        lock (_gate)
        {
            try { _child?.Input.Dispose(); } catch (IOException) { }
        }
    }

    /// <summary>Writes a line to a server. A server that is gone or ending loses it; a request stays pending and is
    /// sent again to the next server.</summary>
    private static void Send(IMcpChild child, byte[] line)
    {
        try
        {
            child.Input.Write(line);
            child.Input.Flush();
        }
        catch (Exception e) when (e is IOException or ObjectDisposedException or InvalidOperationException) { }
    }

    private async Task PumpChildAsync(IMcpChild child)
    {
        var lines = new LineSplitter(child.Output);
        while (await lines.ReadAsync().ConfigureAwait(false) is { } line)
        {
            if (MessageId(line, request: false) is { } id)
                lock (_pendingGate) _pending.RemoveAll(p => p.Key == id.Key);
            await WriteClientAsync(line).ConfigureAwait(false);
        }
    }

    private async Task WriteClientAsync(byte[] line)
    {
        await _out.WaitAsync().ConfigureAwait(false);
        try
        {
            await _clientOut.WriteAsync(line).ConfigureAwait(false);
            await _clientOut.FlushAsync().ConfigureAwait(false);
        }
        catch (Exception e) when (e is IOException or ObjectDisposedException) { }
        finally { _out.Release(); }
    }

    /// <summary>Answers every pending request with a JSON-RPC error, so the client does not wait on a server that is gone.</summary>
    private async Task FailPendingAsync(string message)
    {
        List<Pending> left;
        lock (_pendingGate) { left = [.. _pending]; _pending.Clear(); }
        foreach (var p in left)
        {
            var buf = new MemoryStream();
            using (var w = new Utf8JsonWriter(buf))
            {
                w.WriteStartObject();
                w.WriteString("jsonrpc", "2.0");
                w.WritePropertyName("id");
                w.WriteRawValue(p.RawId);
                w.WriteStartObject("error");
                w.WriteNumber("code", -32603);
                w.WriteString("message", message);
                w.WriteEndObject();
                w.WriteEndObject();
            }
            buf.WriteByte((byte)'\n');
            await WriteClientAsync(buf.ToArray()).ConfigureAwait(false);
        }
    }

    /// <summary>Id of a JSON-RPC request (request: has method and id) or response (id, no method). Key: kind plus value,
    /// so 1 and "1" differ and a string id matches however it was escaped. Null for anything else.</summary>
    internal static (string Key, string Raw)? MessageId(byte[] line, bool request)
    {
        ReadOnlyMemory<byte> json = line;
        if (line.Length >= 3 && line[0] == 0xEF && line[1] == 0xBB && line[2] == 0xBF) json = json[3..];
        try
        {
            using var doc = JsonDocument.Parse(json);
            var root = doc.RootElement;
            if (root.ValueKind != JsonValueKind.Object || !root.TryGetProperty("id", out var id)) return null;
            bool hasMethod = root.TryGetProperty("method", out _);
            if (hasMethod != request) return null;
            return id.ValueKind switch
            {
                JsonValueKind.String => ("s:" + id.GetString(), id.GetRawText()),
                JsonValueKind.Number => ("n:" + id.GetRawText(), id.GetRawText()),
                _ => null,
            };
        }
        catch (JsonException) { return null; }
    }
}

/// <summary>Splits a byte stream into lines, each with its '\n'; the last one may lack it. Null at end of stream.</summary>
internal sealed class LineSplitter(Stream stream)
{
    private readonly byte[] _buf = new byte[16 * 1024];
    private readonly MemoryStream _line = new();
    private int _start, _end;

    public async Task<byte[]?> ReadAsync()
    {
        while (true)
        {
            int nl = Array.IndexOf(_buf, (byte)'\n', _start, _end - _start);
            if (nl >= 0)
            {
                _line.Write(_buf, _start, nl + 1 - _start);
                _start = nl + 1;
                return Take();
            }
            _line.Write(_buf, _start, _end - _start);
            _start = _end = 0;
            int n;
            try { n = await stream.ReadAsync(_buf).ConfigureAwait(false); }
            catch (Exception e) when (e is IOException or ObjectDisposedException) { n = 0; }
            if (n == 0) return _line.Length > 0 ? Take() : null;
            _end = n;
        }
    }

    private byte[] Take()
    {
        var line = _line.ToArray();
        _line.SetLength(0);
        return line;
    }
}

/// <summary>The real MCP server: the install's betterrecall-mcp.exe with stdio redirected; its stderr goes to ours.</summary>
public sealed class ProcessChild : IMcpChild
{
    private readonly Process _p;

    public ProcessChild(string install, IReadOnlyList<string> args, Stream stderr)
    {
        var exe = Path.Combine(install, InstallLayout.McpExe);
        var psi = new ProcessStartInfo(exe)
        {
            UseShellExecute = false,
            CreateNoWindow = true,
            RedirectStandardInput = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            WorkingDirectory = install,
        };
        foreach (var a in args) psi.ArgumentList.Add(a);
        psi.Environment[InstallLayout.McpRelayEnv] = "1";
        _p = Process.Start(psi) ?? throw new InvalidOperationException($"could not start {exe}");
        _ = Task.Run(() => Copy(_p.StandardError.BaseStream, stderr));
        Exited = WaitAsync();
    }

    public Stream Input => _p.StandardInput.BaseStream;
    public Stream Output => _p.StandardOutput.BaseStream;
    public Task<int> Exited { get; }

    private async Task<int> WaitAsync()
    {
        await _p.WaitForExitAsync().ConfigureAwait(false);
        return _p.ExitCode;
    }

    private static void Copy(Stream from, Stream to)
    {
        var buf = new byte[4096];
        try
        {
            int n;
            while ((n = from.Read(buf, 0, buf.Length)) > 0)
            {
                to.Write(buf, 0, n);
                to.Flush();
            }
        }
        catch (Exception e) when (e is IOException or ObjectDisposedException) { }
    }

    public void Dispose() => _p.Dispose();
}
