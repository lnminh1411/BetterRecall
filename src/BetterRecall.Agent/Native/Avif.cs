// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
namespace BetterRecall.Agent.Native;

/// <summary>
/// libavif 1.4 (avif.dll: aom encoder, dav1d decoder, libyuv, static CRT; BSD-2-Clause) through NativeLibrary +
/// unmanaged function pointers (Native AOT safe). Struct offsets follow avif.h of libavif 1.4.2 (x64); only fields at
/// the start of each struct are touched, which have not moved since libavif 1.0.
/// </summary>
internal static unsafe class Avif
{
    private const int ResultOk = 0;
    private const int PixelYuv420 = 3;
    private const int RgbFormatBgra = 4;
    private const int RgbImageSize = 64;
    // avifRGBImage: width 0, height 4, depth 8, format 12, ..., maxThreads 40, pixels 48, rowBytes 56.
    private const int RgbDepth = 8, RgbFormat = 12, RgbIgnoreAlpha = 28, RgbMaxThreads = 40, RgbPixels = 48, RgbRowBytes = 56;
    // avifEncoder: codecChoice 0, maxThreads 4, speed 8, ..., quality 32, qualityAlpha 36.
    private const int EncMaxThreads = 4, EncSpeed = 8, EncQuality = 32;
    // avifDecoder: codecChoice 0, maxThreads 4.
    private const int DecMaxThreads = 4;

    private static delegate* unmanaged<uint, uint, uint, int, nint> _imageCreate;
    private static delegate* unmanaged<nint> _imageCreateEmpty;
    private static delegate* unmanaged<nint, void> _imageDestroy;
    private static delegate* unmanaged<byte*, nint, void> _rgbSetDefaults;
    private static delegate* unmanaged<nint, byte*, int> _rgbToYuv;
    private static delegate* unmanaged<nint, byte*, int> _yuvToRgb;
    private static delegate* unmanaged<nint> _encoderCreate;
    private static delegate* unmanaged<nint, void> _encoderDestroy;
    private static delegate* unmanaged<nint, nint, RwData*, int> _encoderWrite;
    private static delegate* unmanaged<RwData*, void> _rwDataFree;
    private static delegate* unmanaged<nint> _decoderCreate;
    private static delegate* unmanaged<nint, void> _decoderDestroy;
    private static delegate* unmanaged<nint, nint, byte*, nuint, int> _decoderReadMemory;

    private struct RwData
    {
        public byte* Data;
        public nuint Size;
    }

    private static readonly object Gate = new();
    private static int _state;
    public static string? LoadError { get; private set; }

    public static bool TryLoad()
    {
        lock (Gate)
        {
            if (_state != 0) return _state > 0;
            try
            {
                nint lib = NativeLoader.Load("avif.dll");
                _imageCreate = (delegate* unmanaged<uint, uint, uint, int, nint>)NativeLoader.Get(lib, "avifImageCreate");
                _imageCreateEmpty = (delegate* unmanaged<nint>)NativeLoader.Get(lib, "avifImageCreateEmpty");
                _imageDestroy = (delegate* unmanaged<nint, void>)NativeLoader.Get(lib, "avifImageDestroy");
                _rgbSetDefaults = (delegate* unmanaged<byte*, nint, void>)NativeLoader.Get(lib, "avifRGBImageSetDefaults");
                _rgbToYuv = (delegate* unmanaged<nint, byte*, int>)NativeLoader.Get(lib, "avifImageRGBToYUV");
                _yuvToRgb = (delegate* unmanaged<nint, byte*, int>)NativeLoader.Get(lib, "avifImageYUVToRGB");
                _encoderCreate = (delegate* unmanaged<nint>)NativeLoader.Get(lib, "avifEncoderCreate");
                _encoderDestroy = (delegate* unmanaged<nint, void>)NativeLoader.Get(lib, "avifEncoderDestroy");
                _encoderWrite = (delegate* unmanaged<nint, nint, RwData*, int>)NativeLoader.Get(lib, "avifEncoderWrite");
                _rwDataFree = (delegate* unmanaged<RwData*, void>)NativeLoader.Get(lib, "avifRWDataFree");
                _decoderCreate = (delegate* unmanaged<nint>)NativeLoader.Get(lib, "avifDecoderCreate");
                _decoderDestroy = (delegate* unmanaged<nint, void>)NativeLoader.Get(lib, "avifDecoderDestroy");
                _decoderReadMemory = (delegate* unmanaged<nint, nint, byte*, nuint, int>)NativeLoader.Get(lib, "avifDecoderReadMemory");
                _state = 1;
            }
            catch (Exception ex)
            {
                LoadError = ex.Message;
                _state = -1;
            }
            return _state > 0;
        }
    }

    private static int Threads => Math.Clamp(Environment.ProcessorCount / 2, 1, 4);

    /// <summary>8-bit 4:2:0 AVIF from BGRA (alpha ignored). quality 30..95 is libavif's 0..100 quality; speed 0..10.</summary>
    public static byte[] Encode(byte* bgra, int width, int height, int stride, int quality, int speed)
    {
        if (!TryLoad()) throw new InvalidOperationException("AVIF codec unavailable.");
        nint image = 0, enc = 0;
        var output = new RwData();
        byte* rgb = stackalloc byte[RgbImageSize];
        try
        {
            image = _imageCreate((uint)width, (uint)height, 8, PixelYuv420);
            if (image == 0) throw new InvalidOperationException("avifImageCreate failed.");
            new Span<byte>(rgb, RgbImageSize).Clear();
            _rgbSetDefaults(rgb, image);
            *(int*)(rgb + RgbFormat) = RgbFormatBgra;
            *(int*)(rgb + RgbIgnoreAlpha) = 1;
            *(int*)(rgb + RgbMaxThreads) = Threads;
            *(byte**)(rgb + RgbPixels) = bgra;
            *(uint*)(rgb + RgbRowBytes) = (uint)stride;
            if (_rgbToYuv(image, rgb) != ResultOk) throw new InvalidOperationException("AVIF colour conversion failed.");
            enc = _encoderCreate();
            if (enc == 0) throw new InvalidOperationException("avifEncoderCreate failed.");
            *(int*)(enc + EncMaxThreads) = Threads;
            *(int*)(enc + EncSpeed) = Math.Clamp(speed, 0, 10);
            *(int*)(enc + EncQuality) = Math.Clamp(quality, 0, 100);
            if (_encoderWrite(enc, image, &output) != ResultOk || output.Data == null) throw new InvalidOperationException("AVIF encode failed.");
            return new ReadOnlySpan<byte>(output.Data, checked((int)output.Size)).ToArray();
        }
        finally
        {
            if (output.Data != null) _rwDataFree(&output);
            if (enc != 0) _encoderDestroy(enc);
            if (image != 0) _imageDestroy(image); // frees the YUV planes (libavif clears nothing; the planes hold a converted copy)
        }
    }

    /// <summary>Decodes to BGRA (stride width * 4). The caller clears the pixels after use.</summary>
    public static (byte[] Bgra, int Width, int Height) Decode(ReadOnlySpan<byte> data)
    {
        if (!TryLoad()) throw new InvalidOperationException("AVIF codec unavailable.");
        nint dec = 0, image = 0;
        byte* rgb = stackalloc byte[RgbImageSize];
        try
        {
            dec = _decoderCreate();
            image = _imageCreateEmpty();
            if (dec == 0 || image == 0) throw new InvalidOperationException("avifDecoderCreate failed.");
            *(int*)(dec + DecMaxThreads) = Threads;
            fixed (byte* p = data)
                if (_decoderReadMemory(dec, image, p, (nuint)data.Length) != ResultOk) throw new InvalidDataException("AVIF decode failed.");
            int width = (int)*(uint*)image, height = (int)*(uint*)(image + 4);
            if (width <= 0 || height <= 0 || (long)width * height > 64L * 1024 * 1024) throw new InvalidDataException("AVIF size.");
            var pixels = new byte[width * height * 4];
            fixed (byte* px = pixels)
            {
                new Span<byte>(rgb, RgbImageSize).Clear();
                _rgbSetDefaults(rgb, image);
                *(uint*)(rgb + RgbDepth) = 8;
                *(int*)(rgb + RgbFormat) = RgbFormatBgra;
                *(int*)(rgb + RgbMaxThreads) = Threads;
                *(byte**)(rgb + RgbPixels) = px;
                *(uint*)(rgb + RgbRowBytes) = (uint)(width * 4);
                if (_yuvToRgb(image, rgb) != ResultOk)
                {
                    Array.Clear(pixels);
                    throw new InvalidDataException("AVIF colour conversion failed.");
                }
            }
            // depth 8 output: alpha is 255 for images without alpha.
            return (pixels, width, height);
        }
        finally
        {
            if (image != 0) _imageDestroy(image);
            if (dec != 0) _decoderDestroy(dec);
        }
    }
}
