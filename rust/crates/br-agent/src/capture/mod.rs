// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Desktop capture, smart change detection, keyframe encoding, and pipeline (`Capture\`).

pub mod av1;
pub mod avif_writer;
pub mod capture_service;
pub mod change_gate;
pub mod change_map;
pub mod dhash;
pub mod encode_governor;
pub mod gpu_devices;
pub mod image_codec;
pub mod keyframe_encoder;
pub mod keyframe_pipeline;
pub mod monitor_selection;
pub mod output_capture;
pub mod output_rotation;
pub mod video_scaler;

pub use av1::{Av1, BitReader, Obu, SeqInfo};
pub use avif_writer::AvifWriter;
pub use capture_service::{CaptureService, CaptureServiceOptions};
pub use change_gate::ChangeGate;
pub use change_map::{CapturePolicy, ChangeMap, ChangeResult, KeepReason};
pub use dhash::{DHash, DedupFilter};
pub use encode_governor::{EncodeGovernor, EncodeLevel};
pub use gpu_devices::{
    AdapterInfo, GpuDevice, GpuDeviceCache, GpuDevices, OutputInfo, normalize_refresh, refresh_hz,
};
pub use image_codec::{ImageCodec, ImageCodecDecoder};
pub use keyframe_encoder::KeyframeEncoder;
pub use keyframe_pipeline::{
    KeyframePipeline, KeyframeReader, KeyframeStore, KeyframeStoreError, OcrHealth, ReplaySample,
    ReplaySampleSink, WorkerKeyframeReader,
};
pub use monitor_selection::{MonitorPin, MonitorSelection};
pub use output_capture::{DirtyMapper, FramePacer, KeyframeGrab, KeyframeRequest, OutputCapture};
pub use output_rotation::OutputRotation;
pub use video_scaler::{VideoScaler, VideoScalerError, VideoScalerParams};
