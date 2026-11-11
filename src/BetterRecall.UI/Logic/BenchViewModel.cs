// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Collections.ObjectModel;
using System.Globalization;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using BetterRecall.Contracts;

namespace BetterRecall.UI.Logic;

/// <summary>string.Format that never throws: placeholders without an argument become "?".</summary>
public static partial class TextFormat
{
    [GeneratedRegex(@"\{(\d+)(?:[,:][^}]*)?\}")]
    private static partial Regex Placeholder();

    public static string Fill(string template, IReadOnlyList<object?> args, IFormatProvider? provider = null)
    {
        provider ??= CultureInfo.CurrentCulture;
        try { return string.Format(provider, template, [.. args]); }
        catch (FormatException)
        {
            return Placeholder().Replace(template, m =>
            {
                int i = int.Parse(m.Groups[1].Value, CultureInfo.InvariantCulture);
                return i < args.Count ? Convert.ToString(args[i], provider) ?? "" : "?";
            });
        }
    }
}

/// <summary>One recommendation with its checkbox and inline error.</summary>
public sealed class RecommendationRow(Recommendation rec) : ObservableObject
{
    private bool _selected = true;
    private string? _error;

    public Recommendation Rec { get; } = rec;
    public string Key => Rec.Key;
    public JsonNode? Proposed { get; } = ParseLiteral(rec.Value);
    public JsonNode? Current { get; } = ParseLiteral(rec.Current);
    /// <summary>Reason code is the resource key; Args fill its placeholders.</summary>
    public LocalizedText Reason => new(Rec.Reason, [.. Rec.Args]);

    public bool Selected { get => _selected; set => Set(ref _selected, value); }
    public string? Error { get => _error; set => Set(ref _error, value); }

    public static JsonNode? ParseLiteral(string literal)
    {
        try { return JsonNode.Parse(literal); }
        catch (JsonException) { return JsonValue.Create(literal); }   // tolerate a bare string
    }
}

public sealed record InferenceRow(string Device, string Ep, bool Ok, bool Chosen, double? Ms70, double? Ms140, double? Ms280, double TextMs, double Cosine, string? Error);

/// <summary>Benchmark page: last report, full run with cancel, recommendations and apply.</summary>
public sealed class BenchViewModel(AgentApi api, Func<IAgentClient> runClient) : ObservableObject
{
    private BenchReport? _report;
    private bool _running;
    private LocalizedText _message = LocalizedText.Empty;

    public BenchReport? Report { get => _report; private set => Set(ref _report, value); }
    public bool IsRunning { get => _running; private set => Set(ref _running, value); }
    public LocalizedText Message { get => _message; private set => Set(ref _message, value); }
    public ObservableCollection<RecommendationRow> Recommendations { get; } = [];
    public IReadOnlyList<InferenceRow> Inference { get; private set; } = [];

    public event Action<AgentErrorKind>? Failed;

    /// <summary>"bench last". Not found is the normal "never run" state.</summary>
    public async Task LoadLastAsync(CancellationToken ct = default)
    {
        var r = await api.BenchLastAsync(ct);
        if (r.Ok) { SetReport(r.Value!); return; }
        if (r.Error == AgentErrors.NotFound) { Message = new LocalizedText("BenchPage_None"); return; }
        Message = SearchViewModel.ErrorText(r.Kind, r.Error);
        Failed?.Invoke(r.Kind);
    }

    /// <summary>
    /// "bench full" on its own connection, which has no read timeout. Cancelling closes that connection,
    /// which is how the agent learns to stop. Returns true when a report arrived.
    /// </summary>
    public async Task<bool> RunAsync(CancellationToken ct)
    {
        IsRunning = true;
        Message = LocalizedText.Empty;
        var client = runClient();
        try
        {
            var r = await new AgentApi(client).BenchFullAsync(ct);
            if (r.Ok) { SetReport(r.Value!); return true; }
            Message = SearchViewModel.ErrorText(r.Kind, r.Error);
            Failed?.Invoke(r.Kind);
            return false;
        }
        catch (OperationCanceledException)
        {
            Message = new LocalizedText("BenchPage_Cancelled");
            return false;
        }
        finally
        {
            if (client is IAsyncDisposable d) await d.DisposeAsync();
            IsRunning = false;
        }
    }

    public void SetReport(BenchReport report)
    {
        Report = report;
        Recommendations.Clear();
        foreach (var rec in report.Recommendations) Recommendations.Add(new RecommendationRow(rec));
        Inference = InferenceRows(report.Inference);
        Raise(nameof(Inference));
        Message = Recommendations.Count == 0 ? new LocalizedText("BenchPage_NoRecs") : LocalizedText.Empty;
    }

    /// <summary>Table rows. A GPU row on a PC with more than one GPU shows the adapter name; token timings and the
    /// chosen mark belong to the chosen adapter only (ChosenAdapter).</summary>
    public static IReadOnlyList<InferenceRow> InferenceRows(BenchResult? r)
    {
        if (r is null) return [];
        double? Ms(string device, int tokens) =>
            r.Tokens?.FirstOrDefault(t => string.Equals(t.Device, device, StringComparison.OrdinalIgnoreCase) && t.ImageTokens == tokens)?.ImageMs;
        return r.Devices.Select(d =>
        {
            bool mine = d.Adapter is null || string.Equals(d.Adapter, r.ChosenAdapter, StringComparison.OrdinalIgnoreCase);
            double? M(int tokens) => mine ? Ms(d.Device, tokens) : null;
            return new InferenceRow(d.Adapter ?? d.Device, d.Ep, d.Ok, mine && string.Equals(d.Device, r.Chosen, StringComparison.OrdinalIgnoreCase),
                M(70), M(140) ?? (d.Ok ? d.ImageMs : null), M(280), d.TextMs, d.Cosine, d.Error);
        }).ToList();
    }

    /// <summary>Partial settings object with only the selected keys, at their proposed values.</summary>
    public JsonObject BuildPatch()
    {
        var patch = new JsonObject();
        foreach (var row in Recommendations.Where(r => r.Selected))
            SettingsSchema.Set(patch, row.Key, row.Proposed?.DeepClone());
        return patch;
    }

    /// <summary>One "settings set" with the selected keys. On "Invalid key." the error goes on that row and nothing is saved.</summary>
    public async Task<AgentResult<JsonObject>?> ApplyAsync(CancellationToken ct = default)
    {
        foreach (var row in Recommendations) row.Error = null;
        var patch = BuildPatch();
        if (patch.Count == 0)
        {
            Message = new LocalizedText("BenchPage_ApplyNone");
            return null;
        }
        var r = await api.SettingsSetAsync(patch, ct);
        if (!r.Ok)
        {
            var key = AgentErrors.InvalidKey(r.Error);
            var row = key is null ? null : Recommendations.FirstOrDefault(x => x.Key == key);
            if (row is not null) row.Error = r.Error;
            Message = SearchViewModel.ErrorText(r.Kind, r.Error);
            if (r.Kind != AgentErrorKind.Server) Failed?.Invoke(r.Kind);
            return r;
        }
        var applied = Recommendations.Where(x => x.Selected).ToList();
        foreach (var row in applied) Recommendations.Remove(row);
        Message = new LocalizedText("BenchPage_Applied", applied.Count);
        return r;
    }
}
