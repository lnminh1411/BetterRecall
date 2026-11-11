// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Buffers.Binary;
using System.Security.Cryptography;
using BetterRecall.Contracts;

namespace BetterRecall.Core.Crypto;

/// <summary>
/// AES-256-GCM file encryption in 64 KiB chunks. Key = HKDF(data key, "betterrecall-blob").
/// File: header "BRB1" | version 1 | 3 reserved | 12-byte random file nonce, then chunks of ciphertext+16-byte tag.
/// Chunk nonce = file nonce XOR big-endian chunk index in the last 8 bytes. AAD = header | final flag, so a file
/// cut at a chunk boundary fails on its new last chunk.
/// </summary>
public sealed class BlobCipher : IBlobCipher
{
    internal const int ChunkBytes = 64 * 1024;
    internal const int TagBytes = 16;
    internal const int HeaderBytes = 20;
    internal static readonly byte[] Magic = "BRB1"u8.ToArray();

    private readonly Func<byte[]> _key;

    /// <summary>keyProvider returns a fresh copy of the blob key each call; the cipher zeroes it after use.</summary>
    public BlobCipher(Func<byte[]> keyProvider) => _key = keyProvider;

    internal static BlobCipher FromVault(KeyVault vault) => new(() => vault.Derive("betterrecall-blob"));

    public Stream OpenWrite(string path)
    {
        var dir = Path.GetDirectoryName(Path.GetFullPath(path));
        if (dir != null) Directory.CreateDirectory(dir);
        var key = _key();
        var fs = new FileStream(path, FileMode.Create, FileAccess.Write, FileShare.None, 1, FileOptions.None);
        try { return new EncryptStream(fs, key); }
        catch { fs.Dispose(); CryptographicOperations.ZeroMemory(key); throw; }
    }

    public Stream OpenRead(string path)
    {
        var key = _key();
        var fs = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read, 1, FileOptions.SequentialScan);
        try { return new DecryptStream(fs, key); }
        catch { fs.Dispose(); CryptographicOperations.ZeroMemory(key); throw; }
    }

    internal static void ChunkNonce(ReadOnlySpan<byte> fileNonce, long index, Span<byte> nonce)
    {
        fileNonce.CopyTo(nonce);
        Span<byte> idx = stackalloc byte[8];
        BinaryPrimitives.WriteInt64BigEndian(idx, index);
        for (int i = 0; i < 8; i++) nonce[4 + i] ^= idx[i];
    }

    internal static void Aad(ReadOnlySpan<byte> header, bool final, Span<byte> aad)
    {
        header.CopyTo(aad);
        aad[HeaderBytes] = final ? (byte)1 : (byte)0;
    }

    private sealed class EncryptStream : Stream
    {
        private readonly FileStream _fs;
        private readonly AesGcm _gcm;
        private readonly byte[] _key;
        private readonly byte[] _header = new byte[HeaderBytes];
        private readonly byte[] _buf = new byte[ChunkBytes];
        private readonly byte[] _out = new byte[ChunkBytes + TagBytes];
        private int _fill;
        private long _index;
        private long _length;
        private bool _done;

        public EncryptStream(FileStream fs, byte[] key)
        {
            _fs = fs;
            _key = key;
            _gcm = new AesGcm(key, TagBytes);
            Magic.CopyTo(_header, 0);
            _header[4] = 1;
            RandomNumberGenerator.Fill(_header.AsSpan(8, 12));
            _fs.Write(_header);
        }

        public override void Write(byte[] buffer, int offset, int count) => Write(buffer.AsSpan(offset, count));

        public override void Write(ReadOnlySpan<byte> data)
        {
            ObjectDisposedException.ThrowIf(_done, this);
            while (data.Length > 0)
            {
                if (_fill == ChunkBytes) EmitChunk(final: false); // only emit once more data is known to follow
                int n = Math.Min(ChunkBytes - _fill, data.Length);
                data[..n].CopyTo(_buf.AsSpan(_fill));
                _fill += n;
                _length += n;
                data = data[n..];
            }
        }

        private void EmitChunk(bool final)
        {
            Span<byte> nonce = stackalloc byte[12];
            Span<byte> aad = stackalloc byte[HeaderBytes + 1];
            ChunkNonce(_header.AsSpan(8, 12), _index, nonce);
            Aad(_header, final, aad);
            _gcm.Encrypt(nonce, _buf.AsSpan(0, _fill), _out.AsSpan(0, _fill), _out.AsSpan(_fill, TagBytes), aad);
            _fs.Write(_out, 0, _fill + TagBytes);
            CryptographicOperations.ZeroMemory(_buf.AsSpan(0, _fill));
            _index++;
            _fill = 0;
        }

        protected override void Dispose(bool disposing)
        {
            if (!_done)
            {
                _done = true;
                try
                {
                    EmitChunk(final: true);
                    _fs.Flush(flushToDisk: true);
                }
                finally
                {
                    _fs.Dispose();
                    _gcm.Dispose();
                    CryptographicOperations.ZeroMemory(_key);
                }
            }
            base.Dispose(disposing);
        }

        public override void Flush() { }
        public override bool CanRead => false;
        public override bool CanSeek => false;
        public override bool CanWrite => !_done;
        public override long Length => _length;
        public override long Position { get => _length; set => throw new NotSupportedException(); }
        public override int Read(byte[] buffer, int offset, int count) => throw new NotSupportedException();
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
    }

    private sealed class DecryptStream : Stream
    {
        private readonly FileStream _fs;
        private readonly AesGcm _gcm;
        private readonly byte[] _key;
        private readonly byte[] _header = new byte[HeaderBytes];
        private readonly byte[] _cur = new byte[ChunkBytes + TagBytes];
        private readonly byte[] _next = new byte[ChunkBytes + TagBytes];
        private readonly byte[] _plain = new byte[ChunkBytes];
        private int _nextLen;
        private int _plainLen;
        private int _plainPos;
        private long _index;
        private bool _finalSeen;
        private long _position;

        public DecryptStream(FileStream fs, byte[] key)
        {
            _fs = fs;
            _key = key;
            _gcm = new AesGcm(key, TagBytes);
            if (ReadFull(_header) != HeaderBytes || !_header.AsSpan(0, 4).SequenceEqual(Magic) || _header[4] != 1)
                throw new CryptographicException("Blob is damaged.");
            _nextLen = ReadFull(_next);
            if (_nextLen < TagBytes) throw new CryptographicException("Blob is damaged.");
        }

        private int ReadFull(Span<byte> buf)
        {
            int total = 0;
            while (total < buf.Length)
            {
                int n = _fs.Read(buf[total..]);
                if (n == 0) break;
                total += n;
            }
            return total;
        }

        private bool NextChunk()
        {
            if (_finalSeen) return false;
            int curLen = _nextLen;
            _next.AsSpan(0, curLen).CopyTo(_cur);
            _nextLen = curLen == _cur.Length ? ReadFull(_next) : 0;
            bool final = _nextLen == 0;
            if (!final && _nextLen < TagBytes) throw new CryptographicException("Blob is damaged.");
            Span<byte> nonce = stackalloc byte[12];
            Span<byte> aad = stackalloc byte[HeaderBytes + 1];
            ChunkNonce(_header.AsSpan(8, 12), _index, nonce);
            Aad(_header, final, aad);
            int len = curLen - TagBytes;
            try { _gcm.Decrypt(nonce, _cur.AsSpan(0, len), _cur.AsSpan(len, TagBytes), _plain.AsSpan(0, len), aad); }
            catch (AuthenticationTagMismatchException) { throw new CryptographicException("Blob is damaged."); }
            _plainLen = len;
            _plainPos = 0;
            _index++;
            _finalSeen = final;
            return true;
        }

        public override int Read(byte[] buffer, int offset, int count) => Read(buffer.AsSpan(offset, count));

        public override int Read(Span<byte> dest)
        {
            if (dest.Length == 0) return 0;
            while (_plainPos == _plainLen)
                if (!NextChunk()) return 0;
            int n = Math.Min(dest.Length, _plainLen - _plainPos);
            _plain.AsSpan(_plainPos, n).CopyTo(dest);
            _plainPos += n;
            _position += n;
            return n;
        }

        protected override void Dispose(bool disposing)
        {
            if (disposing)
            {
                _fs.Dispose();
                _gcm.Dispose();
                CryptographicOperations.ZeroMemory(_key);
                CryptographicOperations.ZeroMemory(_plain);
            }
            base.Dispose(disposing);
        }

        public override void Flush() { }
        public override bool CanRead => true;
        public override bool CanSeek => false;
        public override bool CanWrite => false;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => _position; set => throw new NotSupportedException(); }
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
    }
}
