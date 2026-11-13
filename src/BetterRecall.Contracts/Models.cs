// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Text.Json.Serialization;

namespace BetterRecall.Contracts;

[JsonConverter(typeof(JsonStringEnumConverter<CaptureKind>))]
public enum CaptureKind
{
    [JsonStringEnumMemberName("screen")] Screen,
    [JsonStringEnumMemberName("audio")] Audio,
    /// <summary>A frame sampled from a saved replay (replay id + offset), searchable like a screen capture.</summary>
    [JsonStringEnumMemberName("replay")] Replay,
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
    string Site = "",        // browser host (no scheme, path or query), empty when unknown
    string? ReplayId = null, // kind replay: saved replay the frame belongs to
    int? ReplayOffsetMs = null);

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
    bool Switch = false,     // first keyframe after a window/app switch (or of the monitor): progressive deletion keeps it.
                             // Change null and Switch false: unknown, the store derives both from neighbouring rows
    string? Text = null);    // OCR text after redaction (ocr.enabled), null when not read

/// <summary>Store files on disk and the free pages inside them (reused by new rows before the files grow).</summary>
public sealed record StorageUse(long FileBytes, long FreeBytes)
{
    public long LiveBytes => Math.Max(0, FileBytes - FreeBytes);
}

/// <summary>export: time window and optional app, written as a zip of decrypted images plus captures.json.</summary>
public sealed record ExportRequest(DateTimeOffset From, DateTimeOffset To, string Path, string? App = null, bool Jpeg = false, bool DryRun = false);

public sealed record ExportResult(int Count, int Images, long Bytes);

/// <summary>Frame sampled from a saved replay, handed from the agent to the store (kind replay).</summary>
public sealed record NewReplayFrame(
    DateTimeOffset Ts,
    string ReplayId,
    int OffsetMs,            // position in the replay file
    string App,
    string Title,
    int Monitor,
    int Width,
    int Height,
    byte[] Image,            // keyframes.format bytes
    string ImageMime,
    string Site = "");

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
    bool TextMatch = false,   // found by keyword search (title, app, transcript, screen text)
    bool Weak = false,        // no keyword match and Cosine below index.weakMatchBelow: show it, label it weak
    string Site = "",
    string? ReplayId = null,  // kind replay: open this replay at ReplayOffsetMs
    int? ReplayOffsetMs = null);

public sealed record SearchResponse(int Count, bool Truncated, long TookMs, IReadOnlyList<SearchHit> Results);

/// <summary>Result of get: hit plus full stored text (screen: redacted OCR text when read, else the title; audio: transcript).</summary>
public sealed record CaptureDetail(
    string Id,
    DateTimeOffset Ts,
    CaptureKind Kind,
    string App,
    string Title,
    string Text,
    bool HasThumb,
    int? DurationS,
    string Site = "",
    string? ReplayId = null,
    int? ReplayOffsetMs = null);

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
    int RetentionDays,
    string[]? HotkeysFailed = null);   // hotkey actions whose RegisterHotKey failed (combination taken by another app)

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

/// <summary>password: unlock password state (security.unlockMethod). RetryAfterS &gt; 0 while too many wrong tries block it.</summary>
public sealed record PasswordStatus(bool Set, bool HelloAvailable, int RetryAfterS);

/// <summary>ai: cloud describe and chat state. Never carries the API key itself. Local: the endpoint is a loopback
/// address, so requests stay on this PC.</summary>
public sealed record AiStatus(bool Enabled, bool KeySet, string Provider, string Model, string Endpoint, AiUsage? Usage = null, bool Local = false);

/// <summary>Today's cloud requests and tokens against the local budget (ai.dailyRequests, ai.dailyTokens; 0 = no limit).</summary>
public sealed record AiUsage(string Day, int Requests, long Tokens, int RequestLimit, long TokenLimit);

/// <summary>A capture a chat answer used, shown as a source card.</summary>
public sealed record ChatSource(string Id, DateTimeOffset Ts, CaptureKind Kind, string App, string Title, string Site,
    string? ReplayId = null, int? ReplayOffsetMs = null);

/// <summary>One capture in an approval prompt: exactly the text and (redacted) image that would be sent.</summary>
public sealed record ChatApprovalItem(string Id, DateTimeOffset Ts, CaptureKind Kind, string App, string Title, string Site,
    string Text, string? ImageMime = null, [property: JsonPropertyName("base64")] byte[]? Image = null);

/// <summary>
/// One line of a chat stream. Type: start (chatId), search (query run locally), text (delta), approval (token, items,
/// share, text = notice), sources, warning (code, text), done (usage), error (code, text, retryAt). The stream ends
/// after done, approval or error.
/// </summary>
public sealed record ChatEvent(string Type, string? ChatId = null, string? Text = null, string? Token = null, string? Query = null,
    IReadOnlyList<ChatSource>? Sources = null, IReadOnlyList<ChatApprovalItem>? Items = null, string? Share = null,
    string? Code = null, DateTimeOffset? RetryAt = null, AiUsage? Usage = null);

public sealed record ChatSummary(string Id, string Title, DateTimeOffset Created, DateTimeOffset Updated, int Messages);

/// <summary>Role user or assistant.</summary>
public sealed record ChatMessage(int Seq, DateTimeOffset Ts, string Role, string Text, IReadOnlyList<ChatSource>? Sources = null);

public sealed record ChatDetail(ChatSummary Chat, IReadOnlyList<ChatMessage> Messages, bool Approved);

/// <summary>A saved chat. Transcript is the provider-native message list (JSON) so later turns append to it unchanged.</summary>
public sealed record ChatRecord(string Id, string Title, DateTimeOffset Created, DateTimeOffset Updated, string Provider, string Model,
    string Transcript, IReadOnlyList<ChatMessage> Messages, string Endpoint = "");   // Endpoint: where the transcript was sent ("" before 0.3)

/// <summary>describe preview: the exact image a later "describe send &lt;token&gt;" sends (token valid 120 s).
/// Redacted is false until redaction exists (batch 3).</summary>
public sealed record DescribePreview(
    string Token, string Provider, string Model, string Endpoint, string Mime, int Width, int Height, bool Redacted,
    [property: JsonPropertyName("base64")] byte[] Bytes);

/// <summary>describe send: the provider's text.</summary>
public sealed record DescribeResult(string Text, string Provider, string Model, long TookMs);

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
    string? NsfwDevice = null, // device of the NSFW gate (NPU | iGPU | GPU <adapter> | CPU), null when not loaded
    string? OcrDevice = null); // OCR engine and device ("onnx iGPU", "windows"), null when OCR is off or not loaded

/// <summary>Entity found by the NER model in an OCR line: UTF-16 Start and Length in the line text.
/// Label: person | address | location (other labels are dropped by the worker).</summary>
public sealed record OcrEntity(int Start, int Length, string Label, float Score);

/// <summary>One recognised text line in keyframe pixels. Cuts has Text.Length + 1 x positions (image pixels): character
/// i spans [Cuts[i], Cuts[i+1]). Entities is null unless the request asked for them.</summary>
public sealed record OcrLine(string Text, int X, int Y, int W, int H, float Confidence, int[] Cuts, OcrEntity[]? Entities = null);

/// <summary>ocr reply: lines top to bottom, the engine that read them ("onnx" | "windows") and the time taken.</summary>
/// <param name="EntitiesMissing">Entities were asked for but the NER model is not installed: the lines carry none, so
/// name and address redaction did not run (pattern redaction on the text still does).</param>
public sealed record OcrResult(OcrLine[] Lines, string Engine, double Ms, bool EntitiesMissing = false);

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

public sealed record CaptureBench(double TickMs, double KeyframeEncodeMs, double CrossAdapterMs,
    string? KeyframeEncoder = null);   // encoder the keyframe timing used, e.g. "oneVPL AV1 VDENC on Intel(R) Graphics", "libaom (software)"

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
    double? NsfwDetector = null, // embed_image: NSFW detector threshold, null skips the detector
    bool? Entities = null);  // ocr: also run the NER model (names, addresses) over the lines

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
    BenchResult? Bench = null,
    OcrResult? Ocr = null);
