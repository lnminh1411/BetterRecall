// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Security.Cryptography;

namespace BetterRecall.Core.Store;

/// <summary>ULID-style ids: kind prefix, 48-bit ms timestamp and 80 random bits in Crockford base32.</summary>
internal static class Ids
{
    private const string Alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    public static string New(char prefix, DateTimeOffset ts)
    {
        Span<byte> b = stackalloc byte[16];
        ulong ms = (ulong)Math.Max(0, ts.ToUnixTimeMilliseconds());
        for (int i = 5; i >= 0; i--) { b[i] = (byte)(ms & 0xFF); ms >>= 8; }
        RandomNumberGenerator.Fill(b[6..]);
        Span<char> c = stackalloc char[28];
        c[0] = prefix;
        c[1] = '_';
        // 128 bits -> 26 base32 chars (first char carries 3 bits).
        UInt128 v = 0;
        for (int i = 0; i < 16; i++) v = (v << 8) | b[i];
        for (int i = 27; i >= 2; i--) { c[i] = Alphabet[(int)(v & 31)]; v >>= 5; }
        return new string(c);
    }
}
