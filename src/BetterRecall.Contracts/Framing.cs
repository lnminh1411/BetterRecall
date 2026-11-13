// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.Text;
using System.Text.Json;

namespace BetterRecall.Contracts;

public sealed class LineTooLongException(int max) : IOException($"Line exceeds {max} bytes.");

/// <summary>Line protocol for the UI, public and MCP pipes: UTF-8, one request per \n, one JSON line back.</summary>
public static class LineProtocol
{
    public const int MaxRequestBytes = 4096;
    /// <summary>UI pipe only: chat questions (4000 characters, escaped in JSON) and settings arrays exceed 4 KiB.</summary>
    public const int MaxUiRequestBytes = 64 * 1024;
    public const string TooLongError = "Line exceeds 4096 bytes.";

    /// <summary>Reads one line. Strips \r and a leading BOM. Null at end of stream.</summary>
    public static async Task<string?> ReadLineAsync(Stream stream, int maxBytes, CancellationToken ct)
    {
        var buffer = new MemoryStream();
        var one = new byte[1];
        while (true)
        {
            int n = await stream.ReadAsync(one, ct).ConfigureAwait(false);
            if (n == 0) return buffer.Length == 0 ? null : Decode(buffer);
            if (one[0] == (byte)'\n') return Decode(buffer);
            if (buffer.Length >= maxBytes) throw new LineTooLongException(maxBytes);
            buffer.WriteByte(one[0]);
        }
    }

    private static string Decode(MemoryStream buffer)
    {
        var s = Encoding.UTF8.GetString(buffer.GetBuffer(), 0, (int)buffer.Length);
        if (s.Length > 0 && s[0] == '﻿') s = s[1..];
        return s.Replace("\r", "");
    }

    public static async Task WriteLineAsync(Stream stream, string line, CancellationToken ct)
    {
        var bytes = Encoding.UTF8.GetBytes(line + "\n");
        await stream.WriteAsync(bytes, ct).ConfigureAwait(false);
        await stream.FlushAsync(ct).ConfigureAwait(false);
    }
}

/// <summary>Buffered line reader for one stream. Use one instance per connection; never mix with LineProtocol.ReadLineAsync on the same stream.</summary>
public sealed class LineReader(Stream stream, int bufferSize = 65_536)
{
    private readonly byte[] _buf = new byte[bufferSize];
    private int _start, _end;

    /// <summary>Reads one line of at most maxBytes. Strips \r and a leading BOM. Null at end of stream.</summary>
    public async Task<string?> ReadLineAsync(int maxBytes, CancellationToken ct)
    {
        var acc = new MemoryStream();
        while (true)
        {
            if (_start == _end)
            {
                _start = 0;
                _end = await stream.ReadAsync(_buf, ct).ConfigureAwait(false);
                if (_end == 0) return acc.Length == 0 ? null : Decode(acc);
            }
            int nl = Array.IndexOf(_buf, (byte)'\n', _start, _end - _start);
            int take = (nl < 0 ? _end : nl) - _start;
            if (acc.Length + take > maxBytes) throw new LineTooLongException(maxBytes);
            acc.Write(_buf, _start, take);
            _start += take;
            if (nl >= 0) { _start++; return Decode(acc); }
        }
    }

    private static string Decode(MemoryStream acc)
    {
        var s = Encoding.UTF8.GetString(acc.GetBuffer(), 0, (int)acc.Length);
        if (s.Length > 0 && s[0] == '﻿') s = s[1..];
        return s.Replace("\r", "");
    }
}

/// <summary>{"ok":true,"data":...} / {"ok":false,"error":"..."} lines.</summary>
public static class Envelope
{
    /// <summary>dataJson must already be valid compact JSON ("null" allowed).</summary>
    public static string Ok(string dataJson) => "{\"ok\":true,\"data\":" + dataJson + "}";

    public static string Ok<T>(T data, System.Text.Json.Serialization.Metadata.JsonTypeInfo<T> info)
        => Ok(JsonSerializer.Serialize(data, info));

    public static string Error(string message)
        => "{\"ok\":false,\"error\":" + JsonSerializer.Serialize(message, ContractsJson.Default.String) + "}";

    /// <summary>Parses a response line. data is default when absent.</summary>
    public static (bool Ok, JsonElement Data, string? Error) Parse(string line)
    {
        using var doc = JsonDocument.Parse(line);
        var root = doc.RootElement;
        bool ok = root.TryGetProperty("ok", out var okEl) && okEl.ValueKind == JsonValueKind.True;
        JsonElement data = root.TryGetProperty("data", out var d) ? d.Clone() : default;
        string? error = root.TryGetProperty("error", out var e) ? e.GetString() : null;
        return (ok, data, error);
    }
}

/// <summary>Worker pipe frame: u32 LE headerLen, JSON header, u32 LE payloadLen, payload.</summary>
public static class BinaryFrame
{
    public const int MaxHeaderBytes = 65_536;
    public const int MaxPayloadBytes = 67_108_864;

    public static async Task WriteAsync(Stream stream, ReadOnlyMemory<byte> header, ReadOnlyMemory<byte> payload, CancellationToken ct)
    {
        if (header.Length > MaxHeaderBytes) throw new IOException("Frame header too large.");
        if (payload.Length > MaxPayloadBytes) throw new IOException("Frame payload too large.");
        var len = new byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(len, (uint)header.Length);
        await stream.WriteAsync(len, ct).ConfigureAwait(false);
        await stream.WriteAsync(header, ct).ConfigureAwait(false);
        BinaryPrimitives.WriteUInt32LittleEndian(len, (uint)payload.Length);
        await stream.WriteAsync(len, ct).ConfigureAwait(false);
        if (payload.Length > 0) await stream.WriteAsync(payload, ct).ConfigureAwait(false);
        await stream.FlushAsync(ct).ConfigureAwait(false);
    }

    /// <summary>Null at clean end of stream before a frame starts.</summary>
    public static async Task<(byte[] Header, byte[] Payload)?> ReadAsync(Stream stream, CancellationToken ct)
    {
        var len = new byte[4];
        if (!await FillAsync(stream, len, allowEof: true, ct).ConfigureAwait(false)) return null;
        int headerLen = checked((int)BinaryPrimitives.ReadUInt32LittleEndian(len));
        if (headerLen > MaxHeaderBytes) throw new IOException("Frame header too large.");
        var header = new byte[headerLen];
        await FillAsync(stream, header, allowEof: false, ct).ConfigureAwait(false);
        await FillAsync(stream, len, allowEof: false, ct).ConfigureAwait(false);
        int payloadLen = checked((int)BinaryPrimitives.ReadUInt32LittleEndian(len));
        if (payloadLen > MaxPayloadBytes) throw new IOException("Frame payload too large.");
        var payload = new byte[payloadLen];
        await FillAsync(stream, payload, allowEof: false, ct).ConfigureAwait(false);
        return (header, payload);
    }

    private static async Task<bool> FillAsync(Stream stream, byte[] buffer, bool allowEof, CancellationToken ct)
    {
        int read = 0;
        while (read < buffer.Length)
        {
            int n = await stream.ReadAsync(buffer.AsMemory(read), ct).ConfigureAwait(false);
            if (n == 0)
            {
                if (allowEof && read == 0) return false;
                throw new EndOfStreamException("Pipe closed mid-frame.");
            }
            read += n;
        }
        return true;
    }

    public static byte[] FloatsToBytes(ReadOnlySpan<float> v)
    {
        var bytes = new byte[v.Length * 4];
        for (int i = 0; i < v.Length; i++) BinaryPrimitives.WriteSingleLittleEndian(bytes.AsSpan(i * 4), v[i]);
        return bytes;
    }

    public static float[] BytesToFloats(ReadOnlySpan<byte> bytes)
    {
        var v = new float[bytes.Length / 4];
        for (int i = 0; i < v.Length; i++) v[i] = BinaryPrimitives.ReadSingleLittleEndian(bytes.Slice(i * 4));
        return v;
    }
}
