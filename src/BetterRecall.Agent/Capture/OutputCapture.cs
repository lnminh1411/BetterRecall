// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using System.Runtime.InteropServices;
using BetterRecall.Agent.Native;
using BetterRecall.Agent.Replay;
using BetterRecall.Agent.Util;
using SharpGen.Runtime;
using Vortice.Direct3D11;
using Vortice.DXGI;

namespace BetterRecall.Agent.Capture;

/// <summary>Keyframe requested by the hub for one output. Metadata is decided before the grab.</summary>
/// DirtyMinPercent is the cheap pre-grab gate on the dirty area since the last grab; the pipeline compares the grab
/// with the last stored keyframe (capture.dirtyMinPercent). Trigger: asked by an event, not the timer.
internal sealed record KeyframeRequest(DateTimeOffset Ts, string App, string ExePath, string Title, string Context, bool Force, int MaxWidth, double DirtyMinPercent, string Site = "", bool Trigger = false);

/// <summary>A grabbed, downscaled BGRA keyframe in native memory, owned by the receiver (Dispose zeroes it).</summary>
internal sealed unsafe class KeyframeGrab : IDisposable
{
    public required int Monitor { get; init; }
    public required KeyframeRequest Request { get; init; }
    public required int Width { get; init; }
    public required int Height { get; init; }
    public required int Stride { get; init; }
    public byte* Pixels { get; private set; }
    public long GrabTicks { get; init; }

    public static KeyframeGrab Alloc(int monitor, KeyframeRequest req, int w, int h, long grabTicks)
    {
        var g = new KeyframeGrab { Monitor = monitor, Request = req, Width = w, Height = h, Stride = w * 4, GrabTicks = grabTicks };
        g.Pixels = (byte*)NativeMemory.Alloc((nuint)(w * h * 4));
        return g;
    }

    public ReadOnlySpan<byte> Span => new(Pixels, Stride * Height);

    public void Dispose()
    {
        if (Pixels == null) return;
        NativeMemory.Clear(Pixels, (nuint)(Stride * Height));
        NativeMemory.Free(Pixels);
        Pixels = null;
    }
}

/// <summary>
/// Desktop Duplication of one output on the GPU that drives it (DuplicateOutput1 with an 8-bit BGRA request,
/// so HDR desktops arrive tone-mapped to SDR). One thread per output: acquire, keep a private copy of the
/// latest frame, feed the replay pipeline at ReplayPollFps and grab keyframes on request.
/// </summary>
internal sealed unsafe class OutputCapture : IDisposable
{
    private static readonly int DXGI_ERROR_WAIT_TIMEOUT = unchecked((int)0x887A0027);
    private static readonly int DXGI_ERROR_ACCESS_LOST = unchecked((int)0x887A0026);

    private readonly GpuDeviceCache _devices;
    private readonly Action<KeyframeGrab> _onGrab;
    private readonly Thread _thread;
    private readonly HighResTimer _timer = new();
    private volatile bool _stop;
    private GpuDevice? _dev;
    private IDXGIOutputDuplication? _dup;
    private ID3D11Texture2D? _latest;
    private int _srcW, _srcH;
    private VideoScaler? _kfScaler;
    private int _kfScalerWidth;
    private RawRectBuffer _dirtyBuf = new();
    private MoveRectBuffer _moveBuf = new();
    private long _lastRetry;

    // Change tracking between keyframe grabs.
    private bool _framesSinceGrab = true;
    private double _dirtyPctSinceGrab = 100;

    private FramePacer? _pollPacer;
    private int _lastPollFps;

    private RECT _feedDirty;

    public OutputInfo Info { get; }
    public volatile KeyframeRequest? PendingKeyframe;
    public volatile int ReplayPollFps;
    public volatile Action<KeyframeGrab>? GrabOverride;
    public GpuDevice? Device => _dev;
    public ID3D11Texture2D? Latest => _latest;
    public int SourceWidth => _srcW;
    public int SourceHeight => _srcH;
    public int FeedGeneration { get; private set; }
    public bool Disposed { get; private set; }
    public string State { get; private set; } = "starting";

    // Measurements.
    public long AcquireCount, FrameCount, AccessLost, GrabTicks, GrabCount, SkippedNoChange;

    public OutputCapture(OutputInfo info, GpuDeviceCache devices, Action<KeyframeGrab> onGrab)
    {
        Info = info;
        _devices = devices;
        _onGrab = onGrab;
        _thread = new Thread(Run) { IsBackground = true, Name = $"capture-{info.Index}", Priority = ThreadPriority.AboveNormal };
    }

    public void Start() => _thread.Start();

    private void Run()
    {
        long freq = Stopwatch.Frequency;
        while (!_stop)
        {
            try
            {
                if (_dup is null && !TryCreateDuplication()) { Thread.Sleep(500); continue; }

                int pollFps = ReplayPollFps;
                if (pollFps > 0)
                {
                    if (_pollPacer is null || _lastPollFps != pollFps)
                    {
                        _pollPacer = new FramePacer(pollFps, freq);
                        _lastPollFps = pollFps;
                    }

                    long now = Stopwatch.GetTimestamp();
                    long deadline = _pollPacer.Base == 0 ? now : _pollPacer.NextDeadline;
                    if (_pendingGrab is not null) deadline = Math.Min(deadline, now + freq / 30);
                    if (deadline > now) _timer.Sleep((deadline - now) * 10_000_000 / freq);
                    if (_pendingGrab is not null) FinishGrab();

                    Acquire(0);

                    _pollPacer.TryTick(Stopwatch.GetTimestamp(), out _);
                }
                else
                {
                    _pollPacer = null;
                    _lastPollFps = 0;
                    long now = Stopwatch.GetTimestamp();
                    long deadline = PendingKeyframe is null ? now + freq / 4 : now;
                    if (_pendingGrab is not null) deadline = Math.Min(deadline, now + freq / 30);
                    if (deadline > now) _timer.Sleep((deadline - now) * 10_000_000 / freq);
                    if (_pendingGrab is not null) FinishGrab();

                    Acquire(0);
                }

                if (PendingKeyframe is { } req)
                {
                    PendingKeyframe = null;
                    Grab(req);
                }
            }
            catch (SharpGenException ex) when (ex.HResult == DXGI_ERROR_ACCESS_LOST)
            {
                AccessLost++;
                ResetDuplication("access lost");
            }
            catch (Exception ex)
            {
                Log.Warn($"capture {Info.Index}", ex);
                ResetDuplication("error");
                Thread.Sleep(500);
            }
        }
        ResetDuplication("stop");
    }

    public static long Qpc100ns(long stopwatchTicks) => FramePacer.ToHundredNs(stopwatchTicks, Stopwatch.Frequency);

    private bool TryCreateDuplication()
    {
        long now = Environment.TickCount64;
        if (now - _lastRetry < 1000) return false;
        _lastRetry = now;
        try
        {
            _dev ??= _devices.Get(Info.AdapterLuid);
            using var factory = DXGI.CreateDXGIFactory1<IDXGIFactory1>();
            using var adapter = GpuDevices.FindAdapter(factory, Info.AdapterLuid) ?? throw new InvalidOperationException("adapter gone");
            IDXGIOutput? output = null;
            for (uint i = 0; adapter.EnumOutputs(i, out var o).Success; i++)
            {
                if (o.Description.DeviceName == Info.DeviceName) { output = o; break; }
                o.Dispose();
            }
            if (output is null) throw new InvalidOperationException("output gone");
            using (output)
            {
                IDXGIOutputDuplication dup;
                using (var o5 = output.QueryInterfaceOrNull<IDXGIOutput5>())
                {
                    if (o5 is not null) dup = o5.DuplicateOutput1(_dev.Device, [Format.B8G8R8A8_UNorm]);
                    else
                    {
                        using var o1 = output.QueryInterface<IDXGIOutput1>();
                        dup = o1.DuplicateOutput(_dev.Device);
                    }
                }
                var desc = dup.Description;
                _srcW = (int)desc.ModeDescription.Width;
                _srcH = (int)desc.ModeDescription.Height;
                lock (_dev.Gate)
                {
                    _latest?.Dispose();
                    _latest = _dev.Device.CreateTexture2D(new Texture2DDescription
                    {
                        Width = (uint)_srcW, Height = (uint)_srcH, MipLevels = 1, ArraySize = 1, Format = Format.B8G8R8A8_UNorm,
                        SampleDescription = new SampleDescription(1, 0), Usage = ResourceUsage.Default,
                        BindFlags = BindFlags.ShaderResource | BindFlags.RenderTarget,
                    });
                    FeedGeneration++;
                    _feedDirty = new RECT(0, 0, _srcW, _srcH);
                }
                _dup = dup;
                _framesSinceGrab = true;
                _dirtyPctSinceGrab = 100;
                State = "running";
                Log.Info($"duplication up on output {Info.Index} ({_srcW}x{_srcH}, vendor 0x{Info.VendorId:X4})");
                return true;
            }
        }
        catch (Exception ex)
        {
            State = "unavailable";
            Log.Debug($"duplication output {Info.Index} failed: 0x{ex.HResult:X8}");
            return false;
        }
    }

    private void ResetDuplication(string why)
    {
        if (_dup is null) return;
        try { _dup.Dispose(); } catch { }
        _dup = null;
        State = why;
    }

    private void Acquire(uint timeoutMs)
    {
        AcquireCount++;
        var r = _dup!.AcquireNextFrame(timeoutMs, out var info, out var resource);
        if (r.Code == DXGI_ERROR_WAIT_TIMEOUT) return;
        if (r.Code == DXGI_ERROR_ACCESS_LOST) { AccessLost++; ResetDuplication("access lost"); return; }
        r.CheckError();
        try
        {
            if (info.LastPresentTime == 0 || info.AccumulatedFrames == 0) return; // pointer-only update
            FrameCount++;
            using var tex = resource!.QueryInterface<ID3D11Texture2D>();
            lock (_dev!.Gate)
            {
                _dev.Context.CopyResource(_latest!, tex);
                var (dirtyRect, dirtyPct) = ProcessChanges(info.TotalMetadataBufferSize);
                _feedDirty = DirtyMapper.Union(_feedDirty, dirtyRect);
                _dirtyPctSinceGrab = Math.Min(100, _dirtyPctSinceGrab + dirtyPct);
            }
            _framesSinceGrab = true;
        }
        finally
        {
            resource?.Dispose();
            _dup.ReleaseFrame();
        }
    }

    private (RECT DirtyRect, double DirtyPct) ProcessChanges(uint metadataSize)
    {
        if (metadataSize == 0)
        {
            return (new RECT(0, 0, _srcW, _srcH), 100.0);
        }

        int dirtyCount = (int)(metadataSize / (uint)sizeof(Vortice.RawRect));
        var dirtyBuf = _dirtyBuf.Get(Math.Max(1, dirtyCount));
        var dirtyRes = _dup!.GetFrameDirtyRects((uint)(dirtyBuf.Length * sizeof(Vortice.RawRect)), dirtyBuf, out uint dirtyRequired);
        if (dirtyRes.Failure)
        {
            return (new RECT(0, 0, _srcW, _srcH), 100.0);
        }

        int moveCount = (int)(metadataSize / (uint)sizeof(OutduplMoveRect));
        var moveBuf = _moveBuf.Get(Math.Max(1, moveCount));
        var moveRes = _dup!.GetFrameMoveRects((uint)(moveBuf.Length * sizeof(OutduplMoveRect)), moveBuf, out uint moveRequired);
        if (moveRes.Failure)
        {
            return (new RECT(0, 0, _srcW, _srcH), 100.0);
        }

        int dirtyN = (int)(dirtyRequired / (uint)sizeof(Vortice.RawRect));
        int moveN = (int)(moveRequired / (uint)sizeof(OutduplMoveRect));

        RECT unionRect = new RECT(0, 0, 0, 0);
        long dirtyArea = 0;
        for (int i = 0; i < dirtyN; i++)
        {
            var dr = dirtyBuf[i];
            dirtyArea += (long)(dr.Right - dr.Left) * (dr.Bottom - dr.Top);
            unionRect = DirtyMapper.Union(unionRect, new RECT(dr.Left, dr.Top, dr.Right, dr.Bottom));
        }

        for (int i = 0; i < moveN; i++)
        {
            var mr = moveBuf[i].DestinationRect;
            unionRect = DirtyMapper.Union(unionRect, new RECT(mr.Left, mr.Top, mr.Right, mr.Bottom));
        }

        double pct = 100.0 * dirtyArea / Math.Max(1L, (long)_srcW * _srcH);
        double dirtyPct = (dirtyN == 0 && moveN == 0) ? 1.0 : pct;

        return (unionRect, dirtyPct);
    }

    /// <summary>Returns current feed dirty rect and resets it to empty. Caller holds Device.Gate.</summary>
    public RECT TakeFeedDirty()
    {
        var r = _feedDirty;
        _feedDirty = new RECT(0, 0, 0, 0);
        return r;
    }

    /// <summary>Marks the feed dirty rect as full frame. Caller holds Device.Gate.</summary>
    public void MarkFeedFull()
    {
        _feedDirty = new RECT(0, 0, _srcW, _srcH);
    }

    private (KeyframeRequest Req, int W, int H)? _pendingGrab;

    /// <summary>
    /// Scales the latest frame on the GPU and queues the copy to a staging texture. The map happens on the
    /// next loop pass (FinishGrab), so the thread never blocks on the GPU (NVIDIA stalled a same-pass map by ~20 ms).
    /// </summary>
    private void Grab(KeyframeRequest req)
    {
        if (_latest is null) return;
        if (_pendingGrab is not null) FinishGrab();
        bool changed = _framesSinceGrab && _dirtyPctSinceGrab >= req.DirtyMinPercent;
        if (!req.Force && !changed) { SkippedNoChange++; return; }
        long t0 = Stopwatch.GetTimestamp();
        int w = Math.Min(req.MaxWidth, _srcW) & ~1;
        int h = Math.Max(2, (int)((long)_srcH * w / _srcW) & ~1);
        lock (_dev!.Gate)
        {
            if (_kfScaler is null || _kfScalerWidth != w || _kfScaler.Height != h)
            {
                _kfScaler?.Dispose();
                _kfScaler = new VideoScaler(_dev, _srcW, _srcH, w, h, Format.B8G8R8A8_UNorm);
                _kfScalerWidth = w;
            }
            _kfScaler.Run(_latest, copyToStaging: true);
        }
        _framesSinceGrab = false;
        _dirtyPctSinceGrab = 0;
        _pendingGrab = (req, w, h);
        GrabTicks += Stopwatch.GetTimestamp() - t0;
    }

    private void FinishGrab()
    {
        var (req, w, h) = _pendingGrab!.Value;
        _pendingGrab = null;
        if (_kfScaler?.Staging is null) return;
        long t0 = Stopwatch.GetTimestamp();
        var grab = KeyframeGrab.Alloc(Info.Index, req, w, h, 0);
        try
        {
            lock (_dev!.Gate)
            {
                var m = _dev.Context.Map(_kfScaler.Staging, 0, MapMode.Read, Vortice.Direct3D11.MapFlags.None);
                try
                {
                    byte* src = (byte*)m.DataPointer;
                    for (int y = 0; y < h; y++)
                        Buffer.MemoryCopy(src + (long)y * m.RowPitch, grab.Pixels + (long)y * grab.Stride, grab.Stride, w * 4);
                }
                finally { _dev.Context.Unmap(_kfScaler.Staging, 0); }
            }
            GrabTicks += Stopwatch.GetTimestamp() - t0;
            GrabCount++;
            var deliver = GrabOverride;
            if (deliver is not null) deliver(grab);
            else _onGrab(grab);
            grab = null!;
        }
        finally { grab?.Dispose(); }
    }

    public void Dispose()
    {
        _stop = true;
        if (_thread.IsAlive) _thread.Join(3000);
        _kfScaler?.Dispose();
        lock (_dev?.Gate ?? new object())
        {
            Disposed = true;
            _latest?.Dispose();
            _latest = null;
        }
        _timer.Dispose();
    }

    private sealed class RawRectBuffer
    {
        private Vortice.RawRect[] _buf = new Vortice.RawRect[64];
        public Vortice.RawRect[] Get(int n)
        {
            if (_buf.Length < n) _buf = new Vortice.RawRect[Math.Max(n, _buf.Length * 2)];
            return _buf;
        }
    }

    private sealed class MoveRectBuffer
    {
        private OutduplMoveRect[] _buf = new OutduplMoveRect[16];
        public OutduplMoveRect[] Get(int n)
        {
            if (_buf.Length < n) _buf = new OutduplMoveRect[Math.Max(n, _buf.Length * 2)];
            return _buf;
        }
    }
}
