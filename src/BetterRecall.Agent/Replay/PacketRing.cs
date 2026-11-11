// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Agent.Native;

namespace BetterRecall.Agent.Replay;

/// <summary>Packet descriptor. Pts in 100 ns units (QPC-derived). Holds no content.</summary>
internal readonly record struct RingPacket(long Offset, int Length, long Pts, bool Key, int Format = 0);

/// <summary>
/// Encoded packets of one track in native memory (never the managed heap, which copies on compaction).
/// The region is reserved once and committed on demand, so RAM follows the real bitrate.
/// With lockMemory each committed chunk is VirtualLock'ed (caller raises the working-set minimum first).
/// Eviction keeps whole GOPs: the oldest kept packet is the last keyframe at or before newest - window.
/// </summary>
internal sealed unsafe class PacketRing : IDisposable
{
    private const long CommitChunk = 1 << 20;
    private readonly object _gate = new();
    private readonly long _capacity;
    private readonly long _windowTicks;
    private readonly bool _lock;
    private byte* _base;
    private long _committed;
    private long _head;
    private readonly LinkedList<RingPacket> _packets = new();
    private bool _lockFailed;

    public PacketRing(long capacityBytes, TimeSpan window, bool lockMemory)
    {
        _capacity = Math.Max(CommitChunk, (capacityBytes + CommitChunk - 1) / CommitChunk * CommitChunk);
        _windowTicks = window.Ticks;
        _lock = lockMemory;
        _base = (byte*)Win32.VirtualAlloc(null, (nuint)_capacity, Win32.MEM_RESERVE, Win32.PAGE_READWRITE);
        if (_base == null) throw new OutOfMemoryException($"PacketRing reserve of {_capacity} bytes failed ({Marshal.GetLastPInvokeError()}).");
    }

    public long Capacity => _capacity;
    public long CommittedBytes { get { lock (_gate) return _committed; } }
    public bool LockFailed => _lockFailed;

    public int Count { get { lock (_gate) return _packets.Count; } }

    public long LiveBytes
    {
        get
        {
            lock (_gate)
            {
                long sum = 0;
                foreach (var p in _packets) sum += p.Length;
                return sum;
            }
        }
    }

    public long? NewestPts { get { lock (_gate) return _packets.Last?.Value.Pts; } }
    public long? OldestPts { get { lock (_gate) return _packets.First?.Value.Pts; } }

    /// <summary>Appends one packet. Returns false when the packet is larger than the whole ring.</summary>
    public bool Write(ReadOnlySpan<byte> data, long pts, bool key, int format = 0)
    {
        if (data.Length == 0 || data.Length > _capacity) return false;
        lock (_gate)
        {
            EvictByTime(pts);
            long offset;
            while (!TryPlace(data.Length, out offset))
            {
                if (_packets.Count == 0) { _head = 0; continue; }
                _packets.RemoveFirst();
            }
            EnsureCommitted(offset + data.Length);
            data.CopyTo(new Span<byte>(_base + offset, data.Length));
            _head = offset + data.Length;
            _packets.AddLast(new RingPacket(offset, data.Length, pts, key, format));
            return true;
        }
    }

    private bool TryPlace(int len, out long offset)
    {
        offset = 0;
        if (_packets.Count == 0)
        {
            _head = 0;
            offset = 0;
            return len <= _capacity;
        }
        long tail = _packets.First!.Value.Offset;
        if (_head > tail || (_head == tail && _packets.Count == 0))
        {
            long live = _head - tail;
            // Wrap early when the front gap is at least as big as the live data: keeps the committed set small.
            if (tail >= len && tail >= live) { offset = 0; return true; }
            if (_capacity - _head >= len) { offset = _head; return true; }
            if (tail >= len) { offset = 0; return true; }
            return false;
        }
        // Wrapped: live data is [tail, end) and [0, head).
        if (tail - _head >= len) { offset = _head; return true; }
        return false;
    }

    private void EvictByTime(long newestPts)
    {
        if (_windowTicks <= 0) return;
        long cutoff = newestPts - _windowTicks;
        // Find the last keyframe with pts <= cutoff; drop everything before it.
        LinkedListNode<RingPacket>? keep = null;
        for (var n = _packets.First; n is not null && n.Value.Pts <= cutoff; n = n.Next)
            if (n.Value.Key) keep = n;
        if (keep is null) return;
        while (_packets.First != keep) _packets.RemoveFirst();
    }

    private void EnsureCommitted(long end)
    {
        if (end <= _committed) return;
        long newCommitted = Math.Min(_capacity, (end + CommitChunk - 1) / CommitChunk * CommitChunk);
        long size = newCommitted - _committed;
        var p = Win32.VirtualAlloc(_base + _committed, (nuint)size, Win32.MEM_COMMIT, Win32.PAGE_READWRITE);
        if (p == null) throw new OutOfMemoryException("PacketRing commit failed.");
        if (_lock && !Win32.VirtualLock(_base + _committed, (nuint)size)) _lockFailed = true;
        _committed = newCommitted;
    }

    /// <summary>
    /// Copies packets from the last keyframe at or before fromPts (or the first keyframe after it) to the newest packet.
    /// The copy lives in native memory owned by the returned snapshot.
    /// </summary>
    public RingSnapshot Snapshot(long fromPts)
    {
        lock (_gate)
        {
            LinkedListNode<RingPacket>? start = null;
            for (var n = _packets.First; n is not null; n = n.Next)
            {
                if (!n.Value.Key) continue;
                if (n.Value.Pts <= fromPts) start = n;
                else { start ??= n; break; }
            }
            if (start is null) return RingSnapshot.Empty;
            long total = 0;
            int count = 0;
            for (var n = start; n is not null; n = n.Next) { total += n.Value.Length; count++; }
            var snap = new RingSnapshot(total, count, _lock);
            long pos = 0;
            for (var n = start; n is not null; n = n.Next)
            {
                var p = n.Value;
                Buffer.MemoryCopy(_base + p.Offset, snap.Base + pos, total - pos, p.Length);
                snap.Add(new RingPacket(pos, p.Length, p.Pts, p.Key, p.Format));
                pos += p.Length;
            }
            return snap;
        }
    }

    /// <summary>Returns the format ID of every live packet in the ring.</summary>
    public HashSet<int> FormatsInUse()
    {
        lock (_gate)
        {
            var set = new HashSet<int>();
            for (var n = _packets.First; n is not null; n = n.Next)
                set.Add(n.Value.Format);
            return set;
        }
    }

    /// <summary>Panic: zero every committed byte and forget all packets.</summary>
    public void Clear()
    {
        lock (_gate)
        {
            if (_committed > 0) NativeMemory.Clear(_base, (nuint)_committed);
            _packets.Clear();
            _head = 0;
        }
    }

    public void Dispose()
    {
        lock (_gate)
        {
            if (_base == null) return;
            if (_committed > 0)
            {
                NativeMemory.Clear(_base, (nuint)_committed);
                if (_lock) Win32.VirtualUnlock(_base, (nuint)_committed);
            }
            Win32.VirtualFree(_base, 0, Win32.MEM_RELEASE);
            _base = null;
            _packets.Clear();
        }
    }
}

/// <summary>Packets copied out of a ring for muxing. Zeroed and freed on dispose.</summary>
internal sealed unsafe class RingSnapshot : IDisposable
{
    public static RingSnapshot Empty => new(0, 0, false);
    private readonly List<RingPacket> _packets;
    private readonly long _size;
    private readonly bool _locked;
    public byte* Base { get; private set; }

    public RingSnapshot(long size, int count, bool lockMemory)
    {
        _size = size;
        _packets = new List<RingPacket>(count);
        _locked = lockMemory && size > 0;
        Base = size > 0 ? (byte*)NativeMemory.Alloc((nuint)size) : null;
        if (_locked) Win32.VirtualLock(Base, (nuint)size);
    }

    public IReadOnlyList<RingPacket> Packets => _packets;
    public long Size => _size;
    internal void Add(RingPacket p) => _packets.Add(p);
    public ReadOnlySpan<byte> Data(RingPacket p) => new(Base + p.Offset, p.Length);

    public void Dispose()
    {
        if (Base == null) return;
        NativeMemory.Clear(Base, (nuint)_size);
        if (_locked) Win32.VirtualUnlock(Base, (nuint)_size);
        NativeMemory.Free(Base);
        Base = null;
    }
}
