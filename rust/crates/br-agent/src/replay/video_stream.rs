// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Encoded video stream combining a [`PacketRing`] with per-session codec formats (`Replay\VideoStream.cs`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::replay::bitstream::VideoCodec;
use crate::replay::fmp4_writer::{MuxTrack, TrackKind, VideoFormat};
use crate::replay::packet_ring::{PacketRing, RingSnapshot};
use crate::replay::video_encoders::CodecBits;

struct StreamState {
    formats: HashMap<i32, VideoFormat>,
    current_session: i32,
    fps: i32,
    last_key_pts: i64,
    last_written_pts: i64,
    dropped_before_key: i64,
    session_codec: VideoCodec,
    session_width: i32,
    session_height: i32,
}

/// One encoded video of the replay: a [`PacketRing`] plus the formats its GOPs use.
pub struct VideoStream {
    key: String,
    ring: PacketRing,
    state: Mutex<StreamState>,
}

impl VideoStream {
    pub fn new(
        key: impl Into<String>,
        capacity_bytes: usize,
        window: Duration,
        lock_memory: bool,
    ) -> Result<Self, String> {
        let ring = PacketRing::new(capacity_bytes, window, lock_memory)?;
        Ok(Self {
            key: key.into(),
            ring,
            state: Mutex::new(StreamState {
                formats: HashMap::new(),
                current_session: 0,
                fps: 0,
                last_key_pts: i64::MIN / 2,
                last_written_pts: i64::MIN,
                dropped_before_key: 0,
                session_codec: VideoCodec::H264,
                session_width: 0,
                session_height: 0,
            }),
        })
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn ring(&self) -> &PacketRing {
        &self.ring
    }

    pub fn fps(&self) -> i32 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).fps
    }

    pub fn last_key_pts(&self) -> i64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_key_pts
    }

    pub fn dropped_before_key(&self) -> i64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dropped_before_key
    }

    pub fn current_session(&self) -> i32 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current_session
    }

    pub fn begin_session(&self, codec: VideoCodec, width: i32, height: i32, fps: i32) -> i32 {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.current_session += 1;
        let current = st.current_session;
        st.fps = fps;
        st.session_codec = codec;
        st.session_width = width;
        st.session_height = height;

        let in_use = self.ring.formats_in_use();
        st.formats
            .retain(|&id, _| id == current || in_use.contains(&id));
        current
    }

    pub fn on_packet(&self, session: i32, data: &[u8], pts: i64, key: bool) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if session != st.current_session {
            return;
        }
        if !st.formats.contains_key(&session) {
            if !key {
                st.dropped_before_key += 1;
                return;
            }
            let Some(config) = CodecBits::build_config(st.session_codec, data) else {
                st.dropped_before_key += 1;
                return;
            };
            let fmt = VideoFormat {
                codec: st.session_codec,
                width: st.session_width,
                height: st.session_height,
                config,
            };
            st.formats.insert(session, fmt);
        }
        if pts <= st.last_written_pts {
            return;
        }
        if self.ring.write(data, pts, key, session) {
            st.last_written_pts = pts;
            if key {
                st.last_key_pts = pts;
            }
        }
    }

    pub fn formats(&self) -> HashMap<i32, VideoFormat> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .formats
            .clone()
    }

    pub fn to_mux_track<'a>(&self, snapshot: &'a RingSnapshot) -> Option<MuxTrack<'a>> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let last_pkt = snapshot.packets().last()?;
        let fmt = st.formats.get(&last_pkt.format)?;
        let fourcc = match fmt.codec {
            VideoCodec::Av1 => "av01",
            VideoCodec::Hevc => "hev1",
            VideoCodec::H264 => "avc1",
        };
        Some(MuxTrack {
            kind: TrackKind::Video,
            codec: fourcc.to_string(),
            video_codec: Some(fmt.codec),
            width: fmt.width,
            height: fmt.height,
            sample_rate: 0,
            channels: 0,
            config: fmt.config.clone(),
            default_duration_100ns: 10_000_000 / st.fps.max(1) as i64,
            samples: snapshot,
            formats: Some(st.formats.clone()),
            fps: st.fps,
        })
    }

    /// Panic or purge: zero the ring and end the current session (A10).
    pub fn clear(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.ring.clear();
        st.current_session += 1;
    }
}
