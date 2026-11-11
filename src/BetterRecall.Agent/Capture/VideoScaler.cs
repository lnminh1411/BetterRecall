// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors
using Vortice.Direct3D11;
using Vortice.DXGI;
using Vortice.Mathematics;

namespace BetterRecall.Agent.Capture;

/// <summary>
/// GPU scale and colour conversion with the D3D11 video processor (fixed-function, no shaders):
/// BGRA source to BGRA (keyframes) or NV12 (replay) at the target size, plus a CPU-readable staging copy.
/// </summary>
internal sealed unsafe class VideoScaler : IDisposable
{
    private readonly GpuDevice _dev;
    private readonly ID3D11VideoProcessorEnumerator _enum;
    private readonly ID3D11VideoProcessor _vp;
    private readonly ID3D11VideoProcessorOutputView _outView;
    private ID3D11VideoProcessorInputView? _inView;
    private ID3D11Texture2D? _inViewSource;

    public ID3D11Texture2D Output { get; }
    public ID3D11Texture2D? Staging { get; }
    public int Width { get; }
    public int Height { get; }
    public Format Format { get; }

    public VideoScaler(GpuDevice dev, int srcW, int srcH, int dstW, int dstH, Format dstFormat, bool withStaging = true)
    {
        _dev = dev;
        Width = dstW;
        Height = dstH;
        Format = dstFormat;
        var content = new VideoProcessorContentDescription
        {
            InputFrameFormat = VideoFrameFormat.Progressive,
            InputFrameRate = new Rational(60, 1),
            InputWidth = (uint)srcW,
            InputHeight = (uint)srcH,
            OutputFrameRate = new Rational(60, 1),
            OutputWidth = (uint)dstW,
            OutputHeight = (uint)dstH,
            Usage = VideoUsage.PlaybackNormal,
        };
        dev.VideoDevice.CreateVideoProcessorEnumerator(ref content, out _enum).CheckError();
        dev.VideoDevice.CreateVideoProcessor(_enum, 0, out _vp).CheckError();

        Output = dev.Device.CreateTexture2D(new Texture2DDescription
        {
            Width = (uint)dstW,
            Height = (uint)dstH,
            MipLevels = 1,
            ArraySize = 1,
            Format = dstFormat,
            SampleDescription = new SampleDescription(1, 0),
            Usage = ResourceUsage.Default,
            BindFlags = BindFlags.RenderTarget,
        });
        if (withStaging) Staging = dev.Device.CreateTexture2D(new Texture2DDescription
        {
            Width = (uint)dstW,
            Height = (uint)dstH,
            MipLevels = 1,
            ArraySize = 1,
            Format = dstFormat,
            SampleDescription = new SampleDescription(1, 0),
            Usage = ResourceUsage.Staging,
            CPUAccessFlags = CpuAccessFlags.Read,
        });
        var ovd = new VideoProcessorOutputViewDescription { ViewDimension = VideoProcessorOutputViewDimension.Texture2D };
        dev.VideoDevice.CreateVideoProcessorOutputView(Output, _enum, ovd, out _outView).CheckError();

        var vc = dev.VideoContext;
        var inCs = new VideoProcessorColorSpace { Usage = 0, RGB_Range = 0 };
        vc.VideoProcessorSetStreamColorSpace(_vp, 0, inCs);
        var outCs = dstFormat == Format.NV12
            ? new VideoProcessorColorSpace { Usage = 0, RGB_Range = 0, YCbCr_Matrix = 1, Nominal_Range = 1 }
            : new VideoProcessorColorSpace { Usage = 0, RGB_Range = 0 };
        vc.VideoProcessorSetOutputColorSpace(_vp, outCs);
        vc.VideoProcessorSetStreamFrameFormat(_vp, 0, VideoFrameFormat.Progressive);
        vc.VideoProcessorSetStreamAutoProcessingMode(_vp, 0, false);
        vc.VideoProcessorSetStreamSourceRect(_vp, 0, true, new Vortice.RawRect(0, 0, srcW, srcH));
        vc.VideoProcessorSetStreamDestRect(_vp, 0, true, new Vortice.RawRect(0, 0, dstW, dstH));
        vc.VideoProcessorSetOutputTargetRect(_vp, true, new Vortice.RawRect(0, 0, dstW, dstH));
    }

    /// <summary>Scales source into Output, then copies Output to Staging. Caller holds the device gate.</summary>
    public void Run(ID3D11Texture2D source, bool copyToStaging)
    {
        if (!ReferenceEquals(_inViewSource, source))
        {
            _inView?.Dispose();
            var ivd = new VideoProcessorInputViewDescription
            {
                FourCC = 0,
                ViewDimension = VideoProcessorInputViewDimension.Texture2D,
                Texture2D = new Texture2DVideoProcessorInputView { MipSlice = 0, ArraySlice = 0 },
            };
            _dev.VideoDevice.CreateVideoProcessorInputView(source, _enum, ivd, out _inView).CheckError();
            _inViewSource = source;
        }
        var stream = new VideoProcessorStream { Enable = true, InputSurface = _inView! };
        _dev.VideoContext.VideoProcessorBlt(_vp, _outView, 0, 1, [stream]).CheckError();
        if (copyToStaging && Staging is not null) _dev.Context.CopyResource(Staging, Output);
    }

    public void Dispose()
    {
        _inView?.Dispose();
        _outView.Dispose();
        Staging?.Dispose();
        Output.Dispose();
        _vp.Dispose();
        _enum.Dispose();
    }
}
