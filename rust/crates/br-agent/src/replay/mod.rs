// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Continuous hardware-encoded replay buffer, fMP4 muxer, encrypted clip library, and sampler.

pub mod bitstream;
pub mod canvas_blit;
pub mod cursor_overlay;
pub mod encoder_watchdog;
pub mod fmp4_writer;
pub mod mf_video_encoder;
pub mod packet_ring;
pub mod replay_engine;
pub mod replay_layout;
pub mod replay_library;
pub mod replay_mux;
pub mod replay_pipeline;
pub mod replay_sampler;
pub mod video_encoders;
pub mod video_stream;
pub mod vpl_av1_encoder;

pub use bitstream::{AnnexB, Av1, BitReader, Nal, Obu, SeqInfo, VideoCodec};
pub use canvas_blit::{CanvasBlit, Nv12Mut, Nv12Ref};
pub use cursor_overlay::{CursorDrawParams, CursorPainter, CursorProbe, CursorShape, CursorState};
pub use encoder_watchdog::EncoderWatchdog;
pub use fmp4_writer::{BoxBuilder, Fmp4Writer, MuxTrack, TrackKind, VideoFormat};
pub use mf_video_encoder::{MediaFoundationRuntime, MfVideoEncoder};
pub use packet_ring::{PacketRing, RingPacket, RingSnapshot};
pub use replay_engine::{ReplayEngine, ReplayEngineConfig};
pub use replay_layout::{
    CanvasLayout, DirtyMapper, FramePacer, MonitorDest, MonitorPlacement, ReplayRate,
    ReplayScaling, dirty_to_dest,
};
pub use replay_library::{ReplayBlobCipher, ReplayBlobWriter, ReplayLibrary};
pub use replay_mux::{AudioRingSource, ReplayMux};
pub use replay_pipeline::{PipelineSpec, ReplayPipeline};
pub use replay_sampler::{ReplaySampler, Sample};
pub use video_encoders::{
    CodecBits, EncoderCandidate, EncoderCreateParams, EncoderFactory, EncoderPlan, LatencyStats,
    PacketSink, VideoEncoder,
};
pub use video_stream::VideoStream;
pub use vpl_av1_encoder::VplAv1Encoder;
