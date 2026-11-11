// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Globalization;
using System.Text.Json;
using System.Text.Json.Serialization;
using BetterRecall.Contracts;

namespace BetterRecall.Core;

// ---- wire shapes that are not in Contracts ----

public sealed record PongData(bool Pong);
public sealed record HelpData(IReadOnlyList<string> Verbs);
public sealed record SubscribedStatus(bool Subscribed);
public sealed record SubscribedCaptures(string Subscribed);
public sealed record CaptureEvent(string Id, DateTimeOffset Ts, CaptureKind Kind, string App);

/// <summary>
/// Source-generated JSON for everything Core writes on the pipes. Same options as ContractsJson plus
/// the Contracts UtcTimestampConverter ("Z" timestamps). Holds the few Core-only reply shapes.
/// </summary>
[JsonSourceGenerationOptions(
    PropertyNamingPolicy = JsonKnownNamingPolicy.CamelCase,
    DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    WriteIndented = false,
    Converters = [typeof(UtcTimestampConverter)])]
[JsonSerializable(typeof(Settings))]
[JsonSerializable(typeof(SearchResponse))]
[JsonSerializable(typeof(SearchHit))]
[JsonSerializable(typeof(CaptureDetail))]
[JsonSerializable(typeof(AgentStatus))]
[JsonSerializable(typeof(StoreStats))]
[JsonSerializable(typeof(ReplayClip))]
[JsonSerializable(typeof(BenchResult))]
[JsonSerializable(typeof(BenchReport))]
[JsonSerializable(typeof(PurgeResult))]
[JsonSerializable(typeof(PongData))]
[JsonSerializable(typeof(VersionReply))]
[JsonSerializable(typeof(HelpData))]
[JsonSerializable(typeof(ThumbData))]
[JsonSerializable(typeof(UnlockReply))]
[JsonSerializable(typeof(PurgeConfirmReply))]
[JsonSerializable(typeof(SubscribedStatus))]
[JsonSerializable(typeof(SubscribedCaptures))]
[JsonSerializable(typeof(CaptureEvent))]
[JsonSerializable(typeof(ItemsReply<CaptureMeta>))]
[JsonSerializable(typeof(ItemsReply<AuditEntry>))]
[JsonSerializable(typeof(ItemsReply<ReplayClip>))]
[JsonSerializable(typeof(string))]
public partial class CoreJson : JsonSerializerContext;
