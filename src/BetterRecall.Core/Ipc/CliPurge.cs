// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Text.Json;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Ipc;

/// <summary>
/// CLI purge: preview and confirm on ONE connection, since purge tokens belong to the connection that made them
/// (a second "betterrecall ipc purge confirm" process always got "Invalid or expired token."). Asks at the console,
/// or confirms at once with --yes.
/// </summary>
public static class CliPurge
{
    public const string HintConfirm = "Purge tokens only work on the connection that made them. Run 'betterrecall ipc purge <scope>' and answer the prompt, or add --yes.";
    public const string HintYes = "Not confirmed. Run it in a console to answer the prompt, or add --yes.";

    /// <summary>True for a purge preview the CLI should confirm itself (not "purge confirm", not a bare "purge").</summary>
    public static bool Applies(IReadOnlyList<string> words) =>
        words.Count >= 2 && words[0].Equals("purge", StringComparison.OrdinalIgnoreCase)
        && !words[1].Equals("confirm", StringComparison.OrdinalIgnoreCase);

    /// <summary>Removes --yes / -y; true when one was present.</summary>
    public static bool TakeYes(List<string> words) => words.RemoveAll(w => w is "--yes" or "-y") > 0;

    /// <summary>
    /// Sends the preview, asks (unless yes), sends the confirm on the same connection. Every response line goes to
    /// output; prompts and hints go to prompt. Returns the CLI exit code (0 purged, 1 otherwise).
    /// </summary>
    public static async Task<int> RunAsync(Func<string, CancellationToken, Task<string?>> request, string previewLine, bool yes,
        bool interactive, Func<string?> readAnswer, TextWriter output, TextWriter prompt, CancellationToken ct)
    {
        var preview = await request(previewLine, ct).ConfigureAwait(false);
        if (preview == null) return 1;
        output.WriteLine(preview);
        string? token;
        int? rows;
        try
        {
            var (ok, data, _) = Envelope.Parse(preview);
            if (!ok || data.ValueKind != JsonValueKind.Object) return 1;
            token = data.TryGetProperty("confirm", out var c) && c.ValueKind == JsonValueKind.String ? c.GetString() : null;
            rows = data.TryGetProperty("rows", out var r) && r.ValueKind == JsonValueKind.Number ? r.GetInt32() : null;
        }
        catch (JsonException) { return 1; }
        if (string.IsNullOrEmpty(token)) return 1;

        if (!yes)
        {
            if (!interactive) { prompt.WriteLine(HintYes); return 1; }
            prompt.Write(rows is { } n
                ? $"Delete {n} capture{(n == 1 ? "" : "s")}? This cannot be undone. Type yes within 30 s to confirm: "
                : "Delete the matching captures? This cannot be undone. Type yes within 30 s to confirm: ");
            var answer = readAnswer()?.Trim();
            if (!string.Equals(answer, "yes", StringComparison.OrdinalIgnoreCase) && !string.Equals(answer, "y", StringComparison.OrdinalIgnoreCase))
            {
                prompt.WriteLine("Cancelled.");
                return 1;
            }
        }

        var done = await request("purge confirm " + token, ct).ConfigureAwait(false);
        if (done == null) return 1;
        output.WriteLine(done);
        try { return Envelope.Parse(done).Ok ? 0 : 1; }
        catch (JsonException) { return 1; }
    }
}
