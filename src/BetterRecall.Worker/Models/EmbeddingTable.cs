// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.IO.MemoryMappedFiles;

namespace BetterRecall.Worker.Models;

/// <summary>Token embedding table read through a memory map: only the rows a request touches become resident.
/// bf16 rows are widened exactly to float32, then multiplied by Scale (as the PyTorch float32 path does).</summary>
public sealed unsafe class EmbeddingTable : IDisposable
{
    private readonly MemoryMappedFile _file;
    private readonly MemoryMappedViewAccessor _view;
    private readonly byte* _base;
    private readonly bool _bf16;

    public int Rows { get; }
    public int Dim { get; }
    public float Scale { get; }

    public EmbeddingTable(string path, EmbeddingTableSpec spec)
    {
        _bf16 = spec.DType switch { "bf16" => true, "f32" => false, _ => throw new InvalidDataException($"embedding dtype {spec.DType}") };
        Rows = spec.Rows; Dim = spec.Dim; Scale = spec.Scale;
        long expect = (long)Rows * Dim * (_bf16 ? 2 : 4);
        if (new FileInfo(path).Length != expect) throw new InvalidDataException("embedding table size does not match the manifest");
        _file = MemoryMappedFile.CreateFromFile(path, FileMode.Open, null, 0, MemoryMappedFileAccess.Read);
        _view = _file.CreateViewAccessor(0, 0, MemoryMappedFileAccess.Read);
        byte* p = null;
        _view.SafeMemoryMappedViewHandle.AcquirePointer(ref p);
        _base = p + _view.PointerOffset;
    }

    /// <summary>Writes the rows of <paramref name="ids"/> into <paramref name="dst"/> (ids.Length * Dim floats).</summary>
    public void Gather(ReadOnlySpan<long> ids, Span<float> dst)
    {
        for (int t = 0; t < ids.Length; t++)
        {
            long id = ids[t];
            if ((ulong)id >= (ulong)Rows) throw new InvalidDataException("token id outside the embedding table");
            var row = dst.Slice(t * Dim, Dim);
            if (_bf16)
            {
                var src = new ReadOnlySpan<ushort>(_base + id * Dim * 2, Dim);
                for (int i = 0; i < Dim; i++) row[i] = BitConverter.Int32BitsToSingle(src[i] << 16) * Scale;
            }
            else
            {
                var src = new ReadOnlySpan<float>(_base + id * Dim * 4, Dim);
                for (int i = 0; i < Dim; i++) row[i] = src[i] * Scale;
            }
        }
    }

    public void Dispose()
    {
        _view.SafeMemoryMappedViewHandle.ReleasePointer();
        _view.Dispose();
        _file.Dispose();
    }
}
