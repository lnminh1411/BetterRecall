// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;
using SharpGen.Runtime;
using Vortice.Direct3D11;
using Vortice.MediaFoundation;

namespace BetterRecall.Agent.Replay;

/// <summary>Media Foundation start-up, once per process.</summary>
internal static class MediaFoundationRuntime
{
    private static int _started;

    public static void EnsureStarted()
    {
        if (Interlocked.Exchange(ref _started, 1) == 0) MediaFactory.MFStartup(true).CheckError();
    }

    public static Guid FourCC(string fourcc) =>
        new((uint)(fourcc[0] | (fourcc[1] << 8) | (fourcc[2] << 16) | (fourcc[3] << 24)), 0x0000, 0x0010, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71);

    public static readonly Guid MFVideoFormat_NV12 = FourCC("NV12");
    public static readonly Guid MFVideoFormat_H264 = FourCC("H264");
    public static readonly Guid MFVideoFormat_HEVC = FourCC("HEVC");
    public static readonly Guid MFVideoFormat_AV1 = FourCC("AV01");
    public static readonly Guid MFT_ENUM_ADAPTER_LUID = new("1d39518c-e220-4da8-a07f-ba172552d6b1");
    public static readonly Guid MF_MT_MAX_KEYFRAME_SPACING = new("c16eb52b-73a1-476f-8d62-839d6a020652");
    public static readonly Guid MF_MT_MPEG2_PROFILE = new("ad76a80b-2d5c-4e0b-b375-64e520137036");
    public static readonly Guid MF_LOW_LATENCY = new("9c27891a-ed7a-40e1-88e8-b22727a024ee");
}

/// <summary>
/// Hardware video encoder MFT on one adapter (matched by MFT_ENUM_ADAPTER_LUID, else by vendor id), async mode,
/// D3D11 NV12 input, 1 s closed GOP, no B-frames, peak-constrained VBR at the configured bitrate.
/// Covers Intel QSV, NVIDIA NVENC and AMD AMF, whichever MFT the adapter's driver registers.
/// </summary>
internal sealed unsafe class MfVideoEncoder : IVideoEncoder
{
    private const int METransformNeedInput = 601, METransformHaveOutput = 602, METransformDrainComplete = 603;
    private static readonly int MF_E_TRANSFORM_NEED_MORE_INPUT = unchecked((int)0xC00D6D72);
    private static readonly int MF_E_TRANSFORM_STREAM_CHANGE = unchecked((int)0xC00D6D61);

    private readonly IMFActivate _activate;
    private readonly IMFTransform _mft;
    private readonly IMFMediaEventGenerator _events;
    private readonly IMFDXGIDeviceManager _devManager;
    private readonly PacketSink _sink;
    private readonly Thread _thread;
    private readonly long _frameDuration;
    private readonly Dictionary<long, long> _submitTicks = new();
    private nint _codecApi;
    private int _needInput;
    private volatile bool _stopping;

    public VideoCodec Codec { get; }
    public string Api => "mf";
    public string Name { get; }
    public int Width { get; }
    public int Height { get; }
    public int SurfaceWidth => Width;
    public int SurfaceHeight => Height;
    public bool CopiesInput => false;
    public int Fps { get; }
    public long PacketsOut { get; private set; }
    public long FramesIn { get; private set; }
    public long FramesDropped { get; private set; }
    public long ForcedKeyframes { get; private set; }
    public string? Error { get; private set; }
    public LatencyStats Latency { get; } = new();

    private MfVideoEncoder(IMFActivate activate, IMFTransform mft, IMFDXGIDeviceManager devManager, VideoCodec codec, string name, int w, int h, int fps, PacketSink sink)
    {
        _activate = activate;
        _mft = mft;
        _devManager = devManager;
        Codec = codec;
        Name = name;
        Width = w;
        Height = h;
        Fps = fps;
        _sink = sink;
        _frameDuration = 10_000_000L / fps;
        _events = mft.QueryInterface<IMFMediaEventGenerator>();
        _thread = new Thread(EventLoop) { IsBackground = true, Name = $"enc-mf-{codec}", Priority = ThreadPriority.AboveNormal };
    }

    public static Guid Subtype(VideoCodec c) => c switch
    {
        VideoCodec.Av1 => MediaFoundationRuntime.MFVideoFormat_AV1,
        VideoCodec.Hevc => MediaFoundationRuntime.MFVideoFormat_HEVC,
        _ => MediaFoundationRuntime.MFVideoFormat_H264,
    };

    /// <summary>Friendly name of the hardware encoder MFT for this adapter and codec, or null.</summary>
    /// <remarks>Activates the MFT once: some vendor MFTs enumerate but refuse ActivateObject (seen: NVIDIA MFT, E_UNEXPECTED, when NVIDIA is not DXGI adapter 0).</remarks>
    public static string? Find(long luid, uint vendorId, VideoCodec codec)
    {
        using var act = FindActivate(luid, vendorId, codec, out var name);
        if (act is null) return null;
        try
        {
            using var mft = act.ActivateObject<IMFTransform>();
            try { MediaFactory.MFShutdownObject(mft); } catch { }
        }
        catch (Exception ex)
        {
            Log.Info($"encoder MFT '{name}' enumerates but does not activate: {ex.Message.Split('\n')[0].Trim()}");
            return null;
        }
        finally { try { act.ShutdownObject(); } catch { } }
        return name;
    }

    private static IMFActivate? FindActivate(long luid, uint vendorId, VideoCodec codec, out string name)
    {
        MediaFoundationRuntime.EnsureStarted();
        name = "";
        var input = new RegisterTypeInfo { GuidMajorType = MediaTypeGuids.Video, GuidSubtype = MediaFoundationRuntime.MFVideoFormat_NV12 };
        var output = new RegisterTypeInfo { GuidMajorType = MediaTypeGuids.Video, GuidSubtype = Subtype(codec) };
        const uint MFT_ENUM_FLAG_HARDWARE = 0x4, MFT_ENUM_FLAG_SORTANDFILTER = 0x40;
        using var activates = MediaFactory.MFTEnumEx(TransformCategoryGuids.VideoEncoder, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, input, output);
        IMFActivate? byLuid = null, byVendor = null;
        string luidName = "", vendorName = "";
        foreach (var act in activates)
        {
            string fn = "";
            try { fn = act.GetString(TransformAttributeKeys.MftFriendlyNameAttribute); } catch { }
            if (act.GetUInt64(MediaFoundationRuntime.MFT_ENUM_ADAPTER_LUID, out ulong l).Success)
            {
                if ((long)l == luid && byLuid is null) { act.AddRef(); byLuid = new IMFActivate(act.NativePointer); luidName = fn; }
                continue;
            }
            string vendor = "";
            try { vendor = act.GetString(TransformAttributeKeys.MftEnumHardwareVendorIdAttribute); } catch { }
            if (byVendor is null && vendor.Contains($"{vendorId:X4}", StringComparison.OrdinalIgnoreCase)) { act.AddRef(); byVendor = new IMFActivate(act.NativePointer); vendorName = fn; }
        }
        if (byLuid is not null)
        {
            byVendor?.Dispose();
            name = luidName;
            return byLuid;
        }
        name = vendorName;
        return byVendor;
    }

    public static MfVideoEncoder? Create(GpuDevice dev, VideoCodec codec, int w, int h, int fps, int kbps, PacketSink sink, out string? error)
    {
        error = null;
        var activate = FindActivate(dev.Luid, dev.VendorId, codec, out var name);
        if (activate is null)
        {
            error = $"no {codec} MFT on adapter vendor 0x{dev.VendorId:X4}";
            return null;
        }
        IMFTransform? mft = null;
        IMFDXGIDeviceManager? devManager = null;
        MfVideoEncoder? enc = null;
        string stage = "activate";
        try
        {
            mft = activate.ActivateObject<IMFTransform>();
            stage = "attributes";
            using (var attrs = mft.Attributes)
            {
                attrs.Set(TransformAttributeKeys.TransformAsyncUnlock, 1u);
                try { attrs.Set(MediaFoundationRuntime.MF_LOW_LATENCY, 1u); } catch { }
            }
            stage = "d3d manager";
            devManager = MediaFactory.MFCreateDXGIDeviceManager();
            devManager.ResetDevice(dev.Device).CheckError();
            mft.ProcessMessage(TMessageType.MessageSetD3DManager, (nuint)devManager.NativePointer);

            stage = "configure";
            enc = new MfVideoEncoder(activate, mft, devManager, codec, name, w, h, fps, sink);
            enc.Configure(kbps);
            stage = "start";
            enc._thread.Start();
            mft.ProcessMessage(TMessageType.MessageNotifyBeginStreaming, 0);
            mft.ProcessMessage(TMessageType.MessageNotifyStartOfStream, 0);
            Log.Info($"encoder {codec} (mf '{name}') ready ({w}x{h}@{fps}, {kbps} kbps)");
            return enc;
        }
        catch (Exception ex)
        {
            error = $"{codec} mf init ({stage}, '{name}'): {ex.Message}";
            if (enc is not null) { try { enc.Dispose(); } catch { } }
            else
            {
                if (mft is not null) { try { MediaFactory.MFShutdownObject(mft); } catch { } }
                try { mft?.Dispose(); } catch { }
                try { devManager?.Dispose(); } catch { }
                try { activate.ShutdownObject(); } catch { }
                activate.Dispose();
            }
            return null;
        }
    }

    private void Configure(int kbps)
    {
        ulong frameSize = ((ulong)Width << 32) | (uint)Height;
        ulong frameRate = ((ulong)Fps << 32) | 1u;
        ulong par = (1UL << 32) | 1u;

        var outType = MediaFactory.MFCreateMediaType();
        outType.Set(MediaTypeAttributeKeys.MajorType, MediaTypeGuids.Video);
        outType.Set(MediaTypeAttributeKeys.Subtype, Subtype(Codec));
        outType.Set(MediaTypeAttributeKeys.AvgBitrate, (uint)(kbps * 1000));
        outType.Set(MediaTypeAttributeKeys.FrameSize, frameSize);
        outType.Set(MediaTypeAttributeKeys.FrameRate, frameRate);
        outType.Set(MediaTypeAttributeKeys.PixelAspectRatio, par);
        outType.Set(MediaTypeAttributeKeys.InterlaceMode, 2u); // progressive
        outType.Set(MediaFoundationRuntime.MF_MT_MAX_KEYFRAME_SPACING, (uint)Fps);
        if (Codec == VideoCodec.H264) outType.Set(MediaFoundationRuntime.MF_MT_MPEG2_PROFILE, 100u);
        if (Codec == VideoCodec.Hevc) outType.Set(MediaFoundationRuntime.MF_MT_MPEG2_PROFILE, 1u);
        _mft.SetOutputType(0, outType, 0);
        outType.Dispose();

        IMFMediaType? inType = null;
        for (int i = 0; ; i++)
        {
            IMFMediaType t;
            try { t = _mft.GetInputAvailableType(0, i); }
            catch { break; }
            if (t.GetGUID(MediaTypeAttributeKeys.Subtype) == MediaFoundationRuntime.MFVideoFormat_NV12) { inType = t; break; }
            t.Dispose();
        }
        inType ??= MediaFactory.MFCreateMediaType();
        inType.Set(MediaTypeAttributeKeys.MajorType, MediaTypeGuids.Video);
        inType.Set(MediaTypeAttributeKeys.Subtype, MediaFoundationRuntime.MFVideoFormat_NV12);
        inType.Set(MediaTypeAttributeKeys.FrameSize, frameSize);
        inType.Set(MediaTypeAttributeKeys.FrameRate, frameRate);
        inType.Set(MediaTypeAttributeKeys.PixelAspectRatio, par);
        inType.Set(MediaTypeAttributeKeys.InterlaceMode, 2u);
        _mft.SetInputType(0, inType, 0);
        inType.Dispose();

        _codecApi = Ole.QueryInterface(_mft.NativePointer, CodecApi.IID_ICodecAPI);
        if (_codecApi != 0)
        {
            uint bps = (uint)(kbps * 1000);
            Try(CodecApi.RateControlMode, VARIANT.UInt32(CodecApi.RatePeakVbr));
            Try(CodecApi.MeanBitRate, VARIANT.UInt32(bps * 3 / 4));
            Try(CodecApi.MaxBitRate, VARIANT.UInt32(bps));
            Try(CodecApi.GopSize, VARIANT.UInt32((uint)Fps));
            Try(CodecApi.BPictureCount, VARIANT.UInt32(0));
            Try(CodecApi.LowLatency, VARIANT.Bool(true));
            Try(CodecApi.QualityVsSpeed, VARIANT.UInt32(33));
        }
    }

    private void Try(Guid key, VARIANT v)
    {
        int hr = CodecApi.Set(_codecApi, key, v);
        if (hr < 0) Log.Debug($"ICodecAPI {key} -> 0x{hr:X8}");
    }

    public bool ReadyForInput => Volatile.Read(ref _needInput) > 0 && !_stopping;

    /// <summary>Submits one NV12 texture on the encoder's adapter. The MFT keeps a reference until it has encoded it.</summary>
    public bool TrySubmit(ID3D11Texture2D nv12, long pts100ns, bool forceKey)
    {
        if (_stopping) return false;
        if (Volatile.Read(ref _needInput) <= 0) { FramesDropped++; return false; }
        if (forceKey && _codecApi != 0 && CodecApi.Set(_codecApi, CodecApi.ForceKeyFrame, VARIANT.UInt32(1)) >= 0) ForcedKeyframes++;
        using var buffer = MediaFactory.MFCreateDXGISurfaceBuffer(typeof(ID3D11Texture2D).GUID, nv12, 0, false);
        using var sample = MediaFactory.MFCreateSample();
        sample.AddBuffer(buffer);
        sample.SampleTime = pts100ns;
        sample.SampleDuration = _frameDuration;
        try
        {
            lock (_submitTicks) _submitTicks[pts100ns] = Stopwatch.GetTimestamp();
            _mft.ProcessInput(0, sample, 0);
            Interlocked.Decrement(ref _needInput);
            FramesIn++;
            return true;
        }
        catch (SharpGenException ex)
        {
            lock (_submitTicks) _submitTicks.Remove(pts100ns);
            FramesDropped++;
            Log.Debug($"encoder ProcessInput 0x{ex.HResult:X8}");
            return false;
        }
    }

    private void EventLoop()
    {
        try
        {
            while (!_stopping)
            {
                IMFMediaEvent ev;
                try { ev = _events.GetEvent(0); }
                catch (SharpGenException) { break; }
                using (ev)
                {
                    int type = (int)ev.EventType;
                    if (type == METransformNeedInput) Interlocked.Increment(ref _needInput);
                    else if (type == METransformHaveOutput) DrainOutput();
                    else if (type == METransformDrainComplete) break;
                    else if (ev.Status.Failure) { Error = $"0x{ev.Status.Code:X8}"; Log.Warn($"encoder event {type} status {Error}"); }
                }
            }
        }
        catch (Exception ex)
        {
            Error = ex.Message;
            Log.Error("encoder event loop", ex);
        }
    }

    private void DrainOutput()
    {
        while (true)
        {
            var odb = new OutputDataBuffer { StreamID = 0 };
            var r = _mft.ProcessOutput(ProcessOutputFlags.None, 1, ref odb, out _);
            if (r.Code == MF_E_TRANSFORM_NEED_MORE_INPUT) return;
            if (r.Code == MF_E_TRANSFORM_STREAM_CHANGE)
            {
                using var t = _mft.GetOutputAvailableType(0, 0);
                _mft.SetOutputType(0, t, 0);
                continue;
            }
            if (r.Failure)
            {
                Log.Debug($"encoder ProcessOutput 0x{r.Code:X8}");
                return;
            }
            odb.Events?.Dispose();
            if (odb.Sample is null) return;
            using (var s = odb.Sample) WritePacket(s);
            // Async hardware MFTs deliver one sample per METransformHaveOutput.
            return;
        }
    }

    private void WritePacket(IMFSample s)
    {
        long pts = s.SampleTime;
        bool key = s.GetUInt32(SampleAttributeKeys.CleanPoint, out uint cp).Success && cp != 0;
        long now = Stopwatch.GetTimestamp();
        lock (_submitTicks)
        {
            if (_submitTicks.Remove(pts, out long t0)) Latency.Add(CodecBits.TicksToMs(now - t0));
            if (_submitTicks.Count > 256) _submitTicks.Clear();
        }
        using var buf = s.ConvertToContiguousBuffer();
        buf.Lock(out nint ptr, out _, out int len);
        try
        {
            var data = new ReadOnlySpan<byte>((void*)ptr, len);
            if (!key) key = CodecBits.DetectKey(Codec, data);
            _sink(data, pts, key);
            PacketsOut++;
        }
        finally { buf.Unlock(); }
    }

    public void Dispose()
    {
        if (_stopping) return;
        var sw = Stopwatch.StartNew();
        try
        {
            _mft.ProcessMessage(TMessageType.MessageNotifyEndOfStream, 0);
            _mft.ProcessMessage(TMessageType.MessageCommandDrain, 0);
        }
        catch { }
        // The drain delivers the last packets of the open GOP before the loop ends.
        bool drained = !_thread.IsAlive || _thread.Join(1000);
        _stopping = true;
        // Async hardware MFTs hold their surfaces until IMFShutdown::Shutdown; Release alone leaks them.
        try { MediaFactory.MFShutdownObject(_mft); } catch { }
        if (!drained) _thread.Join(1000);
        if (_codecApi != 0) Ole.Release(_codecApi);
        _events.Dispose();
        _mft.Dispose();
        _devManager.Dispose();
        try { _activate.ShutdownObject(); } catch { }
        _activate.Dispose();
        Log.Debug($"encoder {Codec} (mf) disposed in {sw.ElapsedMilliseconds} ms");
    }
}
