// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using System.Diagnostics;
using BetterRecall.Agent.Capture;
using BetterRecall.Agent.Native;
using Vortice.Direct3D11;
using Vortice.DXGI;

namespace BetterRecall.Agent.Replay;

internal sealed record PipelineSpec(
    string Key,
    IReadOnlyList<OutputCapture> Sources,
    EncoderCandidate Candidate,
    int Fps,
    int Kbps,
    string Resolution,
    int ScalePercent);

internal sealed unsafe class ReplayPipeline : IDisposable
{
    private readonly GpuDevice _encDev;
    private readonly ID3D11Texture2D _mirror;
    private readonly ID3D11Texture2D[]? _pool;
    private readonly List<Slot> _slots;
    private int _poolIndex = -1;
    private bool _canvasChanged = true;
    private long _lastSubmitPts = long.MinValue / 2;
    private long _lastForcedPts = long.MinValue / 2;
    private readonly CursorPainter _painter = new();
    private CursorState _cursor = CursorState.Hidden;

    public PipelineSpec Spec { get; }
    public VideoStream Stream { get; }
    public IVideoEncoder Encoder { get; }
    public CanvasLayout Layout { get; }
    public FramePacer Pacer { get; }
    public bool NeedsRebuild { get; private set; }
    /// <summary>Benchmark only: paint moving content over the canvas every tick (see CanvasBlit.PaintMotion).</summary>
    public bool SyntheticMotion { get; set; }
    /// <summary>Stopwatch ticks spent painting synthetic motion, so the benchmark can leave it out of the CPU cost.</summary>
    public long MotionTicks, MotionTicksAtWarm;
    private long _motionFrame;

    public long CopyTicks, CopyCount, SubmitCount, RepeatCount, BusyCount;

    private ReplayPipeline(
        PipelineSpec spec,
        VideoStream stream,
        IVideoEncoder encoder,
        CanvasLayout layout,
        GpuDevice encDev,
        ID3D11Texture2D mirror,
        ID3D11Texture2D[]? pool,
        List<Slot> slots)
    {
        Spec = spec;
        Stream = stream;
        Encoder = encoder;
        Layout = layout;
        Pacer = new FramePacer(spec.Fps, Stopwatch.Frequency);
        _encDev = encDev;
        _mirror = mirror;
        _pool = pool;
        _slots = slots;
    }

    public static ReplayPipeline? Create(PipelineSpec spec, VideoStream stream, GpuDeviceCache devices, out string? error)
    {
        error = null;
        IVideoEncoder? encoder = null;
        ID3D11Texture2D? mirror = null;
        ID3D11Texture2D[]? pool = null;
        var slots = new List<Slot>();
        GpuDevice? encDev = null;

        try
        {
            foreach (var s in spec.Sources)
            {
                if (s.SourceWidth <= 0 || s.Device is null)
                {
                    error = "source not ready";
                    return null;
                }
            }

            var placements = spec.Sources
                .Select(o => new MonitorPlacement(o.Info.Index, o.Info.Left, o.Info.Top, o.SourceWidth, o.SourceHeight))
                .ToList();

            encDev = devices.Get(spec.Candidate.Adapter.Luid);
            CanvasLayout? layout = null;
            int prevW = -1, prevH = -1;
            string? lastErr = null;

            foreach (var (maxW, maxH) in EncoderFactory.SizeLimits)
            {
                var candidateLayout = CanvasLayout.Compute(placements, spec.Resolution, spec.ScalePercent, maxW, maxH);
                if (candidateLayout.Width == prevW && candidateLayout.Height == prevH)
                    continue;
                prevW = candidateLayout.Width;
                prevH = candidateLayout.Height;

                int session = stream.BeginSession(spec.Candidate.Codec, candidateLayout.Width, candidateLayout.Height, spec.Fps);
                encoder = EncoderFactory.Create(
                    spec.Candidate,
                    encDev,
                    candidateLayout.Width,
                    candidateLayout.Height,
                    spec.Fps,
                    spec.Kbps,
                    (d, p, k) => stream.OnPacket(session, d, p, k),
                    out lastErr);

                if (encoder is not null)
                {
                    layout = candidateLayout;
                    break;
                }
            }

            if (encoder is null || layout is null)
            {
                error = lastErr ?? "encoder creation failed";
                return null;
            }

            int surfW = encoder.SurfaceWidth;
            int surfH = encoder.SurfaceHeight;

            lock (encDev.Gate)
            {
                var mirrorDesc = new Texture2DDescription
                {
                    Width = (uint)surfW,
                    Height = (uint)surfH,
                    MipLevels = 1,
                    ArraySize = 1,
                    Format = Format.NV12,
                    SampleDescription = new SampleDescription(1, 0),
                    Usage = ResourceUsage.Staging,
                    CPUAccessFlags = CpuAccessFlags.Read | CpuAccessFlags.Write,
                };
                mirror = encDev.Device.CreateTexture2D(mirrorDesc);
                var mapped = encDev.Context.Map(mirror, 0, MapMode.ReadWrite, Vortice.Direct3D11.MapFlags.None);
                try
                {
                    CanvasBlit.Clear((byte*)mapped.DataPointer, (int)mapped.RowPitch, surfH, surfW);
                }
                finally
                {
                    encDev.Context.Unmap(mirror, 0);
                }

                if (!encoder.CopiesInput)
                {
                    pool = new ID3D11Texture2D[4];
                    var poolDesc = new Texture2DDescription
                    {
                        Width = (uint)surfW,
                        Height = (uint)surfH,
                        MipLevels = 1,
                        ArraySize = 1,
                        Format = Format.NV12,
                        SampleDescription = new SampleDescription(1, 0),
                        Usage = ResourceUsage.Default,
                        BindFlags = BindFlags.RenderTarget | BindFlags.ShaderResource,
                    };
                    for (int i = 0; i < 4; i++)
                    {
                        pool[i] = encDev.Device.CreateTexture2D(poolDesc);
                    }
                }
            }

            foreach (var src in spec.Sources)
            {
                var destDest = layout.For(src.Info.Index) ?? throw new InvalidOperationException($"No destination for monitor {src.Info.Index}");
                var destRect = destDest.Rect;
                int destW = destRect.Right - destRect.Left;
                int destH = destRect.Bottom - destRect.Top;

                var srcDev = src.Device!;
                VideoScaler scaler;
                var stagings = new ID3D11Texture2D[2];

                lock (srcDev.Gate)
                {
                    scaler = new VideoScaler(srcDev, src.SourceWidth, src.SourceHeight, destW, destH, Format.NV12, withStaging: false);
                    var stagingDesc = new Texture2DDescription
                    {
                        Width = (uint)destW,
                        Height = (uint)destH,
                        MipLevels = 1,
                        ArraySize = 1,
                        Format = Format.NV12,
                        SampleDescription = new SampleDescription(1, 0),
                        Usage = ResourceUsage.Staging,
                        CPUAccessFlags = CpuAccessFlags.Read,
                    };
                    for (int i = 0; i < 2; i++)
                    {
                        stagings[i] = srcDev.Device.CreateTexture2D(stagingDesc);
                    }
                }

                slots.Add(new Slot
                {
                    Source = src,
                    Dest = destRect,
                    Scaler = scaler,
                    Stagings = stagings,
                    Generation = src.FeedGeneration,
                    SourceWidth = src.SourceWidth,
                    SourceHeight = src.SourceHeight,
                });
            }

            return new ReplayPipeline(spec, stream, encoder, layout, encDev, mirror, pool, slots);
        }
        catch (Exception ex)
        {
            error = ex.Message;
            encoder?.Dispose();
            if (encDev is not null)
            {
                lock (encDev.Gate)
                {
                    mirror?.Dispose();
                    if (pool is not null)
                    {
                        foreach (var t in pool) t?.Dispose();
                    }
                }
            }
            else
            {
                mirror?.Dispose();
                if (pool is not null)
                {
                    foreach (var t in pool) t?.Dispose();
                }
            }
            foreach (var s in slots)
            {
                var srcDev = s.Source.Device;
                if (srcDev is not null)
                {
                    lock (srcDev.Gate)
                    {
                        s.Scaler.Dispose();
                        foreach (var t in s.Stagings) t?.Dispose();
                    }
                }
                else
                {
                    s.Scaler.Dispose();
                    foreach (var t in s.Stagings) t?.Dispose();
                }
            }
            return null;
        }
    }

    public void Tick(long pts100ns, RECT[] desktopMasks, bool forceAll)
    {
        foreach (var slot in _slots)
        {
            if (slot.Source.Disposed ||
                slot.Source.FeedGeneration != slot.Generation ||
                slot.Source.SourceWidth != slot.SourceWidth ||
                slot.Source.SourceHeight != slot.SourceHeight)
            {
                NeedsRebuild = true;
                return;
            }
        }

        long t0 = Stopwatch.GetTimestamp();
        bool copiedAnything = false;
        bool canvasChanged = false;

        byte* mirrorPtr = null;
        int mirrorPitch = 0;
        bool mirrorMapped = false;

        void EnsureMirrorMapped()
        {
            if (!mirrorMapped)
            {
                lock (_encDev.Gate)
                {
                    var mapped = _encDev.Context.Map(_mirror, 0, MapMode.ReadWrite, Vortice.Direct3D11.MapFlags.None);
                    mirrorPtr = (byte*)mapped.DataPointer;
                    mirrorPitch = (int)mapped.RowPitch;
                    mirrorMapped = true;
                }
            }
        }

        // Pointer: put back the pixels under last tick's pointer before anything is copied over them.
        var cursor = CursorProbe.Current();
        bool cursorMoved = cursor != _cursor;
        _cursor = cursor;
        bool anyPending = false;
        foreach (var s in _slots) anyPending |= s.Pending >= 0;
        bool restored = false;
        if (_painter.HasPatch && (cursorMoved || anyPending))
        {
            EnsureMirrorMapped();
            _painter.Restore(mirrorPtr, mirrorPitch, Encoder.SurfaceHeight);
            restored = true;
            canvasChanged = true;
        }

        var slotWritten = new bool[_slots.Count];
        for (int i = 0; i < _slots.Count; i++)
        {
            var slot = _slots[i];
            if (slot.Pending >= 0)
            {
                var srcDev = slot.Source.Device;
                if (srcDev is not null)
                {
                    int destH = slot.Dest.Bottom - slot.Dest.Top;
                    lock (srcDev.Gate)
                    {
                        var staging = slot.Stagings[slot.Pending];
                        var srcMapped = srcDev.Context.Map(staging, 0, MapMode.Read, Vortice.Direct3D11.MapFlags.None);
                        try
                        {
                            EnsureMirrorMapped();
                            CanvasBlit.CopyRect(
                                (byte*)srcMapped.DataPointer,
                                (int)srcMapped.RowPitch,
                                destH,
                                mirrorPtr,
                                mirrorPitch,
                                Encoder.SurfaceHeight,
                                slot.PendingRect,
                                slot.Dest.Left,
                                slot.Dest.Top);
                        }
                        finally
                        {
                            srcDev.Context.Unmap(staging, 0);
                        }
                    }
                    slot.Pending = -1;
                    slotWritten[i] = true;
                    canvasChanged = true;
                    copiedAnything = true;
                }
            }
        }

        for (int i = 0; i < _slots.Count; i++)
        {
            var slot = _slots[i];
            int srcLeft = slot.Source.Info.Left;
            int srcTop = slot.Source.Info.Top;
            int srcW = slot.SourceWidth;
            int srcH = slot.SourceHeight;
            int destW = slot.Dest.Right - slot.Dest.Left;
            int destH = slot.Dest.Bottom - slot.Dest.Top;

            var newMasks = new List<RECT>();
            foreach (var m in desktopMasks)
            {
                int l = Math.Max(m.Left, srcLeft);
                int t = Math.Max(m.Top, srcTop);
                int r = Math.Min(m.Right, srcLeft + srcW);
                int b = Math.Min(m.Bottom, srcTop + srcH);
                if (r > l && b > t)
                {
                    var srcLocal = new RECT(l - srcLeft, t - srcTop, r - srcLeft, b - srcTop);
                    var destLocal = DirtyMapper.ToDest(srcLocal, srcW, srcH, destW, destH, pad: 0);
                    var canvasMask = new RECT(
                        destLocal.Left + slot.Dest.Left,
                        destLocal.Top + slot.Dest.Top,
                        destLocal.Right + slot.Dest.Left,
                        destLocal.Bottom + slot.Dest.Top);
                    newMasks.Add(canvasMask);
                }
            }

            bool maskChanged = !slot.Masks.SequenceEqual(newMasks);
            if (maskChanged)
            {
                slot.Masks = newMasks.ToArray();
                slot.ForceFull = true;
            }

            if ((slotWritten[i] || maskChanged) && slot.Masks.Length > 0)
            {
                EnsureMirrorMapped();
                foreach (var mr in slot.Masks)
                {
                    CanvasBlit.FillBlack(mirrorPtr, mirrorPitch, Encoder.SurfaceHeight, mr);
                }
                canvasChanged = true;
            }
        }

        if (cursor.Visible && !_painter.HasPatch && (cursorMoved || restored) && FindCursorSlot(cursor) is { } cs)
        {
            EnsureMirrorMapped();
            _painter.Draw(mirrorPtr, mirrorPitch, Encoder.SurfaceHeight, cs.Shape, cs.X, cs.Y, cs.Sx, cs.Sy, cs.Clip);
            canvasChanged = true;
        }

        if (SyntheticMotion)
        {
            long m0 = Stopwatch.GetTimestamp();
            EnsureMirrorMapped();
            CanvasBlit.PaintMotion(mirrorPtr, mirrorPitch, Encoder.SurfaceHeight, Layout.Width, Layout.Height, ++_motionFrame);
            MotionTicks += Stopwatch.GetTimestamp() - m0;
            canvasChanged = true;
        }

        for (int i = 0; i < _slots.Count; i++)
        {
            var slot = _slots[i];
            var srcDev = slot.Source.Device;
            if (srcDev is not null)
            {
                int destW = slot.Dest.Right - slot.Dest.Left;
                int destH = slot.Dest.Bottom - slot.Dest.Top;
                int srcW = slot.SourceWidth;
                int srcH = slot.SourceHeight;

                lock (srcDev.Gate)
                {
                    var dirty = slot.Source.TakeFeedDirty();
                    if (slot.ForceFull || forceAll) dirty = new RECT(0, 0, srcW, srcH);
                    slot.ForceFull = false;

                    if (!DirtyMapper.IsEmpty(dirty) && slot.Source.Latest is not null)
                    {
                        var rect = DirtyMapper.ToDest(dirty, srcW, srcH, destW, destH);
                        slot.Scaler.Run(slot.Source.Latest, copyToStaging: false);
                        int k = slot.NextStaging ^= 1;
                        srcDev.Context.CopyResource(slot.Stagings[k], slot.Scaler.Output);
                        slot.Pending = k;
                        slot.PendingRect = rect;
                        copiedAnything = true;
                    }
                }
            }
        }

        if (copiedAnything)
        {
            CopyTicks += Stopwatch.GetTimestamp() - t0;
            CopyCount++;
        }

        if (mirrorMapped)
        {
            lock (_encDev.Gate)
            {
                _encDev.Context.Unmap(_mirror, 0);
            }
            mirrorMapped = false;
        }

        _canvasChanged |= canvasChanged;

        long halfFrame = 5_000_000L / Spec.Fps;
        bool newPicture = _canvasChanged || forceAll;
        if (!newPicture && pts100ns - _lastSubmitPts < 10_000_000L - halfFrame)
        {
            RepeatCount++;
            return;
        }

        if (!Encoder.ReadyForInput)
        {
            BusyCount++;
            return;
        }

        bool forceKey = SubmitCount == 0 || pts100ns - Math.Max(Stream.LastKeyPts, _lastForcedPts) >= 10_000_000L - halfFrame;

        ID3D11Texture2D tex;
        if (Encoder.CopiesInput)
        {
            tex = _mirror;
        }
        else
        {
            int nextPool = (_poolIndex + 1) % 4;
            tex = _pool![nextPool];
            lock (_encDev.Gate)
            {
                _encDev.Context.CopyResource(tex, _mirror);
            }
            _poolIndex = nextPool;
        }

        if (Encoder.TrySubmit(tex, pts100ns, forceKey))
        {
            _canvasChanged = false;
            _lastSubmitPts = pts100ns;
            if (forceKey) _lastForcedPts = pts100ns;
            SubmitCount++;
        }
    }

    /// <summary>Where the pointer goes: the slot whose monitor holds its hotspot, scaled like that monitor.</summary>
    private (CursorShape Shape, double X, double Y, double Sx, double Sy, RECT Clip)? FindCursorSlot(CursorState cursor)
    {
        foreach (var slot in _slots)
        {
            var info = slot.Source.Info;
            int lx = cursor.X - info.Left, ly = cursor.Y - info.Top;
            if (lx < 0 || ly < 0 || lx >= slot.SourceWidth || ly >= slot.SourceHeight) continue;
            var shape = CursorProbe.Shape(cursor.Handle);
            if (shape is null) return null;
            double sx = (double)(slot.Dest.Right - slot.Dest.Left) / slot.SourceWidth;
            double sy = (double)(slot.Dest.Bottom - slot.Dest.Top) / slot.SourceHeight;
            return (shape, slot.Dest.Left + lx * sx, slot.Dest.Top + ly * sy, sx, sy, slot.Dest);
        }
        return null;
    }

    public void Dispose()
    {
        Encoder.Dispose();
        lock (_encDev.Gate)
        {
            _mirror?.Dispose();
            if (_pool is not null)
            {
                foreach (var t in _pool) t?.Dispose();
            }
        }
        foreach (var slot in _slots)
        {
            var srcDev = slot.Source.Device;
            if (srcDev is not null)
            {
                lock (srcDev.Gate)
                {
                    slot.Scaler.Dispose();
                    foreach (var t in slot.Stagings) t?.Dispose();
                }
            }
            else
            {
                slot.Scaler.Dispose();
                foreach (var t in slot.Stagings) t?.Dispose();
            }
        }
    }

    private sealed class Slot
    {
        public required OutputCapture Source;
        public required RECT Dest;
        public required VideoScaler Scaler;
        public required ID3D11Texture2D[] Stagings;
        public required int Generation;
        public required int SourceWidth;
        public required int SourceHeight;
        public int Pending = -1;
        public RECT PendingRect;
        public int NextStaging;
        public RECT[] Masks = [];
        public bool ForceFull = true;
    }
}
