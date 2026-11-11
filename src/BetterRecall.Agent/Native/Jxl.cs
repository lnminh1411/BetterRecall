// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;

namespace BetterRecall.Agent.Native;

/// <summary>
/// libjxl (jxl.dll + jxl_threads.dll, BSD-3-Clause, built with vcpkg: brotli, highway and a static CRT inside) through
/// NativeLibrary + unmanaged function pointers (Native AOT safe). Lossy VarDCT encode from 8-bit RGB, decode to BGRA.
/// Only the stable C API of libjxl 0.12 is used; JxlBasicInfo and JxlColorEncoding live in oversized zeroed buffers
/// filled by libjxl's own init functions, so only the fields written here need fixed offsets (decode/types.h, codestream_header.h).
/// </summary>
internal static unsafe class Jxl
{
    private const int EncSuccess = 0, EncNeedMoreOutput = 2;
    private const int DecSuccess = 0, DecError = 1, DecNeedMoreInput = 2, DecNeedImageOutBuffer = 5, DecBasicInfo = 0x40, DecFullImage = 0x1000;
    private const int TypeUint8 = 2;
    private const int SettingEffort = 0;
    private const int BasicInfoSize = 512, ColorEncodingSize = 512;
    // JxlBasicInfo offsets (x64): xsize 4, ysize 8, bits_per_sample 12, uses_original_profile 36,
    // num_color_channels 52, alpha_bits 60.
    private const int BiXsize = 4, BiYsize = 8, BiBits = 12, BiOriginalProfile = 36, BiColorChannels = 52, BiAlphaBits = 60;

    /// <summary>JxlPixelFormat: num_channels, data_type, endianness, align (size_t).</summary>
    [StructLayout(LayoutKind.Sequential)]
    private struct PixelFormat
    {
        public uint NumChannels;
        public int DataType;
        public int Endianness;
        public nuint Align;
    }

    private static delegate* unmanaged<nint, nint> _encoderCreate;
    private static delegate* unmanaged<nint, void> _encoderDestroy;
    private static delegate* unmanaged<nint, nint, nint, int> _encoderSetParallelRunner;
    private static delegate* unmanaged<byte*, void> _encoderInitBasicInfo;
    private static delegate* unmanaged<nint, byte*, int> _encoderSetBasicInfo;
    private static delegate* unmanaged<byte*, int, void> _colorEncodingSetToSrgb;
    private static delegate* unmanaged<nint, byte*, int> _encoderSetColorEncoding;
    private static delegate* unmanaged<nint, nint, nint> _frameSettingsCreate;
    private static delegate* unmanaged<nint, float, int> _setFrameDistance;
    private static delegate* unmanaged<float, float> _distanceFromQuality;
    private static delegate* unmanaged<nint, int, long, int> _frameSettingsSetOption;
    private static delegate* unmanaged<nint, PixelFormat*, void*, nuint, int> _addImageFrame;
    private static delegate* unmanaged<nint, void> _closeInput;
    private static delegate* unmanaged<nint, byte**, nuint*, int> _processOutput;

    private static delegate* unmanaged<nint, nint> _decoderCreate;
    private static delegate* unmanaged<nint, void> _decoderDestroy;
    private static delegate* unmanaged<nint, int, int> _subscribeEvents;
    private static delegate* unmanaged<nint, nint, nint, int> _decoderSetParallelRunner;
    private static delegate* unmanaged<nint, byte*, nuint, int> _setInput;
    private static delegate* unmanaged<nint, void> _decoderCloseInput;
    private static delegate* unmanaged<nint, int> _processInput;
    private static delegate* unmanaged<nint, byte*, int> _getBasicInfo;
    private static delegate* unmanaged<nint, PixelFormat*, nuint*, int> _imageOutBufferSize;
    private static delegate* unmanaged<nint, PixelFormat*, void*, nuint, int> _setImageOutBuffer;

    private static delegate* unmanaged<nint, nuint, nint> _runnerCreate;
    private static delegate* unmanaged<nint, void> _runnerDestroy;
    private static nint _runner; // JxlThreadParallelRunner, passed as the runner function

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
                nint lib = NativeLoader.Load("jxl.dll");
                nint thr = NativeLoader.Load("jxl_threads.dll");
                _encoderCreate = (delegate* unmanaged<nint, nint>)NativeLoader.Get(lib, "JxlEncoderCreate");
                _encoderDestroy = (delegate* unmanaged<nint, void>)NativeLoader.Get(lib, "JxlEncoderDestroy");
                _encoderSetParallelRunner = (delegate* unmanaged<nint, nint, nint, int>)NativeLoader.Get(lib, "JxlEncoderSetParallelRunner");
                _encoderInitBasicInfo = (delegate* unmanaged<byte*, void>)NativeLoader.Get(lib, "JxlEncoderInitBasicInfo");
                _encoderSetBasicInfo = (delegate* unmanaged<nint, byte*, int>)NativeLoader.Get(lib, "JxlEncoderSetBasicInfo");
                _colorEncodingSetToSrgb = (delegate* unmanaged<byte*, int, void>)NativeLoader.Get(lib, "JxlColorEncodingSetToSRGB");
                _encoderSetColorEncoding = (delegate* unmanaged<nint, byte*, int>)NativeLoader.Get(lib, "JxlEncoderSetColorEncoding");
                _frameSettingsCreate = (delegate* unmanaged<nint, nint, nint>)NativeLoader.Get(lib, "JxlEncoderFrameSettingsCreate");
                _setFrameDistance = (delegate* unmanaged<nint, float, int>)NativeLoader.Get(lib, "JxlEncoderSetFrameDistance");
                _distanceFromQuality = (delegate* unmanaged<float, float>)NativeLoader.Get(lib, "JxlEncoderDistanceFromQuality");
                _frameSettingsSetOption = (delegate* unmanaged<nint, int, long, int>)NativeLoader.Get(lib, "JxlEncoderFrameSettingsSetOption");
                _addImageFrame = (delegate* unmanaged<nint, PixelFormat*, void*, nuint, int>)NativeLoader.Get(lib, "JxlEncoderAddImageFrame");
                _closeInput = (delegate* unmanaged<nint, void>)NativeLoader.Get(lib, "JxlEncoderCloseInput");
                _processOutput = (delegate* unmanaged<nint, byte**, nuint*, int>)NativeLoader.Get(lib, "JxlEncoderProcessOutput");
                _decoderCreate = (delegate* unmanaged<nint, nint>)NativeLoader.Get(lib, "JxlDecoderCreate");
                _decoderDestroy = (delegate* unmanaged<nint, void>)NativeLoader.Get(lib, "JxlDecoderDestroy");
                _subscribeEvents = (delegate* unmanaged<nint, int, int>)NativeLoader.Get(lib, "JxlDecoderSubscribeEvents");
                _decoderSetParallelRunner = (delegate* unmanaged<nint, nint, nint, int>)NativeLoader.Get(lib, "JxlDecoderSetParallelRunner");
                _setInput = (delegate* unmanaged<nint, byte*, nuint, int>)NativeLoader.Get(lib, "JxlDecoderSetInput");
                _decoderCloseInput = (delegate* unmanaged<nint, void>)NativeLoader.Get(lib, "JxlDecoderCloseInput");
                _processInput = (delegate* unmanaged<nint, int>)NativeLoader.Get(lib, "JxlDecoderProcessInput");
                _getBasicInfo = (delegate* unmanaged<nint, byte*, int>)NativeLoader.Get(lib, "JxlDecoderGetBasicInfo");
                _imageOutBufferSize = (delegate* unmanaged<nint, PixelFormat*, nuint*, int>)NativeLoader.Get(lib, "JxlDecoderImageOutBufferSize");
                _setImageOutBuffer = (delegate* unmanaged<nint, PixelFormat*, void*, nuint, int>)NativeLoader.Get(lib, "JxlDecoderSetImageOutBuffer");
                _runnerCreate = (delegate* unmanaged<nint, nuint, nint>)NativeLoader.Get(thr, "JxlThreadParallelRunnerCreate");
                _runnerDestroy = (delegate* unmanaged<nint, void>)NativeLoader.Get(thr, "JxlThreadParallelRunnerDestroy");
                _runner = NativeLoader.Get(thr, "JxlThreadParallelRunner");
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

    /// <summary>Lossy JPEG XL from BGRA (alpha dropped). quality 30..95 maps through JxlEncoderDistanceFromQuality.</summary>
    public static byte[] Encode(byte* bgra, int width, int height, int stride, int quality, int effort)
    {
        if (!TryLoad()) throw new InvalidOperationException("JPEG XL codec unavailable.");
        int rgbLen = checked(width * height * 3);
        byte* rgb = (byte*)NativeMemory.Alloc((nuint)rgbLen);
        nint enc = 0, runner = 0;
        byte* basic = stackalloc byte[BasicInfoSize];
        byte* color = stackalloc byte[ColorEncodingSize];
        try
        {
            for (int y = 0; y < height; y++)
            {
                byte* s = bgra + (long)y * stride;
                byte* d = rgb + (long)y * width * 3;
                for (int x = 0; x < width; x++, s += 4, d += 3) { d[0] = s[2]; d[1] = s[1]; d[2] = s[0]; }
            }
            enc = _encoderCreate(0);
            if (enc == 0) throw new InvalidOperationException("JxlEncoderCreate failed.");
            runner = _runnerCreate(0, (nuint)Math.Clamp(Environment.ProcessorCount / 2, 1, 4));
            if (runner != 0) Check(_encoderSetParallelRunner(enc, _runner, runner), "runner");
            new Span<byte>(basic, BasicInfoSize).Clear();
            _encoderInitBasicInfo(basic);
            *(uint*)(basic + BiXsize) = (uint)width;
            *(uint*)(basic + BiYsize) = (uint)height;
            *(uint*)(basic + BiBits) = 8;
            *(int*)(basic + BiOriginalProfile) = 0;
            *(uint*)(basic + BiColorChannels) = 3;
            *(uint*)(basic + BiAlphaBits) = 0;
            Check(_encoderSetBasicInfo(enc, basic), "basic info");
            new Span<byte>(color, ColorEncodingSize).Clear();
            _colorEncodingSetToSrgb(color, 0);
            Check(_encoderSetColorEncoding(enc, color), "color");
            nint settings = _frameSettingsCreate(enc, 0);
            Check(_setFrameDistance(settings, _distanceFromQuality(quality)), "distance");
            Check(_frameSettingsSetOption(settings, SettingEffort, Math.Clamp(effort, 1, 9)), "effort");
            var fmt = new PixelFormat { NumChannels = 3, DataType = TypeUint8, Endianness = 0, Align = 0 };
            Check(_addImageFrame(settings, &fmt, rgb, (nuint)rgbLen), "frame");
            _closeInput(enc);
            var output = new byte[Math.Max(64 * 1024, rgbLen / 8)];
            long used = 0;
            while (true)
            {
                int status;
                fixed (byte* o = output)
                {
                    byte* next = o + used;
                    nuint avail = (nuint)(output.Length - used);
                    status = _processOutput(enc, &next, &avail);
                    used = next - o;
                }
                if (status == EncSuccess) break;
                if (status != EncNeedMoreOutput) throw new InvalidOperationException("JPEG XL encode failed.");
                var bigger = new byte[output.Length * 2];
                Buffer.BlockCopy(output, 0, bigger, 0, (int)used);
                Array.Clear(output);
                output = bigger;
            }
            var result = output.AsSpan(0, (int)used).ToArray();
            Array.Clear(output);
            return result;
        }
        finally
        {
            if (enc != 0) _encoderDestroy(enc);
            if (runner != 0) _runnerDestroy(runner);
            NativeMemory.Clear(rgb, (nuint)rgbLen);
            NativeMemory.Free(rgb);
        }
    }

    /// <summary>Decodes to BGRA (stride width * 4). The caller clears the pixels after use.</summary>
    public static (byte[] Bgra, int Width, int Height) Decode(ReadOnlySpan<byte> data)
    {
        if (!TryLoad()) throw new InvalidOperationException("JPEG XL codec unavailable.");
        nint dec = 0, runner = 0;
        byte* basic = stackalloc byte[BasicInfoSize];
        byte[]? pixels = null;
        int width = 0, height = 0;
        try
        {
            dec = _decoderCreate(0);
            if (dec == 0) throw new InvalidOperationException("JxlDecoderCreate failed.");
            runner = _runnerCreate(0, (nuint)Math.Clamp(Environment.ProcessorCount / 2, 1, 4));
            if (runner != 0) _decoderSetParallelRunner(dec, _runner, runner);
            if (_subscribeEvents(dec, DecBasicInfo | DecFullImage) != DecSuccess) throw new InvalidDataException("JPEG XL decoder setup failed.");
            var fmt = new PixelFormat { NumChannels = 4, DataType = TypeUint8, Endianness = 0, Align = 0 };
            fixed (byte* input = data)
            {
                if (_setInput(dec, input, (nuint)data.Length) != DecSuccess) throw new InvalidDataException("JPEG XL input rejected.");
                _decoderCloseInput(dec);
                byte* outPtr = null;
                GCHandle pin = default;
                try
                {
                    while (true)
                    {
                        int st = _processInput(dec);
                        if (st == DecError || st == DecNeedMoreInput) throw new InvalidDataException("JPEG XL decode failed.");
                        if (st == DecBasicInfo)
                        {
                            new Span<byte>(basic, BasicInfoSize).Clear();
                            if (_getBasicInfo(dec, basic) != DecSuccess) throw new InvalidDataException("JPEG XL header unreadable.");
                            width = (int)*(uint*)(basic + BiXsize);
                            height = (int)*(uint*)(basic + BiYsize);
                            if (width <= 0 || height <= 0 || (long)width * height > 64L * 1024 * 1024) throw new InvalidDataException("JPEG XL size.");
                        }
                        else if (st == DecNeedImageOutBuffer)
                        {
                            nuint size;
                            if (_imageOutBufferSize(dec, &fmt, &size) != DecSuccess) throw new InvalidDataException("JPEG XL buffer size.");
                            if ((long)size != (long)width * height * 4) throw new InvalidDataException("JPEG XL buffer size.");
                            pixels = new byte[(int)size];
                            pin = GCHandle.Alloc(pixels, GCHandleType.Pinned);
                            outPtr = (byte*)pin.AddrOfPinnedObject();
                            if (_setImageOutBuffer(dec, &fmt, outPtr, size) != DecSuccess) throw new InvalidDataException("JPEG XL output rejected.");
                        }
                        else if (st == DecFullImage || st == DecSuccess)
                        {
                            if (pixels is null) throw new InvalidDataException("JPEG XL has no image.");
                            break;
                        }
                    }
                }
                finally { if (pin.IsAllocated) pin.Free(); }
            }
            // RGBA to BGRA in place.
            for (int i = 0; i < pixels.Length; i += 4) (pixels[i], pixels[i + 2]) = (pixels[i + 2], pixels[i]);
            var result = (pixels, width, height);
            pixels = null;
            return result;
        }
        finally
        {
            if (pixels is not null) Array.Clear(pixels);
            if (dec != 0) _decoderDestroy(dec);
            if (runner != 0) _runnerDestroy(runner);
        }
    }

    private static void Check(int status, string what)
    {
        if (status != EncSuccess) throw new InvalidOperationException($"JPEG XL encoder: {what} failed.");
    }
}

/// <summary>Loads a native DLL from the install folder (next to the exe) only.</summary>
internal static class NativeLoader
{
    public static nint Load(string name)
    {
        var path = Path.Combine(AppContext.BaseDirectory, name);
        if (!File.Exists(path)) throw new DllNotFoundException($"{name} not found");
        return NativeLibrary.Load(path);
    }

    public static nint Get(nint lib, string name) =>
        NativeLibrary.TryGetExport(lib, name, out var p) ? p : throw new EntryPointNotFoundException($"missing export {name}");
}
