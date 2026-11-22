// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! WASAPI loopback/mic capture, Media Foundation AAC encoder, speech segmentation, and transcription.

pub mod aac_encoder;
pub mod audio_services;
pub mod audio_timeline;
pub mod speech_segmenter;
pub mod wasapi;

pub use aac_encoder::AacEncoder;
pub use audio_services::{
    AudioGuardTimeline, AudioReplayEncoder, AudioStore, AudioStoreError, BoundarySpanPair,
    LoopbackHub, QueueClosed, TranscribeBackend, Transcriber, TranscriptSource,
};
pub use audio_timeline::{AudioTimeline, TimelineStep};
pub use speech_segmenter::{Pcm, Resampler, SpeechSegment, SpeechSegmenter};
pub use wasapi::{
    CaptureStream, PcmFormat, PcmHandler, WAVE_FORMAT_IEEE_FLOAT, WAVE_FORMAT_PCM, WasapiCapture,
    wave_format_float32, wave_format_pcm16,
};
