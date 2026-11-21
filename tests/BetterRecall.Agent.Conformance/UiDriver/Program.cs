// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.IO.Pipes;
using System.Security.Principal;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using BetterRecall.Contracts;
using BetterRecall.UI.Logic;

// Headless UI-pipe test driver staged at <install>\ui\BetterRecall.UI.exe.
// Uses the real UI client types (AgentConnection + ServerTrustVerifier) and reads JSON-line commands from stdin.

var utf8 = new UTF8Encoding(false);
using var input = new StreamReader(Console.OpenStandardInput(), utf8);
using var output = new StreamWriter(Console.OpenStandardOutput(), utf8) { AutoFlush = true, NewLine = "\n" };

var expectedAgent = ServerTrustVerifier.InstalledAgentPath(AppContext.BaseDirectory);
var trust = new ServerTrustVerifier(expectedAgent);
var sid = PipeNames.UserSid();

var conns = new Dictionary<string, AgentConnection>(StringComparer.Ordinal);
var rawConns = new Dictionary<string, (NamedPipeClientStream Stream, LineReader Reader)>(StringComparer.Ordinal);

try
{
    while (await input.ReadLineAsync().ConfigureAwait(false) is { } cmdLine)
    {
        if (string.IsNullOrWhiteSpace(cmdLine)) continue;
        JsonObject reply;
        long? id = null;
        try
        {
            using var doc = JsonDocument.Parse(cmdLine);
            var root = doc.RootElement;
            if (root.TryGetProperty("id", out var idEl) && idEl.ValueKind == JsonValueKind.Number)
                id = idEl.GetInt64();
            var op = root.GetProperty("op").GetString() ?? "";
            reply = op switch
            {
                "connect" => await ConnectAsync(root).ConfigureAwait(false),
                "request" => await RequestAsync(root).ConfigureAwait(false),
                "parallelRequest" => await ParallelRequestAsync(root).ConfigureAwait(false),
                "rawOpen" => await RawOpenAsync(root).ConfigureAwait(false),
                "rawWrite" => await RawWriteAsync(root).ConfigureAwait(false),
                "rawReadLine" => await RawReadLineAsync(root).ConfigureAwait(false),
                "close" => await CloseAsync(root).ConfigureAwait(false),
                "quit" => MakeOk(id, o => o["bye"] = true),
                _ => MakeError(id, $"Unknown op: {op}"),
            };
            if (id is { } reqId) reply["id"] = reqId;
            await output.WriteLineAsync(reply.ToJsonString()).ConfigureAwait(false);
            if (op == "quit") break;
        }
        catch (Exception ex)
        {
            var err = MakeError(id, $"{ex.GetType().Name}: {ex.Message}");
            await output.WriteLineAsync(err.ToJsonString()).ConfigureAwait(false);
        }
    }
}
finally
{
    foreach (var c in conns.Values) await c.DisposeAsync().ConfigureAwait(false);
    foreach (var r in rawConns.Values) await r.Stream.DisposeAsync().ConfigureAwait(false);
}

return 0;

async Task<JsonObject> ConnectAsync(JsonElement root)
{
    var name = root.GetProperty("conn").GetString()!;
    var home = root.GetProperty("home").GetString()!;
    int timeoutMs = root.TryGetProperty("timeoutMs", out var t) ? t.GetInt32() : 1000;
    if (conns.Remove(name, out var old)) await old.DisposeAsync().ConfigureAwait(false);

    var pipeName = PipeNames.Ui(sid, home);
    var (conn, err) = await AgentConnection.ConnectAsync(pipeName, trust, TimeSpan.FromMilliseconds(timeoutMs), CancellationToken.None).ConfigureAwait(false);
    if (conn is null)
    {
        return new JsonObject
        {
            ["ok"] = false,
            ["errorKind"] = err.ToString(),
            ["error"] = err.ToString(),
        };
    }
    conns[name] = conn;
    return new JsonObject
    {
        ["ok"] = true,
        ["serverPid"] = conn.ServerPid,
    };
}

async Task<JsonObject> RequestAsync(JsonElement root)
{
    var name = root.GetProperty("conn").GetString()!;
    var line = root.GetProperty("line").GetString()!;
    if (!conns.TryGetValue(name, out var conn)) return MakeError(null, $"No connection '{name}'.");
    int timeoutMs = root.TryGetProperty("timeoutMs", out var t) ? t.GetInt32() : 15000;
    using var cts = new CancellationTokenSource(timeoutMs);
    await conn.WriteLineAsync(line, cts.Token).ConfigureAwait(false);
    var resp = await conn.ReadLineAsync(cts.Token).ConfigureAwait(false);
    return resp is null
        ? new JsonObject { ["ok"] = true, ["eof"] = true }
        : new JsonObject { ["ok"] = true, ["line"] = resp };
}

async Task<JsonObject> ParallelRequestAsync(JsonElement root)
{
    var names = root.GetProperty("conns").EnumerateArray().Select(e => e.GetString()!).ToArray();
    var line = root.GetProperty("line").GetString()!;
    int timeoutMs = root.TryGetProperty("timeoutMs", out var t) ? t.GetInt32() : 15000;
    using var cts = new CancellationTokenSource(timeoutMs);

    var tasks = names.Select(async name =>
    {
        if (!conns.TryGetValue(name, out var conn))
            throw new InvalidOperationException($"No connection '{name}'.");
        await conn.WriteLineAsync(line, cts.Token).ConfigureAwait(false);
        return await conn.ReadLineAsync(cts.Token).ConfigureAwait(false);
    }).ToArray();

    var results = await Task.WhenAll(tasks).ConfigureAwait(false);
    var arr = new JsonArray();
    foreach (var r in results) arr.Add(r is null ? null : JsonValue.Create(r));
    return new JsonObject { ["ok"] = true, ["lines"] = arr };
}

async Task<JsonObject> RawOpenAsync(JsonElement root)
{
    var name = root.GetProperty("conn").GetString()!;
    var home = root.GetProperty("home").GetString()!;
    int timeoutMs = root.TryGetProperty("timeoutMs", out var t) ? t.GetInt32() : 1000;
    if (rawConns.Remove(name, out var old)) await old.Stream.DisposeAsync().ConfigureAwait(false);

    var pipe = new NamedPipeClientStream(".", PipeNames.Ui(sid, home), PipeDirection.InOut, PipeOptions.Asynchronous, TokenImpersonationLevel.Anonymous);
    using var cts = new CancellationTokenSource(timeoutMs);
    await pipe.ConnectAsync(cts.Token).ConfigureAwait(false);
    var check = trust.Verify(pipe.SafePipeHandle);
    if (!check.Trusted)
    {
        await pipe.DisposeAsync().ConfigureAwait(false);
        return new JsonObject { ["ok"] = false, ["errorKind"] = AgentErrorKind.NotTrusted.ToString(), ["error"] = "NotTrusted" };
    }
    rawConns[name] = (pipe, new LineReader(pipe));
    return new JsonObject { ["ok"] = true, ["serverPid"] = check.ServerPid };
}

async Task<JsonObject> RawWriteAsync(JsonElement root)
{
    var name = root.GetProperty("conn").GetString()!;
    var bytes = Convert.FromBase64String(root.GetProperty("base64").GetString()!);
    if (!rawConns.TryGetValue(name, out var entry)) return MakeError(null, $"No raw connection '{name}'.");
    await entry.Stream.WriteAsync(bytes).ConfigureAwait(false);
    await entry.Stream.FlushAsync().ConfigureAwait(false);
    return new JsonObject { ["ok"] = true };
}

async Task<JsonObject> RawReadLineAsync(JsonElement root)
{
    var name = root.GetProperty("conn").GetString()!;
    int maxBytes = root.TryGetProperty("maxBytes", out var m) ? m.GetInt32() : 1024 * 1024;
    int timeoutMs = root.TryGetProperty("timeoutMs", out var t) ? t.GetInt32() : 5000;
    if (!rawConns.TryGetValue(name, out var entry)) return MakeError(null, $"No raw connection '{name}'.");
    using var cts = new CancellationTokenSource(timeoutMs);
    var line = await entry.Reader.ReadLineAsync(maxBytes, cts.Token).ConfigureAwait(false);
    return line is null
        ? new JsonObject { ["ok"] = true, ["eof"] = true }
        : new JsonObject { ["ok"] = true, ["line"] = line };
}

async Task<JsonObject> CloseAsync(JsonElement root)
{
    var name = root.GetProperty("conn").GetString()!;
    if (conns.Remove(name, out var c)) await c.DisposeAsync().ConfigureAwait(false);
    if (rawConns.Remove(name, out var r)) await r.Stream.DisposeAsync().ConfigureAwait(false);
    return new JsonObject { ["ok"] = true };
}

static JsonObject MakeOk(long? id, Action<JsonObject>? fill = null)
{
    var o = new JsonObject { ["ok"] = true };
    if (id is { } v) o["id"] = v;
    fill?.Invoke(o);
    return o;
}

static JsonObject MakeError(long? id, string error)
{
    var o = new JsonObject { ["ok"] = false, ["error"] = error };
    if (id is { } v) o["id"] = v;
    return o;
}
