// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Worker.Vectors;

public static class VectorMath
{
    /// <summary>Matryoshka: keep the first <paramref name="dim"/> values, then L2-renormalise.</summary>
    public static float[] Matryoshka(ReadOnlySpan<float> full, int dim)
    {
        if (dim <= 0 || dim > full.Length) throw new ArgumentOutOfRangeException(nameof(dim), $"dim {dim} not in 1..{full.Length}");
        var v = full[..dim].ToArray();
        L2NormalizeInPlace(v);
        return v;
    }

    public static void L2NormalizeInPlace(Span<float> v)
    {
        double sum = 0;
        foreach (var x in v) sum += (double)x * x;
        if (sum <= 0 || double.IsNaN(sum) || double.IsInfinity(sum)) return;
        float inv = (float)(1.0 / Math.Sqrt(sum));
        for (int i = 0; i < v.Length; i++) v[i] *= inv;
    }

    public static bool HasNonFinite(ReadOnlySpan<float> v)
    {
        foreach (var x in v) if (!float.IsFinite(x)) return true;
        return false;
    }

    /// <summary>Cosine similarity in double precision. NaN when either side is zero or not finite.</summary>
    public static double Cosine(ReadOnlySpan<float> a, ReadOnlySpan<float> b)
    {
        if (a.Length != b.Length) throw new ArgumentException("Length mismatch.");
        if (HasNonFinite(a) || HasNonFinite(b)) return double.NaN;
        double dot = 0, na = 0, nb = 0;
        for (int i = 0; i < a.Length; i++) { dot += (double)a[i] * b[i]; na += (double)a[i] * a[i]; nb += (double)b[i] * b[i]; }
        if (na == 0 || nb == 0) return double.NaN;
        return dot / Math.Sqrt(na * nb);
    }

}
