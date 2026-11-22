// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Video encoder trait, selection plan, factory, latency stats, and bitstream helpers (`Replay\VideoEncoders.cs`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::System::Performance::QueryPerformanceFrequency;

use crate::capture::gpu_devices::{AdapterInfo, GpuDevice, GpuDevices, OutputInfo};
use crate::replay::bitstream::{AnnexB, Av1, VideoCodec};
use crate::replay::mf_video_encoder::MfVideoEncoder;
use crate::replay::vpl_av1_encoder::VplAv1Encoder;
use crate::util::AgentLog;

/// Hardware encoder probe results keyed by (adapter LUID, codec, encoder API).
type ProbeCache = HashMap<(i64, VideoCodec, String), bool>;

/// Receives encoded packets in output order. `pts` is in 100 ns units.
pub type PacketSink = Arc<dyn Fn(&[u8], i64, bool) + Send + Sync>;

/// Parameters for creating a hardware [`VideoEncoder`].
#[derive(Debug, Clone, Copy)]
pub struct EncoderCreateParams {
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub kbps: i32,
}

/// A hardware video encoder fed with NV12 D3D11 textures on its own adapter.
pub trait VideoEncoder: Send + Sync {
    fn codec(&self) -> VideoCodec;
    /// `"vpl"` (oneVPL) or `"mf"` (Media Foundation hardware MFT).
    fn api(&self) -> &str;
    fn name(&self) -> &str;
    fn width(&self) -> i32;
    fn height(&self) -> i32;
    /// Size of the NV12 texture `try_submit` expects (picture is top-left `width x height`).
    fn surface_width(&self) -> i32;
    fn surface_height(&self) -> i32;
    /// True: `try_submit` copies the texture before returning, so the caller may reuse it at once.
    fn copies_input(&self) -> bool;
    fn ready_for_input(&self) -> bool;
    fn try_submit(&self, nv12: &ID3D11Texture2D, pts_100ns: i64, force_key: bool) -> bool;
    fn frames_in(&self) -> i64;
    fn packets_out(&self) -> i64;
    fn frames_dropped(&self) -> i64;
    fn forced_keyframes(&self) -> i64;
    fn error(&self) -> Option<String>;
    fn latency(&self) -> &LatencyStats;
    fn dispose(&self);
}

/// Submit-to-packet times in ms (bounded sample buffer).
#[derive(Default)]
pub struct LatencyStats {
    ms: Mutex<Vec<f64>>,
}

impl LatencyStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, ms: f64) {
        let mut g = self.ms.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() >= 8192 {
            g.drain(..4096);
        }
        g.push(ms);
    }

    pub fn count(&self) -> usize {
        self.ms.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn reset(&self) {
        self.ms.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    pub fn percentile(&self, p: f64) -> f64 {
        let g = self.ms.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_empty() {
            return 0.0;
        }
        let mut a = g.clone();
        a.sort_by(f64::total_cmp);
        let idx = ((p / 100.0 * a.len() as f64).ceil() as isize - 1).clamp(0, a.len() as isize - 1)
            as usize;
        a[idx]
    }
}

/// One encoder attempt: adapter, codec, and API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderCandidate {
    pub adapter: AdapterInfo,
    pub codec: VideoCodec,
    pub api: String,
}

/// Pure encoder selection: codec order, adapter order, candidate list (`Replay.EncoderPlan`).
pub struct EncoderPlan;

impl EncoderPlan {
    /// `replay.encoder`: `auto` (av1, hevc, h264) | `av1` | `hevc` | `h264`.
    pub fn codecs(preference: Option<&str>) -> Vec<VideoCodec> {
        match preference.unwrap_or("auto").to_ascii_lowercase().as_str() {
            "av1" | "av1_qsv" => vec![VideoCodec::Av1],
            "hevc" | "hevc_qsv" => vec![VideoCodec::Hevc],
            "h264" | "h264_qsv" => vec![VideoCodec::H264],
            _ => vec![VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264],
        }
    }

    /// APIs to try for a codec on an adapter: AV1 on Intel goes through oneVPL first.
    pub fn apis(a: &AdapterInfo, c: VideoCodec) -> Vec<&'static str> {
        if c == VideoCodec::Av1 && a.vendor_id == GpuDevices::VENDOR_INTEL {
            vec!["vpl", "mf"]
        } else {
            vec!["mf"]
        }
    }

    /// `replay.encoderAdapter`: `auto` = integrated GPU first, then GPU driving the most captured
    /// monitors, then the rest; any other value is a case-insensitive substring of adapter name.
    pub fn adapter_order(
        adapters: &[AdapterInfo],
        captured: &[OutputInfo],
        setting: Option<&str>,
    ) -> Vec<AdapterInfo> {
        let hw: Vec<AdapterInfo> = adapters.iter().filter(|a| !a.software).cloned().collect();
        let driven = |a: &AdapterInfo| -> usize {
            captured.iter().filter(|o| o.adapter_luid == a.luid).count()
        };
        let mut auto: Vec<(usize, AdapterInfo)> = hw.into_iter().enumerate().collect();
        auto.sort_by(|(ia, a), (ib, b)| {
            b.integrated
                .cmp(&a.integrated)
                .then_with(|| driven(b).cmp(&driven(a)))
                .then_with(|| ia.cmp(ib))
        });
        let auto: Vec<AdapterInfo> = auto.into_iter().map(|(_, a)| a).collect();
        let Some(set) = setting.map(str::trim) else {
            return auto;
        };
        if set.is_empty() || set.eq_ignore_ascii_case("auto") {
            return auto;
        }
        let needle = set.to_ascii_lowercase();
        let mut named = Vec::new();
        let mut rest = Vec::new();
        for a in auto {
            if a.name.to_ascii_lowercase().contains(&needle) {
                named.push(a);
            } else {
                rest.push(a);
            }
        }
        named.extend(rest);
        named
    }

    /// Candidates in try order: adapter-major, codec order, API order.
    pub fn candidates(
        adapters: &[AdapterInfo],
        captured: &[OutputInfo],
        adapter_setting: Option<&str>,
        encoder_setting: Option<&str>,
        mut available: impl FnMut(&AdapterInfo, VideoCodec, &str) -> bool,
    ) -> Vec<EncoderCandidate> {
        let mut list = Vec::new();
        for a in Self::adapter_order(adapters, captured, adapter_setting) {
            for c in Self::codecs(encoder_setting) {
                for api in Self::apis(&a, c) {
                    if available(&a, c, api) {
                        list.push(EncoderCandidate {
                            adapter: a.clone(),
                            codec: c,
                            api: api.to_string(),
                        });
                    }
                }
            }
        }
        list
    }
}

/// Creates encoders and probes which codecs each adapter exposes (cached per adapter LUID).
pub struct EncoderFactory;

impl EncoderFactory {
    /// Largest picture each attempt allows: native first (capped at 16384), then 8K, then 4K.
    pub const SIZE_LIMITS: &'static [(i32, i32)] =
        &[(16384, 16384), (7680, 4320), (4096, 2160)];

    fn probe_cache() -> &'static Mutex<ProbeCache> {
        static CACHE: OnceLock<Mutex<ProbeCache>> = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub fn available(a: &AdapterInfo, c: VideoCodec, api: &str) -> bool {
        let key = (a.luid, c, api.to_string());
        let mut map = Self::probe_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&ok) = map.get(&key) {
            return ok;
        }
        let ok = if api == "vpl" {
            VplAv1Encoder::probe(a.luid)
        } else {
            MfVideoEncoder::find(a.luid, a.vendor_id, c).is_some()
        };
        map.insert(key, ok);
        ok
    }

    /// `"av1:vpl"`, `"hevc:mf"`, ... for `HardwareInfo`.
    pub fn codecs(a: &AdapterInfo) -> Vec<String> {
        let mut list = Vec::new();
        for c in [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264] {
            for api in EncoderPlan::apis(a, c) {
                if Self::available(a, c, api) {
                    list.push(format!("{}:{api}", Self::codec_name(c)));
                }
            }
        }
        list
    }

    pub const fn codec_name(c: VideoCodec) -> &'static str {
        match c {
            VideoCodec::Av1 => "av1",
            VideoCodec::Hevc => "hevc",
            VideoCodec::H264 => "h264",
        }
    }

    pub fn create(
        c: &EncoderCandidate,
        dev: Arc<GpuDevice>,
        params: EncoderCreateParams,
        sink: PacketSink,
    ) -> Result<Box<dyn VideoEncoder>, String> {
        if c.api == "vpl" {
            VplAv1Encoder::create(dev, params, sink).map(|e| Box::new(e) as Box<dyn VideoEncoder>)
        } else {
            MfVideoEncoder::create(dev, c.codec, params, sink)
                .map(|e| Box::new(e) as Box<dyn VideoEncoder>)
        }
    }
}

/// Bitstream helpers shared by every encoder and the muxer (`Replay.CodecBits`).
pub struct CodecBits;

impl CodecBits {
    pub fn detect_key(codec: VideoCodec, data: &[u8]) -> bool {
        match codec {
            VideoCodec::Av1 => Av1::split(data)
                .map(|obus| obus.iter().any(|o| o.obu_type == Av1::OBU_SEQUENCE_HEADER))
                .unwrap_or(false),
            VideoCodec::H264 => AnnexB::split(data).iter().any(|n| {
                n.length > 0 && AnnexB::h264_type(&data[n.start..n.start + n.length]) == 5
            }),
            VideoCodec::Hevc => AnnexB::split(data).iter().any(|n| {
                n.length > 1
                    && matches!(
                        AnnexB::hevc_type(&data[n.start..n.start + n.length]),
                        19..=21
                    )
            }),
        }
    }

    /// `av1C` / `hvcC` / `avcC` payload from a keyframe, or `None` when it carries no parameter sets.
    pub fn build_config(codec: VideoCodec, keyframe: &[u8]) -> Option<Vec<u8>> {
        match codec {
            VideoCodec::Av1 => match Av1::find_sequence_header(keyframe) {
                Ok(Some(seq)) => match Av1::build_av1c(&seq) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        AgentLog::warn_err("codec config parse", &e);
                        None
                    }
                },
                Ok(None) => None,
                Err(e) => {
                    AgentLog::warn_err("codec config parse", &e);
                    None
                }
            },
            VideoCodec::Hevc => AnnexB::build_hvcc(keyframe),
            VideoCodec::H264 => AnnexB::build_avcc(keyframe),
        }
    }

    pub fn ticks_to_ms(ticks: i64) -> f64 {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        ticks as f64 * 1000.0 / f.max(1) as f64
    }
}
