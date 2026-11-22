// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Snapshots video and audio rings and muxes them to fragmented MP4 (`Replay\ReplayMux.cs`).

use std::io::Write;
use std::sync::Arc;

use crate::capture::output_capture::OutputCapture;
use crate::native::qpc;
use crate::replay::fmp4_writer::{Fmp4Writer, MuxTrack, TrackKind};
use crate::replay::packet_ring::{PacketRing, RingSnapshot};
use crate::replay::video_stream::VideoStream;

/// Audio track source for the mux: an AAC ring plus its format.
#[derive(Clone)]
pub struct AudioRingSource {
    pub ring: Arc<PacketRing>,
    pub sample_rate: i32,
    pub channels: i32,
    pub audio_specific_config: Vec<u8>,
}

pub struct ReplayMux;

impl ReplayMux {
    /// Snapshots every ring from `(now - seconds)` and writes one fragmented MP4.
    /// Returns duration in ms.
    pub fn save<W: Write + ?Sized>(
        output: &mut W,
        video: &[Arc<VideoStream>],
        audio: Option<&AudioRingSource>,
        seconds: i32,
    ) -> Result<i32, String> {
        let now = OutputCapture::qpc_100ns(qpc());
        let from = now - (seconds as i64) * 10_000_000;

        let video_snaps: Vec<RingSnapshot> = video.iter().map(|v| v.ring().snapshot(from)).collect();
        let audio_snap: Option<RingSnapshot> = audio.map(|a| a.ring.snapshot(from));

        let mut tracks: Vec<MuxTrack<'_>> = Vec::new();
        for (v, snap) in video.iter().zip(video_snaps.iter()) {
            if let Some(track) = v.to_mux_track(snap) {
                tracks.push(track);
            }
        }
        if tracks.is_empty() {
            return Err("Replay buffer is empty.".to_string());
        }
        if let (Some(a), Some(snap)) = (audio, audio_snap.as_ref())
            && !snap.packets().is_empty()
        {
            tracks.push(MuxTrack {
                kind: TrackKind::Audio,
                codec: "mp4a".to_string(),
                video_codec: None,
                width: 0,
                height: 0,
                sample_rate: a.sample_rate,
                channels: a.channels,
                config: a.audio_specific_config.clone(),
                default_duration_100ns: 1024 * 10_000_000 / a.sample_rate as i64,
                samples: snap,
                formats: None,
                fps: 0,
            });
        }
        Fmp4Writer::write(output, &tracks)
    }
}
