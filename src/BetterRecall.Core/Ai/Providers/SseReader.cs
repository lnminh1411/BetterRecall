// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.CompilerServices;
using System.Text;

namespace BetterRecall.Core.Ai;

internal static class SseReader
{
    /// <summary>Longest line accepted (characters). Real events are a few KiB; a buggy or hostile server could send GBs.</summary>
    internal const int MaxLineChars = 1 << 20;
    /// <summary>Longest event (data lines joined).</summary>
    internal const int MaxEventChars = 8 << 20;
    /// <summary>Whole stream: bounds the answer text and tool arguments the providers build from it.</summary>
    internal const long MaxStreamChars = 16L << 20;
    internal const string TooLargeMessage = "The provider's response is too large.";

    internal static async IAsyncEnumerable<(string Event, string Data)> ReadAsync(
        Stream body,
        [EnumeratorCancellation] CancellationToken ct = default)
    {
        using var reader = new StreamReader(body, Encoding.UTF8, detectEncodingFromByteOrderMarks: false, bufferSize: 1024, leaveOpen: true);
        var lines = new BoundedLines(reader);
        var currentEvent = "";
        var data = new StringBuilder();
        var hasData = false;

        while (await lines.ReadLineAsync(ct).ConfigureAwait(false) is { } line)
        {
            if (line.StartsWith(':'))
            {
                continue;
            }

            if (line.Length == 0)
            {
                if (hasData)
                {
                    yield return (currentEvent, data.ToString());
                    currentEvent = "";
                    data.Clear();
                    hasData = false;
                }
                else
                {
                    currentEvent = "";
                }
                continue;
            }

            if (line.StartsWith("event:", StringComparison.Ordinal))
            {
                var val = line.Substring(6);
                if (val.StartsWith(' ')) val = val.Substring(1);
                currentEvent = val;
            }
            else if (line.StartsWith("data:", StringComparison.Ordinal))
            {
                var val = line.Substring(5);
                if (val.StartsWith(' ')) val = val.Substring(1);
                if (data.Length + val.Length + 1 > MaxEventChars) throw TooLarge();
                if (hasData)
                {
                    data.Append('\n').Append(val);
                }
                else
                {
                    data.Append(val);
                    hasData = true;
                }
            }
        }

        if (hasData)
        {
            yield return (currentEvent, data.ToString());
        }
    }

    internal static ProviderStreamException TooLarge() => new("invalid_request_error", TooLargeMessage);

    /// <summary>ReadLineAsync with caps: CR, LF and CRLF end a line (SSE), as in StreamReader.</summary>
    private sealed class BoundedLines(StreamReader reader)
    {
        private readonly char[] _buf = new char[4096];
        private int _pos, _len;
        private long _total;
        private bool _skipLf;

        public async ValueTask<string?> ReadLineAsync(CancellationToken ct)
        {
            StringBuilder? sb = null;
            while (true)
            {
                if (_pos == _len)
                {
                    _len = await reader.ReadAsync(_buf.AsMemory(), ct).ConfigureAwait(false);
                    _pos = 0;
                    if (_len == 0) return sb?.ToString();
                    _total += _len;
                    if (_total > MaxStreamChars) throw TooLarge();
                }
                if (_skipLf)
                {
                    _skipLf = false;
                    if (_buf[_pos] == '\n') { _pos++; continue; }
                }
                int start = _pos;
                while (_pos < _len && _buf[_pos] is not ('\r' or '\n')) _pos++;
                int take = _pos - start;
                if ((sb?.Length ?? 0) + take > MaxLineChars) throw TooLarge();
                if (_pos < _len)
                {
                    _skipLf = _buf[_pos] == '\r';
                    _pos++;
                    if (sb == null) return new string(_buf, start, take);
                    sb.Append(_buf, start, take);
                    return sb.ToString();
                }
                (sb ??= new StringBuilder()).Append(_buf, start, take);
            }
        }
    }
}
