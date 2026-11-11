// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Runtime.InteropServices;

namespace BetterRecall.Agent.Native;

/// <summary>VARIANT / PROPVARIANT (24 bytes on x64).</summary>
[StructLayout(LayoutKind.Explicit, Size = 24)]
internal struct VARIANT
{
    [FieldOffset(0)] public ushort vt;
    [FieldOffset(8)] public uint ulVal;
    [FieldOffset(8)] public int lVal;
    [FieldOffset(8)] public ulong ullVal;
    [FieldOffset(8)] public short boolVal;
    [FieldOffset(8)] public nint ptr;
    // VT_BLOB: cbSize at 8, pBlobData at 16
    [FieldOffset(8)] public uint blobSize;
    [FieldOffset(16)] public nint blobData;

    public const ushort VT_EMPTY = 0, VT_I4 = 3, VT_BSTR = 8, VT_BOOL = 11, VT_UI4 = 19, VT_UI8 = 21, VT_BLOB = 65;

    public static VARIANT UInt32(uint v) => new() { vt = VT_UI4, ulVal = v };
    public static VARIANT Bool(bool v) => new() { vt = VT_BOOL, boolVal = v ? (short)-1 : (short)0 };
    public static VARIANT Int32(int v) => new() { vt = VT_I4, lVal = v };
}

internal static unsafe partial class Ole
{
    public const uint COINIT_MULTITHREADED = 0x0, COINIT_APARTMENTTHREADED = 0x2;
    public const uint CLSCTX_INPROC_SERVER = 0x1, CLSCTX_ALL = 0x17;

    [LibraryImport("ole32.dll")]
    public static partial int CoInitializeEx(nint reserved, uint coinit);

    [LibraryImport("ole32.dll")]
    public static partial void CoUninitialize();

    [LibraryImport("ole32.dll")]
    public static partial int CoCreateInstance(Guid* clsid, nint outer, uint clsctx, Guid* iid, nint* obj);

    [LibraryImport("ole32.dll")]
    public static partial void CoTaskMemFree(void* p);

    [LibraryImport("oleaut32.dll")]
    public static partial nint SysAllocString([MarshalAs(UnmanagedType.LPWStr)] string s);

    [LibraryImport("oleaut32.dll")]
    public static partial void SysFreeString(nint bstr);

    [LibraryImport("oleaut32.dll")]
    public static partial int VariantClear(VARIANT* v);

    public static nint Create(Guid clsid, Guid iid)
    {
        nint obj;
        int hr = CoCreateInstance(&clsid, 0, CLSCTX_INPROC_SERVER, &iid, &obj);
        Marshal.ThrowExceptionForHR(hr);
        return obj;
    }

    public static nint QueryInterface(nint unk, Guid iid)
    {
        nint result;
        var fn = (delegate* unmanaged[Stdcall]<nint, Guid*, nint*, int>)(*(void***)unk)[0];
        int hr = fn(unk, &iid, &result);
        return hr < 0 ? 0 : result;
    }

    public static uint Release(nint unk)
    {
        if (unk == 0) return 0;
        var fn = (delegate* unmanaged[Stdcall]<nint, uint>)(*(void***)unk)[2];
        return fn(unk);
    }

    public static void* Slot(nint obj, int index) => (*(void***)obj)[index];
}

/// <summary>ICodecAPI::SetValue through the raw vtable (not in Vortice).</summary>
internal static unsafe class CodecApi
{
    public static readonly Guid IID_ICodecAPI = new("901db4c7-31ce-41a2-85dc-8fa0bf41b8da");
    public static readonly Guid RateControlMode = new("1c0608e9-370c-4710-8a58-cb6181c42423");
    public static readonly Guid MeanBitRate = new("f7222374-2144-4815-b550-a37f8e12ee52");
    public static readonly Guid MaxBitRate = new("9651eae4-39b9-4ebf-85ef-d7f444ec7465");
    public static readonly Guid GopSize = new("95f31b26-95a4-41aa-9303-246a7fc6eef1");
    public static readonly Guid BPictureCount = new("8d390aac-dc5c-4200-b57f-814d04babab2");
    public static readonly Guid LowLatency = new("9c27891a-ed7a-40e1-88e8-b22727a024ee");
    public static readonly Guid ForceKeyFrame = new("398c1b98-8353-475a-9ef2-8f265d260345");
    public static readonly Guid QualityVsSpeed = new("98332df8-03cd-476b-89fa-3f9e442dec9f");
    public static readonly Guid NumEncoderThreads = new("b0c8bf60-16f7-4951-a30b-1db1609293d6");

    public const uint RateCbr = 0, RatePeakVbr = 1, RateUnconstrainedVbr = 2, RateLowDelayVbr = 4;

    /// <summary>Returns the HRESULT; encoders ignore unknown keys with E_NOTIMPL, which callers may tolerate.</summary>
    public static int Set(nint codecApi, Guid key, VARIANT value)
    {
        var fn = (delegate* unmanaged[Stdcall]<nint, Guid*, VARIANT*, int>)Ole.Slot(codecApi, 9);
        return fn(codecApi, &key, &value);
    }
}
