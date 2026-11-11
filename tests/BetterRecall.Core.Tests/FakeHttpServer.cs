// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Net;
using System.Net.Sockets;
using System.Text;

namespace BetterRecall.Core.Tests;

/// <summary>
/// Minimal HTTP/1.1 server on 127.0.0.1 for cloud describe tests: records each request (method, path, headers, body)
/// and answers with the configured status and JSON. No real provider is ever called.
/// </summary>
internal sealed class FakeHttpServer : IAsyncDisposable
{
    public sealed record Request(string Method, string Path, Dictionary<string, string> Headers, string Body);

    private readonly TcpListener _listener = new(IPAddress.Loopback, 0);
    private readonly CancellationTokenSource _cts = new();
    private readonly Task _loop;
    public List<Request> Requests { get; } = [];
    public int Status { get; set; } = 200;
    public string ResponseBody { get; set; } = "{}";

    public FakeHttpServer()
    {
        _listener.Start();
        _loop = Task.Run(LoopAsync);
    }

    public string BaseUrl => $"http://127.0.0.1:{((IPEndPoint)_listener.LocalEndpoint).Port}";

    private async Task LoopAsync()
    {
        while (!_cts.IsCancellationRequested)
        {
            TcpClient client;
            try { client = await _listener.AcceptTcpClientAsync(_cts.Token); }
            catch { return; }
            _ = Task.Run(() => ServeAsync(client));
        }
    }

    private async Task ServeAsync(TcpClient client)
    {
        using (client)
        {
            var stream = client.GetStream();
            var buf = new MemoryStream();
            var tmp = new byte[8192];
            int headerEnd = -1;
            while (headerEnd < 0)
            {
                int n = await stream.ReadAsync(tmp);
                if (n == 0) return;
                buf.Write(tmp, 0, n);
                headerEnd = IndexOf(buf.ToArray(), "\r\n\r\n"u8.ToArray());
            }
            var all = buf.ToArray();
            var head = Encoding.ASCII.GetString(all, 0, headerEnd).Split("\r\n");
            var first = head[0].Split(' ');
            var headers = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
            foreach (var h in head.Skip(1))
            {
                int c = h.IndexOf(':');
                if (c > 0) headers[h[..c].Trim()] = h[(c + 1)..].Trim();
            }
            int len = headers.TryGetValue("Content-Length", out var cl) ? int.Parse(cl) : 0;
            var body = new MemoryStream();
            body.Write(all, headerEnd + 4, all.Length - headerEnd - 4);
            while (body.Length < len)
            {
                int n = await stream.ReadAsync(tmp);
                if (n == 0) break;
                body.Write(tmp, 0, n);
            }
            lock (Requests) Requests.Add(new Request(first[0], first[1], headers, Encoding.UTF8.GetString(body.ToArray())));
            var payload = Encoding.UTF8.GetBytes(ResponseBody);
            var resp = $"HTTP/1.1 {Status} X\r\nContent-Type: application/json\r\nContent-Length: {payload.Length}\r\nConnection: close\r\n\r\n";
            await stream.WriteAsync(Encoding.ASCII.GetBytes(resp));
            await stream.WriteAsync(payload);
        }
    }

    private static int IndexOf(byte[] data, byte[] pattern)
    {
        for (int i = 0; i + pattern.Length <= data.Length; i++)
            if (data.AsSpan(i, pattern.Length).SequenceEqual(pattern)) return i;
        return -1;
    }

    public async ValueTask DisposeAsync()
    {
        _cts.Cancel();
        _listener.Stop();
        try { await _loop; } catch { }
    }
}
