// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Capture;

namespace BetterRecall.Agent.Replay;

/// <summary>Audio track source for the mux: an AAC ring plus its format.</summary>
internal sealed record AudioRingSource(PacketRing Ring, int SampleRate, int Channels, byte[] AudioSpecificConfig);

internal static class ReplayMux
{
    /// <summary>Snapshots every ring from (now - seconds) and writes one fragmented MP4. Returns duration ms.</summary>
    public static int Save(Stream output, IReadOnlyList<VideoStream> video, AudioRingSource? audio, int seconds)
    {
        long now = OutputCapture.Qpc100ns(Stopwatch.GetTimestamp());
        long from = now - seconds * 10_000_000L;
        var tracks = new List<MuxTrack>();
        var snaps = new List<RingSnapshot>();
        try
        {
            foreach (var v in video)
            {
                var snap = v.Ring.Snapshot(from);
                snaps.Add(snap);
                var track = v.ToMuxTrack(snap);
                if (track is not null) tracks.Add(track);
            }
            if (tracks.Count == 0) throw new InvalidOperationException("Replay buffer is empty.");
            if (audio is not null)
            {
                var snap = audio.Ring.Snapshot(from);
                snaps.Add(snap);
                if (snap.Packets.Count > 0)
                    tracks.Add(new MuxTrack(TrackKind.Audio, "mp4a", null, 0, 0, audio.SampleRate, audio.Channels, audio.AudioSpecificConfig,
                        1024L * 10_000_000L / audio.SampleRate, snap));
            }
            return Fmp4Writer.Write(output, tracks);
        }
        finally
        {
            foreach (var s in snaps) s.Dispose();
        }
    }
}
