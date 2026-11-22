// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Fragmented MP4 (ISO/IEC 14496-12) muxer (`Replay\Fmp4Writer.cs`).

use std::collections::HashMap;
use std::io::Write;

use zeroize::Zeroize;

use crate::replay::bitstream::{AnnexB, Av1, VideoCodec};
use crate::replay::packet_ring::RingSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Debug, Clone)]
pub struct VideoFormat {
    pub codec: VideoCodec,
    pub width: i32,
    pub height: i32,
    pub config: Vec<u8>,
}

/// One track to mux. Video: `codec` is `av01`/`hev1`/`avc1`, `config` is `av1C`/`hvcC`/`avcC`.
/// Audio: `codec` is `mp4a`, `config` is `AudioSpecificConfig`.
pub struct MuxTrack<'a> {
    pub kind: TrackKind,
    pub codec: String,
    pub video_codec: Option<VideoCodec>,
    pub width: i32,
    pub height: i32,
    pub sample_rate: i32,
    pub channels: i32,
    pub config: Vec<u8>,
    pub default_duration_100ns: i64,
    pub samples: &'a RingSnapshot,
    pub formats: Option<HashMap<i32, VideoFormat>>,
    pub fps: i32,
}

struct TrackPlan<'a> {
    track_id: u32,
    track: &'a MuxTrack<'a>,
    timescale: i32,
    format_map: HashMap<i32, VideoFormat>,
    indices: Vec<usize>,
    used_formats: Vec<i32>,
    format_indices: HashMap<i32, u32>,
    times: Vec<i64>,
    durations: Vec<u32>,
    cursor: usize,
}

pub struct Fmp4Writer;

impl Fmp4Writer {
    pub const VIDEO_TIMESCALE: i32 = 90_000;

    pub fn video_timescale_for(fps: i32) -> i32 {
        if fps <= 0 || 90_000 % fps == 0 {
            return 90_000;
        }
        let g = gcd(90_000, fps as i64);
        ((90_000 / g) * fps as i64) as i32
    }

    fn effective_formats(t: &MuxTrack<'_>) -> HashMap<i32, VideoFormat> {
        if let Some(ref fm) = t.formats {
            return fm.clone();
        }
        let mut map = HashMap::new();
        if let Some(vc) = t.video_codec {
            map.insert(
                0,
                VideoFormat {
                    codec: vc,
                    width: t.width,
                    height: t.height,
                    config: t.config.clone(),
                },
            );
        }
        map
    }

    /// Writes a fragmented MP4 to `output`. Returns the duration in milliseconds.
    pub fn write<W: Write + ?Sized>(output: &mut W, tracks: &[MuxTrack<'_>]) -> Result<i32, String> {
        if tracks.is_empty() {
            return Err("No tracks.".to_string());
        }

        let mut start = i64::MIN;
        for t in tracks {
            if t.kind != TrackKind::Video || t.samples.packets().is_empty() {
                continue;
            }
            let fm = Self::effective_formats(t);
            for pkt in t.samples.packets() {
                if fm.contains_key(&pkt.format) {
                    start = start.max(pkt.pts);
                    break;
                }
            }
        }
        if start == i64::MIN {
            start = tracks
                .iter()
                .filter_map(|t| t.samples.packets().first().map(|p| p.pts))
                .min()
                .unwrap_or(0);
        }

        let mut plans: Vec<TrackPlan<'_>> = Vec::new();
        for (i, t) in tracks.iter().enumerate() {
            let timescale = if t.kind == TrackKind::Video {
                Self::video_timescale_for(t.fps)
            } else {
                t.sample_rate
            };
            let format_map = Self::effective_formats(t);
            let pk = t.samples.packets();
            let first = if t.kind == TrackKind::Video {
                let Some(idx) = pk
                    .iter()
                    .position(|p| format_map.contains_key(&p.format) && p.key && p.pts >= start)
                else {
                    continue;
                };
                idx
            } else {
                let mut f = 0usize;
                while f < pk.len() && pk[f].pts < start {
                    f += 1;
                }
                f
            };

            let mut indices = Vec::new();
            let mut used_formats = Vec::new();
            let mut format_indices = HashMap::new();
            for (k, pkt) in pk.iter().enumerate().skip(first) {
                if t.kind == TrackKind::Video && !format_map.contains_key(&pkt.format) {
                    continue;
                }
                indices.push(k);
                if t.kind == TrackKind::Video && !format_indices.contains_key(&pkt.format) {
                    used_formats.push(pkt.format);
                    format_indices.insert(pkt.format, used_formats.len() as u32);
                }
            }
            if indices.is_empty() {
                continue;
            }
            let mut times = Vec::with_capacity(indices.len());
            for &idx in &indices {
                times.push(to_scale(pk[idx].pts - start, timescale));
            }
            let mut durations = Vec::with_capacity(indices.len());
            for k in 0..indices.len() {
                let d = if k + 1 < indices.len() {
                    times[k + 1] - times[k]
                } else {
                    to_scale(t.default_duration_100ns, timescale)
                };
                durations.push(d.max(1) as u32);
            }
            plans.push(TrackPlan {
                track_id: (i + 1) as u32,
                track: t,
                timescale,
                format_map,
                indices,
                used_formats,
                format_indices,
                times,
                durations,
                cursor: 0,
            });
        }
        if plans.is_empty() {
            return Err("Nothing to write.".to_string());
        }
        for (i, p) in plans.iter_mut().enumerate() {
            p.track_id = (i + 1) as u32;
        }

        let mut duration_ms = 0i64;
        for p in &plans {
            let end = *p.times.last().unwrap() + *p.durations.last().unwrap() as i64;
            duration_ms = duration_ms.max(end * 1000 / p.timescale as i64);
        }

        output
            .write_all(&build_ftyp(&plans))
            .map_err(|e| format!("write ftyp: {e}"))?;
        output
            .write_all(&build_moov(&plans, duration_ms))
            .map_err(|e| format!("write moov: {e}"))?;

        let lead_idx = plans
            .iter()
            .position(|p| p.track.kind == TrackKind::Video)
            .unwrap_or(0);
        let mut bounds: Vec<i64> = {
            let lead = &plans[lead_idx];
            let lpk = lead.track.samples.packets();
            let mut b = Vec::new();
            for &idx in &lead.indices {
                if lpk[idx].key || lead.track.kind == TrackKind::Audio {
                    b.push(lpk[idx].pts);
                }
            }
            if lead.track.kind == TrackKind::Audio {
                let mut b2: Vec<i64> = Vec::new();
                for pt in b {
                    if b2.is_empty() || pt - *b2.last().unwrap() >= 10_000_000 {
                        b2.push(pt);
                    }
                }
                b2
            } else {
                b
            }
        };
        if !bounds.is_empty() {
            bounds[0] = i64::MIN;
        }

        let mut sequence = 1u32;
        let max_sample = tracks
            .iter()
            .flat_map(|t| t.samples.packets().iter())
            .map(|p| p.length)
            .max()
            .unwrap_or(0);
        let mut scratch = vec![0u8; (max_sample * 2 + 64).max(1)];

        let res = (|| -> Result<(), String> {
            for s in 0..bounds.len() {
                let seg_start = bounds[s];
                let seg_end = if s + 1 < bounds.len() {
                    bounds[s + 1]
                } else {
                    i64::MAX
                };
                for p in &mut plans {
                    let pk = p.track.samples.packets();
                    let from = p.cursor;
                    let mut to = from;
                    while to < p.indices.len()
                        && pk[p.indices[to]].pts < seg_end
                        && pk[p.indices[to]].pts >= seg_start
                    {
                        to += 1;
                    }
                    while to < p.indices.len() && pk[p.indices[to]].pts < seg_start {
                        to += 1;
                    }
                    if to == from {
                        continue;
                    }
                    write_plan_runs(output, p, from, to, &mut sequence, &mut scratch)?;
                    p.cursor = to;
                }
            }
            for p in &mut plans {
                if p.cursor < p.indices.len() {
                    let from = p.cursor;
                    let to = p.indices.len();
                    write_plan_runs(output, p, from, to, &mut sequence, &mut scratch)?;
                    p.cursor = to;
                }
            }
            Ok(())
        })();
        scratch.zeroize();
        res?;
        Ok(duration_ms.min(i32::MAX as i64) as i32)
    }
}

fn write_plan_runs<W: Write + ?Sized>(
    output: &mut W,
    p: &TrackPlan<'_>,
    from: usize,
    to: usize,
    sequence: &mut u32,
    scratch: &mut [u8],
) -> Result<(), String> {
    if p.track.kind == TrackKind::Video {
        let pk = p.track.samples.packets();
        let mut run_start = from;
        while run_start < to {
            let fmt = pk[p.indices[run_start]].format;
            let mut run_end = run_start + 1;
            while run_end < to && pk[p.indices[run_end]].format == fmt {
                run_end += 1;
            }
            write_fragment(output, p, run_start, run_end, *sequence, scratch)?;
            *sequence += 1;
            run_start = run_end;
        }
    } else {
        write_fragment(output, p, from, to, *sequence, scratch)?;
        *sequence += 1;
    }
    Ok(())
}

fn write_fragment<W: Write + ?Sized>(
    output: &mut W,
    p: &TrackPlan<'_>,
    from: usize,
    to: usize,
    sequence: u32,
    scratch: &mut [u8],
) -> Result<(), String> {
    let pk = p.track.samples.packets();
    let n = to - from;
    let mut sizes = vec![0usize; n];
    let frag_codec = if p.track.kind == TrackKind::Video {
        Some(p.format_map[&pk[p.indices[from]].format].codec)
    } else {
        None
    };
    for (i, sz) in sizes.iter_mut().enumerate() {
        *sz = transform(
            frag_codec,
            p.track.samples.data(pk[p.indices[from + i]]),
            scratch,
        )?;
    }

    let mut moof = BoxBuilder::new();
    let moof_start = moof.begin(b"moof");
    moof.full_box(b"mfhd", 0, 0, |b| b.u32(sequence));
    let traf_start = moof.begin(b"traf");
    let sample_desc_index = if p.track.kind == TrackKind::Audio {
        1
    } else {
        p.format_indices[&pk[p.indices[from]].format]
    };
    moof.full_box(b"tfhd", 0, 0x020000 | 0x000002, |b| {
        b.u32(p.track_id);
        b.u32(sample_desc_index);
    });
    moof.full_box(b"tfdt", 1, 0, |b| b.u64(p.times[from].max(0) as u64));
    const TRUN_FLAGS: u32 = 0x000001 | 0x000100 | 0x000200 | 0x000400;
    let trun_start = moof.begin_full(b"trun", 0, TRUN_FLAGS);
    moof.u32(n as u32);
    let data_offset_pos = moof.position();
    moof.u32(0); // patched
    for (i, &sz) in sizes.iter().enumerate() {
        let pkt = pk[p.indices[from + i]];
        moof.u32(p.durations[from + i]);
        moof.u32(sz as u32);
        let flags = if p.track.kind == TrackKind::Audio || pkt.key {
            0x0200_0000u32
        } else {
            0x0101_0000u32
        };
        moof.u32(flags);
    }
    moof.end(trun_start);
    moof.end(traf_start);
    moof.end(moof_start);
    let mut moof_bytes = moof.into_vec();
    let data_offset = (moof_bytes.len() + 8) as u32;
    moof_bytes[data_offset_pos..data_offset_pos + 4].copy_from_slice(&data_offset.to_be_bytes());
    output
        .write_all(&moof_bytes)
        .map_err(|e| format!("write moof: {e}"))?;

    let mdat_size: u32 = 8 + sizes.iter().sum::<usize>() as u32;
    let mut hdr = [0u8; 8];
    hdr[..4].copy_from_slice(&mdat_size.to_be_bytes());
    hdr[4..8].copy_from_slice(b"mdat");
    output
        .write_all(&hdr)
        .map_err(|e| format!("write mdat hdr: {e}"))?;
    for i in 0..n {
        let len = transform(
            frag_codec,
            p.track.samples.data(pk[p.indices[from + i]]),
            scratch,
        )?;
        output
            .write_all(&scratch[..len])
            .map_err(|e| format!("write mdat data: {e}"))?;
    }
    Ok(())
}

fn transform(codec: Option<VideoCodec>, src: &[u8], dst: &mut [u8]) -> Result<usize, String> {
    match codec {
        Some(VideoCodec::Av1) => Av1::strip_for_mp4(src, dst),
        Some(c @ (VideoCodec::H264 | VideoCodec::Hevc)) => {
            Ok(AnnexB::to_length_prefixed(src, dst, c))
        }
        None => {
            dst[..src.len()].copy_from_slice(src);
            Ok(src.len())
        }
    }
}

fn to_scale(ticks_100ns: i64, timescale: i32) -> i64 {
    ((ticks_100ns as f64) * (timescale as f64) / 10_000_000.0).round() as i64
}

fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

fn build_ftyp(plans: &[TrackPlan<'_>]) -> Vec<u8> {
    let mut b = BoxBuilder::new();
    let f = b.begin(b"ftyp");
    b.fourcc(b"iso6");
    b.u32(0);
    b.fourcc(b"iso6");
    b.fourcc(b"iso5");
    b.fourcc(b"isom");
    b.fourcc(b"mp41");
    let has_av1 = plans.iter().any(|p| {
        p.track.kind == TrackKind::Video
            && p.used_formats.iter().any(|fmt_id| {
                p.format_map
                    .get(fmt_id)
                    .is_some_and(|fmt| fmt.codec == VideoCodec::Av1)
            })
    });
    if has_av1 {
        b.fourcc(b"av01");
    }
    b.end(f);
    b.into_vec()
}

fn build_moov(plans: &[TrackPlan<'_>], duration_ms: i64) -> Vec<u8> {
    let mut b = BoxBuilder::new();
    let moov = b.begin(b"moov");
    b.full_box(b"mvhd", 0, 0, |x| {
        x.u32(0);
        x.u32(0);
        x.u32(1000);
        x.u32(duration_ms as u32);
        x.u32(0x0001_0000);
        x.u16(0x0100);
        x.u16(0);
        x.u32(0);
        x.u32(0);
        matrix(x);
        for _ in 0..6 {
            x.u32(0);
        }
        x.u32((plans.len() + 1) as u32);
    });
    for p in plans {
        trak(&mut b, p, duration_ms);
    }
    let mvex = b.begin(b"mvex");
    b.full_box(b"mehd", 0, 0, |x| x.u32(duration_ms as u32));
    for p in plans {
        b.full_box(b"trex", 0, 0, |x| {
            x.u32(p.track_id);
            x.u32(1);
            x.u32(0);
            x.u32(0);
            x.u32(0);
        });
    }
    b.end(mvex);
    b.end(moov);
    b.into_vec()
}

fn matrix(x: &mut BoxBuilder) {
    x.u32(0x0001_0000);
    x.u32(0);
    x.u32(0);
    x.u32(0);
    x.u32(0x0001_0000);
    x.u32(0);
    x.u32(0);
    x.u32(0);
    x.u32(0x4000_0000);
}

fn trak(b: &mut BoxBuilder, p: &TrackPlan<'_>, duration_ms: i64) {
    let t = p.track;
    let video = t.kind == TrackKind::Video;
    let (mut track_w, mut track_h) = (0i32, 0i32);
    if video {
        let last_pkt = p.track.samples.packets()[*p.indices.last().unwrap()];
        if let Some(last_fmt) = p.format_map.get(&last_pkt.format) {
            track_w = last_fmt.width;
            track_h = last_fmt.height;
        }
    }
    let trak_pos = b.begin(b"trak");
    b.full_box(b"tkhd", 0, 3, |x| {
        x.u32(0);
        x.u32(0);
        x.u32(p.track_id);
        x.u32(0);
        x.u32(duration_ms as u32);
        x.u32(0);
        x.u32(0);
        x.u16(0);
        x.u16(0);
        x.u16(if video { 0 } else { 0x0100 });
        x.u16(0);
        matrix(x);
        x.u32(if video { (track_w as u32) << 16 } else { 0 });
        x.u32(if video { (track_h as u32) << 16 } else { 0 });
    });
    let mdia = b.begin(b"mdia");
    b.full_box(b"mdhd", 0, 0, |x| {
        x.u32(0);
        x.u32(0);
        x.u32(p.timescale as u32);
        x.u32(0);
        x.u16(0x55C4); // 'und'
        x.u16(0);
    });
    b.full_box(b"hdlr", 0, 0, |x| {
        x.u32(0);
        x.fourcc(if video { b"vide" } else { b"soun" });
        x.u32(0);
        x.u32(0);
        x.u32(0);
        x.bytes(if video {
            b"BetterRecall video\0"
        } else {
            b"BetterRecall audio\0"
        });
    });
    let minf = b.begin(b"minf");
    if video {
        b.full_box(b"vmhd", 0, 1, |x| {
            x.u16(0);
            x.u16(0);
            x.u16(0);
            x.u16(0);
        });
    } else {
        b.full_box(b"smhd", 0, 0, |x| {
            x.u16(0);
            x.u16(0);
        });
    }
    let dinf = b.begin(b"dinf");
    b.full_box(b"dref", 0, 0, |x| {
        x.u32(1);
        x.full_box(b"url ", 0, 1, |_| {});
    });
    b.end(dinf);
    let stbl = b.begin(b"stbl");
    b.full_box(b"stsd", 0, 0, |x| {
        if video {
            x.u32(p.used_formats.len() as u32);
            for fmt_id in &p.used_formats {
                visual_entry(x, &p.format_map[fmt_id]);
            }
        } else {
            x.u32(1);
            audio_entry(x, t);
        }
    });
    b.full_box(b"stts", 0, 0, |x| x.u32(0));
    b.full_box(b"stsc", 0, 0, |x| x.u32(0));
    b.full_box(b"stsz", 0, 0, |x| {
        x.u32(0);
        x.u32(0);
    });
    b.full_box(b"stco", 0, 0, |x| x.u32(0));
    b.end(stbl);
    b.end(minf);
    b.end(mdia);
    b.end(trak_pos);
}

fn visual_entry(x: &mut BoxBuilder, fmt: &VideoFormat) {
    let fourcc = match fmt.codec {
        VideoCodec::Av1 => b"av01",
        VideoCodec::Hevc => b"hev1",
        VideoCodec::H264 => b"avc1",
    };
    let e = x.begin(fourcc);
    x.zero(6);
    x.u16(1);
    x.u16(0);
    x.u16(0);
    x.zero(12);
    x.u16(fmt.width as u16);
    x.u16(fmt.height as u16);
    x.u32(0x0048_0000);
    x.u32(0x0048_0000);
    x.u32(0);
    x.u16(1);
    let mut name = [0u8; 32];
    let label = b"BetterRecall";
    name[0] = label.len() as u8;
    name[1..1 + label.len()].copy_from_slice(label);
    x.bytes(&name);
    x.u16(0x0018);
    x.u16(0xFFFF);
    let cfg = match fmt.codec {
        VideoCodec::Av1 => b"av1C",
        VideoCodec::Hevc => b"hvcC",
        VideoCodec::H264 => b"avcC",
    };
    let c = x.begin(cfg);
    x.bytes(&fmt.config);
    x.end(c);
    x.end(e);
}

fn audio_entry(x: &mut BoxBuilder, t: &MuxTrack<'_>) {
    let e = x.begin(b"mp4a");
    x.zero(6);
    x.u16(1);
    x.u32(0);
    x.u32(0);
    x.u16(t.channels as u16);
    x.u16(16);
    x.u16(0);
    x.u16(0);
    x.u32((t.sample_rate as u32) << 16);
    x.full_box(b"esds", 0, 0, |z| {
        let dsi = descriptor(0x05, &t.config);
        let mut dcd = vec![
            0x40, 0x15, 0, 0x18, 0, 0, 0x02, 0xEE, 0x00, 0, 0x02, 0xEE, 0x00,
        ];
        dcd.extend_from_slice(&dsi);
        let mut es = vec![0, 1, 0];
        es.extend_from_slice(&descriptor(0x04, &dcd));
        es.extend_from_slice(&descriptor(0x06, &[0x02]));
        z.bytes(&descriptor(0x03, &es));
    });
    x.end(e);
}

fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let len = body.len();
    let mut o = Vec::with_capacity(5 + len);
    o.push(tag);
    o.push(0x80 | ((len >> 21) & 0x7F) as u8);
    o.push(0x80 | ((len >> 14) & 0x7F) as u8);
    o.push(0x80 | ((len >> 7) & 0x7F) as u8);
    o.push((len & 0x7F) as u8);
    o.extend_from_slice(body);
    o
}

/// Big-endian ISOBMFF box builder for metadata boxes.
pub struct BoxBuilder {
    buf: Vec<u8>,
}

impl Default for BoxBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BoxBuilder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn position(&self) -> usize {
        self.buf.len()
    }

    pub fn begin(&mut self, ty: &[u8; 4]) -> usize {
        let start = self.buf.len();
        self.u32(0);
        self.fourcc(ty);
        start
    }

    pub fn begin_full(&mut self, ty: &[u8; 4], version: u8, flags: u32) -> usize {
        let start = self.begin(ty);
        self.u32(((version as u32) << 24) | (flags & 0x00FF_FFFF));
        start
    }

    pub fn end(&mut self, start: usize) {
        let size = (self.buf.len() - start) as u32;
        self.buf[start..start + 4].copy_from_slice(&size.to_be_bytes());
    }

    pub fn full_box(&mut self, ty: &[u8; 4], version: u8, flags: u32, body: impl FnOnce(&mut Self)) {
        let s = self.begin_full(ty, version, flags);
        body(self);
        self.end(s);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn fourcc(&mut self, s: &[u8; 4]) {
        self.buf.extend_from_slice(s);
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn zero(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}
