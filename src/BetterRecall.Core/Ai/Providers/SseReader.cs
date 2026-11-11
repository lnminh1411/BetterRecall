// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.CompilerServices;
using System.Text;

namespace BetterRecall.Core.Ai;

internal static class SseReader
{
    internal static async IAsyncEnumerable<(string Event, string Data)> ReadAsync(
        Stream body,
        [EnumeratorCancellation] CancellationToken ct = default)
    {
        using var reader = new StreamReader(body, Encoding.UTF8, detectEncodingFromByteOrderMarks: false, bufferSize: 1024, leaveOpen: true);
        var currentEvent = "";
        var data = new StringBuilder();
        var hasData = false;

        while (await reader.ReadLineAsync(ct).ConfigureAwait(false) is { } line)
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
}
