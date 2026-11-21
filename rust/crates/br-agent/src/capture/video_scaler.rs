// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! D3D11 fixed-function video processor scaler and rotator (`Capture\VideoScaler.cs`).

use std::sync::Arc;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_TEX2D_VPIV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CAPS, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Resource,
    ID3D11Texture2D, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use crate::capture::gpu_devices::GpuDevice;
use crate::capture::output_rotation::OutputRotation;

#[derive(Debug, Clone)]
pub enum VideoScalerError {
    RotationUnsupported,
    Failed(String),
}

impl std::fmt::Display for VideoScalerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RotationUnsupported => write!(f, "The video processor cannot rotate."),
            Self::Failed(s) => write!(f, "{s}"),
        }
    }
}

/// GPU scale, colour conversion, and rotation via D3D11 `ID3D11VideoProcessor`.
pub struct VideoScaler {
    dev: Arc<GpuDevice>,
    enumerator: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    out_view: ID3D11VideoProcessorOutputView,
    in_view: Option<ID3D11VideoProcessorInputView>,
    in_view_source_ptr: usize,
    pub output: ID3D11Texture2D,
    pub staging: Option<ID3D11Texture2D>,
    pub width: i32,
    pub height: i32,
    pub format: DXGI_FORMAT,
}

unsafe impl Send for VideoScaler {}

/// Parameters for [`VideoScaler::new`].
#[derive(Debug, Clone, Copy)]
pub struct VideoScalerParams {
    pub src_w: i32,
    pub src_h: i32,
    pub dst_w: i32,
    pub dst_h: i32,
    pub dst_format: DXGI_FORMAT,
    pub with_staging: bool,
    pub rotation_degrees: i32,
}

impl VideoScaler {
    pub fn new(dev: Arc<GpuDevice>, params: VideoScalerParams) -> Result<Self, VideoScalerError> {
        let VideoScalerParams {
            src_w,
            src_h,
            dst_w,
            dst_h,
            dst_format,
            with_staging,
            rotation_degrees,
        } = params;
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: src_w as u32,
            InputHeight: src_h as u32,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: dst_w as u32,
            OutputHeight: dst_h as u32,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { dev.video_device.CreateVideoProcessorEnumerator(&content) }
            .map_err(|e| VideoScalerError::Failed(format!("CreateVideoProcessorEnumerator: {e}")))?;
        if rotation_degrees != 0 {
            let mut caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
            unsafe { enumerator.GetVideoProcessorCaps(&mut caps) }.map_err(|e| {
                VideoScalerError::Failed(format!("GetVideoProcessorCaps: {e}"))
            })?;
            if (caps.FeatureCaps & (D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION.0 as u32)) == 0 {
                return Err(VideoScalerError::RotationUnsupported);
            }
        }
        let vp = unsafe { dev.video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|e| VideoScalerError::Failed(format!("CreateVideoProcessor: {e}")))?;

        let out_desc = D3D11_TEXTURE2D_DESC {
            Width: dst_w as u32,
            Height: dst_h as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: dst_format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut output = None;
        unsafe { dev.device.CreateTexture2D(&out_desc, None, Some(&mut output)) }
            .map_err(|e| VideoScalerError::Failed(format!("CreateTexture2D output: {e}")))?;
        let output =
            output.ok_or_else(|| VideoScalerError::Failed("output texture null".to_string()))?;

        let staging = if with_staging {
            let st_desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..out_desc
            };
            let mut st = None;
            unsafe { dev.device.CreateTexture2D(&st_desc, None, Some(&mut st)) }
                .map_err(|e| VideoScalerError::Failed(format!("CreateTexture2D staging: {e}")))?;
            st
        } else {
            None
        };

        let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let out_res: ID3D11Resource = output
            .cast()
            .map_err(|e| VideoScalerError::Failed(format!("output resource cast: {e}")))?;
        let mut out_view = None;
        unsafe {
            dev.video_device.CreateVideoProcessorOutputView(
                &out_res,
                &enumerator,
                &ovd,
                Some(&mut out_view),
            )
        }
        .map_err(|e| VideoScalerError::Failed(format!("CreateVideoProcessorOutputView: {e}")))?;
        let out_view =
            out_view.ok_or_else(|| VideoScalerError::Failed("out_view null".to_string()))?;

        let vc = &dev.video_context;
        // D3D11_VIDEO_PROCESSOR_COLOR_SPACE bitfield:
        // bit 0 Usage, bit 1 RGB_Range, bit 2 YCbCr_Matrix (1 = BT.709), bit 3 xvYCC, bits 4..5 Nominal_Range (1 = 16_235).
        let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
        let out_cs = if dst_format == DXGI_FORMAT_NV12 {
            D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (1 << 2) | (1 << 4),
            }
        } else {
            D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 }
        };
        let src_rect = RECT {
            left: 0,
            top: 0,
            right: src_w,
            bottom: src_h,
        };
        let dst_rect = RECT {
            left: 0,
            top: 0,
            right: dst_w,
            bottom: dst_h,
        };
        unsafe {
            vc.VideoProcessorSetStreamColorSpace(&vp, 0, &in_cs);
            vc.VideoProcessorSetOutputColorSpace(&vp, &out_cs);
            vc.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vc.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            if rotation_degrees != 0 {
                vc.VideoProcessorSetStreamRotation(
                    &vp,
                    0,
                    true,
                    OutputRotation::to_video_processor(rotation_degrees),
                );
            }
            vc.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&src_rect));
            vc.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&dst_rect));
            vc.VideoProcessorSetOutputTargetRect(&vp, true, Some(&dst_rect));
        }

        Ok(Self {
            dev,
            enumerator,
            vp,
            out_view,
            in_view: None,
            in_view_source_ptr: 0,
            output,
            staging,
            width: dst_w,
            height: dst_h,
            format: dst_format,
        })
    }

    /// Scales `source` into `self.output`, then optionally copies `self.output` to `self.staging`.
    /// Caller must hold `dev.gate`.
    pub fn run(&mut self, source: &ID3D11Texture2D, copy_to_staging: bool) -> Result<(), String> {
        let src_ptr = source.as_raw() as usize;
        if self.in_view_source_ptr != src_ptr || self.in_view.is_none() {
            self.in_view = None;
            let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let src_res: ID3D11Resource = source
                .cast()
                .map_err(|e| format!("source resource cast: {e}"))?;
            let mut iv = None;
            unsafe {
                self.dev.video_device.CreateVideoProcessorInputView(
                    &src_res,
                    &self.enumerator,
                    &ivd,
                    Some(&mut iv),
                )
            }
            .map_err(|e| format!("CreateVideoProcessorInputView: {e}"))?;
            self.in_view = iv;
            self.in_view_source_ptr = src_ptr;
        }
        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(self.in_view.clone()),
            ..Default::default()
        }];
        let res = unsafe {
            self.dev
                .video_context
                .VideoProcessorBlt(&self.vp, &self.out_view, 0, &streams)
        };
        // Drop the cloned COM pointer inside ManuallyDrop after Blt completes.
        unsafe {
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
        }
        res.map_err(|e| format!("VideoProcessorBlt: {e}"))?;
        if copy_to_staging
            && let Some(ref st) = self.staging
        {
            let st_res: ID3D11Resource = st
                .cast()
                .map_err(|e| format!("staging resource cast: {e}"))?;
            let out_res: ID3D11Resource = self
                .output
                .cast()
                .map_err(|e| format!("output resource cast: {e}"))?;
            unsafe {
                self.dev.context.CopyResource(&st_res, &out_res);
            }
        }
        Ok(())
    }
}
