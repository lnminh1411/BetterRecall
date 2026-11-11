// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json.Serialization;

namespace BetterRecall.Contracts;

[JsonConverter(typeof(JsonStringEnumConverter<CaptureKind>))]
public enum CaptureKind
{
    [JsonStringEnumMemberName("screen")] Screen,
    [JsonStringEnumMemberName("audio")] Audio,
}

[JsonConverter(typeof(JsonStringEnumConverter<SearchMode>))]
public enum SearchMode
{
    [JsonStringEnumMemberName("hybrid")] Hybrid,
    [JsonStringEnumMemberName("text")] Text,
    [JsonStringEnumMemberName("vector")] Vector,
}

/// <summary>Row metadata. Never carries text or pixels.</summary>
public sealed record CaptureMeta(
    string Id,
    DateTimeOffset Ts,
    CaptureKind Kind,
    string App,
    string Title,
    int Monitor,
    int DurationMs,
    bool HasThumb,
    bool Embedded,
    string Site = "");       // browser host (no scheme, path or query), empty when unknown

/// <summary>Screen capture handed from the agent to the store. Thumb is WebP bytes or null.</summary>
public sealed record NewScreenCapture(
    DateTimeOffset Ts,
    string App,
    string ExePath,
    string Title,
    int Monitor,
    int Width,
    int Height,
    ulong DHash,
    byte[] Image,            // keyframe, keyframes.format, capture.maxWidth wide; pending job input
    string ImageMime,        // image/avif | image/webp | image/jxl | image/jpeg
    bool KeepKeyframe,       // false when keyframes.enabled is false: image deleted once the job is done
    string Site = "",        // browser host from the address bar, empty when unknown or privacy.recordSite is off
    double? Change = null,   // change score vs the previous stored keyframe of the monitor (0-100), null when unknown
    bool Switch = false);    // first keyframe after a window/app switch (or of the monitor): progressive deletion keeps it.
                             // Change null and Switch false: unknown, the store derives both from neighbouring rows

/// <summary>Store files on disk and the free pages inside them (reused by new rows before the files grow).</summary>
public sealed record StorageUse(long FileBytes, long FreeBytes)
{
    public long LiveBytes => Math.Max(0, FileBytes - FreeBytes);
}

/// <summary>export: time window and optional app, written as a zip of decrypted images plus captures.json.</summary>
public sealed record ExportRequest(DateTimeOffset From, DateTimeOffset To, string Path, string? App = null, bool Jpeg = false, bool DryRun = false);

public sealed record ExportResult(int Count, int Images, long Bytes);

/// <summary>Speech segment after transcription.</summary>
public sealed record NewAudioCapture(
    DateTimeOffset Ts,
    string Source,           // system | mic
    int DurationMs,
    string Text,
    string Language,
    string App);             // foreground app at segment start, may be empty

/// <summary>Work the scheduler hands to the worker.</summary>
public sealed record PendingJob(
    string Id,
    CaptureKind Kind,
    byte[]? Image,           // screen: keyframe bytes
    string? ImageMime,
    string? Text);           // audio: transcript, embedded as a document

public sealed record EmbeddingInfo(string ModelId, string Revision, int Dim);

public sealed record SearchRequest(
    string Q,
    int? Limit = null,
    DateTimeOffset? From = null,
    DateTimeOffset? To = null,
    string? App = null,
    CaptureKind[]? Kind = null,
    SearchMode Mode = SearchMode.Hybrid,
    string? Site = null);

public sealed record SearchHit(
    string Id,
    DateTimeOffset Ts,
    CaptureKind Kind,
    string App,
    string Title,
    float Score,
    string Snippet,
    bool HasThumb,
    int? DurationS,
    float? Cosine = null,     // query-to-capture cosine when the hit was in the vector candidates
    bool TextMatch = false,   // found by keyword search (title, app, transcript)
    bool Weak = false,        // no keyword match and Cosine below index.weakMatchBelow: show it, label it weak
    string Site = "");

public sealed record SearchResponse(int Count, bool Truncated, long TookMs, IReadOnlyList<SearchHit> Results);

/// <summary>Result of get: hit plus full stored text (title for screen, transcript for audio).</summary>
public sealed record CaptureDetail(
    string Id,
    DateTimeOffset Ts,
    CaptureKind Kind,
    string App,
    string Title,
    string Text,
    bool HasThumb,
    int? DurationS,
    string Site = "");

/// <summary>Bytes serialise as the JSON field "base64" (research.md 7.6).</summary>
public sealed record ThumbData(string Id, string Mime, int Width, int Height, [property: JsonPropertyName("base64")] byte[] Bytes);

public sealed record StoreStats(
    long Rows,
    long Bytes,
    long ThumbBytes,
    DateTimeOffset? Oldest,
    DateTimeOffset? Newest,
    int PendingEmbeddings,
    long Vectors);

public sealed record AudioState(bool System, bool Mic);

public sealed record QueueState(int Pending);

public sealed record AgentStatus(
    bool Capturing,
    bool Paused,
    DateTimeOffset? PausedUntil,
    string? PauseReason,     // user | ipc | fullscreen | lock | battery | denylist | consent | keylocked | storage
    bool Locked,
    DateTimeOffset? UnlockedUntil,
    AudioState Audio,
    QueueState Queue,
    string Device,           // NPU | GPU | CPU | none
    StoreStats Store,
    int RetentionDays);

public sealed record AuditEntry(
    DateTimeOffset Ts,
    string Pipe,             // ui | ipc | mcp
    int ClientPid,
    string ClientImage,
    string Verb,
    int ResultCount,
    string QuerySha256);     // empty when the verb has no query

public sealed record PurgeResult(int Purged);

/// <summary>{"items":[...]} replies (timeline, audit, replay list).</summary>
public sealed record ItemsReply<T>(IReadOnlyList<T> Items);

/// <summary>One app or site with its number of searchable captures.</summary>
public sealed record FacetCount(string Name, int Count);

/// <summary>facets: top apps and browser sites in a time window, most captures first.</summary>
public sealed record FacetsReply(IReadOnlyList<FacetCount> Apps, IReadOnlyList<FacetCount> Sites);

/// <summary>One saved search (privacy.searchHistory). Stored only in the encrypted store.</summary>
public sealed record HistoryEntry(string Q, DateTimeOffset Ts);

public sealed record UnlockReply(bool Unlocked, DateTimeOffset? Until);

/// <summary>Step 1 of a two-step purge. Rows is null for purge all.</summary>
public sealed record PurgeConfirmReply(string Confirm, int? Rows);

public sealed record VersionReply(string App, int Protocol);

public sealed record ReplayClip(string Id, DateTimeOffset Ts, int DurationMs, long Bytes, bool Encrypted, string Path);

// ---- worker protocol payloads (docs/v1-design.md section 4) ----

public sealed record WorkerHello(
    int Protocol,
    string Device,           // NPU | GPU | CPU | none (no device passed the checks)
    string Ep,               // OpenVINO | QNN | DirectML | CPU | fake
    string Model,
    string Revision,
    int Dim,
    bool Asr,
    bool Vad,
    string[] Missing,        // model names not installed
    string? NsfwDevice = null); // device of the NSFW gate (NPU | iGPU | GPU <adapter> | CPU), null when not loaded

public sealed record TranscriptSegment(int StartMs, int EndMs, string Text);

public sealed record TranscribeResult(bool Speech, string Text, string Language, TranscriptSegment[] Segments, long Ms);

public sealed record BenchDevice(
    string Device, string Ep, bool Ok, double ImageMs, double TextMs, double Cosine, string? Error,
    string? Model = null,        // embeddinggemma-2 | siglip2-base
    string? TextDevice = null,   // set when the text tower runs on another device (split mode)
    double? CompileMs = null,    // first-compile time, cache miss only
    string? Adapter = null,      // GPU rows on a PC with more than one GPU: the GPU's name (inference.gpuAdapter value)
    bool AdapterAuto = false);   // that GPU is the one inference.gpuAdapter "auto" picks

/// <summary>Per device and image-token budget, measured by the worker bench (on the chosen device only).</summary>
public sealed record TokenBench(string Device, int ImageTokens, double ImageMs);

/// <summary>ChosenAdapter: the chosen GPU's name when Chosen is a GPU on a PC with more than one GPU.</summary>
public sealed record BenchResult(string Chosen, BenchDevice[] Devices, TokenBench[]? Tokens = null, string? ChosenAdapter = null);

// ---- full benchmark and recommendations (docs/v1-design.md section 10) ----

public sealed record GpuInfo(string Name, string Vendor, bool Integrated, long VramMb, string Luid, string Driver, string[] Codecs);
public sealed record NpuInfo(string Name, string Driver);
public sealed record MonitorInfo(int Index, int Width, int Height, int RefreshHz, string Adapter);
public sealed record HardwareInfo(string Cpu, int Cores, int Threads, long RamMb, bool HasBattery, GpuInfo[] Gpus, NpuInfo? Npu, MonitorInfo[] Monitors, string Os);

/// <summary>One encoder run: Api is "vpl" or "mf"; Layout composited | perMonitor; size is the encoded video.</summary>
public sealed record EncoderBench(
    string Adapter, string Codec, string Api, string Layout, int Width, int Height, int Fps,
    bool Ok, double EncodeMsP95, double CpuPercent, long WorkingSetMb, string? Error,
    string? Resolution = null);   // replay.resolution value that produced this size (native, 1080p, ...)

public sealed record CaptureBench(double TickMs, double KeyframeEncodeMs, double CrossAdapterMs);

public sealed record SearchBench(long Rows, double VectorMs, double HybridMs);

/// <summary>Value and Current are compact JSON literals for the setting key (e.g. "60", "\"av1\"").
/// Reason is a code the UI localises (docs/v1-design.md section 10); Args fill its placeholders.</summary>
public sealed record Recommendation(string Key, string Value, string Current, string Reason, string[] Args);

public sealed record BenchReport(
    DateTimeOffset Ts,
    long TookMs,
    HardwareInfo Hardware,
    BenchResult? Inference,
    EncoderBench[] Encoders,
    CaptureBench? Capture,
    SearchBench? Search,
    Recommendation[] Recommendations);

/// <summary>Request header for the worker pipe. Unused fields stay null.</summary>
public sealed record WorkerRequest(
    long Id,
    string Op,
    string? Format = null,
    int? Width = null,
    int? Height = null,
    int? Stride = null,
    string? Text = null,
    string? Kind = null,
    int? SampleRate = null,
    int? Channels = null,
    string? Source = null,
    string? Language = null,
    double? Nsfw = null,     // embed_image: NSFW threshold, null skips the check
    int? NsfwGrid = null,    // embed_image: 1 whole frame, 2 adds quadrants + centre, 3 adds a 3x3 grid; null means 1
    double? NsfwDetector = null); // embed_image: NSFW detector threshold, null skips the detector

/// <summary>Response header for the worker pipe.</summary>
public sealed record WorkerResponse(
    long Id,
    bool Ok,
    string? Error = null,
    int? Dim = null,
    double? Ms = null,
    string? Dropped = null,  // embed_image: "nsfw" when blocked, no payload follows
    WorkerHello? Hello = null,
    TranscribeResult? Transcript = null,
    BenchResult? Bench = null);
