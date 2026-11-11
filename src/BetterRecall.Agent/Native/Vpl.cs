// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;
using BetterRecall.Agent.Util;

namespace BetterRecall.Agent.Native;

/// <summary>mfxVariant passed by value to MFXSetConfigFilterProperty (16 bytes on x64).</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct MfxVariant
{
    public ushort Version;
    public int Type;
    public ulong Data;

    public static MfxVariant U32(uint v) => new() { Version = 0x0100, Type = 5, Data = v };
}

/// <summary>
/// oneVPL dispatcher (libvpl.dll, Intel, MIT) bound through NativeLibrary + unmanaged function pointers (AOT-safe,
/// no ffmpeg). The dispatcher finds the GPU runtime (libmfx64-gen.dll) that ships with the Intel graphics driver.
/// Struct layouts follow the oneVPL 2.x headers (mfxstructures.h, x64); only the fields used here are named.
/// </summary>
internal static unsafe class Vpl
{
    public const int ErrNone = 0, ErrUnsupported = -3, ErrNotFound = -9, ErrMoreData = -10, ErrDeviceLost = -13;
    public const int WrnInExecution = 1, WrnDeviceBusy = 2;
    public const uint ImplTypeHardware = 2, AccelD3D11 = 0x0300, ImplCapsDeviceIdExtended = 4;
    public const int HandleD3D11Device = 3;
    public const uint CodecAv1 = 'A' | ('V' << 8) | ('1' << 16) | (' ' << 24);
    public const uint FourccNv12 = 'N' | ('V' << 8) | ('1' << 16) | ('2' << 24);
    public const ushort FrameTypeI = 0x1, FrameTypeRef = 0x40, FrameTypeIdr = 0x80;
    public const int ResourceDx11Texture = 5;
    public const uint MapWrite = 2;

    // mfxVideoParam (208 bytes) offsets; mfxInfoMFX starts at 16, its mfxFrameInfo at 48.
    public const int ParSize = 208;
    public const int P_AsyncDepth = 14, P_LowPower = 44, P_BrcMultiplier = 46;
    public const int P_BitDepthLuma = 66, P_BitDepthChroma = 68;
    public const int P_FourCC = 80, P_Width = 84, P_Height = 86, P_CropX = 88, P_CropY = 90, P_CropW = 92, P_CropH = 94;
    public const int P_FrameRateN = 96, P_FrameRateD = 100, P_AspectW = 106, P_AspectH = 108, P_PicStruct = 110, P_ChromaFormat = 112;
    public const int P_CodecId = 116, P_CodecProfile = 120, P_CodecLevel = 122;
    public const int P_TargetUsage = 126, P_GopPicSize = 128, P_GopRefDist = 130, P_GopOptFlag = 132, P_IdrInterval = 134;
    public const int P_RateControl = 136, P_BufferSizeKB = 140, P_TargetKbps = 142, P_MaxKbps = 144, P_NumRefFrame = 148;
    public const int P_IOPattern = 186;

    // mfxBitstream (72 bytes): Data 40, DataOffset 48, DataLength 52, MaxLength 56, FrameType 62.
    public const int BsSize = 72, Bs_Data = 40, Bs_DataOffset = 48, Bs_DataLength = 52, Bs_MaxLength = 56, Bs_FrameType = 62;
    // mfxEncodeCtrl (56 bytes): FrameType 32.
    public const int CtrlSize = 56, Ctrl_FrameType = 32;
    // mfxFrameSurface1: FrameInterface 0, Data at 88 -> PitchHigh 118, Pitch(Low) 134, Y 136, UV 144.
    public const int S_PitchHigh = 118, S_PitchLow = 134, S_Y = 136, S_UV = 144;
    // mfxFrameSurfaceInterface: Release 24, Map 40, Unmap 48, GetNativeHandle 56.
    public const int FI_Release = 24, FI_Map = 40, FI_Unmap = 48, FI_GetNativeHandle = 56;

    public static delegate* unmanaged<nint> MFXLoad;
    public static delegate* unmanaged<nint, void> MFXUnload;
    public static delegate* unmanaged<nint, nint> MFXCreateConfig;
    public static delegate* unmanaged<nint, byte*, MfxVariant, int> MFXSetConfigFilterProperty;
    public static delegate* unmanaged<nint, uint, uint, nint*, int> MFXEnumImplementations;
    public static delegate* unmanaged<nint, nint, int> MFXDispReleaseImplDescription;
    public static delegate* unmanaged<nint, uint, nint*, int> MFXCreateSession;
    public static delegate* unmanaged<nint, int> MFXClose;
    public static delegate* unmanaged<nint, int, nint, int> MFXVideoCORE_SetHandle;
    public static delegate* unmanaged<nint, nint, uint, int> MFXVideoCORE_SyncOperation;
    public static delegate* unmanaged<nint, byte*, byte*, int> MFXVideoENCODE_Query;
    public static delegate* unmanaged<nint, byte*, int> MFXVideoENCODE_Init;
    public static delegate* unmanaged<nint, int> MFXVideoENCODE_Close;
    public static delegate* unmanaged<nint, byte*, int> MFXVideoENCODE_GetVideoParam;
    public static delegate* unmanaged<nint, byte*, byte*, byte*, nint*, int> MFXVideoENCODE_EncodeFrameAsync;
    public static delegate* unmanaged<nint, byte**, int> MFXMemory_GetSurfaceForEncode;

    private static readonly object Gate = new();
    private static int _state; // 0 untried, 1 loaded, -1 failed
    public static string? LoadError { get; private set; }
    public static string? LoadedFrom { get; private set; }

    /// <summary>Loads libvpl.dll from the install folder first, then the system search path.</summary>
    public static bool TryLoad()
    {
        lock (Gate)
        {
            if (_state != 0) return _state > 0;
            nint lib = 0;
            var local = Path.Combine(AppContext.BaseDirectory, "libvpl.dll");
            if (File.Exists(local) && NativeLibrary.TryLoad(local, out lib)) LoadedFrom = local;
            else if (NativeLibrary.TryLoad("libvpl.dll", typeof(Vpl).Assembly, DllImportSearchPath.System32, out lib)) LoadedFrom = "system32";
            if (lib == 0)
            {
                LoadError = "libvpl.dll not found";
                _state = -1;
                return false;
            }
            try
            {
                MFXLoad = (delegate* unmanaged<nint>)Get(lib, "MFXLoad");
                MFXUnload = (delegate* unmanaged<nint, void>)Get(lib, "MFXUnload");
                MFXCreateConfig = (delegate* unmanaged<nint, nint>)Get(lib, "MFXCreateConfig");
                MFXSetConfigFilterProperty = (delegate* unmanaged<nint, byte*, MfxVariant, int>)Get(lib, "MFXSetConfigFilterProperty");
                MFXEnumImplementations = (delegate* unmanaged<nint, uint, uint, nint*, int>)Get(lib, "MFXEnumImplementations");
                MFXDispReleaseImplDescription = (delegate* unmanaged<nint, nint, int>)Get(lib, "MFXDispReleaseImplDescription");
                MFXCreateSession = (delegate* unmanaged<nint, uint, nint*, int>)Get(lib, "MFXCreateSession");
                MFXClose = (delegate* unmanaged<nint, int>)Get(lib, "MFXClose");
                MFXVideoCORE_SetHandle = (delegate* unmanaged<nint, int, nint, int>)Get(lib, "MFXVideoCORE_SetHandle");
                MFXVideoCORE_SyncOperation = (delegate* unmanaged<nint, nint, uint, int>)Get(lib, "MFXVideoCORE_SyncOperation");
                MFXVideoENCODE_Query = (delegate* unmanaged<nint, byte*, byte*, int>)Get(lib, "MFXVideoENCODE_Query");
                MFXVideoENCODE_Init = (delegate* unmanaged<nint, byte*, int>)Get(lib, "MFXVideoENCODE_Init");
                MFXVideoENCODE_Close = (delegate* unmanaged<nint, int>)Get(lib, "MFXVideoENCODE_Close");
                MFXVideoENCODE_GetVideoParam = (delegate* unmanaged<nint, byte*, int>)Get(lib, "MFXVideoENCODE_GetVideoParam");
                MFXVideoENCODE_EncodeFrameAsync = (delegate* unmanaged<nint, byte*, byte*, byte*, nint*, int>)Get(lib, "MFXVideoENCODE_EncodeFrameAsync");
                MFXMemory_GetSurfaceForEncode = (delegate* unmanaged<nint, byte**, int>)Get(lib, "MFXMemory_GetSurfaceForEncode");
                _state = 1;
                Log.Info($"oneVPL dispatcher loaded ({LoadedFrom})");
                return true;
            }
            catch (Exception ex)
            {
                LoadError = ex.Message;
                _state = -1;
                return false;
            }
        }
    }

    private static nint Get(nint lib, string name) =>
        NativeLibrary.TryGetExport(lib, name, out var p) ? p : throw new EntryPointNotFoundException($"libvpl.dll lacks {name}");

    public static int SetFilter(nint config, string name, MfxVariant value)
    {
        var bytes = System.Text.Encoding.ASCII.GetBytes(name + "\0");
        fixed (byte* p = bytes) return MFXSetConfigFilterProperty(config, p, value);
    }

    /// <summary>
    /// Index of the hardware implementation on the adapter with this LUID: the extended device id
    /// (mfxExtendedDeviceId) carries the LUID; it is matched by bytes so only the vendor id offset is assumed.
    /// Returns -1 when no implementation exists, or the first one when none reports a LUID.
    /// </summary>
    public static int FindImplementation(nint loader, long luid, out int count)
    {
        count = 0;
        int firstIntel = -1;
        for (uint i = 0; i < 16; i++)
        {
            nint desc;
            int st = MFXEnumImplementations(loader, i, ImplCapsDeviceIdExtended, &desc);
            if (st == ErrNotFound) break;
            count++;
            if (st < 0 || desc == 0)
            {
                if (firstIntel < 0) firstIntel = (int)i;
                continue;
            }
            try
            {
                var bytes = new ReadOnlySpan<byte>((void*)desc, 64);
                ushort vendor = (ushort)(bytes[2] | (bytes[3] << 8));
                if (vendor == 0x8086 && firstIntel < 0) firstIntel = (int)i;
                Span<byte> want = stackalloc byte[8];
                System.Buffers.Binary.BinaryPrimitives.WriteInt64LittleEndian(want, luid);
                if (bytes.IndexOf(want) >= 0) return (int)i;
            }
            finally { MFXDispReleaseImplDescription(loader, desc); }
        }
        return firstIntel;
    }

    public static void W16(byte* p, int off, int v) => *(ushort*)(p + off) = (ushort)v;
    public static void W32(byte* p, int off, uint v) => *(uint*)(p + off) = v;
    public static ushort R16(byte* p, int off) => *(ushort*)(p + off);
    public static uint R32(byte* p, int off) => *(uint*)(p + off);

    public static int SurfaceCallMap(byte* surface, uint flags) =>
        ((delegate* unmanaged<byte*, uint, int>)(*(nint*)(*(byte**)surface + FI_Map)))(surface, flags);

    public static int SurfaceCall(byte* surface, int slot) =>
        ((delegate* unmanaged<byte*, int>)(*(nint*)(*(byte**)surface + slot)))(surface);
}
