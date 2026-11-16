// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json.Nodes;

namespace BetterRecall.UI.Logic;

public enum SaveOutcome { NothingToSave, Saved, Invalid, TooLarge, Failed }

/// <summary>Working copy of settings shared by all settings pages. Save sends only changed keys.</summary>
public sealed class SettingsEditor
{
    public JsonObject Original { get; private set; } = new();
    public JsonObject Working { get; private set; } = new();

    /// <summary>Inline errors by key ("Invalid key." text from the agent or the local check).</summary>
    public Dictionary<string, string> Errors { get; } = [];

    public bool Loaded { get; private set; }

    public event Action? Changed;

    public void Load(JsonObject settings)
    {
        Original = (JsonObject)settings.DeepClone();
        Working = (JsonObject)settings.DeepClone();
        Errors.Clear();
        Loaded = true;
        Changed?.Invoke();
    }

    /// <summary>Fresh settings from the agent (after a reconnect, D2): Original follows them, so a later save sends only
    /// the user's pending edits, which stay on top of the fresh values.</summary>
    public void Refresh(JsonObject settings)
    {
        if (!Loaded) { Load(settings); return; }
        var pending = Diff();
        Original = (JsonObject)settings.DeepClone();
        Working = (JsonObject)settings.DeepClone();
        foreach (var k in SettingsDiff.Keys(pending))
            SettingsSchema.Set(Working, k, SettingsSchema.Get(pending, k)?.DeepClone());
        Changed?.Invoke();
    }

    public JsonNode? Get(string key) => SettingsSchema.Get(Working, key);

    public void Set(string key, JsonNode? value)
    {
        SettingsSchema.Set(Working, key, value);
        if (SettingsSchema.Find(key) is { } d)
        {
            if (SettingsSchema.Validate(d, Working) is { } e) Errors[key] = e;
            else Errors.Remove(key);
        }
        Changed?.Invoke();
    }

    public JsonObject Diff() => SettingsDiff.Diff(Original, Working);

    public bool HasChanges => Diff().Count > 0;

    public void Discard() => Load(Original);

    /// <summary>Saves one key now (home page switches). Pending edits to other keys stay pending; a pending edit to this
    /// key is replaced. Returns the failed result, or the last saved one.</summary>
    public async Task<AgentResult<JsonObject>> SaveKeyAsync(AgentApi api, string key, JsonNode? value, CancellationToken ct = default)
    {
        var pending = Diff();
        var one = new JsonObject();
        SettingsSchema.Set(one, key, value?.DeepClone());
        var line = SettingsDiff.ToRequestLine(one)!;   // one scalar key always fits
        var r = (await api.Client.SendAsync(line, ct)).As(Responses.SettingsObject);
        if (!r.Ok)
        {
            if (AgentErrors.InvalidKey(r.Error) is { } bad) Errors[bad] = r.Error!;
            Changed?.Invoke();
            return r;
        }
        Original = (JsonObject)r.Value!.DeepClone();
        Working = (JsonObject)r.Value!.DeepClone();
        foreach (var k in SettingsDiff.Keys(pending))
            if (k != key) SettingsSchema.Set(Working, k, SettingsSchema.Get(pending, k)?.DeepClone());
        Errors.Remove(key);
        Changed?.Invoke();
        return r;
    }

    /// <summary>Validates locally, then sends the diff. On "Invalid key." from the agent the error is kept for that key.</summary>
    public async Task<(SaveOutcome Outcome, AgentResult<JsonObject>? Last)> SaveAsync(AgentApi api, CancellationToken ct = default)
    {
        var diff = Diff();
        if (diff.Count == 0) return (SaveOutcome.NothingToSave, null);

        Errors.Clear();
        foreach (var key in SettingsDiff.Keys(diff))
            if (SettingsSchema.Find(key) is { } d && SettingsSchema.Validate(d, Working) is { } e) Errors[key] = e;
        // a change can make an unchanged key invalid (ipc.maxResults below ipc.defaultResults); a key that was already
        // invalid by the UI's rules but accepted by the agent (CLI, settings.json) must not block an unrelated save
        var before = SettingsSchema.ValidateAll(Original);
        foreach (var (k, e) in SettingsSchema.ValidateAll(Working))
            if (!before.ContainsKey(k)) Errors.TryAdd(k, e);
        if (Errors.Count > 0) { Changed?.Invoke(); return (SaveOutcome.Invalid, null); }

        // One request for the whole diff: the agent validates and saves it as a unit, so an error leaves nothing half saved.
        var line = SettingsDiff.ToRequestLine(diff);
        if (line is null) return (SaveOutcome.TooLarge, null);

        var r = (await api.Client.SendAsync(line, ct)).As(Responses.SettingsObject);
        if (!r.Ok)
        {
            if (AgentErrors.InvalidKey(r.Error) is { } bad) Errors[bad] = r.Error!;
            Changed?.Invoke();
            return (AgentErrors.InvalidKey(r.Error) is null ? SaveOutcome.Failed : SaveOutcome.Invalid, r);
        }
        // keep edits made while the request ran: rebase the working copy on what the agent saved
        var pending = SettingsDiff.Diff(r.Value!, Working);
        Original = (JsonObject)r.Value!.DeepClone();
        Working = (JsonObject)r.Value!.DeepClone();
        foreach (var key in SettingsDiff.Keys(pending)) SettingsSchema.Set(Working, key, SettingsSchema.Get(pending, key)?.DeepClone());
        Changed?.Invoke();
        return (SaveOutcome.Saved, r);
    }
}
