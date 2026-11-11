// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Numerics;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Runtime.Intrinsics;

namespace BetterRecall.Core.Store;

/// <summary>Filter for the first-stage scan. AppId -1 means any app; KindMask bit 0 screen, bit 1 audio.</summary>
public readonly record struct VectorFilter(long FromMs, long ToMs, int KindMask, int AppId)
{
    public static readonly VectorFilter All = new(long.MinValue, long.MaxValue, 3, -1);
}

/// <summary>Candidate from the binary scan: store row id, Hamming distance, bits compared.</summary>
public readonly record struct VectorCandidate(long Rid, int Distance, int Dim);

/// <summary>First-stage vector search. Hides the scan so an HNSW sidecar can replace it later.</summary>
public interface IVectorIndex
{
    int Count { get; }
    void Upsert(long rid, ReadOnlySpan<float> vector, long tsMs, int kind, string appLower);
    void Remove(long rid);
    void Clear();
    /// <summary>Top-N rows by Hamming distance between sign bits; each row compared over min(query dim, row dim).</summary>
    IReadOnlyList<VectorCandidate> Scan(ReadOnlySpan<float> query, VectorFilter filter, int topN);
    int AppId(string appLower);
}

/// <summary>
/// In-RAM binary copy (one sign bit per dimension) per row, grouped by row dim. Scan is a POPCNT over
/// XOR'ed 64-bit words, 4-way unrolled; 768-d is 12 words per row. Thread-safe (reader/writer lock).
/// </summary>
public sealed class BinaryVectorIndex : IVectorIndex
{
    private sealed class Segment(int dim)
    {
        public readonly int Dim = dim;
        public readonly int Words = (dim + 63) / 64;
        public ulong[] Bits = new ulong[1024 * ((dim + 63) / 64)];
        public long[] Ts = new long[1024];
        public long[] Rid = new long[1024];
        public byte[] Kind = new byte[1024];
        public int[] App = new int[1024];
        public bool[] Alive = new bool[1024];
        public int Count;
        public int Dead;

        public void Grow()
        {
            int cap = Ts.Length * 2;
            Array.Resize(ref Bits, cap * Words);
            Array.Resize(ref Ts, cap);
            Array.Resize(ref Rid, cap);
            Array.Resize(ref Kind, cap);
            Array.Resize(ref App, cap);
            Array.Resize(ref Alive, cap);
        }
    }

    private readonly ReaderWriterLockSlim _lock = new(LockRecursionPolicy.NoRecursion);
    private readonly Dictionary<int, Segment> _segments = [];
    private readonly Dictionary<long, (Segment Seg, int Slot)> _where = [];
    private readonly Dictionary<string, int> _apps = new(StringComparer.Ordinal);

    public int Count { get { _lock.EnterReadLock(); try { return _where.Count; } finally { _lock.ExitReadLock(); } } }

    public int AppId(string appLower)
    {
        _lock.EnterReadLock();
        try { return _apps.TryGetValue(appLower, out var id) ? id : -2; }
        finally { _lock.ExitReadLock(); }
    }

    /// <summary>Sign bits of v[0..dim), little-endian bit order within each 64-bit word.</summary>
    public static void SignBits(ReadOnlySpan<float> v, Span<ulong> words)
    {
        words.Clear();
        for (int i = 0; i < v.Length; i++)
            if (v[i] > 0) words[i >> 6] |= 1UL << (i & 63);
    }

    public static byte[] SignBitsBytes(ReadOnlySpan<float> v)
    {
        var words = new ulong[(v.Length + 63) / 64];
        SignBits(v, words);
        return MemoryMarshal.AsBytes(words.AsSpan()).ToArray();
    }

    public void Upsert(long rid, ReadOnlySpan<float> vector, long tsMs, int kind, string appLower)
    {
        int words = (vector.Length + 63) / 64;
        Span<ulong> bits = words <= 64 ? stackalloc ulong[words] : new ulong[words];
        SignBits(vector, bits);
        UpsertBits(rid, bits, vector.Length, tsMs, kind, appLower);
    }

    /// <summary>Adds a row from its stored binary copy (load path, no float vector needed).</summary>
    public void UpsertBits(long rid, ReadOnlySpan<ulong> bits, int dim, long tsMs, int kind, string appLower)
    {
        _lock.EnterWriteLock();
        try
        {
            RemoveLocked(rid);
            if (!_segments.TryGetValue(dim, out var seg)) _segments[dim] = seg = new Segment(dim);
            if (seg.Count == seg.Ts.Length) seg.Grow();
            int slot = seg.Count++;
            bits[..seg.Words].CopyTo(seg.Bits.AsSpan(slot * seg.Words, seg.Words));
            seg.Ts[slot] = tsMs;
            seg.Rid[slot] = rid;
            seg.Kind[slot] = (byte)(1 << kind);
            if (!_apps.TryGetValue(appLower, out var app)) _apps[appLower] = app = _apps.Count;
            seg.App[slot] = app;
            seg.Alive[slot] = true;
            _where[rid] = (seg, slot);
        }
        finally { _lock.ExitWriteLock(); }
    }

    public void Remove(long rid)
    {
        _lock.EnterWriteLock();
        try { RemoveLocked(rid); }
        finally { _lock.ExitWriteLock(); }
    }

    private void RemoveLocked(long rid)
    {
        if (!_where.Remove(rid, out var at)) return;
        at.Seg.Alive[at.Slot] = false;
        at.Seg.Dead++;
        if (at.Seg.Dead > 4096 && at.Seg.Dead * 4 > at.Seg.Count) Compact(at.Seg);
    }

    private void Compact(Segment seg)
    {
        int w = seg.Words, dst = 0;
        for (int src = 0; src < seg.Count; src++)
        {
            if (!seg.Alive[src]) continue;
            if (dst != src)
            {
                Array.Copy(seg.Bits, src * w, seg.Bits, dst * w, w);
                seg.Ts[dst] = seg.Ts[src];
                seg.Rid[dst] = seg.Rid[src];
                seg.Kind[dst] = seg.Kind[src];
                seg.App[dst] = seg.App[src];
                seg.Alive[dst] = true;
                _where[seg.Rid[dst]] = (seg, dst);
            }
            dst++;
        }
        Array.Clear(seg.Alive, dst, seg.Count - dst);
        seg.Count = dst;
        seg.Dead = 0;
    }

    public void Clear()
    {
        _lock.EnterWriteLock();
        try { _segments.Clear(); _where.Clear(); _apps.Clear(); }
        finally { _lock.ExitWriteLock(); }
    }

    public IReadOnlyList<VectorCandidate> Scan(ReadOnlySpan<float> query, VectorFilter filter, int topN)
    {
        if (topN <= 0 || query.Length == 0 || filter.AppId == -2) return [];
        int qWords = (query.Length + 63) / 64;
        var q = new ulong[qWords];
        SignBits(query, q);
        var heap = new PriorityQueue<VectorCandidate, long>(topN + 1);
        _lock.EnterReadLock();
        try
        {
            foreach (var seg in _segments.Values)
            {
                int dim = Math.Min(seg.Dim, query.Length);
                // Compare over whole words only when both sides have them; a partial last word is masked.
                int fullWords = dim / 64;
                int tailBits = dim % 64;
                ulong tailMask = tailBits == 0 ? 0 : (1UL << tailBits) - 1;
                ScanSegment(seg, q, fullWords, tailBits, tailMask, dim, filter, topN, heap);
            }
        }
        finally { _lock.ExitReadLock(); }
        var result = new VectorCandidate[heap.Count];
        for (int i = result.Length - 1; i >= 0; i--) result[i] = heap.Dequeue();
        return result;
    }

    private static void ScanSegment(Segment seg, ulong[] q, int fullWords, int tailBits, ulong tailMask, int dim,
        VectorFilter f, int topN, PriorityQueue<VectorCandidate, long> heap)
    {
        int w = seg.Words;
        var bits = seg.Bits;
        var ts = seg.Ts;
        var kind = seg.Kind;
        var app = seg.App;
        var alive = seg.Alive;
        int worst = int.MaxValue;
        if (heap.Count == topN && heap.TryPeek(out var top, out _)) worst = top.Distance;
        ref ulong qRef = ref MemoryMarshal.GetArrayDataReference(q);
        ref ulong bRef = ref MemoryMarshal.GetArrayDataReference(bits);
        // Distances are normalised to 768 bits so rows of different dims rank together.
        int scale = 768 * 1024 / Math.Max(dim, 1);
        for (int i = 0; i < seg.Count; i++)
        {
            if (!alive[i]) continue;
            long t = ts[i];
            if (t < f.FromMs || t >= f.ToMs) continue;
            if ((kind[i] & f.KindMask) == 0) continue;
            if (f.AppId >= 0 && app[i] != f.AppId) continue;
            ref ulong row = ref Unsafe.Add(ref bRef, i * w);
            int d = Hamming(ref row, ref qRef, fullWords);
            if (tailBits != 0) d += BitOperations.PopCount((Unsafe.Add(ref row, fullWords) ^ Unsafe.Add(ref qRef, fullWords)) & tailMask);
            int nd = (int)((long)d * scale >> 10);
            if (heap.Count < topN)
            {
                heap.Enqueue(new VectorCandidate(seg.Rid[i], nd, dim), -(long)nd);
                if (heap.Count == topN && heap.TryPeek(out var tp, out _)) worst = tp.Distance;
            }
            else if (nd < worst)
            {
                heap.DequeueEnqueue(new VectorCandidate(seg.Rid[i], nd, dim), -(long)nd);
                heap.TryPeek(out var tp, out _);
                worst = tp.Distance;
            }
        }
    }

    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    private static int Hamming(ref ulong a, ref ulong b, int words)
    {
        int d = 0, j = 0;
        if (Vector256.IsHardwareAccelerated)
        {
            // XOR four words at a time in a 256-bit register, then POPCNT each lane.
            for (; j + 4 <= words; j += 4)
            {
                var x = Vector256.LoadUnsafe(ref a, (nuint)j) ^ Vector256.LoadUnsafe(ref b, (nuint)j);
                d += BitOperations.PopCount(x.GetElement(0)) + BitOperations.PopCount(x.GetElement(1))
                   + BitOperations.PopCount(x.GetElement(2)) + BitOperations.PopCount(x.GetElement(3));
            }
        }
        for (; j < words; j++) d += BitOperations.PopCount(Unsafe.Add(ref a, j) ^ Unsafe.Add(ref b, j));
        return d;
    }

    /// <summary>Cosine of the first dim components of a and b, each renormalised over that prefix.</summary>
    public static float PrefixCosine(ReadOnlySpan<float> a, ReadOnlySpan<float> b, int dim)
    {
        a = a[..dim];
        b = b[..dim];
        float dot = 0, na = 0, nb = 0;
        int i = 0;
        if (Vector256.IsHardwareAccelerated && dim >= 8)
        {
            var vd = Vector256<float>.Zero; var va = Vector256<float>.Zero; var vb = Vector256<float>.Zero;
            ref float ra = ref MemoryMarshal.GetReference(a);
            ref float rb = ref MemoryMarshal.GetReference(b);
            for (; i + 8 <= dim; i += 8)
            {
                var x = Vector256.LoadUnsafe(ref ra, (nuint)i);
                var y = Vector256.LoadUnsafe(ref rb, (nuint)i);
                vd += x * y; va += x * x; vb += y * y;
            }
            dot = Vector256.Sum(vd); na = Vector256.Sum(va); nb = Vector256.Sum(vb);
        }
        for (; i < dim; i++) { dot += a[i] * b[i]; na += a[i] * a[i]; nb += b[i] * b[i]; }
        if (na <= 0 || nb <= 0) return 0;
        return dot / MathF.Sqrt(na * nb);
    }
}
