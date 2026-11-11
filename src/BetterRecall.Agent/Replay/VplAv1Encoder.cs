// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Collections.Concurrent;
using System.Diagnostics;
using System.Runtime.InteropServices;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Util;
using Vortice.Direct3D11;
using Vortice.Mathematics;

namespace BetterRecall.Agent.Replay;

/// <summary>
/// AV1 on Intel through oneVPL (the driver registers no AV1 Media Foundation encoder). Session on the adapter with
/// the encode LUID, our D3D11 device handed to the runtime, video-memory input: TrySubmit copies the NV12 texture
/// into a runtime surface on the GPU (no CPU copy). Low-power (VDENC) mode, 1 s closed GOP, no B-frames,
/// peak-constrained VBR. A sync thread waits for each task and hands packets on in submit order.
/// When the runtime's surfaces are texture arrays the encoder falls back to system-memory input (CPU copy).
/// </summary>
internal sealed unsafe class VplAv1Encoder : IVideoEncoder
{
    private const int Depth = 3;
    private readonly GpuDevice _dev;
    private readonly PacketSink _sink;
    private nint _loader, _session;
    private readonly bool _videoMemory;
    private readonly int _fps;
    private readonly Slot[] _slots;
    private readonly ConcurrentQueue<Slot> _free = new();
    private readonly BlockingCollection<Slot> _pending = new();
    private readonly ConcurrentQueue<(long Pts, long Ticks)> _order = new();
    private readonly Dictionary<nint, ID3D11Texture2D> _surfaceTextures = new();
    private readonly Thread _syncThread;
    private volatile bool _stopping;
    private readonly object _submitGate = new();

    private sealed class Slot
    {
        public byte* Bs;
        public byte* Ctrl;
        public byte* Data;
        public int MaxLength;
        public nint Sync;
    }

    public VideoCodec Codec => VideoCodec.Av1;
    public string Api => "vpl";
    public string Name { get; }
    public int Width { get; }
    public int Height { get; }
    public int SurfaceWidth { get; }
    public int SurfaceHeight { get; }
    public bool CopiesInput => true;
    public bool LowPower { get; }
    public long FramesIn { get; private set; }
    public long PacketsOut { get; private set; }
    public long FramesDropped { get; private set; }
    public long ForcedKeyframes { get; private set; }
    public string? Error { get; private set; }
    public LatencyStats Latency { get; } = new();
    public bool ReadyForInput => !_stopping && !_free.IsEmpty;

    private VplAv1Encoder(GpuDevice dev, nint loader, nint session, int w, int h, int sw, int sh, int fps, bool videoMemory, bool lowPower, int maxLength, PacketSink sink)
    {
        _dev = dev;
        _loader = loader;
        _session = session;
        Width = w;
        Height = h;
        SurfaceWidth = sw;
        SurfaceHeight = sh;
        _fps = fps;
        _videoMemory = videoMemory;
        LowPower = lowPower;
        _sink = sink;
        Name = $"oneVPL AV1{(lowPower ? " VDENC" : "")}{(videoMemory ? "" : " sysmem")}";
        _slots = new Slot[Depth];
        for (int i = 0; i < Depth; i++)
        {
            var s = new Slot
            {
                Bs = (byte*)NativeMemory.AllocZeroed(Vpl.BsSize),
                Ctrl = (byte*)NativeMemory.AllocZeroed(Vpl.CtrlSize),
                Data = (byte*)NativeMemory.Alloc((nuint)maxLength),
                MaxLength = maxLength,
            };
            *(byte**)(s.Bs + Vpl.Bs_Data) = s.Data;
            Vpl.W32(s.Bs, Vpl.Bs_MaxLength, (uint)maxLength);
            _slots[i] = s;
            _free.Enqueue(s);
        }
        _syncThread = new Thread(SyncLoop) { IsBackground = true, Name = "enc-vpl-av1", Priority = ThreadPriority.AboveNormal };
        _syncThread.Start();
    }

    /// <summary>True when libvpl.dll loads and a hardware implementation with an AV1 encoder exists on that adapter.</summary>
    public static bool Probe(long luid)
    {
        if (!Vpl.TryLoad()) return false;
        nint loader = OpenLoader();
        if (loader == 0) return false;
        try { return Vpl.FindImplementation(loader, luid, out _) >= 0; }
        finally { Vpl.MFXUnload(loader); }
    }

    private static nint OpenLoader()
    {
        nint loader = Vpl.MFXLoad();
        if (loader == 0) return 0;
        nint c1 = Vpl.MFXCreateConfig(loader), c2 = Vpl.MFXCreateConfig(loader), c3 = Vpl.MFXCreateConfig(loader);
        Vpl.SetFilter(c1, "mfxImplDescription.Impl", MfxVariant.U32(Vpl.ImplTypeHardware));
        Vpl.SetFilter(c2, "mfxImplDescription.AccelerationMode", MfxVariant.U32(Vpl.AccelD3D11));
        Vpl.SetFilter(c3, "mfxImplDescription.mfxEncoderDescription.encoder.CodecID", MfxVariant.U32(Vpl.CodecAv1));
        return loader;
    }

    private static int Align(int v, int a) => (v + a - 1) / a * a;

    public static VplAv1Encoder? Create(GpuDevice dev, int w, int h, int fps, int kbps, PacketSink sink, out string? error)
    {
        error = null;
        if (!Vpl.TryLoad()) { error = Vpl.LoadError ?? "libvpl.dll not loaded"; return null; }
        // Video memory first; system memory when the runtime's surfaces are not plain textures.
        foreach (bool video in new[] { true, false })
            foreach (bool lowPower in new[] { true, false })
            {
                var enc = TryCreate(dev, w, h, fps, kbps, sink, video, lowPower, out error, out bool retrySysmem);
                if (enc is not null) return enc;
                if (video && retrySysmem) break;
            }
        return null;
    }

    private static VplAv1Encoder? TryCreate(GpuDevice dev, int w, int h, int fps, int kbps, PacketSink sink, bool videoMemory, bool lowPower,
        out string? error, out bool retrySysmem)
    {
        error = null;
        retrySysmem = false;
        nint loader = OpenLoader();
        if (loader == 0) { error = "MFXLoad failed"; return null; }
        nint session = 0;
        byte* par = (byte*)NativeMemory.AllocZeroed(Vpl.ParSize);
        bool ok = false;
        try
        {
            int idx = Vpl.FindImplementation(loader, dev.Luid, out int count);
            if (idx < 0) { error = $"no oneVPL AV1 implementation ({count} found)"; return null; }
            int st = Vpl.MFXCreateSession(loader, (uint)idx, &session);
            if (st < 0) { error = $"MFXCreateSession {st}"; return null; }
            st = Vpl.MFXVideoCORE_SetHandle(session, Vpl.HandleD3D11Device, dev.Device.NativePointer);
            if (st < 0) { error = $"SetHandle {st}"; return null; }

            int sw = Align(w, 16), sh = Align(h, 16);
            Vpl.W16(par, Vpl.P_AsyncDepth, 1);
            Vpl.W16(par, Vpl.P_LowPower, lowPower ? 0x10 : 0);
            Vpl.W16(par, Vpl.P_BrcMultiplier, 1);
            Vpl.W32(par, Vpl.P_FourCC, Vpl.FourccNv12);
            Vpl.W16(par, Vpl.P_BitDepthLuma, 8);
            Vpl.W16(par, Vpl.P_BitDepthChroma, 8);
            Vpl.W16(par, Vpl.P_Width, sw);
            Vpl.W16(par, Vpl.P_Height, sh);
            Vpl.W16(par, Vpl.P_CropW, w);
            Vpl.W16(par, Vpl.P_CropH, h);
            Vpl.W32(par, Vpl.P_FrameRateN, (uint)fps);
            Vpl.W32(par, Vpl.P_FrameRateD, 1);
            Vpl.W16(par, Vpl.P_AspectW, 1);
            Vpl.W16(par, Vpl.P_AspectH, 1);
            Vpl.W16(par, Vpl.P_PicStruct, 1);    // progressive
            Vpl.W16(par, Vpl.P_ChromaFormat, 1); // 4:2:0
            Vpl.W32(par, Vpl.P_CodecId, Vpl.CodecAv1);
            Vpl.W16(par, Vpl.P_CodecProfile, 1); // AV1 main
            Vpl.W16(par, Vpl.P_TargetUsage, 7);  // best speed
            Vpl.W16(par, Vpl.P_GopPicSize, Math.Min(fps, 65535));
            Vpl.W16(par, Vpl.P_GopRefDist, 1);   // no B-frames
            Vpl.W16(par, Vpl.P_GopOptFlag, 1);   // closed GOP
            Vpl.W16(par, Vpl.P_IdrInterval, 0);
            Vpl.W16(par, Vpl.P_RateControl, 2);  // VBR
            int k = Math.Clamp(kbps, 100, 65535);
            Vpl.W16(par, Vpl.P_TargetKbps, k * 3 / 4);
            Vpl.W16(par, Vpl.P_MaxKbps, k);
            Vpl.W16(par, Vpl.P_NumRefFrame, 1);
            Vpl.W16(par, Vpl.P_IOPattern, videoMemory ? 0x01 : 0x02);

            // Query needs the output pre-filled with the input (at least CodecId), else it rejects as unsupported.
            byte* q = stackalloc byte[Vpl.ParSize];
            new Span<byte>(par, Vpl.ParSize).CopyTo(new Span<byte>(q, Vpl.ParSize));
            st = Vpl.MFXVideoENCODE_Query(session, par, q);
            if (st < 0) { error = $"AV1 Query {st} at {w}x{h} (lowPower {lowPower})"; return null; }
            st = Vpl.MFXVideoENCODE_Init(session, par);
            if (st < 0) { error = $"AV1 Init {st} at {w}x{h} (lowPower {lowPower})"; return null; }

            new Span<byte>(q, Vpl.ParSize).Clear();
            st = Vpl.MFXVideoENCODE_GetVideoParam(session, q);
            if (st < 0) { error = $"GetVideoParam {st}"; Vpl.MFXVideoENCODE_Close(session); return null; }
            if (Vpl.R32(q, Vpl.P_CodecId) != Vpl.CodecAv1 || Vpl.R16(q, Vpl.P_CropW) != w)
            {
                error = "mfxVideoParam layout mismatch";
                Vpl.MFXVideoENCODE_Close(session);
                return null;
            }
            int bufKb = Vpl.R16(q, Vpl.P_BufferSizeKB) * Math.Max(1, (int)Vpl.R16(q, Vpl.P_BrcMultiplier));
            int maxLength = Math.Max(bufKb * 1000, w * h);

            if (videoMemory && !ProbeVideoSurface(session, dev, sw, sh, out string? why))
            {
                error = $"video-memory surfaces unusable ({why})";
                retrySysmem = true;
                Vpl.MFXVideoENCODE_Close(session);
                return null;
            }
            var enc = new VplAv1Encoder(dev, loader, session, w, h, sw, sh, fps, videoMemory, lowPower, maxLength, sink);
            ok = true;
            Log.Info($"encoder Av1 ({enc.Name}) ready ({w}x{h}@{fps}, {kbps} kbps, buffer {maxLength / 1024} KiB)");
            return enc;
        }
        finally
        {
            NativeMemory.Free(par);
            if (!ok)
            {
                if (session != 0) Vpl.MFXClose(session);
                Vpl.MFXUnload(loader);
            }
        }
    }

    /// <summary>The runtime surface must be a single-subresource NV12 texture at least the surface size.</summary>
    private static bool ProbeVideoSurface(nint session, GpuDevice dev, int sw, int sh, out string? why)
    {
        why = null;
        byte* surf;
        int st = Vpl.MFXMemory_GetSurfaceForEncode(session, &surf);
        if (st < 0 || surf == null) { why = $"GetSurfaceForEncode {st}"; return false; }
        try
        {
            if (!GetTexture(surf, out nint res, out why)) return false;
            Marshal.AddRef(res);
            using var tex = new ID3D11Texture2D(res);
            var d = tex.Description;
            if (d.ArraySize != 1 || d.Width < sw || d.Height < sh || d.Format != Vortice.DXGI.Format.NV12)
            {
                why = $"texture {d.Width}x{d.Height} array {d.ArraySize} {d.Format}";
                return false;
            }
            return true;
        }
        finally { Vpl.SurfaceCall(surf, Vpl.FI_Release); }
    }

    private static bool GetTexture(byte* surf, out nint resource, out string? why)
    {
        why = null;
        nint res = 0;
        int type = 0;
        var fn = (delegate* unmanaged<byte*, nint*, int*, int>)(*(nint*)(*(byte**)surf + Vpl.FI_GetNativeHandle));
        int st = fn(surf, &res, &type);
        resource = res;
        if (st < 0 || res == 0 || type != Vpl.ResourceDx11Texture) { why = $"GetNativeHandle {st} type {type}"; return false; }
        return true;
    }

    public bool TrySubmit(ID3D11Texture2D nv12, long pts100ns, bool forceKey)
    {
        if (_stopping) return false;
        lock (_submitGate)
        {
            if (!_free.TryDequeue(out var slot)) { FramesDropped++; return false; }
            byte* surf;
            int st = Vpl.MFXMemory_GetSurfaceForEncode(_session, &surf);
            if (st < 0 || surf == null) { _free.Enqueue(slot); FramesDropped++; Error = $"GetSurfaceForEncode {st}"; return false; }
            try
            {
                if (!Fill(surf, nv12)) { _free.Enqueue(slot); FramesDropped++; return false; }
                new Span<byte>(slot.Ctrl, Vpl.CtrlSize).Clear();
                if (forceKey)
                {
                    Vpl.W16(slot.Ctrl, Vpl.Ctrl_FrameType, Vpl.FrameTypeI | Vpl.FrameTypeIdr | Vpl.FrameTypeRef);
                    ForcedKeyframes++;
                }
                Vpl.W32(slot.Bs, Vpl.Bs_DataLength, 0);
                Vpl.W32(slot.Bs, Vpl.Bs_DataOffset, 0);
                nint sync = 0;
                _order.Enqueue((pts100ns, Stopwatch.GetTimestamp()));
                st = Vpl.MFXVideoENCODE_EncodeFrameAsync(_session, forceKey ? slot.Ctrl : null, surf, slot.Bs, &sync);
                if (st == Vpl.ErrMoreData) { _free.Enqueue(slot); FramesIn++; return true; } // buffered, output comes later
                if (st < 0 || st == Vpl.WrnDeviceBusy || sync == 0)
                {
                    DropLastOrder();
                    _free.Enqueue(slot);
                    FramesDropped++;
                    if (st < 0) Error = $"EncodeFrameAsync {st}";
                    return false;
                }
                slot.Sync = sync;
                _pending.Add(slot);
                FramesIn++;
                return true;
            }
            finally { Vpl.SurfaceCall(surf, Vpl.FI_Release); }
        }
    }

    private void DropLastOrder()
    {
        // The pts was queued for a frame that will never produce output: rebuild the queue without its tail.
        var items = new List<(long, long)>();
        while (_order.TryDequeue(out var o)) items.Add(o);
        for (int i = 0; i < items.Count - 1; i++) _order.Enqueue(items[i]);
    }

    private bool Fill(byte* surf, ID3D11Texture2D src)
    {
        if (_videoMemory)
        {
            if (!GetTexture(surf, out nint res, out _)) return false;
            if (!_surfaceTextures.TryGetValue(res, out var tex))
            {
                Marshal.AddRef(res);
                tex = new ID3D11Texture2D(res);
                _surfaceTextures[res] = tex;
            }
            lock (_dev.Gate)
                _dev.Context.CopySubresourceRegion(tex, 0, 0, 0, 0, src, 0, new Box(0, 0, 0, SurfaceWidth, SurfaceHeight, 1));
            return true;
        }
        // System memory: map the (CPU-readable staging) source and the surface, copy both planes.
        if (Vpl.SurfaceCallMap(surf, Vpl.MapWrite) < 0) return false;
        try
        {
            byte* y = *(byte**)(surf + Vpl.S_Y), uv = *(byte**)(surf + Vpl.S_UV);
            int pitch = Vpl.R16(surf, Vpl.S_PitchLow) | (Vpl.R16(surf, Vpl.S_PitchHigh) << 16);
            lock (_dev.Gate)
            {
                var m = _dev.Context.Map(src, 0, Vortice.Direct3D11.MapMode.Read, Vortice.Direct3D11.MapFlags.None);
                try
                {
                    byte* s = (byte*)m.DataPointer;
                    int sp = (int)m.RowPitch;
                    for (int r = 0; r < SurfaceHeight; r++) Buffer.MemoryCopy(s + (long)r * sp, y + (long)r * pitch, pitch, SurfaceWidth);
                    byte* suv = s + (long)sp * SurfaceHeight;
                    for (int r = 0; r < SurfaceHeight / 2; r++) Buffer.MemoryCopy(suv + (long)r * sp, uv + (long)r * pitch, pitch, SurfaceWidth);
                }
                finally { _dev.Context.Unmap(src, 0); }
            }
            return true;
        }
        finally { Vpl.SurfaceCall(surf, Vpl.FI_Unmap); }
    }

    private void SyncLoop()
    {
        try
        {
            foreach (var slot in _pending.GetConsumingEnumerable())
            {
                int st;
                do { st = Vpl.MFXVideoCORE_SyncOperation(_session, slot.Sync, 1000); }
                while (st == Vpl.WrnInExecution && !_stopping);
                long now = Stopwatch.GetTimestamp();
                _order.TryDequeue(out var order);
                if (st >= 0)
                {
                    int off = (int)Vpl.R32(slot.Bs, Vpl.Bs_DataOffset), len = (int)Vpl.R32(slot.Bs, Vpl.Bs_DataLength);
                    if (len > 0)
                    {
                        var data = new ReadOnlySpan<byte>(slot.Data + off, len);
                        ushort ft = Vpl.R16(slot.Bs, Vpl.Bs_FrameType);
                        bool key = (ft & (Vpl.FrameTypeI | Vpl.FrameTypeIdr)) != 0 && CodecBits.DetectKey(VideoCodec.Av1, data);
                        if (!key && (ft & Vpl.FrameTypeIdr) != 0) key = true;
                        Latency.Add(CodecBits.TicksToMs(now - order.Ticks));
                        try { _sink(data, order.Pts, key); }
                        catch (Exception ex) { Log.Warn("replay sink", ex); }
                        PacketsOut++;
                    }
                }
                else
                {
                    Error = $"SyncOperation {st}";
                    Log.Warn($"AV1 encode task failed: {st}");
                }
                new Span<byte>(slot.Data, Math.Min(slot.MaxLength, Math.Max(0, (int)Vpl.R32(slot.Bs, Vpl.Bs_DataLength) + (int)Vpl.R32(slot.Bs, Vpl.Bs_DataOffset)))).Clear();
                Vpl.W32(slot.Bs, Vpl.Bs_DataLength, 0);
                Vpl.W32(slot.Bs, Vpl.Bs_DataOffset, 0);
                _free.Enqueue(slot);
            }
        }
        catch (Exception ex)
        {
            Error = ex.Message;
            Log.Error("AV1 sync loop", ex);
        }
    }

    /// <summary>Drains buffered frames so the open GOP reaches the sink, then closes the session.</summary>
    public void Dispose()
    {
        if (_session == 0) return;
        var sw = Stopwatch.StartNew();
        lock (_submitGate)
        {
            for (int i = 0; i < 8; i++)
            {
                Slot? slot = null;
                var deadline = Stopwatch.GetTimestamp() + Stopwatch.Frequency;
                while (!_free.TryDequeue(out slot) && Stopwatch.GetTimestamp() < deadline) Thread.Sleep(2);
                if (slot is null) break;
                nint sync = 0;
                int st = Vpl.MFXVideoENCODE_EncodeFrameAsync(_session, null, null, slot.Bs, &sync);
                if (st < 0 || sync == 0) { _free.Enqueue(slot); break; }
                slot.Sync = sync;
                _pending.Add(slot);
            }
            _stopping = true;
        }
        _pending.CompleteAdding();
        _syncThread.Join(3000);
        Vpl.MFXVideoENCODE_Close(_session);
        lock (_dev.Gate)
        {
            foreach (var t in _surfaceTextures.Values) t.Dispose();
            _surfaceTextures.Clear();
        }
        Vpl.MFXClose(_session);
        _session = 0;
        Vpl.MFXUnload(_loader);
        _loader = 0;
        foreach (var s in _slots)
        {
            NativeMemory.Clear(s.Data, (nuint)s.MaxLength);
            NativeMemory.Free(s.Data);
            NativeMemory.Free(s.Bs);
            NativeMemory.Free(s.Ctrl);
        }
        _pending.Dispose();
        Log.Debug($"encoder Av1 (vpl) disposed in {sw.ElapsedMilliseconds} ms");
    }
}
