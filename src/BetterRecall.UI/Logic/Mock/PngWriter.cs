// SPDX-License-Identifier: MIT
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.IO.Compression;
using System.Text;

namespace BetterRecall.UI.Logic.Mock;

/// <summary>Minimal RGB PNG encoder for mock previews. No dependencies.</summary>
public static class PngWriter
{
    private static readonly uint[] CrcTable = BuildCrc();

    public static byte[] Encode(int width, int height, Func<int, int, (byte R, byte G, byte B)> pixel)
    {
        var raw = new byte[(width * 3 + 1) * height];
        int o = 0;
        for (int y = 0; y < height; y++)
        {
            raw[o++] = 0; // filter: none
            for (int x = 0; x < width; x++)
            {
                var (r, g, b) = pixel(x, y);
                raw[o++] = r; raw[o++] = g; raw[o++] = b;
            }
        }
        byte[] idat;
        using (var ms = new MemoryStream())
        {
            using (var z = new ZLibStream(ms, CompressionLevel.Fastest, leaveOpen: true)) z.Write(raw);
            idat = ms.ToArray();
        }

        using var png = new MemoryStream();
        png.Write([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        var ihdr = new byte[13];
        BinaryPrimitives.WriteInt32BigEndian(ihdr, width);
        BinaryPrimitives.WriteInt32BigEndian(ihdr.AsSpan(4), height);
        ihdr[8] = 8; ihdr[9] = 2; // 8-bit RGB
        Chunk(png, "IHDR", ihdr);
        Chunk(png, "IDAT", idat);
        Chunk(png, "IEND", []);
        return png.ToArray();
    }

    private static void Chunk(Stream s, string type, byte[] data)
    {
        Span<byte> len = stackalloc byte[4];
        BinaryPrimitives.WriteInt32BigEndian(len, data.Length);
        s.Write(len);
        var t = Encoding.ASCII.GetBytes(type);
        s.Write(t);
        s.Write(data);
        uint crc = 0xFFFFFFFF;
        foreach (var b in t) crc = CrcTable[(crc ^ b) & 0xFF] ^ (crc >> 8);
        foreach (var b in data) crc = CrcTable[(crc ^ b) & 0xFF] ^ (crc >> 8);
        BinaryPrimitives.WriteUInt32BigEndian(len, crc ^ 0xFFFFFFFF);
        s.Write(len);
    }

    private static uint[] BuildCrc()
    {
        var t = new uint[256];
        for (uint n = 0; n < 256; n++)
        {
            uint c = n;
            for (int k = 0; k < 8; k++) c = (c & 1) != 0 ? 0xEDB88320 ^ (c >> 1) : c >> 1;
            t[n] = c;
        }
        return t;
    }
}
