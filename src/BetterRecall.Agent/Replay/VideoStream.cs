// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Replay;

/// <summary>
/// One encoded video of the replay: a packet ring plus the formats its GOPs use. Each encoder (re)start opens a session.
/// A session's packets are dropped until its first keyframe whose codec config parses, so every session starts with a
/// closed GOP and its config is stored as format id == session id from that GOP on.
/// </summary>
internal sealed class VideoStream : IDisposable
{
    private readonly object _gate = new();
    private readonly Dictionary<int, VideoFormat> _formats = new();
    private int _currentSession;
    private int _fps;
    private long _lastKeyPts = long.MinValue / 2;
    private long _lastWrittenPts = long.MinValue;
    private long _droppedBeforeKey;

    private VideoCodec _sessionCodec;
    private int _sessionWidth;
    private int _sessionHeight;

    public VideoStream(string key, long capacityBytes, TimeSpan window, bool lockMemory)
    {
        Key = key;
        Ring = new PacketRing(capacityBytes, window, lockMemory);
    }

    public string Key { get; }
    public PacketRing Ring { get; }
    public int Fps { get { lock (_gate) return _fps; } }
    public long LastKeyPts { get { lock (_gate) return _lastKeyPts; } }
    public long DroppedBeforeKey { get { lock (_gate) return _droppedBeforeKey; } }
    public int CurrentSession { get { lock (_gate) return _currentSession; } }

    public int BeginSession(VideoCodec codec, int width, int height, int fps)
    {
        lock (_gate)
        {
            _currentSession++;
            _fps = fps;
            _sessionCodec = codec;
            _sessionWidth = width;
            _sessionHeight = height;

            var inUse = Ring.FormatsInUse();
            var toRemove = new List<int>();
            foreach (var id in _formats.Keys)
            {
                if (id != _currentSession && !inUse.Contains(id))
                    toRemove.Add(id);
            }
            foreach (var id in toRemove) _formats.Remove(id);

            return _currentSession;
        }
    }

    public void OnPacket(int session, ReadOnlySpan<byte> data, long pts, bool key)
    {
        lock (_gate)
        {
            if (session != _currentSession) return;

            if (!_formats.ContainsKey(session))
            {
                if (!key)
                {
                    _droppedBeforeKey++;
                    return;
                }
                var config = CodecBits.BuildConfig(_sessionCodec, data);
                if (config == null)
                {
                    _droppedBeforeKey++;
                    return;
                }
                _formats[session] = new VideoFormat(_sessionCodec, _sessionWidth, _sessionHeight, config);
            }

            if (pts <= _lastWrittenPts) return;

            if (Ring.Write(data, pts, key, session))
            {
                _lastWrittenPts = pts;
                if (key) _lastKeyPts = pts;
            }
        }
    }

    public IReadOnlyDictionary<int, VideoFormat> Formats()
    {
        lock (_gate)
        {
            return new Dictionary<int, VideoFormat>(_formats);
        }
    }

    public MuxTrack? ToMuxTrack(RingSnapshot snapshot)
    {
        lock (_gate)
        {
            if (snapshot.Packets.Count == 0) return null;
            var lastPkt = snapshot.Packets[^1];
            if (!_formats.TryGetValue(lastPkt.Format, out var fmt)) return null;

            string fourcc = fmt.Codec switch
            {
                VideoCodec.Av1 => "av01",
                VideoCodec.Hevc => "hev1",
                _ => "avc1",
            };

            return new MuxTrack(
                TrackKind.Video,
                fourcc,
                fmt.Codec,
                fmt.Width,
                fmt.Height,
                0,
                0,
                fmt.Config,
                10_000_000L / Math.Max(1, _fps),
                snapshot,
                new Dictionary<int, VideoFormat>(_formats),
                _fps);
        }
    }

    public void Clear()
    {
        lock (_gate)
        {
            Ring.Clear();
        }
    }

    public void Dispose()
    {
        lock (_gate)
        {
            Ring.Dispose();
        }
    }
}
