// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Ported replay tests: PacketRingTests, Fmp4WriterTests, ReplayLayoutTests, ReplaySamplerTests,
//! CanvasBlitTests, CursorPainterTests, and replay cases from Sweep2F, Sweep3J, Sweep4P, Sweep4U,
//! Sweep5U, and ReliabilitySweep2.

mod common;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use br_agent::capture::{AdapterInfo, GpuDevices, OutputInfo};
use br_agent::host::{AgentHost, AgentOptions};
use br_agent::native::Rect;
use br_agent::replay::{
    AnnexB, Av1, CanvasBlit, CanvasLayout, CursorDrawParams, CursorPainter, CursorShape,
    DirtyMapper, EncoderPlan, EncoderWatchdog, Fmp4Writer, FramePacer, MonitorPlacement, MuxTrack,
    Nv12Mut, Nv12Ref, PacketRing, ReplayBlobCipher, ReplayBlobWriter, ReplayEngine,
    ReplayEngineConfig, ReplayLibrary,
    ReplayRate, ReplaySampler, ReplayScaling, Sample, TrackKind, VideoCodec, VideoStream,
    dirty_to_dest,
};
use br_contracts::{CoreHostOptions, DataPaths, PurgeNotice, Settings};
use br_core::{AgentControl, AgentError, CancelSource, CancelToken, CoreHost, ExportJournal};
use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};

use common::{TempDir, until};

const SEC: i64 = 10_000_000;

fn packet(len: usize, fill: u8) -> Vec<u8> {
    vec![fill; len]
}

// =========================================================================
// PacketRingTests
// =========================================================================

#[test]
fn cut_starts_at_last_keyframe_at_or_before_request() {
    let ring = PacketRing::new(1 << 20, Duration::from_secs(60), false).unwrap();
    for i in 0..16i64 {
        ring.write(&packet(100, i as u8), i * SEC / 4, i % 4 == 0, 0);
    }
    let snap = ring.snapshot(2 * SEC + SEC / 2);
    assert_eq!(2 * SEC, snap.packets()[0].pts);
    assert!(snap.packets()[0].key);
    assert_eq!(8, snap.packets().len());
    assert_eq!(8u8, snap.data(snap.packets()[0])[0]);
    assert_eq!(15u8, snap.data(*snap.packets().last().unwrap())[0]);

    let snap2 = ring.snapshot(-5 * SEC);
    assert_eq!(0, snap2.packets()[0].pts);

    let snap3 = ring.snapshot(SEC);
    assert_eq!(SEC, snap3.packets()[0].pts);
}

#[test]
fn time_eviction_keeps_whole_gops() {
    let ring = PacketRing::new(1 << 20, Duration::from_secs(2), false).unwrap();
    for i in 0..40i64 {
        ring.write(&packet(50, 1), i * SEC / 4, i % 4 == 0, 0);
    }
    assert_eq!(Some(7 * SEC), ring.oldest_pts());
    let snap = ring.snapshot(0);
    assert!(snap.packets()[0].key);
}

#[test]
fn wraps_and_evicts_by_space_without_corruption() {
    let ring = PacketRing::new(1 << 20, Duration::ZERO, false).unwrap();
    let mut seed: u64 = 3;
    let mut next_len = || -> usize {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        1000 + ((seed >> 33) as usize % 8000)
    };
    let mut pts = 0i64;
    for i in 0..2000i32 {
        let len = next_len();
        let mut p = vec![0u8; len];
        p[..4].copy_from_slice(&i.to_le_bytes());
        *p.last_mut().unwrap() = i as u8;
        assert!(ring.write(&p, pts, i % 15 == 0, 0));
        pts += SEC / 15;
    }
    assert!(ring.live_bytes() <= ring.capacity());
    let snap = ring.snapshot(0);
    let mut prev = -1i32;
    for &p in snap.packets() {
        let d = snap.data(p);
        let idx = i32::from_le_bytes([d[0], d[1], d[2], d[3]]);
        assert!(idx > prev);
        assert_eq!(idx as u8, *d.last().unwrap());
        prev = idx;
    }
    assert_eq!(1999, prev);
    assert!(ring.committed_bytes() <= ring.capacity());
}

#[test]
fn clear_zeroes_and_forgets() {
    let ring = PacketRing::new(1 << 20, Duration::from_secs(10), false).unwrap();
    ring.write(&packet(500, 0xAB), 0, true, 0);
    assert!(!ring.committed_bytes_all_zero_for_test());
    ring.clear();
    assert_eq!(0, ring.count());
    assert!(ring.committed_bytes_all_zero_for_test());
    let snap = ring.snapshot(0);
    assert!(snap.packets().is_empty());
}

#[test]
fn commits_on_demand() {
    let ring = PacketRing::new(64 << 20, Duration::from_secs(120), false).unwrap();
    for i in 0..10i64 {
        ring.write(&packet(1000, 1), i * SEC, true, 0);
    }
    assert!(
        ring.committed_bytes() <= (1 << 20),
        "committed {}",
        ring.committed_bytes()
    );
}

#[test]
fn rejects_oversized_packet() {
    let ring = PacketRing::new(1 << 20, Duration::from_secs(10), false).unwrap();
    assert!(!ring.write(&vec![0u8; (1 << 20) + 1], 0, true, 0));
}

// =========================================================================
// Minimal ISO BMFF box parser + Fmp4WriterTests
// =========================================================================

struct Mp4Box {
    ty: String,
    offset: usize,
    size: usize,
    children: Vec<Mp4Box>,
}

impl Mp4Box {
    fn parse(data: &[u8], start: usize, end: usize) -> Vec<Self> {
        const CONTAINERS: &[&str] = &[
            "moov", "trak", "mdia", "minf", "stbl", "dinf", "mvex", "moof", "traf",
        ];
        let mut list = Vec::new();
        let mut pos = start;
        while pos + 8 <= end {
            let size = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
            let ty = std::str::from_utf8(&data[pos + 4..pos + 8])
                .unwrap()
                .to_string();
            assert!(size >= 8 && pos + size <= end, "Bad box {ty} at {pos} size {size}");
            let children = if CONTAINERS.contains(&ty.as_str()) {
                Self::parse(data, pos + 8, pos + size)
            } else {
                Vec::new()
            };
            list.push(Self {
                ty,
                offset: pos,
                size,
                children,
            });
            pos += size;
        }
        assert_eq!(pos, end, "Trailing bytes at {pos}");
        list
    }

    fn child(&self, ty: &str) -> &Self {
        let matches: Vec<&Self> = self.children.iter().filter(|c| c.ty == ty).collect();
        assert_eq!(1, matches.len(), "expected single child {ty}");
        matches[0]
    }
}

fn av1_seq_header_obu() -> Vec<u8> {
    let mut bits = Vec::new();
    let mut w = |v: i64, n: usize| {
        for i in (0..n).rev() {
            bits.push(((v >> i) & 1) as u8);
        }
    };
    w(0, 3);
    w(0, 1);
    w(0, 1);
    w(0, 1);
    w(0, 1);
    w(0, 5);
    w(0, 12);
    w(8, 5);
    w(0, 1);
    w(15, 4);
    w(15, 4);
    w(1279, 16);
    w(719, 16);
    w(0, 1);
    w(0, 1);
    w(1, 1);
    w(1, 1);
    w(0, 1);
    w(0, 1);
    w(0, 1);
    w(0, 1);
    w(1, 1);
    w(0, 1);
    w(0, 1);
    w(1, 1);
    w(1, 1);
    w(6, 3);
    w(0, 1);
    w(1, 1);
    w(1, 1);
    w(0, 1);
    w(0, 1);
    w(0, 1);
    w(0, 1);
    w(0, 2);
    w(0, 1);
    w(1, 1);
    while bits.len() % 8 != 0 {
        bits.push(0);
    }
    let mut payload = vec![0u8; bits.len() / 8];
    for (i, &b) in bits.iter().enumerate() {
        if b == 1 {
            payload[i / 8] |= 0x80 >> (i % 8);
        }
    }
    let mut obu = vec![(1 << 3) | 0x02, payload.len() as u8];
    obu.extend_from_slice(&payload);
    obu
}

const TEMPORAL_DELIMITER: [u8; 2] = [(2 << 3) | 0x02, 0];

fn make_av1_frame(seq: &[u8], key: bool, index: u8) -> Vec<u8> {
    let mut frame = TEMPORAL_DELIMITER.to_vec();
    if key {
        frame.extend_from_slice(seq);
    }
    frame.extend_from_slice(&[(6 << 3) | 0x02, 4, 1, 2, 3, index]);
    frame
}

#[test]
fn av1c_record_from_sequence_header() {
    let seq = av1_seq_header_obu();
    let info = Av1::parse_sequence_header(&seq).unwrap();
    assert_eq!(0, info.profile);
    assert_eq!(8, info.level);
    assert_eq!(1280, info.max_width);
    assert_eq!(720, info.max_height);
    assert_eq!(1, info.sub_x);
    assert_eq!(1, info.sub_y);
    let c = Av1::build_av1c(&seq).unwrap();
    assert_eq!(0x81, c[0]);
    assert_eq!(8, c[1]);
    assert_eq!(seq.as_slice(), &c[4..]);
}

#[test]
fn writes_parseable_fragmented_mp4_with_video_and_audio() {
    let seq = av1_seq_header_obu();
    let vring = PacketRing::new(1 << 20, Duration::ZERO, false).unwrap();
    for i in 0..30i64 {
        let key = i % 10 == 0;
        vring.write(&make_av1_frame(&seq, key, i as u8), 1000 * SEC + i * SEC / 10, key, 0);
    }
    let aring = PacketRing::new(1 << 20, Duration::ZERO, false).unwrap();
    for i in 0..(3 * 48_000 / 1024) as i64 {
        aring.write(
            &[0x21, 0x10, i as u8],
            1000 * SEC + i * 1024 * SEC / 48_000,
            true,
            0,
        );
    }
    let vs = vring.snapshot(0);
    let aus = aring.snapshot(0);
    let tracks = vec![
        MuxTrack {
            kind: TrackKind::Video,
            codec: "av01".to_string(),
            video_codec: Some(VideoCodec::Av1),
            width: 1280,
            height: 720,
            sample_rate: 0,
            channels: 0,
            config: Av1::build_av1c(&seq).unwrap(),
            default_duration_100ns: SEC / 10,
            samples: &vs,
            formats: None,
            fps: 0,
        },
        MuxTrack {
            kind: TrackKind::Audio,
            codec: "mp4a".to_string(),
            video_codec: None,
            width: 0,
            height: 0,
            sample_rate: 48_000,
            channels: 2,
            config: vec![0x11, 0x90],
            default_duration_100ns: 1024 * SEC / 48_000,
            samples: &aus,
            formats: None,
            fps: 0,
        },
    ];
    let mut data = Vec::new();
    let duration_ms = Fmp4Writer::write(&mut data, &tracks).unwrap();
    let boxes = Mp4Box::parse(&data, 0, data.len());

    assert_eq!("ftyp", boxes[0].ty);
    assert_eq!("moov", boxes[1].ty);
    let moov = &boxes[1];
    let traks: Vec<&Mp4Box> = moov.children.iter().filter(|b| b.ty == "trak").collect();
    assert_eq!(2, traks.len());
    assert_eq!(
        2,
        moov.child("mvex")
            .children
            .iter()
            .filter(|b| b.ty == "trex")
            .count()
    );
    let stsd = traks[0].child("mdia").child("minf").child("stbl").child("stsd");
    assert!(
        String::from_utf8_lossy(&data[stsd.offset..stsd.offset + stsd.size]).contains("av1C")
    );
    let audio_stsd = traks[1].child("mdia").child("minf").child("stbl").child("stsd");
    assert!(
        String::from_utf8_lossy(&data[audio_stsd.offset..audio_stsd.offset + audio_stsd.size])
            .contains("esds")
    );

    let moof_indices: Vec<usize> = boxes
        .iter()
        .enumerate()
        .filter(|(_, b)| b.ty == "moof")
        .map(|(i, _)| i)
        .collect();
    let mdat_count = boxes.iter().filter(|b| b.ty == "mdat").count();
    assert_eq!(moof_indices.len(), mdat_count);
    assert!(moof_indices.len() >= 5);

    let mut video_samples = 0u32;
    for idx in moof_indices {
        let moof = &boxes[idx];
        let next = &boxes[idx + 1];
        assert_eq!("mdat", next.ty);
        let traf = moof.child("traf");
        let trun = traf.child("trun");
        let count = u32::from_be_bytes(
            data[trun.offset + 12..trun.offset + 16].try_into().unwrap(),
        );
        let data_offset = u32::from_be_bytes(
            data[trun.offset + 16..trun.offset + 20].try_into().unwrap(),
        ) as usize;
        assert_eq!(moof.size + 8, data_offset);
        let mut sum = 0usize;
        for i in 0..count as usize {
            let off = trun.offset + 20 + i * 12 + 4;
            sum += u32::from_be_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        }
        assert_eq!(next.size - 8, sum);
        let tfhd = traf.child("tfhd");
        let track_id = u32::from_be_bytes(
            data[tfhd.offset + 12..tfhd.offset + 16].try_into().unwrap(),
        );
        if track_id == 1 {
            video_samples += count;
            let flags = u32::from_be_bytes(
                data[trun.offset + 28..trun.offset + 32].try_into().unwrap(),
            );
            assert_eq!(0x0200_0000u32, flags);
            assert_ne!(2, (data[next.offset + 8] >> 3) & 0x0F);
        }
    }
    assert_eq!(30, video_samples);
    assert!((2900..=3100).contains(&duration_ms));
    assert_eq!(
        duration_ms,
        ReplayLibrary::read_mvhd_duration_ms(&mut Cursor::new(&data))
    );
}

#[test]
fn annex_b_to_length_prefixed_and_avcc() {
    let sps: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9];
    let pps: &[u8] = &[0x68, 0xEB, 0xE3];
    let idr: &[u8] = &[0x65, 0x88, 0x84, 0x00];
    let mut au = vec![0, 0, 0, 1, 0x09, 0xF0]; // AUD, dropped
    au.extend_from_slice(&[0, 0, 0, 1]);
    au.extend_from_slice(sps);
    au.extend_from_slice(&[0, 0, 1]);
    au.extend_from_slice(pps);
    au.extend_from_slice(&[0, 0, 0, 1]);
    au.extend_from_slice(idr);

    let mut dst = vec![0u8; au.len() * 2];
    let n = AnnexB::to_length_prefixed(&au, &mut dst, VideoCodec::H264);
    assert_eq!(4 + sps.len() + 4 + pps.len() + 4 + idr.len(), n);
    assert_eq!(
        sps.len() as i32,
        i32::from_be_bytes([dst[0], dst[1], dst[2], dst[3]])
    );
    let avcc = AnnexB::build_avcc(&au).unwrap();
    assert_eq!(1, avcc[0]);
    assert_eq!(0x64, avcc[1]);
    assert_eq!(0xFF, avcc[4]);
    assert_eq!(0xE1, avcc[5]);
}

// =========================================================================
// ReplayLayoutTests
// =========================================================================

#[test]
fn replay_scaling_fit_options() {
    let (max_w, max_h) = (7680, 4320);
    assert_eq!((2560, 1440), ReplayScaling::fit(2560, 1440, Some("native"), 100, max_w, max_h));
    assert_eq!((2560, 1440), ReplayScaling::fit(2560, 1440, Some("2160p"), 100, max_w, max_h));
    assert_eq!((2560, 1440), ReplayScaling::fit(2560, 1440, Some("1440p"), 100, max_w, max_h));
    assert_eq!((1920, 1080), ReplayScaling::fit(2560, 1440, Some("1080p"), 100, max_w, max_h));
    assert_eq!((1600, 900), ReplayScaling::fit(2560, 1440, Some("900p"), 100, max_w, max_h));
    assert_eq!((1280, 720), ReplayScaling::fit(2560, 1440, Some("720p"), 100, max_w, max_h));
    assert_eq!((960, 540), ReplayScaling::fit(2560, 1440, Some("540p"), 100, max_w, max_h));
    assert_eq!((1280, 720), ReplayScaling::fit(2560, 1440, Some("scale"), 50, max_w, max_h));
    assert_eq!((640, 360), ReplayScaling::fit(2560, 1440, Some("scale"), 10, max_w, max_h));

    assert_eq!((900, 720), ReplayScaling::fit(1280, 1024, Some("720p"), 100, max_w, max_h));
    assert_eq!((1920, 1080), ReplayScaling::fit(1921, 1081, Some("native"), 100, max_w, max_h));
    assert_eq!((7680, 1920), ReplayScaling::fit(8000, 2000, Some("native"), 100, max_w, max_h));

    let (w, h) = ReplayScaling::fit(1921, 1081, Some("native"), 100, max_w, max_h);
    assert!(w % 2 == 0 && h % 2 == 0);
}

#[test]
fn canvas_layout_computations() {
    let m = |index, left, top, width, height| MonitorPlacement {
        index,
        left,
        top,
        width,
        height,
    };

    let layout_a =
        CanvasLayout::compute(&[m(0, -1280, 0, 1280, 1024), m(1, 0, 0, 1920, 1080)], Some("native"), 100, 7680, 4320)
            .unwrap();
    assert_eq!(3200, layout_a.width);
    assert_eq!(1080, layout_a.height);
    assert_eq!(-1280, layout_a.origin_left);
    assert_eq!(0, layout_a.origin_top);
    assert_eq!(Rect::new(0, 0, 1280, 1024), layout_a.for_monitor(0).unwrap().rect);
    assert_eq!(Rect::new(1280, 0, 3200, 1080), layout_a.for_monitor(1).unwrap().rect);

    let layout_b =
        CanvasLayout::compute(&[m(0, 0, 0, 1920, 1080), m(1, 1920, -200, 1280, 1024)], Some("native"), 100, 7680, 4320)
            .unwrap();
    assert_eq!(3200, layout_b.width);
    assert_eq!(1280, layout_b.height);
    assert_eq!(0, layout_b.origin_left);
    assert_eq!(-200, layout_b.origin_top);
    assert_eq!(Rect::new(0, 200, 1920, 1280), layout_b.for_monitor(0).unwrap().rect);
    assert_eq!(Rect::new(1920, 0, 3200, 1024), layout_b.for_monitor(1).unwrap().rect);

    let layout_c = CanvasLayout::compute(
        &[
            m(0, 0, 0, 3840, 2160),
            m(1, 3840, 0, 3840, 2160),
            m(2, 7680, 0, 3840, 2160),
        ],
        Some("native"),
        100,
        7680,
        4320,
    )
    .unwrap();
    assert_eq!(7680, layout_c.width);
    assert_eq!(1440, layout_c.height);
    assert_eq!(Rect::new(0, 0, 2560, 1440), layout_c.for_monitor(0).unwrap().rect);
    assert_eq!(Rect::new(2560, 0, 5120, 1440), layout_c.for_monitor(1).unwrap().rect);
    assert_eq!(Rect::new(5120, 0, 7680, 1440), layout_c.for_monitor(2).unwrap().rect);

    let layout_d =
        CanvasLayout::compute(&[m(0, -1280, 0, 1280, 1024), m(1, 0, 0, 1920, 1080)], Some("1080p"), 100, 7680, 4320)
            .unwrap();
    assert_eq!(1080, layout_d.height);
    assert_eq!(3200, layout_d.width);

    let layout_e =
        CanvasLayout::compute(&[m(0, 100, 50, 2560, 1440)], Some("720p"), 100, 7680, 4320)
            .unwrap();
    assert_eq!(1280, layout_e.width);
    assert_eq!(720, layout_e.height);
    assert_eq!(Rect::new(0, 0, 1280, 720), layout_e.for_monitor(0).unwrap().rect);

    // Merged from bench_tests::canvas_layout_per_monitor_scaling:
    let layout_scale =
        CanvasLayout::compute(&[m(0, 0, 0, 2560, 1440)], Some("scale"), 75, 7680, 4320).unwrap();
    assert_eq!(1920, layout_scale.width);
    assert_eq!(1080, layout_scale.height);

    let layout_pair = CanvasLayout::compute(
        &[m(0, 0, 0, 1920, 1080), m(1, 1920, 0, 1280, 1024)],
        Some("720p"),
        100,
        7680,
        4320,
    )
    .unwrap();
    assert_eq!(2132, layout_pair.width);
    assert_eq!(720, layout_pair.height);

    assert!(layout_e.for_monitor(999).is_none());
    assert!(CanvasLayout::compute(&[], Some("native"), 100, 1920, 1080).is_err());
}

#[test]
fn dirty_mapper_empty_and_scaling() {
    assert!(DirtyMapper::is_empty(dirty_to_dest(Rect::new(0, 0, 0, 0), 1920, 1080, 960, 540, 2)));
    assert!(DirtyMapper::is_empty(dirty_to_dest(Rect::new(10, 10, 5, 5), 1920, 1080, 960, 540, 2)));

    let full = dirty_to_dest(Rect::new(0, 0, 1920, 1080), 1920, 1080, 960, 540, 2);
    assert_eq!(Rect::new(0, 0, 960, 540), full);

    let mapped = dirty_to_dest(Rect::new(100, 100, 110, 110), 1920, 1080, 960, 540, 2);
    assert_eq!(Rect::new(48, 48, 58, 58), mapped);
    assert_eq!(0, mapped.left % 2);
    assert_eq!(0, mapped.top % 2);
    assert_eq!(0, mapped.right % 2);
    assert_eq!(0, mapped.bottom % 2);
    assert!((0..=960).contains(&mapped.left));
    assert!((0..=960).contains(&mapped.right));
    assert!((0..=540).contains(&mapped.top));
    assert!((0..=540).contains(&mapped.bottom));

    let r = Rect::new(10, 20, 30, 40);
    let empty = Rect::default();
    assert_eq!(r, DirtyMapper::union(r, empty));
    assert_eq!(r, DirtyMapper::union(empty, r));
}

#[test]
fn frame_pacer_pacing_without_drift() {
    for fps in [24, 25, 144, 240] {
        let freq = 10_000_000i64;
        let mut pacer = FramePacer::new(fps, freq);
        let mut pts_list = Vec::new();
        let start = 1_000_000i64;
        let end = start + freq;
        let mut now = start;
        while now <= end {
            if let Some(pts) = pacer.try_tick(now) {
                pts_list.push(pts);
            }
            now += 50;
        }
        assert_eq!((fps + 1) as usize, pts_list.len());
        let expected_step = freq as f64 / fps as f64;
        for w in pts_list.windows(2) {
            assert!(w[1] > w[0]);
            let diff = w[1] - w[0];
            assert!(
                diff >= (expected_step - 1.0).floor() as i64
                    && diff <= (expected_step + 1.0).ceil() as i64
            );
        }
        assert_eq!(10_000_000, pts_list[fps as usize] - pts_list[0]);
    }
}

#[test]
fn frame_pacer_late_resets_and_skips_without_bunching() {
    let freq = 10_000_000i64;
    let mut pacer = FramePacer::new(25, freq);
    let mut now = 1_000_000i64;
    let mut pts_list = Vec::new();
    for _ in 0..50_000 {
        if let Some(pts) = pacer.try_tick(now) {
            pts_list.push(pts);
        }
        now += 50;
    }
    assert!(pts_list.len() >= 2);
    let last_pts = *pts_list.last().unwrap();

    now += 2 * freq;
    let late_pts = pacer.try_tick(now).expect("expected late tick");
    assert!(late_pts > last_pts);

    let period = freq / 25;
    let mut t = 1;
    while t < period {
        assert!(pacer.try_tick(now + t).is_none());
        t += 50;
    }
    assert!(pacer.try_tick(now + period).is_some());
}

#[test]
fn fmp4_writer_video_timescale_for() {
    assert_eq!(90_000, Fmp4Writer::video_timescale_for(30));
    assert_eq!(90_000, Fmp4Writer::video_timescale_for(24));
    assert_eq!(90_000, Fmp4Writer::video_timescale_for(144));
    assert_eq!(90_000, Fmp4Writer::video_timescale_for(240));
    assert_eq!(990_000, Fmp4Writer::video_timescale_for(165));
    assert_eq!(90_000, Fmp4Writer::video_timescale_for(0));
}

#[test]
fn replay_rate_normalize_and_effective() {
    assert_eq!(60, ReplayRate::normalize_refresh(59));
    assert_eq!(144, ReplayRate::normalize_refresh(143));
    assert_eq!(60, ReplayRate::normalize_refresh(60));
    assert_eq!(60, ReplayRate::normalize_refresh(0));

    assert_eq!(60, ReplayRate::effective(144, 60));
    assert_eq!(60, ReplayRate::effective(60, 144));
    assert_eq!(30, ReplayRate::effective(7, 60));
    assert_eq!(240, ReplayRate::effective(240, 240));
}

#[test]
fn video_stream_continuity_and_multi_format_fmp4() {
    let seq = av1_seq_header_obu();
    let stream = VideoStream::new("cam", 16 << 20, Duration::from_secs(60), false).unwrap();

    let s1 = stream.begin_session(VideoCodec::Av1, 1280, 720, 30);
    assert_eq!(1, s1);

    stream.on_packet(s1, &make_av1_frame(&seq, false, 0), 1000 * SEC, false);
    assert_eq!(1, stream.dropped_before_key());

    let mut pts = 1000 * SEC;
    for i in 0..60u8 {
        let key = i % 30 == 0;
        pts += SEC / 30;
        stream.on_packet(s1, &make_av1_frame(&seq, key, i), pts, key);
    }

    let s2 = stream.begin_session(VideoCodec::Av1, 1920, 1080, 30);
    assert_eq!(2, s2);

    pts += SEC / 30;
    stream.on_packet(s1, &make_av1_frame(&seq, false, 60), pts, false);

    pts += SEC / 30;
    stream.on_packet(s2, &make_av1_frame(&seq, false, 61), pts, false);
    assert_eq!(2, stream.dropped_before_key());

    for i in 0..60u8 {
        let key = i % 30 == 0;
        pts += SEC / 30;
        stream.on_packet(s2, &make_av1_frame(&seq, key, 62 + i), pts, key);
    }

    let formats = stream.formats();
    assert_eq!(2, formats.len());
    assert!(formats.contains_key(&1));
    assert!(formats.contains_key(&2));

    let snap = stream.ring().snapshot(i64::MIN);
    let track = stream.to_mux_track(&snap).unwrap();

    let mut data = Vec::new();
    Fmp4Writer::write(&mut data, &[track]).unwrap();
    let boxes = Mp4Box::parse(&data, 0, data.len());

    let moov = boxes.iter().find(|b| b.ty == "moov").unwrap();
    let trak = moov.child("trak");
    let stsd = trak.child("mdia").child("minf").child("stbl").child("stsd");
    let entry_count = u32::from_be_bytes(
        data[stsd.offset + 12..stsd.offset + 16].try_into().unwrap(),
    );
    assert_eq!(2, entry_count);

    let moofs: Vec<&Mp4Box> = boxes.iter().filter(|b| b.ty == "moof").collect();
    assert!(!moofs.is_empty());

    let mut sample_desc_indices = Vec::new();
    for moof in &moofs {
        let tfhd = moof.child("traf").child("tfhd");
        let flags = u32::from_be_bytes(
            data[tfhd.offset + 8..tfhd.offset + 12].try_into().unwrap(),
        ) & 0x00FF_FFFF;
        assert_ne!(0, flags & 2, "tfhd must have flags & 2 set");
        let idx = u32::from_be_bytes(
            data[tfhd.offset + 16..tfhd.offset + 20].try_into().unwrap(),
        );
        sample_desc_indices.push(idx);
    }

    let first_two = sample_desc_indices.iter().position(|&x| x == 2).unwrap();
    assert!(first_two > 0);
    for &idx in &sample_desc_indices[..first_two] {
        assert_eq!(1, idx);
    }
    for &idx in &sample_desc_indices[first_two..] {
        assert_eq!(2, idx);
    }
    let trun = moofs[first_two].child("traf").child("trun");
    let first_sample_flags = u32::from_be_bytes(
        data[trun.offset + 28..trun.offset + 32].try_into().unwrap(),
    );
    assert_eq!(0x0200_0000u32, first_sample_flags);
}

#[test]
fn packet_ring_preserves_format_and_reports_in_use() {
    let ring = PacketRing::new(1 << 20, Duration::from_secs(10), false).unwrap();
    ring.write(&[1, 2, 3], 1000, true, 7);
    let snap = ring.snapshot(0);
    assert_eq!(1, snap.packets().len());
    assert_eq!(7, snap.packets()[0].format);
    assert!(ring.formats_in_use().contains(&7));
}

// =========================================================================
// ReplaySamplerTests + Sweep2F carry-in test
// =========================================================================

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 12, 10, 0, 0).unwrap()
}

fn make_sample(ts: DateTime<Utc>, mon: i32, mark: u8) -> Sample {
    Sample {
        ts,
        monitor: mon,
        app: "game.exe".to_string(),
        title: "Boss".to_string(),
        site: String::new(),
        width: 1280,
        height: 720,
        bytes: vec![mark, mark, mark],
        mime: "image/webp".to_string(),
        grabbed: None,
    }
}

#[test]
fn due_per_monitor_with_jitter_slack() {
    let now = Arc::new(Mutex::new(t0()));
    let n_cb = now.clone();
    let r = ReplaySampler::with_clock(Arc::new(move || *n_cb.lock().unwrap()));
    assert!(r.due(0, 5));
    let t = *now.lock().unwrap();
    r.add(make_sample(t, 0, 1), 60);
    assert!(!r.due(0, 5));
    assert!(r.due(1, 5));
    *now.lock().unwrap() += ChronoDuration::milliseconds(4700);
    assert!(r.due(0, 5));
}

#[test]
fn add_copies_bytes_and_clear_zeroes() {
    let now = t0();
    let r = ReplaySampler::with_clock(Arc::new(move || now));
    let mut s = make_sample(now, 0, 7);
    s.bytes = vec![7, 7];
    r.add(s, 60);
    assert_eq!(7, r.take(now, now, 5)[0].bytes[0]);
    let cleared = r.clear_for_test();
    assert_eq!(1, cleared.len());
    let (buf, orig_len) = &cleared[0];
    assert_eq!(2, *orig_len);
    assert!(buf.is_empty());
    assert!(buf.capacity() >= *orig_len);
    unsafe {
        assert_eq!(&[0u8, 0u8], std::slice::from_raw_parts(buf.as_ptr(), *orig_len));
    }
    assert_eq!(0, r.count());
    assert!(r.due(0, 5));
}

#[test]
fn trim_drops_old_samples_but_keeps_newest_old_one_per_monitor() {
    let now = Arc::new(Mutex::new(t0()));
    let n_cb = now.clone();
    let r = ReplaySampler::with_clock(Arc::new(move || *n_cb.lock().unwrap()));
    for i in 0..10i64 {
        r.add(make_sample(t0() + ChronoDuration::seconds(i * 5), 0, i as u8), 120);
    }
    r.add(make_sample(t0(), 1, 99), 120);
    assert_eq!(11, r.count());
    *now.lock().unwrap() = t0() + ChronoDuration::seconds(200);
    let t = *now.lock().unwrap();
    r.add(make_sample(t, 0, 50), 120);
    assert_eq!(2, r.count());
    assert!(r.take(t0(), t, 5).iter().all(|x| x.monitor != 1));
}

#[test]
fn take_returns_window_and_carries_in_still_screen() {
    let r = ReplaySampler::with_clock(Arc::new(t0));
    r.add(make_sample(t0(), 0, 1), 300);
    r.add(make_sample(t0() + ChronoDuration::seconds(30), 0, 2), 300);
    r.add(make_sample(t0() + ChronoDuration::seconds(5), 1, 3), 300);
    r.add(make_sample(t0() + ChronoDuration::seconds(21), 1, 4), 300);
    r.add(make_sample(t0() + ChronoDuration::seconds(100), 0, 5), 300);
    let start = t0() + ChronoDuration::seconds(20);
    let got = r.take(start, t0() + ChronoDuration::seconds(60), 5);
    let summary: Vec<(DateTime<Utc>, i32, u8)> =
        got.iter().map(|s| (s.ts, s.monitor, s.bytes[0])).collect();
    assert_eq!(
        vec![
            (start, 0, 1),
            (t0() + ChronoDuration::seconds(21), 1, 4),
            (t0() + ChronoDuration::seconds(30), 0, 2),
        ],
        summary
    );
}

#[test]
fn to_frames_offsets_are_clamped_to_the_clip() {
    let start = t0() + ChronoDuration::seconds(10);
    let samples = vec![
        make_sample(start, 0, 1),
        make_sample(start + ChronoDuration::milliseconds(12_500), 0, 1),
        make_sample(start + ChronoDuration::seconds(40), 0, 1),
        make_sample(start - ChronoDuration::seconds(1), 0, 1),
    ];
    let frames = ReplaySampler::to_frames("r1", start, 30_000, &samples);
    let offsets: Vec<i32> = frames.iter().map(|f| f.offset_ms).collect();
    assert_eq!(vec![0, 12_500, 30_000, 0], offsets);
    assert!(frames.iter().all(|f| f.replay_id == "r1"));
    assert_eq!("Boss", frames[0].title);
    assert_eq!("image/webp", frames[0].image_mime);
}

#[test]
fn carry_in_sample_keeps_its_grab_time_for_the_fence() {
    let now = Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap();
    let sampler = ReplaySampler::with_clock(Arc::new(move || now));
    let grabbed = now - ChronoDuration::seconds(30);
    sampler.add(
        Sample {
            ts: grabbed,
            monitor: 0,
            app: "app".to_string(),
            title: "t".to_string(),
            site: String::new(),
            width: 64,
            height: 32,
            bytes: vec![1, 2, 3],
            mime: "image/webp".to_string(),
            grabbed: None,
        },
        130,
    );
    let start = now - ChronoDuration::seconds(10);
    let taken = sampler.take(start, now, 5);
    assert_eq!(1, taken.len());
    assert_eq!(start, taken[0].ts);
    assert_eq!(Some(grabbed), taken[0].grabbed);
    let frames = ReplaySampler::to_frames("rp", start, 10_000, &taken);
    assert_eq!(1, frames.len());
    assert_eq!(start, frames[0].ts);
    assert_eq!(Some(grabbed), frames[0].captured_at);

    sampler.add(
        Sample {
            ts: now - ChronoDuration::seconds(8),
            monitor: 0,
            app: "app".to_string(),
            title: "t".to_string(),
            site: String::new(),
            width: 64,
            height: 32,
            bytes: vec![4],
            mime: "image/webp".to_string(),
            grabbed: None,
        },
        130,
    );
    assert!(sampler.take(start, now, 5).iter().all(|s| s.grabbed.is_none()));
}

// =========================================================================
// CanvasBlitTests & CursorPainterTests
// =========================================================================

#[test]
fn dirty_region_update() {
    let (dst_pitch, dst_plane_h) = (64usize, 32usize);
    let mut canvas = vec![0xAAu8; dst_pitch * 48];
    let (src_pitch, src_plane_h) = (16usize, 8usize);
    let source = vec![0x55u8; src_pitch * 12];

    unsafe {
        CanvasBlit::copy_rect(
            Nv12Ref {
                ptr: source.as_ptr(),
                pitch: src_pitch,
                plane_height: src_plane_h,
            },
            Nv12Mut {
                ptr: canvas.as_mut_ptr(),
                pitch: dst_pitch,
                plane_height: dst_plane_h,
            },
            Rect::new(4, 2, 10, 6),
            20,
            10,
        );
    }

    for y in 0..dst_plane_h {
        for x in 0..dst_pitch {
            let changed = (12..=15).contains(&y) && (24..=29).contains(&x);
            assert_eq!(if changed { 0x55 } else { 0xAA }, canvas[y * dst_pitch + x]);
        }
    }
    let uv_base = dst_pitch * dst_plane_h;
    for y in 0..dst_plane_h / 2 {
        for x in 0..dst_pitch {
            let changed = (6..=7).contains(&y) && (24..=29).contains(&x);
            assert_eq!(
                if changed { 0x55 } else { 0xAA },
                canvas[uv_base + y * dst_pitch + x]
            );
        }
    }
}

#[test]
fn fill_black_updates_rect_and_empty_rect_does_nothing() {
    let (pitch, plane_h) = (64usize, 32usize);
    let mut canvas = vec![0xAAu8; pitch * 48];
    unsafe {
        CanvasBlit::fill_black(
            Nv12Mut {
                ptr: canvas.as_mut_ptr(),
                pitch,
                plane_height: plane_h,
            },
            Rect::new(2, 2, 6, 6),
        );
    }
    for y in 0..plane_h {
        for x in 0..pitch {
            let in_y = (2..=5).contains(&y) && (2..=5).contains(&x);
            assert_eq!(if in_y { 16 } else { 0xAA }, canvas[y * pitch + x]);
        }
    }
    let uv_base = pitch * plane_h;
    for y in 0..plane_h / 2 {
        for x in 0..pitch {
            let in_uv = (1..=2).contains(&y) && (2..=5).contains(&x);
            assert_eq!(
                if in_uv { 128 } else { 0xAA },
                canvas[uv_base + y * pitch + x]
            );
        }
    }
    let before = canvas.clone();
    unsafe {
        for r in [
            Rect::new(0, 0, 0, 0),
            Rect::new(10, 10, 10, 10),
            Rect::new(10, 10, 8, 8),
        ] {
            CanvasBlit::fill_black(
                Nv12Mut {
                    ptr: canvas.as_mut_ptr(),
                    pitch,
                    plane_height: plane_h,
                },
                r,
            );
        }
    }
    assert_eq!(before, canvas);
}

#[test]
fn clear_sets_all_y_to_16_and_all_uv_to_128() {
    let (pitch, plane_h, width) = (64usize, 32usize, 64usize);
    let mut canvas = vec![0u8; pitch * 48];
    unsafe {
        CanvasBlit::clear(
            Nv12Mut {
                ptr: canvas.as_mut_ptr(),
                pitch,
                plane_height: plane_h,
            },
            width,
        );
    }
    for y in 0..plane_h {
        for x in 0..width {
            assert_eq!(16, canvas[y * pitch + x]);
        }
    }
    let uv_base = pitch * plane_h;
    for y in 0..plane_h / 2 {
        for x in 0..width {
            assert_eq!(128, canvas[uv_base + y * pitch + x]);
        }
    }
}

fn test_arrow_shape() -> CursorShape {
    let n = 16usize;
    let mut black = vec![0u8; n * 4];
    let mut white = vec![0u8; n * 4];
    for i in 0..n {
        white[i * 4] = 255;
        white[i * 4 + 1] = 255;
        white[i * 4 + 2] = 255;
    }
    for i in [0, 1, 4, 5] {
        black[i * 4] = 255;
        black[i * 4 + 1] = 255;
        black[i * 4 + 2] = 255;
    }
    black[10 * 4] = 255;
    black[10 * 4 + 1] = 255;
    black[10 * 4 + 2] = 255;
    white[10 * 4] = 0;
    white[10 * 4 + 1] = 0;
    white[10 * 4 + 2] = 0;
    CursorShape::from_renders(&black, &white, 4, 4, 0, 0)
}

fn random_canvas(pitch: usize, h: usize) -> Vec<u8> {
    let mut c = vec![0u8; pitch * h * 3 / 2];
    let mut s = 7u64;
    for b in &mut c {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
        *b = (s >> 56) as u8;
    }
    c
}

#[test]
fn shape_from_renders_finds_alpha_and_invert() {
    let s = test_arrow_shape();
    assert_eq!(255, s.a[0]);
    assert_eq!(235, s.y[0]);
    assert_eq!(0, s.a[2]);
    assert!(s.invert[10]);
}

#[test]
fn draw_then_restore_gives_back_the_canvas() {
    let (w, h, pitch) = (64usize, 32usize, 64usize);
    let mut c = random_canvas(pitch, h);
    let before = c.clone();
    let mut p = CursorPainter::new();
    unsafe {
        p.draw(
            Nv12Mut {
                ptr: c.as_mut_ptr(),
                pitch,
                plane_height: h,
            },
            &test_arrow_shape(),
            CursorDrawParams {
                cx: 9.0,
                cy: 7.0,
                sx: 1.0,
                sy: 1.0,
                clip: Rect::new(0, 0, w as i32, h as i32),
            },
        );
        assert!(p.has_patch());
        assert_eq!(235, c[7 * pitch + 9]);
        assert_eq!(251u8.wrapping_sub(before[9 * pitch + 11]), c[9 * pitch + 11]);
        assert_eq!(before[7 * pitch + 11], c[7 * pitch + 11]);
        p.restore(Nv12Mut {
            ptr: c.as_mut_ptr(),
            pitch,
            plane_height: h,
        });
    }
    assert!(!p.has_patch());
    assert_eq!(before, c);
}

#[test]
fn draw_is_clipped_to_the_monitor() {
    let (h, pitch) = (32usize, 64usize);
    let mut c = random_canvas(pitch, h);
    let before = c.clone();
    let mut p = CursorPainter::new();
    unsafe {
        p.draw(
            Nv12Mut {
                ptr: c.as_mut_ptr(),
                pitch,
                plane_height: h,
            },
            &test_arrow_shape(),
            CursorDrawParams {
                cx: 30.0,
                cy: 30.0,
                sx: 1.0,
                sy: 1.0,
                clip: Rect::new(0, 0, 32, 32),
            },
        );
        assert_eq!(before[30 * pitch + 32], c[30 * pitch + 32]);
        p.restore(Nv12Mut {
            ptr: c.as_mut_ptr(),
            pitch,
            plane_height: h,
        });
    }
    assert_eq!(before, c);
}

// =========================================================================
// Sweep3J: A10, A11
// =========================================================================

#[test]
fn a10_late_packet_of_the_old_session_is_dropped_after_clear() {
    let seq = av1_seq_header_obu();
    let s = VideoStream::new("canvas", 16 << 20, Duration::from_secs(60), false).unwrap();
    let sess = s.begin_session(VideoCodec::Av1, 1280, 720, 30);
    for i in 0..30i64 {
        s.on_packet(sess, &make_av1_frame(&seq, i == 0, i as u8), (i + 1) * 333_333, i == 0);
    }
    s.clear();
    s.on_packet(sess, &make_av1_frame(&seq, true, 99), 31 * 333_333, true);
    assert!(s.ring().snapshot(i64::MIN).packets().is_empty());

    let next = s.begin_session(VideoCodec::Av1, 1280, 720, 30);
    s.on_packet(next, &make_av1_frame(&seq, true, 100), 40 * 333_333, true);
    assert_eq!(1, s.ring().snapshot(i64::MIN).packets().len());
}

#[test]
fn a11_locked_bytes_cover_rings_and_snapshot() {
    let ring = ReplayEngine::ring_capacity(8000, 120);
    assert!(ReplayEngine::locked_bytes(8000, 120, 4) >= ring * 4 * 2);
    assert!(ReplayEngine::locked_bytes(8000, 120, 1) > ReplayEngine::locked_bytes(8000, 30, 1));
}

// =========================================================================
// ReliabilitySweep2: Watchdog, ReplayLibrary ids & partials
// =========================================================================

#[test]
fn watchdog_reports_an_encoder_error_at_once() {
    let mut w = EncoderWatchdog::new(0, EncoderWatchdog::DEFAULT_STALL_MS);
    assert!(w.check(10, 0, None).is_none());
    assert_eq!(
        Some("SyncOperation -17".to_string()),
        w.check(20, 0, Some("SyncOperation -17"))
    );
}

#[test]
fn watchdog_reports_a_stall_only_while_frames_go_in() {
    let mut w = EncoderWatchdog::new(0, 3000);
    let mut packets = 0i64;
    for t in (0..5000).step_by(100) {
        w.attempt(t);
        packets += 1;
        assert!(w.check(t, packets, None).is_none());
    }
    let mut failure = None;
    for t in (5000..9000).step_by(100) {
        w.attempt(t);
        failure = w.check(t, packets, None);
        if failure.is_some() {
            break;
        }
    }
    let msg = failure.expect("expected stall failure");
    assert!(msg.starts_with("no output for"), "got {msg}");
}

#[test]
fn watchdog_ignores_a_pause_without_attempts() {
    let mut w = EncoderWatchdog::new(0, 3000);
    w.attempt(0);
    assert!(w.check(0, 1, None).is_none());
    w.attempt(60_000);
    assert!(w.check(60_000, 1, None).is_none());
    w.attempt(61_000);
    assert!(w.check(61_000, 2, None).is_none());
}

#[test]
fn replay_ids_are_unique_within_a_second_and_old_ids_still_parse() {
    let dir = TempDir::new("rid");
    let settings = Arc::new(Mutex::new(Settings::default()));
    let s_cb = settings.clone();
    let lib = ReplayLibrary::new(
        dir.path().to_path_buf(),
        Arc::new(move || s_cb.lock().unwrap().clone()),
        None,
    );
    let ts = Utc.with_ymd_and_hms(2026, 11, 15, 10, 20, 30).unwrap()
        + ChronoDuration::milliseconds(123);
    let a = lib.unique_id(ts);
    let b = lib.unique_id(ts);
    assert_eq!("20261115T102030123Z", a);
    assert_ne!(a, b);
    let tb = ReplayLibrary::try_parse_id(&b).unwrap();
    assert_eq!(ts + ChronoDuration::milliseconds(1), tb);
    let legacy = ReplayLibrary::try_parse_id("20261115T102030Z").unwrap();
    assert_eq!(Utc.with_ymd_and_hms(2026, 11, 15, 10, 20, 30).unwrap(), legacy);

    settings.lock().unwrap().replay.encrypt_exports = false;
    fs::create_dir_all(lib.plain_dir()).unwrap();
    let ts2 = ts + ChronoDuration::seconds(5);
    fs::write(
        lib.plain_dir()
            .join(format!("{}.mp4.partial", ReplayLibrary::new_id(ts2))),
        [1u8],
    )
    .unwrap();
    assert_ne!(ReplayLibrary::new_id(ts2), lib.unique_id(ts2));
}

#[test]
fn partial_replay_files_are_deleted_on_start_and_clips_kept() {
    let dir = TempDir::new("rpartials");
    let lib = ReplayLibrary::new(
        dir.path().to_path_buf(),
        Arc::new(Settings::default),
        None,
    );
    fs::create_dir_all(lib.encrypted_dir()).unwrap();
    fs::write(lib.encrypted_dir().join("20261115T102030Z.mp4.enc.partial"), [1]).unwrap();
    fs::write(lib.encrypted_dir().join("20261115T102031123Z.mp4.partial"), [1]).unwrap();
    fs::write(lib.encrypted_dir().join("20261115T102032Z.mp4"), [1]).unwrap();
    assert_eq!(2, lib.delete_partials());
    let remaining: Vec<String> = fs::read_dir(lib.encrypted_dir())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(vec!["20261115T102032Z.mp4".to_string()], remaining);
    assert_eq!(1, lib.list().len());
}

// =========================================================================
// Sweep4P, Sweep4U, Sweep5U: ReplayLibrary purge/cancel/lock fences
// =========================================================================

struct PlainWriter(File);
impl Write for PlainWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
impl ReplayBlobWriter for PlainWriter {
    fn finish_blob(mut self: Box<Self>) -> io::Result<()> {
        self.0.flush()
    }
}

struct FakeCipher;
impl ReplayBlobCipher for FakeCipher {
    fn open_write(&self, path: &Path) -> Result<Box<dyn ReplayBlobWriter>, String> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map(|f| Box::new(PlainWriter(f)) as Box<dyn ReplayBlobWriter>)
            .map_err(|e| e.to_string())
    }
    fn open_read(&self, path: &Path) -> Result<Box<dyn Read + Send>, String> {
        File::open(path)
            .map(|f| Box::new(f) as Box<dyn Read + Send>)
            .map_err(|e| e.to_string())
    }
}

struct SlowSource {
    started: Arc<AtomicBool>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    reads: i32,
    _file: Option<File>,
}

impl Read for SlowSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.reads == 0 {
            self.reads = 1;
            let n = buf.len().min(8);
            buf[..n].fill(7);
            return Ok(n);
        }
        self.started.store(true, Ordering::Release);
        let (lock, cv) = &*self.gate;
        let g = lock.lock().unwrap();
        let _ = cv.wait_timeout(g, Duration::from_millis(50));
        Ok(1)
    }
}

struct SlowCipher {
    started: Arc<AtomicBool>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    open_calls: AtomicI32,
    hold_file: bool,
}

impl SlowCipher {
    fn new(hold_file: bool) -> Arc<Self> {
        Arc::new(Self {
            started: Arc::new(AtomicBool::new(false)),
            gate: Arc::new((Mutex::new(false), Condvar::new())),
            open_calls: AtomicI32::new(0),
            hold_file,
        })
    }
}

impl ReplayBlobCipher for SlowCipher {
    fn open_write(&self, _path: &Path) -> Result<Box<dyn ReplayBlobWriter>, String> {
        Err("unsupported".to_string())
    }
    fn open_read(&self, path: &Path) -> Result<Box<dyn Read + Send>, String> {
        // First open is duration probe in `list()`; return empty immediately.
        if self.open_calls.fetch_add(1, Ordering::AcqRel) == 0 {
            return Ok(Box::new(Cursor::new(Vec::<u8>::new())));
        }
        let f = if self.hold_file {
            Some(File::open(path).map_err(|e| e.to_string())?)
        } else {
            None
        };
        Ok(Box::new(SlowSource {
            started: self.started.clone(),
            gate: self.gate.clone(),
            reads: 0,
            _file: f,
        }))
    }
}

fn new_clip(home: &Path) -> String {
    let dir = DataPaths::replays_dir(home);
    fs::create_dir_all(&dir).unwrap();
    let id = ReplayLibrary::new_id(Utc::now());
    fs::write(dir.join(format!("{id}.mp4.enc")), [1, 2, 3]).unwrap();
    id
}

#[test]
fn c1_bc_s1_replay_save_during_purge_all_leaves_no_clip() {
    for encrypted in [true, false] {
        let dir = TempDir::new("c1save");
        let lib = Arc::new(ReplayLibrary::new(
            dir.path().to_path_buf(),
            Arc::new(Settings::default),
            Some(Arc::new(FakeCipher)),
        ));
        let path = dir.path().join("replays").join(format!(
            "20261117T120000000Z{}",
            if encrypted { ".mp4.enc" } else { ".mp4" }
        ));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lib2 = lib.clone();
        let err = lib
            .write_clip(
                &path,
                encrypted,
                move |s| {
                    s.write_all(&[1, 2, 3, 4]).unwrap();
                    lib2.on_purge_all();
                    Ok(1000)
                },
                &CancelToken::none(),
            )
            .expect_err("expected purge error");
        assert!(matches!(err, AgentError::InvalidOperation(_)));
        assert_eq!(
            "A purge ran during the replay save; the clip was discarded.",
            err.to_string()
        );
        assert!(!path.exists());
        assert!(!path.with_extension(if encrypted { "enc.partial" } else { "mp4.partial" }).exists());

        let ms = lib
            .write_clip(
                &path,
                encrypted,
                |s| {
                    s.write_all(&[5]).unwrap();
                    Ok(1000)
                },
                &CancelToken::none(),
            )
            .unwrap();
        assert_eq!(1000, ms);
        assert!(path.exists());
    }
}

#[test]
fn c1_purge_all_cancels_a_running_replay_export_and_deletes_the_copy() {
    let dir = TempDir::new("c1exp");
    let cipher = SlowCipher::new(false);
    let lib = Arc::new(ReplayLibrary::new(
        dir.path().to_path_buf(),
        Arc::new(Settings::default),
        Some(cipher.clone()),
    ));
    let id = new_clip(dir.path());
    let target = dir.path().join("out").join("clip.mp4");
    let partial = dir.path().join("out").join("clip.mp4.partial");

    let lib_bg = lib.clone();
    let id_bg = id.clone();
    let target_bg = target.clone();
    let handle = thread::spawn(move || lib_bg.export(&id_bg, &target_bg, &CancelToken::none()));

    assert!(until(|| cipher.started.load(Ordering::Acquire), Duration::from_secs(5)));
    assert!(partial.exists());
    let journal = fs::read_to_string(ExportJournal::path_for(dir.path())).unwrap();
    assert!(journal.contains(&partial.to_string_lossy().into_owned()));
    assert!(!target.exists());

    lib.on_purge_all();
    let err = handle.join().unwrap().expect_err("expected NotFound");
    assert!(matches!(err, AgentError::NotFound));
    assert!(!target.exists());
    assert!(!partial.exists());
    assert!(!ExportJournal::path_for(dir.path()).exists());
}

#[test]
fn c3_replay_export_cancelled_by_the_caller_deletes_its_partial_and_finished_export_is_renamed() {
    let dir = TempDir::new("c3exp");
    let cipher = SlowCipher::new(false);
    let lib = Arc::new(ReplayLibrary::new(
        dir.path().to_path_buf(),
        Arc::new(Settings::default),
        Some(cipher.clone()),
    ));
    let id = new_clip(dir.path());
    let target = dir.path().join("out").join("cancelled.mp4");
    let partial = dir.path().join("out").join("cancelled.mp4.partial");
    let cts = CancelSource::new();

    let lib_bg = lib.clone();
    let id_bg = id.clone();
    let target_bg = target.clone();
    let tok = cts.token();
    let handle = thread::spawn(move || lib_bg.export(&id_bg, &target_bg, &tok));

    assert!(until(|| cipher.started.load(Ordering::Acquire), Duration::from_secs(5)));
    cts.cancel();
    let err = handle.join().unwrap().expect_err("expected Cancelled");
    assert!(matches!(err, AgentError::Cancelled));
    assert!(!target.exists());
    assert!(!partial.exists());

    let plain = ReplayLibrary::new(
        dir.path().to_path_buf(),
        Arc::new(Settings::default),
        Some(Arc::new(FakeCipher)),
    );
    let done = dir.path().join("out").join("done.mp4");
    plain.export(&id, &done, &CancelToken::none()).unwrap();
    assert_eq!(vec![1, 2, 3], fs::read(&done).unwrap());
    assert!(!dir.path().join("out").join("done.mp4.partial").exists());
    assert!(!ExportJournal::path_for(dir.path()).exists());
}

#[test]
fn c1_purge_all_through_the_agent_stops_a_replay_export_and_empties_replays() {
    let dir = TempDir::new("c1seam");
    fs::write(
        DataPaths::settings_file(dir.path()),
        r#"{"security":{"keyProtection":"dpapi"}}"#,
    )
    .unwrap();
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: dir.path().to_path_buf(),
            install_dir: dir.path().to_path_buf(),
        },
        false,
    );
    host.open_store(&CancelToken::none()).unwrap();
    let agent = AgentHost::new(
        host.clone(),
        AgentOptions {
            install_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            autostart: false,
        },
    );
    let cipher = SlowCipher::new(true);
    let lib = Arc::new(ReplayLibrary::new(
        dir.path().to_path_buf(),
        Arc::new(Settings::default),
        Some(cipher.clone()),
    ));
    let host_p = host.clone();
    lib.set_purging_all(Some(Arc::new(move || host_p.purging_all())));
    agent.set_replays_for_test(lib.clone());
    let agent_p = agent.clone();
    let _guard = host.on_purged(move |n| agent_p.on_purged(n));

    let id = new_clip(dir.path());
    let target = dir.path().join("out").join("seam.mp4");
    let lib_bg = lib.clone();
    let id_bg = id.clone();
    let target_bg = target.clone();
    let handle = thread::spawn(move || lib_bg.export(&id_bg, &target_bg, &CancelToken::none()));

    assert!(until(|| cipher.started.load(Ordering::Acquire), Duration::from_secs(5)));
    host.purge_all(&CancelToken::none());

    let err = handle.join().unwrap().expect_err("expected NotFound");
    assert!(matches!(err, AgentError::NotFound));
    assert!(!target.exists());
    assert!(!dir.path().join("out").join("seam.mp4.partial").exists());
    let replays_left = fs::read_dir(DataPaths::replays_dir(dir.path()))
        .map(|it| it.count())
        .unwrap_or(0);
    assert_eq!(0, replays_left);
    assert_eq!(0, host.last_purge_all_undeleted());
    agent.shutdown();
    host.shutdown();
}

#[test]
fn c1_save_and_export_are_refused_while_purge_all_runs() {
    let dir = TempDir::new("c1refuse");
    let purging = Arc::new(AtomicBool::new(true));
    let lib = ReplayLibrary::new(
        dir.path().to_path_buf(),
        Arc::new(Settings::default),
        Some(Arc::new(FakeCipher)),
    );
    let p_cb = purging.clone();
    lib.set_purging_all(Some(Arc::new(move || p_cb.load(Ordering::Acquire))));

    let id = new_clip(dir.path());
    let clip = DataPaths::replays_dir(dir.path()).join("20261117T130000000Z.mp4.enc");
    let err = lib
        .write_clip(&clip, true, |_| Ok(1000), &CancelToken::none())
        .expect_err("expected refusal");
    assert!(matches!(err, AgentError::InvalidOperation(_)));
    assert_eq!("Purge all is running.", err.to_string());
    assert!(!clip.exists());

    let target = dir.path().join("out").join("refused.mp4");
    let err = lib
        .export(&id, &target, &CancelToken::none())
        .expect_err("expected NotFound");
    assert!(matches!(err, AgentError::NotFound));
    assert!(!target.exists());
}

struct FlagCipher {
    on_open: Arc<dyn Fn() + Send + Sync>,
}

impl ReplayBlobCipher for FlagCipher {
    fn open_write(&self, _path: &Path) -> Result<Box<dyn ReplayBlobWriter>, String> {
        Err("unsupported".to_string())
    }
    fn open_read(&self, _path: &Path) -> Result<Box<dyn Read + Send>, String> {
        (self.on_open)();
        Ok(Box::new(Cursor::new(vec![1u8, 2, 3])))
    }
}

#[test]
fn c1_export_whose_source_opened_as_purge_all_began_writes_nothing() {
    let dir = TempDir::new("c1flag");
    let purging = Arc::new(AtomicBool::new(false));
    let p_set = purging.clone();
    let cipher = Arc::new(FlagCipher {
        on_open: Arc::new(move || p_set.store(true, Ordering::Release)),
    });
    let lib = ReplayLibrary::new(
        dir.path().to_path_buf(),
        Arc::new(Settings::default),
        Some(cipher),
    );
    let p_get = purging.clone();
    lib.set_purging_all(Some(Arc::new(move || p_get.load(Ordering::Acquire))));
    let id = new_clip(dir.path());
    purging.store(false, Ordering::Release);
    let target = dir.path().join("out").join("late.mp4");
    let err = lib
        .export(&id, &target, &CancelToken::none())
        .expect_err("expected NotFound");
    assert!(matches!(err, AgentError::NotFound));
    assert!(!target.exists());
    assert!(!dir.path().join("out").join("late.mp4.partial").exists());
    assert!(!ExportJournal::path_for(dir.path()).exists());
}

#[test]
fn app_purge_and_panic_discard_a_replay_save_in_flight() {
    let dir = TempDir::new("appdiscard");
    fs::write(
        DataPaths::settings_file(dir.path()),
        r#"{"security":{"keyProtection":"dpapi"}}"#,
    )
    .unwrap();
    let host = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: dir.path().to_path_buf(),
            install_dir: dir.path().to_path_buf(),
        },
        false,
    );
    let agent = AgentHost::new(
        host.clone(),
        AgentOptions {
            install_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            autostart: false,
        },
    );
    let lib = agent.replays();
    let replays_dir = DataPaths::replays_dir(dir.path());
    fs::create_dir_all(&replays_dir).unwrap();

    let app = replays_dir.join("20261117T140000000Z.mp4");
    let agent_a = agent.clone();
    let err = lib
        .write_clip(
            &app,
            false,
            move |s| {
                s.write_all(&[1]).unwrap();
                agent_a.on_purged(&PurgeNotice {
                    reason: "purge".to_string(),
                    ids: Vec::new(),
                    from: None,
                    to: None,
                    newest: None,
                    scope: Some("app".to_string()),
                });
                Ok(1000)
            },
            &CancelToken::none(),
        )
        .expect_err("expected discard");
    assert!(matches!(err, AgentError::InvalidOperation(_)));
    assert!(!app.exists());

    let panic_path = replays_dir.join("20261117T140001000Z.mp4");
    let agent_b = agent.clone();
    let err = lib
        .write_clip(
            &panic_path,
            false,
            move |s| {
                s.write_all(&[1]).unwrap();
                agent_b.panic();
                Ok(1000)
            },
            &CancelToken::none(),
        )
        .expect_err("expected discard");
    assert!(matches!(err, AgentError::InvalidOperation(_)));
    assert!(!panic_path.exists());
    assert!(!replays_dir.join("20261117T140001000Z.mp4.partial").exists());
    agent.shutdown();
    host.shutdown();
}

struct FlippingStream {
    step: i32,
    on_end: Arc<dyn Fn() + Send + Sync>,
}

impl Read for FlippingStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.step == 0 {
            self.step = 1;
            let n = buf.len().min(8);
            buf[..n].fill(0x42);
            (self.on_end)();
            return Ok(n);
        }
        Ok(0)
    }
}

struct LockMidExportCipher {
    reads: AtomicI32,
    on_second: Arc<dyn Fn() + Send + Sync>,
}

impl ReplayBlobCipher for LockMidExportCipher {
    fn open_write(&self, path: &Path) -> Result<Box<dyn ReplayBlobWriter>, String> {
        FakeCipher.open_write(path)
    }
    fn open_read(&self, path: &Path) -> Result<Box<dyn Read + Send>, String> {
        if self.reads.fetch_add(1, Ordering::AcqRel) == 1 {
            return Ok(Box::new(FlippingStream {
                step: 0,
                on_end: self.on_second.clone(),
            }));
        }
        FakeCipher.open_read(path)
    }
}

#[test]
fn b1_replay_library_aborts_and_deletes_partial_when_gate_locks_mid_export() {
    let dir = TempDir::new("b1lock");
    let id = new_clip(dir.path());
    let locked = Arc::new(AtomicBool::new(false));
    let l_set = locked.clone();
    let cipher = Arc::new(LockMidExportCipher {
        reads: AtomicI32::new(0),
        on_second: Arc::new(move || l_set.store(true, Ordering::Release)),
    });
    let lib = ReplayLibrary::new(
        dir.path().to_path_buf(),
        Arc::new(Settings::default),
        Some(cipher),
    );
    let l_get = locked.clone();
    lib.set_locked(Some(Arc::new(move || l_get.load(Ordering::Acquire))));

    let target = dir.path().join("out").join("locked.mp4");
    let err = lib
        .export(&id, &target, &CancelToken::none())
        .expect_err("expected locked error");
    assert!(matches!(err, AgentError::InvalidOperation(_)));
    assert!(!target.exists());
    assert!(!dir.path().join("out").join("locked.mp4.partial").exists());
}

// =========================================================================
// EncoderPlan unit tests
// =========================================================================

#[test]
fn encoder_plan_orders_codecs_apis_and_adapters() {
    assert_eq!(
        vec![VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264],
        EncoderPlan::codecs(Some("auto"))
    );
    assert_eq!(vec![VideoCodec::Av1], EncoderPlan::codecs(Some("av1_qsv")));
    assert_eq!(vec![VideoCodec::Hevc], EncoderPlan::codecs(Some("hevc_qsv")));
    assert_eq!(vec![VideoCodec::H264], EncoderPlan::codecs(Some("h264_qsv")));

    let intel = AdapterInfo {
        luid: 1,
        vendor_id: GpuDevices::VENDOR_INTEL,
        name: "Intel Arc".to_string(),
        software: false,
        integrated: true,
        vram_mb: 128,
        device_id: 0,
    };
    let nvidia = AdapterInfo {
        luid: 2,
        vendor_id: GpuDevices::VENDOR_NVIDIA,
        name: "NVIDIA RTX 3050".to_string(),
        software: false,
        integrated: false,
        vram_mb: 8192,
        device_id: 0,
    };
    let warp = AdapterInfo {
        luid: 3,
        vendor_id: 0x1414,
        name: "Microsoft Basic Render".to_string(),
        software: true,
        integrated: false,
        vram_mb: 0,
        device_id: 0,
    };
    assert_eq!(vec!["vpl", "mf"], EncoderPlan::apis(&intel, VideoCodec::Av1));
    assert_eq!(vec!["mf"], EncoderPlan::apis(&nvidia, VideoCodec::Av1));

    let out = OutputInfo {
        index: 0,
        device_name: "D0".to_string(),
        adapter_luid: 2,
        vendor_id: GpuDevices::VENDOR_NVIDIA,
        adapter_name: nvidia.name.clone(),
        left: 0,
        top: 0,
        width: 1920,
        height: 1080,
        monitor: 0,
        refresh_hz: 60,
    };
    let auto = EncoderPlan::adapter_order(
        &[warp.clone(), nvidia.clone(), intel.clone()],
        std::slice::from_ref(&out),
        Some("auto"),
    );
    assert_eq!(vec![intel.clone(), nvidia.clone()], auto);

    let named = EncoderPlan::adapter_order(
        &[warp, nvidia.clone(), intel.clone()],
        std::slice::from_ref(&out),
        Some("rtx"),
    );
    assert_eq!(vec![nvidia, intel], named);
}

#[test]
fn encoder_plan_candidates_selection_order() {
    let make_output =
        |index: i32, name: &str, luid: i64, vendor_id: u32, adapter_name: &str, left: i32| {
            OutputInfo {
                index,
                device_name: name.to_string(),
                adapter_luid: luid,
                vendor_id,
                adapter_name: adapter_name.to_string(),
                left,
                top: 0,
                width: 1920,
                height: 1080,
                monitor: 0,
                refresh_hz: 60,
            }
        };

    let intel = AdapterInfo {
        luid: 1,
        vendor_id: GpuDevices::VENDOR_INTEL,
        name: "Intel Iris Xe".to_string(),
        software: false,
        integrated: true,
        vram_mb: 0,
        device_id: 0,
    };
    let nvidia = AdapterInfo {
        luid: 2,
        vendor_id: GpuDevices::VENDOR_NVIDIA,
        name: "NVIDIA GeForce RTX 4080".to_string(),
        software: false,
        integrated: false,
        vram_mb: 16384,
        device_id: 0,
    };
    let amd = AdapterInfo {
        luid: 3,
        vendor_id: GpuDevices::VENDOR_AMD,
        name: "AMD Radeon RX 7900".to_string(),
        software: false,
        integrated: false,
        vram_mb: 24576,
        device_id: 0,
    };
    let adapters = [intel.clone(), nvidia.clone(), amd.clone()];
    let outputs = [
        make_output(0, "Display1", 3, GpuDevices::VENDOR_AMD, "AMD Radeon RX 7900", 0),
        make_output(1, "Display2", 2, GpuDevices::VENDOR_NVIDIA, "NVIDIA GeForce RTX 4080", 1920),
        make_output(2, "Display3", 2, GpuDevices::VENDOR_NVIDIA, "NVIDIA GeForce RTX 4080", 3840),
    ];

    let available_a = |a: &AdapterInfo, c: VideoCodec, api: &str| {
        matches!(
            (a.luid, c, api),
            (1, VideoCodec::Av1, "vpl")
                | (1, VideoCodec::Hevc, "mf")
                | (1, VideoCodec::H264, "mf")
                | (2, VideoCodec::Hevc, "mf")
                | (2, VideoCodec::H264, "mf")
                | (3, VideoCodec::Hevc, "mf")
                | (3, VideoCodec::H264, "mf")
        )
    };

    let candidates_a =
        EncoderPlan::candidates(&adapters, &outputs, Some("auto"), Some("auto"), available_a);
    let expected_a = [
        (1i64, VideoCodec::Av1, "vpl"),
        (1, VideoCodec::Hevc, "mf"),
        (1, VideoCodec::H264, "mf"),
        (2, VideoCodec::Hevc, "mf"),
        (2, VideoCodec::H264, "mf"),
        (3, VideoCodec::Hevc, "mf"),
        (3, VideoCodec::H264, "mf"),
    ];
    assert_eq!(expected_a.len(), candidates_a.len());
    for (i, &(luid, codec, api)) in expected_a.iter().enumerate() {
        assert_eq!(luid, candidates_a[i].adapter.luid);
        assert_eq!(codec, candidates_a[i].codec);
        assert_eq!(api, candidates_a[i].api);
    }

    let outputs_b = [make_output(
        0,
        "Display1",
        2,
        GpuDevices::VENDOR_NVIDIA,
        "NVIDIA GeForce RTX 4080",
        0,
    )];
    let available_b = |a: &AdapterInfo, c: VideoCodec, api: &str| {
        matches!(
            (a.luid, c, api),
            (2, VideoCodec::Hevc, "mf") | (2, VideoCodec::H264, "mf")
        )
    };
    let candidates_b = EncoderPlan::candidates(
        std::slice::from_ref(&nvidia),
        &outputs_b,
        Some("auto"),
        Some("auto"),
        available_b,
    );
    let expected_b = [(2i64, VideoCodec::Hevc, "mf"), (2, VideoCodec::H264, "mf")];
    assert_eq!(expected_b.len(), candidates_b.len());
    for (i, &(luid, codec, api)) in expected_b.iter().enumerate() {
        assert_eq!(luid, candidates_b[i].adapter.luid);
        assert_eq!(codec, candidates_b[i].codec);
        assert_eq!(api, candidates_b[i].api);
    }

    let outputs_c = [make_output(
        0,
        "Display1",
        3,
        GpuDevices::VENDOR_AMD,
        "AMD Radeon RX 7900",
        0,
    )];
    let available_c = |a: &AdapterInfo, c: VideoCodec, api: &str| {
        matches!(
            (a.luid, c, api),
            (3, VideoCodec::Av1, "mf") | (3, VideoCodec::Hevc, "mf") | (3, VideoCodec::H264, "mf")
        )
    };
    let candidates_c = EncoderPlan::candidates(
        std::slice::from_ref(&amd),
        &outputs_c,
        Some("auto"),
        Some("auto"),
        available_c,
    );
    let expected_c = [
        (3i64, VideoCodec::Av1, "mf"),
        (3, VideoCodec::Hevc, "mf"),
        (3, VideoCodec::H264, "mf"),
    ];
    assert_eq!(expected_c.len(), candidates_c.len());
    for (i, &(luid, codec, api)) in expected_c.iter().enumerate() {
        assert_eq!(luid, candidates_c[i].adapter.luid);
        assert_eq!(codec, candidates_c[i].codec);
        assert_eq!(api, candidates_c[i].api);
    }

    let candidates_d =
        EncoderPlan::candidates(&adapters, &outputs, Some("auto"), Some("auto"), |_, _, _| false);
    assert!(candidates_d.is_empty());

    let candidates_e1 =
        EncoderPlan::candidates(&adapters, &outputs, Some("geforce"), Some("auto"), available_a);
    let expected_e1 = [
        (2i64, VideoCodec::Hevc, "mf"),
        (2, VideoCodec::H264, "mf"),
        (1, VideoCodec::Av1, "vpl"),
        (1, VideoCodec::Hevc, "mf"),
        (1, VideoCodec::H264, "mf"),
        (3, VideoCodec::Hevc, "mf"),
        (3, VideoCodec::H264, "mf"),
    ];
    assert_eq!(expected_e1.len(), candidates_e1.len());
    for (i, &(luid, codec, api)) in expected_e1.iter().enumerate() {
        assert_eq!(luid, candidates_e1[i].adapter.luid);
        assert_eq!(codec, candidates_e1[i].codec);
        assert_eq!(api, candidates_e1[i].api);
    }

    let candidates_e2 =
        EncoderPlan::candidates(&adapters, &outputs, Some("auto"), Some("h264"), available_a);
    let expected_e2 = [
        (1i64, VideoCodec::H264, "mf"),
        (2, VideoCodec::H264, "mf"),
        (3, VideoCodec::H264, "mf"),
    ];
    assert_eq!(expected_e2.len(), candidates_e2.len());
    for (i, &(luid, codec, api)) in expected_e2.iter().enumerate() {
        assert_eq!(luid, candidates_e2[i].adapter.luid);
        assert_eq!(codec, candidates_e2[i].codec);
        assert_eq!(api, candidates_e2[i].api);
    }
}

#[test]
fn ring_snapshot_drop_unlocks_with_original_length() {
    let ring = PacketRing::new(64 * 1024, Duration::from_secs(10), true).unwrap();
    let snap = ring.snapshot(0);
    drop(snap);
}

#[test]
fn replay_disable_clears_streams_and_refuses_save() {
    let devices = Arc::new(br_agent::capture::GpuDeviceCache::new());
    let eng = ReplayEngine::new(
        devices,
        Arc::new(Vec::new),
        Arc::new(Vec::new),
    );
    eng.configure(ReplayEngineConfig {
        enabled: false,
        layout: "allMonitors".to_string(),
        fps: 30,
        kbps: 4000,
        seconds: 30,
        encoder: "auto".to_string(),
        encoder_adapter: "auto".to_string(),
        resolution: "source".to_string(),
        scale_percent: 100,
        lock_memory: false,
    });
    let mut out = Vec::new();
    let res = eng.save(&mut out, None, 30);
    assert_eq!(res.unwrap_err(), "Instant replay is off.");
    assert_eq!(eng.ring_bytes(), 0);
}

// A1: paused replay holding packets and samples from 10 min ago + range purge of that time
// clears both ring and samples so replay save writes nothing from the purged range.
#[test]
fn a1_paused_replay_range_purge_clears_ring_and_samples() {
    use br_agent::capture::{CaptureService, GpuDeviceCache, OutputCapture};
    use br_agent::guardrails::GuardrailService;
    use br_agent::native::qpc;

    let dir = TempDir::new("a1-paused-purge");
    let core = CoreHost::create_with_hello_probe(
        CoreHostOptions {
            home: dir.path().to_path_buf(),
            install_dir: dir.path().to_path_buf(),
        },
        false,
    );
    let patch = serde_json::json!({
        "replay": { "enabled": true, "seconds": 30, "searchable": true, "encryptExports": false }
    });
    assert!(core.settings().try_update(&patch).is_none());

    let agent = AgentHost::new(
        core.clone(),
        AgentOptions {
            install_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            autostart: false,
        },
    );

    let settings_store = core.settings().clone();
    let guard = GuardrailService::new(
        move || settings_store.current(),
        || false,
        Vec::new,
        dir.path(),
    );
    let settings_store2 = core.settings().clone();
    let capture = CaptureService::new(
        Arc::new(move || settings_store2.current()),
        Arc::new(|| true),
        Arc::new(|| true),
        guard,
        None,
        None,
        false,
    );

    let devices = Arc::new(GpuDeviceCache::new());
    let eng = ReplayEngine::new(devices, Arc::new(Vec::new), Arc::new(Vec::new));
    eng.set_paused(true);

    let stream = Arc::new(VideoStream::new("0", 1 << 20, Duration::from_secs(30), false).unwrap());
    let session = stream.begin_session(VideoCodec::Av1, 1280, 720, 30);
    let seq = av1_seq_header_obu();
    let now_qpc = OutputCapture::qpc_100ns(qpc());
    let ten_min_ago_pts = now_qpc - 600 * SEC;
    stream.on_packet(session, &make_av1_frame(&seq, true, 0), ten_min_ago_pts, true);
    stream.on_packet(
        session,
        &make_av1_frame(&seq, false, 1),
        ten_min_ago_pts + SEC / 30,
        false,
    );
    assert_eq!(2, stream.ring().count());
    eng.insert_stream_for_test(stream.clone(), true);
    capture.attach_engine_for_test(eng);
    agent.set_capture_for_test(capture);

    let now = Utc::now();
    let ten_min_ago = now - ChronoDuration::minutes(10);
    agent.sampler_for_test().push_for_test(Sample {
        ts: ten_min_ago,
        monitor: 0,
        app: "notepad".to_string(),
        title: "Secret".to_string(),
        site: String::new(),
        width: 64,
        height: 64,
        bytes: vec![0xAB; 64],
        mime: "image/webp".to_string(),
        grabbed: None,
    });
    assert_eq!(1, agent.sampler_for_test().count());

    // Range purge covering 10 minutes ago while replay is paused.
    agent.on_purged(&PurgeNotice {
        reason: "purge".to_string(),
        ids: Vec::new(),
        from: Some(ten_min_ago - ChronoDuration::minutes(1)),
        to: Some(ten_min_ago + ChronoDuration::minutes(1)),
        newest: Some(ten_min_ago),
        scope: Some("range".to_string()),
    });

    assert_eq!(0, stream.ring().count(), "paused ring must be cleared");
    assert!(
        stream.ring().committed_bytes_all_zero_for_test(),
        "ring memory must be zeroed"
    );
    assert_eq!(
        0,
        agent.sampler_for_test().count(),
        "paused replay samples must be cleared"
    );
    assert!(
        agent.save_replay(&CancelToken::none()).is_err(),
        "replay save after purge must write nothing from the purged range"
    );

    // Non-overlapping purge still drops stale replay samples older than the ring window while paused.
    agent.sampler_for_test().push_for_test(Sample {
        ts: ten_min_ago,
        monitor: 0,
        app: "notepad".to_string(),
        title: "Stale".to_string(),
        site: String::new(),
        width: 64,
        height: 64,
        bytes: vec![0xCD; 64],
        mime: "image/webp".to_string(),
        grabbed: None,
    });
    assert_eq!(1, agent.sampler_for_test().count());
    agent.on_purged(&PurgeNotice {
        reason: "purge".to_string(),
        ids: Vec::new(),
        from: Some(now - ChronoDuration::days(2)),
        to: Some(now - ChronoDuration::days(1)),
        newest: Some(now - ChronoDuration::days(1)),
        scope: Some("range".to_string()),
    });
    assert_eq!(
        0,
        agent.sampler_for_test().count(),
        "stale paused sample older than ring window must be dropped on purge"
    );

    agent.shutdown();
    core.shutdown();
}
