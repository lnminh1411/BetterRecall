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

/// <summary>Keyframe requested by the hub for one output. Metadata is decided before the grab. Keyframe: store it as a
/// screen capture (dedup applies). ReplaySample: also offer it to the replay sampler (frames of saved replays).
/// DirtyMinPercent is the cheap pre-grab gate on the dirty area since the last grab; the pipeline compares the grab
/// with the last stored keyframe (capture.dirtyMinPercent). Trigger: asked by an event, not the timer.
/// Secrets: password fields (UI Automation) in desktop pixels; Bounds: the output's desktop rectangle (maps desktop to
/// keyframe pixels).</summary>
internal sealed record KeyframeRequest(DateTimeOffset Ts, string App, string ExePath, string Title, string Context, bool Force, int MaxWidth, double DirtyMinPercent, string Site = "",
    bool Keyframe = true, bool ReplaySample = false, bool Trigger = false, RECT[]? Secrets = null, RECT Bounds = default);

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
    private ID3D11Texture2D? _latest;   // desktop orientation, _srcW x _srcH
    private int _srcW, _srcH;           // desktop size of the output
    private int _texW, _texH;           // duplication texture size (scan-out orientation)
    private int _rotation;              // degrees clockwise from the texture to the desktop (OutputRotation)
    private bool _rotationBlocked;      // rotated output the video processor cannot turn: black, no keyframes
    private ID3D11Texture2D? _raw;      // rotated outputs: copy of the acquired texture, input of _rotator
    private VideoScaler? _rotator;
    private VideoScaler? _kfScaler;
    private GpuDevice? _kfScalerDev;
    private int _kfScalerWidth, _kfScalerSrcW, _kfScalerSrcH;
    private RawRectBuffer _dirtyBuf = new();
    private MoveRectBuffer _moveBuf = new();
    private long _lastRetry;

    // Change tracking since the last keyframe grab and since the last sample grab, kept apart (A8).
    private readonly ChangeGate _keyframeGate = new(), _sampleGate = new();

    private FramePacer? _pollPacer;
    private int _lastPollFps;

    private RECT _feedDirty;

    public OutputInfo Info { get; }
    public volatile KeyframeRequest? PendingKeyframe;
    public volatile int ReplayPollFps;
    public volatile Action<KeyframeGrab>? GrabOverride;
    public GpuDevice? Device => _dev;
    public ID3D11Texture2D? Latest => _latest;
    /// <summary>Desktop-oriented size of Latest: a rotated output is turned upright on the capture thread.</summary>
    public int SourceWidth => _srcW;
    public int SourceHeight => _srcH;
    /// <summary>Degrees clockwise from the duplication texture to the desktop (0, 90, 180, 270).</summary>
    public int Rotation => _rotation;
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
                // Nothing may leave this handler: an exception out of a capture thread ends the agent. With two outputs
                // on one GPU the other output's recovery can dispose the shared device under this one (A1).
                try
                {
                    Log.Warn($"capture {Info.Index}", ex);
                    ResetDuplication("error");
                    DropDeviceIfRemoved();
                }
                catch (Exception inner) { try { Log.Warn($"capture {Info.Index} recovery", inner); } catch { } }
                Thread.Sleep(500);
            }
        }
        try { ResetDuplication("stop"); } catch { }
    }

    public static long Qpc100ns(long stopwatchTicks) => FramePacer.ToHundredNs(stopwatchTicks, Stopwatch.Frequency);

    private bool TryCreateDuplication()
    {
        long now = Environment.TickCount64;
        if (!RetryDue(now, _lastRetry, _barren.NextAt)) return false;
        _lastRetry = now;
        try
        {
            // Another output on this adapter may have dropped and replaced the shared device since this one last ran.
            if (_dev is { } held && !_devices.IsCurrent(held)) DropDevice();
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
                var desk = output.Description.DesktopCoordinates;
                int rotation = OutputRotation.Degrees(desc.Rotation);
                var (texW, texH) = OutputRotation.TextureSizeFor((int)desc.ModeDescription.Width, (int)desc.ModeDescription.Height,
                    desk.Right - desk.Left, desk.Bottom - desk.Top, rotation);
                var (deskW, deskH) = OutputRotation.DesktopSize(texW, texH, rotation);
                try { SetUpFrames(texW, texH, deskW, deskH, rotation); }
                catch { try { dup.Dispose(); } catch { } throw; }
                _dup = dup;
                _keyframeGate.Invalidate();
                _sampleGate.Invalidate();
                State = _rotationBlocked ? "rotation unsupported" : "running";
                _upAt = Environment.TickCount64;
                _framesAtUp = FrameCount;
                Log.Info($"duplication up on output {Info.Index} ({_srcW}x{_srcH}{(rotation != 0 ? $", rotated {rotation}" : "")}, vendor 0x{Info.VendorId:X4}{(_lostWhy is { } w ? $", again after {w} #{_resets}" : "")})");
                _lostWhy = null;
                if (_rotationBlocked)
                    Log.Warn($"output {Info.Index} is rotated {rotation} degrees and the GPU's video processor cannot rotate: it is recorded black and gets no keyframes");
                return true;
            }
        }
        catch (Exception ex)
        {
            State = "unavailable";
            Log.Debug($"duplication output {Info.Index} failed: 0x{ex.HResult:X8}");
            DropDeviceIfRemoved();   // a removed device fails DuplicateOutput forever; the next try gets a new one
            return false;
        }
    }

    /// <summary>Creates Latest (desktop orientation) and, for a rotated output, the texture copy and the rotating video
    /// processor. A rotation the GPU cannot do fails closed: Latest stays black and keyframes are skipped.</summary>
    private void SetUpFrames(int texW, int texH, int deskW, int deskH, int rotation)
    {
        var dev = _dev!;
        lock (dev.Gate)
        {
            // A new duplication of the same output (after access lost: a desktop switch, UAC, a mode blip on a virtual
            // GPU) keeps the frame textures, which live on the same device: a new FeedGeneration made the replay rebuild
            // its encoders each time, so frequent resets became a rebuild loop with black first frames (T3).
            if (SameFrames(_latest is not null, (_texW, _texH, _srcW, _srcH, _rotation), (texW, texH, deskW, deskH, rotation)))
            {
                _feedDirty = new RECT(0, 0, _srcW, _srcH);
                return;
            }
            DisposeFrameResources();
            _texW = texW; _texH = texH; _srcW = deskW; _srcH = deskH; _rotation = rotation;
            _rotationBlocked = false;
            _latest = dev.Device.CreateTexture2D(new Texture2DDescription
            {
                Width = (uint)deskW, Height = (uint)deskH, MipLevels = 1, ArraySize = 1, Format = Format.B8G8R8A8_UNorm,
                SampleDescription = new SampleDescription(1, 0), Usage = ResourceUsage.Default,
                BindFlags = BindFlags.ShaderResource | BindFlags.RenderTarget,
            });
            if (rotation != 0)
            {
                try
                {
                    _rotator = new VideoScaler(dev, texW, texH, deskW, deskH, Format.B8G8R8A8_UNorm, withStaging: false, rotationDegrees: rotation);
                    _raw = dev.Device.CreateTexture2D(new Texture2DDescription
                    {
                        Width = (uint)texW, Height = (uint)texH, MipLevels = 1, ArraySize = 1, Format = Format.B8G8R8A8_UNorm,
                        SampleDescription = new SampleDescription(1, 0), Usage = ResourceUsage.Default,
                        BindFlags = BindFlags.ShaderResource | BindFlags.RenderTarget,
                    });
                }
                catch (NotSupportedException)
                {
                    _rotationBlocked = true;
                    using var rtv = dev.Device.CreateRenderTargetView(_latest);
                    dev.Context.ClearRenderTargetView(rtv, new Vortice.Mathematics.Color4(0, 0, 0, 1));
                }
            }
            FeedGeneration++;
            _feedDirty = new RECT(0, 0, _srcW, _srcH);
        }
    }

    /// <summary>The frame textures made for the old sizes and rotation serve the new duplication. Pure.</summary>
    internal static bool SameFrames(bool haveFrames, (int TexW, int TexH, int SrcW, int SrcH, int Rotation) old,
        (int TexW, int TexH, int SrcW, int SrcH, int Rotation) now) => haveFrames && old == now;

    /// <summary>Releases everything created on the current device. Caller holds its gate (or the device is gone).</summary>
    private void DisposeFrameResources()
    {
        _pendingGrab = null;
        try { _kfScaler?.Dispose(); } catch { }
        _kfScaler = null;
        _kfScalerDev = null;
        try { _rotator?.Dispose(); } catch { }
        _rotator = null;
        try { _raw?.Dispose(); } catch { }
        _raw = null;
        try { _latest?.Dispose(); } catch { }
        _latest = null;
    }

    /// <summary>Stops using the current device: removed by the driver, or already dropped or replaced by another
    /// output on the same adapter. Never throws.</summary>
    private void DropDeviceIfRemoved()
    {
        if (_dev is { } d && _devices.InvalidateIfRemoved(d)) DropDevice();
    }

    private void DropDevice()
    {
        if (_dev is not { } d) return;
        try { lock (d.Gate) DisposeFrameResources(); }
        catch { DisposeFrameResources(); }
        _dev = null;
    }

    private void ResetDuplication(string why)
    {
        if (_dup is null) return;
        try { _dup.Dispose(); } catch { }
        _dup = null;
        State = why;
        // T3: say why the duplication comes up again; repeats at most once a minute, with the count, and once when
        // the backoff starts. The next "duplication up" line names the loss too.
        if (why == "stop") return;
        _resets++;
        _lostWhy = why;
        long now = Environment.TickCount64;
        long lived = now - _upAt, frames = FrameCount - _framesAtUp;
        // A duplication lost before its first frame is dead on arrival (in Windows Sandbox's vGPU display every new one
        // got ACCESS_LOST on its first AcquireNextFrame): back off instead of re-creating it every second (T3).
        long delay = 0;
        if (Barren(frames, lived)) delay = _barren.Failed(now);
        else _barren.Succeeded();
        if (_resets == 1 || now - _resetLoggedAt >= 60_000 || _barren.Failures == 2)
        {
            _resetLoggedAt = now;
            Log.Info($"duplication on output {Info.Index} lost: {why} after {lived} ms, {frames} frames ({_resets} so far"
                + (_barren.Failures > 1 ? $", {_barren.Failures} before a frame in a row, next try in {delay / 1000} s)" : ")"));
        }
    }

    /// <summary>A duplication lost before it delivered a frame and within BarrenMs of coming up. Pure.</summary>
    internal static bool Barren(long framesSinceUp, long livedMs) => framesSinceUp == 0 && livedMs < BarrenMs;
    internal const long BarrenMs = 5000, BarrenMaxMs = 15_000;

    /// <summary>A new duplication may be tried: 1 s after the last try and past the barren backoff. Pure.</summary>
    internal static bool RetryDue(long now, long lastTry, long barrenNextAt) => now - lastTry >= 1000 && now >= barrenNextAt;

    private long _resets, _resetLoggedAt, _upAt, _framesAtUp;
    private string? _lostWhy;
    // Barren losses in a row: 1 s, 2 s, 4 s, 8 s, then every 15 s. Reset by a duplication that delivered a frame.
    private readonly RetryBackoff _barren = new(1000, BarrenMaxMs);

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
                if (_rotationBlocked) return;   // stays black
                if (_rotator is not null)
                {
                    var td = tex.Description;
                    if (td.Width != (uint)_texW || td.Height != (uint)_texH)
                    {
                        // Sizes guessed wrong: CopyResource would silently copy nothing. Fail closed.
                        _rotationBlocked = true;
                        using var rtv = _dev.Device.CreateRenderTargetView(_latest!);
                        _dev.Context.ClearRenderTargetView(rtv, new Vortice.Mathematics.Color4(0, 0, 0, 1));
                        Log.Warn($"output {Info.Index}: rotated frame is {td.Width}x{td.Height}, expected {_texW}x{_texH}; recorded black");
                        return;
                    }
                    // Scan-out orientation to desktop orientation: copy (the duplication texture is not a safe processor
                    // input), turn on the video processor, then into Latest.
                    _dev.Context.CopyResource(_raw!, tex);
                    _rotator.Run(_raw!, copyToStaging: false);
                    _dev.Context.CopyResource(_latest!, _rotator.Output);
                }
                else _dev.Context.CopyResource(_latest!, tex);
                var (dirtyRect, dirtyPct) = ProcessChanges(info.TotalMetadataBufferSize);
                _feedDirty = DirtyMapper.Union(_feedDirty, dirtyRect);
                _keyframeGate.Frame(dirtyPct);
                _sampleGate.Frame(dirtyPct);
            }
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

        // Dirty and move rectangles are in texture pixels; Latest and the replay feed are in desktop orientation.
        if (_rotation != 0 && !DirtyMapper.IsEmpty(unionRect))
            unionRect = OutputRotation.TextureToDesktop(unionRect, _texW, _texH, _rotation);
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
        if (_latest is null || _dev is null || _rotationBlocked) return;
        if (_pendingGrab is not null) FinishGrab();
        bool changed = (req.Keyframe && _keyframeGate.Changed(req.DirtyMinPercent))
                       || (req.ReplaySample && _sampleGate.Changed(req.DirtyMinPercent));
        if (!req.Force && !changed) { SkippedNoChange++; return; }
        long t0 = Stopwatch.GetTimestamp();
        int w = Math.Min(req.MaxWidth, _srcW) & ~1;
        int h = Math.Max(2, (int)((long)_srcH * w / _srcW) & ~1);
        lock (_dev!.Gate)
        {
            // Keyed on the device and the source size too: after a device loss the old scaler holds the dead device's
            // video processor and failed every grab (A6).
            if (_kfScaler is null || _kfScalerWidth != w || _kfScaler.Height != h || !ReferenceEquals(_kfScalerDev, _dev)
                || _kfScalerSrcW != _srcW || _kfScalerSrcH != _srcH)
            {
                try { _kfScaler?.Dispose(); } catch { }
                _kfScaler = null;
                _kfScaler = new VideoScaler(_dev, _srcW, _srcH, w, h, Format.B8G8R8A8_UNorm);
                _kfScalerWidth = w;
                _kfScalerDev = _dev;
                _kfScalerSrcW = _srcW;
                _kfScalerSrcH = _srcH;
            }
            _kfScaler.Run(_latest, copyToStaging: true);
        }
        // Only the gates of what this grab serves: a sample-only grab leaves the keyframe gate armed.
        if (req.Keyframe || req.Force) _keyframeGate.Reset();
        if (req.ReplaySample || req.Force) _sampleGate.Reset();
        _pendingGrab = (req, w, h);
        GrabTicks += Stopwatch.GetTimestamp() - t0;
    }

    private void FinishGrab()
    {
        var (req, w, h) = _pendingGrab!.Value;
        _pendingGrab = null;
        if (_kfScaler?.Staging is null || _dev is null || !ReferenceEquals(_kfScalerDev, _dev)) return;
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
        lock (_dev?.Gate ?? new object())
        {
            Disposed = true;
            DisposeFrameResources();
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
