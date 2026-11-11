// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;

namespace BetterRecall.Agent.Audio;

[StructLayout(LayoutKind.Sequential, Pack = 2)]
internal struct WAVEFORMATEX
{
    public ushort wFormatTag;
    public ushort nChannels;
    public uint nSamplesPerSec;
    public uint nAvgBytesPerSec;
    public ushort nBlockAlign;
    public ushort wBitsPerSample;
    public ushort cbSize;

    public const ushort WAVE_FORMAT_PCM = 1, WAVE_FORMAT_IEEE_FLOAT = 3, WAVE_FORMAT_EXTENSIBLE = 0xFFFE;

    public static WAVEFORMATEX Pcm16(int rate, int channels) => new()
    {
        wFormatTag = WAVE_FORMAT_PCM, nChannels = (ushort)channels, nSamplesPerSec = (uint)rate,
        wBitsPerSample = 16, nBlockAlign = (ushort)(channels * 2), nAvgBytesPerSec = (uint)(rate * channels * 2),
    };

    public static WAVEFORMATEX Float32(int rate, int channels) => new()
    {
        wFormatTag = WAVE_FORMAT_IEEE_FLOAT, nChannels = (ushort)channels, nSamplesPerSec = (uint)rate,
        wBitsPerSample = 32, nBlockAlign = (ushort)(channels * 4), nAvgBytesPerSec = (uint)(rate * channels * 4),
    };
}

/// <summary>Sample format of a capture stream.</summary>
internal readonly record struct PcmFormat(int SampleRate, int Channels, bool Float, int BitsPerSample)
{
    public int BlockAlign => Channels * BitsPerSample / 8;
}

/// <summary>Callback: interleaved PCM, frame count, QPC time of the first frame in 100 ns, silent flag.</summary>
internal delegate void PcmHandler(ReadOnlySpan<byte> data, int frames, long qpc100ns, bool silent);

/// <summary>
/// WASAPI shared-mode capture on a dedicated MTA thread through raw COM vtables (Native AOT safe).
/// Source "loopback": process loopback of the whole system excluding this process tree (build 20348+).
/// Source "mic": default capture endpoint.
/// </summary>
internal sealed unsafe class WasapiCapture : IDisposable
{
    private static readonly Guid CLSID_MMDeviceEnumerator = new("BCDE0395-E52F-467C-8E3D-C4579291692E");
    private static readonly Guid IID_IMMDeviceEnumerator = new("A95664D2-9614-4F35-A746-DE8DB63617E6");
    private static readonly Guid IID_IAudioClient = new("1CB9AD4C-DBFA-4c32-B178-C2F568A703B2");
    private static readonly Guid IID_IAudioCaptureClient = new("C8ADBD64-E71E-48a0-A4DE-185C395CD317");

    private const uint AUDCLNT_STREAMFLAGS_LOOPBACK = 0x00020000, AUDCLNT_STREAMFLAGS_EVENTCALLBACK = 0x00040000,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM = 0x80000000, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY = 0x08000000;
    private const uint AUDCLNT_BUFFERFLAGS_SILENT = 0x2;

    private readonly string _source;
    private readonly PcmHandler _handler;
    private readonly WAVEFORMATEX _requested;
    private readonly Thread _thread;
    private readonly ManualResetEventSlim _ready = new();
    private volatile bool _stop;
    private Exception? _startError;

    public PcmFormat Format { get; private set; }
    public long Packets { get; private set; }
    public string Source => _source;

    private WasapiCapture(string source, WAVEFORMATEX requested, PcmHandler handler)
    {
        _source = source;
        _requested = requested;
        _handler = handler;
        _thread = new Thread(Run) { IsBackground = true, Name = $"wasapi-{source}", Priority = ThreadPriority.Highest };
    }

    /// <summary>Starts capture. Throws when the stream cannot be opened.</summary>
    public static WasapiCapture Start(string source, WAVEFORMATEX requested, PcmHandler handler)
    {
        var c = new WasapiCapture(source, requested, handler);
        c._thread.Start();
        if (!c._ready.Wait(5000)) { c.Dispose(); throw new TimeoutException("Audio start timed out."); }
        if (c._startError is not null) { var e = c._startError; c.Dispose(); throw e; }
        return c;
    }

    private void Run()
    {
        Ole.CoInitializeEx(0, Ole.COINIT_MULTITHREADED);
        nint client = 0, capture = 0, evt = 0;
        try
        {
            client = _source == "loopback" ? ActivateProcessLoopback() : ActivateDefaultMic();
            var fmt = _requested;
            // Polling, not event mode: an event fires every device period (10 ms); a 40 ms poll costs far less CPU.
            uint flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            if (_source == "loopback") flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
            var init = (delegate* unmanaged[Stdcall]<nint, int, uint, long, long, WAVEFORMATEX*, Guid*, int>)Ole.Slot(client, 3);
            Check(init(client, 0, flags, 2_000_000, 0, &fmt, null), "IAudioClient.Initialize");
            Format = new PcmFormat((int)fmt.nSamplesPerSec, fmt.nChannels, fmt.wFormatTag == WAVEFORMATEX.WAVE_FORMAT_IEEE_FLOAT, fmt.wBitsPerSample);
            var getService = (delegate* unmanaged[Stdcall]<nint, Guid*, nint*, int>)Ole.Slot(client, 14);
            var iid = IID_IAudioCaptureClient;
            Check(getService(client, &iid, &capture), "GetService");
            var start = (delegate* unmanaged[Stdcall]<nint, int>)Ole.Slot(client, 10);
            Check(start(client), "Start");
        }
        catch (Exception ex)
        {
            _startError = ex;
            _ready.Set();
            Cleanup(client, capture, evt);
            Ole.CoUninitialize();
            return;
        }
        _ready.Set();
        Log.Info($"audio {_source} started {Format.SampleRate} Hz {Format.Channels} ch");

        var getBuffer = (delegate* unmanaged[Stdcall]<nint, byte**, uint*, uint*, ulong*, ulong*, int>)Ole.Slot(capture, 3);
        var release = (delegate* unmanaged[Stdcall]<nint, uint, int>)Ole.Slot(capture, 4);
        var nextSize = (delegate* unmanaged[Stdcall]<nint, uint*, int>)Ole.Slot(capture, 5);
        int block = Format.BlockAlign;
        using var timer = new HighResTimer();
        try
        {
            while (!_stop)
            {
                timer.Sleep(400_000); // 40 ms; the 200 ms buffer leaves plenty of headroom
                while (!_stop)
                {
                    uint pending;
                    if (nextSize(capture, &pending) < 0 || pending == 0) break;
                    byte* data;
                    uint frames, bflags;
                    ulong devPos, qpcPos;
                    int hr = getBuffer(capture, &data, &frames, &bflags, &devPos, &qpcPos);
                    if (hr < 0) { Log.Warn($"audio {_source} GetBuffer 0x{hr:X8}"); _stop = true; break; }
                    try
                    {
                        long now100 = (long)(Stopwatch.GetTimestamp() * (10_000_000.0 / Stopwatch.Frequency));
                        // qpcPos is already in 100 ns units; process loopback may report 0.
                        long t = qpcPos != 0 ? (long)qpcPos : now100 - frames * 10_000_000L / Format.SampleRate;
                        Packets++;
                        _handler(new ReadOnlySpan<byte>(data, (int)frames * block), (int)frames, t, (bflags & AUDCLNT_BUFFERFLAGS_SILENT) != 0);
                    }
                    finally { release(capture, frames); }
                }
            }
        }
        catch (Exception ex) { Log.Error($"audio {_source} loop", ex); }
        finally
        {
            try { ((delegate* unmanaged[Stdcall]<nint, int>)Ole.Slot(client, 11))(client); } catch { }
            Cleanup(client, capture, evt);
            Ole.CoUninitialize();
        }
    }

    private static void Cleanup(nint client, nint capture, nint evt)
    {
        Ole.Release(capture);
        Ole.Release(client);
        if (evt != 0) Win32.CloseHandle(evt);
    }

    private static void Check(int hr, string what)
    {
        if (hr < 0) throw new IOException($"{what} failed (0x{hr:X8}).", hr);
    }

    private static nint ActivateDefaultMic()
    {
        nint enumerator = Ole.Create(CLSID_MMDeviceEnumerator, IID_IMMDeviceEnumerator);
        nint device = 0;
        try
        {
            var getDefault = (delegate* unmanaged[Stdcall]<nint, int, int, nint*, int>)Ole.Slot(enumerator, 4);
            Check(getDefault(enumerator, 1 /* eCapture */, 0 /* eConsole */, &device), "GetDefaultAudioEndpoint");
            var activate = (delegate* unmanaged[Stdcall]<nint, Guid*, uint, void*, nint*, int>)Ole.Slot(device, 3);
            var iid = IID_IAudioClient;
            nint client;
            Check(activate(device, &iid, Ole.CLSCTX_ALL, null, &client), "IMMDevice.Activate");
            return client;
        }
        finally
        {
            Ole.Release(device);
            Ole.Release(enumerator);
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct AUDIOCLIENT_ACTIVATION_PARAMS
    {
        public int ActivationType;      // 1 = process loopback
        public uint TargetProcessId;
        public int ProcessLoopbackMode; // 1 = exclude target process tree
    }

    private static nint ActivateProcessLoopback()
    {
        var p = new AUDIOCLIENT_ACTIVATION_PARAMS { ActivationType = 1, TargetProcessId = Win32.GetCurrentProcessId(), ProcessLoopbackMode = 1 };
        var pv = new VARIANT { vt = VARIANT.VT_BLOB, blobSize = (uint)sizeof(AUDIOCLIENT_ACTIVATION_PARAMS), blobData = (nint)(&p) };
        var handler = CompletionHandler.Create();
        try
        {
            var iid = IID_IAudioClient;
            nint op;
            int hr = ActivateAudioInterfaceAsync("VAD\\Process_Loopback", &iid, &pv, handler, &op);
            Check(hr, "ActivateAudioInterfaceAsync");
            try
            {
                if (!CompletionHandler.Wait(handler, 5000)) throw new TimeoutException("Process loopback activation timed out.");
                var getResult = (delegate* unmanaged[Stdcall]<nint, int*, nint*, int>)Ole.Slot(op, 3);
                int activateHr;
                nint unk;
                Check(getResult(op, &activateHr, &unk), "GetActivateResult");
                Check(activateHr, "Process loopback activation");
                nint client = Ole.QueryInterface(unk, IID_IAudioClient);
                Ole.Release(unk);
                if (client == 0) throw new IOException("No IAudioClient.");
                return client;
            }
            finally { Ole.Release(op); }
        }
        finally { Ole.Release(handler); }
    }

    public void Dispose()
    {
        _stop = true;
        if (_thread.IsAlive) _thread.Join(2000);
        _ready.Dispose();
    }

    [DllImport("mmdevapi.dll", ExactSpelling = true)]
    private static extern int ActivateAudioInterfaceAsync([MarshalAs(UnmanagedType.LPWStr)] string path, Guid* riid, VARIANT* activationParams, nint handler, nint* op);

    [DllImport("kernel32.dll", ExactSpelling = true)]
    private static extern nint CreateEventW(nint attrs, int manualReset, int initialState, nint name);

    [DllImport("kernel32.dll", ExactSpelling = true)]
    internal static extern uint WaitForSingleObject(nint handle, uint ms);

    [DllImport("kernel32.dll", ExactSpelling = true)]
    internal static extern int SetEvent(nint handle);

    /// <summary>
    /// Hand-built COM object implementing IActivateAudioInterfaceCompletionHandler and IAgileObject.
    /// Layout: vtable pointer, ref count, event handle.
    /// </summary>
    private static class CompletionHandler
    {
        private static readonly Guid IID_IUnknown = new("00000000-0000-0000-C000-000000000046");
        private static readonly Guid IID_Handler = new("41D949AB-9862-444A-80F6-C261334DA5EB");
        private static readonly Guid IID_IAgileObject = new("94ea2b94-e9cc-49e0-c0ff-ee64ca8f5b90");
        private static readonly void** Vtbl = CreateVtbl();

        [StructLayout(LayoutKind.Sequential)]
        private struct Obj { public void** Vtbl; public int Ref; public nint Event; }

        private static void** CreateVtbl()
        {
            var v = (void**)NativeMemory.Alloc((nuint)(4 * sizeof(void*)));
            v[0] = (delegate* unmanaged[Stdcall]<nint, Guid*, nint*, int>)&QueryInterface;
            v[1] = (delegate* unmanaged[Stdcall]<nint, uint>)&AddRef;
            v[2] = (delegate* unmanaged[Stdcall]<nint, uint>)&Release;
            v[3] = (delegate* unmanaged[Stdcall]<nint, nint, int>)&ActivateCompleted;
            return v;
        }

        public static nint Create()
        {
            var o = (Obj*)NativeMemory.AllocZeroed((nuint)sizeof(Obj));
            o->Vtbl = Vtbl;
            o->Ref = 1;
            o->Event = CreateEventW(0, 1, 0, 0);
            return (nint)o;
        }

        public static bool Wait(nint obj, uint ms) => WaitForSingleObject(((Obj*)obj)->Event, ms) == 0;

        [UnmanagedCallersOnly(CallConvs = [typeof(CallConvStdcall)])]
        private static int QueryInterface(nint self, Guid* iid, nint* ppv)
        {
            if (*iid == IID_IUnknown || *iid == IID_Handler || *iid == IID_IAgileObject)
            {
                Interlocked.Increment(ref ((Obj*)self)->Ref);
                *ppv = self;
                return 0;
            }
            *ppv = 0;
            return unchecked((int)0x80004002); // E_NOINTERFACE
        }

        [UnmanagedCallersOnly(CallConvs = [typeof(CallConvStdcall)])]
        private static uint AddRef(nint self) => (uint)Interlocked.Increment(ref ((Obj*)self)->Ref);

        [UnmanagedCallersOnly(CallConvs = [typeof(CallConvStdcall)])]
        private static uint Release(nint self)
        {
            var o = (Obj*)self;
            int r = Interlocked.Decrement(ref o->Ref);
            if (r == 0)
            {
                Win32.CloseHandle(o->Event);
                NativeMemory.Free(o);
            }
            return (uint)r;
        }

        [UnmanagedCallersOnly(CallConvs = [typeof(CallConvStdcall)])]
        private static int ActivateCompleted(nint self, nint op)
        {
            SetEvent(((Obj*)self)->Event);
            return 0;
        }
    }
}
