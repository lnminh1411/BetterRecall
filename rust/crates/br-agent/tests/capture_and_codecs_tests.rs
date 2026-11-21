// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Stage A5 tests: dHash, ChangeMap, CapturePolicy, DedupFilter, OutputRotation, GpuDeviceCache,
//! DuplicationBackoff, EncodeGovernor, ImageCodec, AvifWriter, MonitorSelection, ChangeGate,
//! Thumbnailer / ByteLruCache, and Sweep3K.

mod common;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use br_agent::capture::{
    Av1, AvifWriter, CapturePolicy, ChangeGate, ChangeMap, ChangeResult, DHash, DedupFilter,
    EncodeGovernor, EncodeLevel, GpuDeviceCache, GpuDevices, ImageCodec, KeepReason,
    KeyframeEncoder, KeyframeRequest, MonitorPin, MonitorSelection, OutputCapture, OutputInfo,
    OutputRotation, VideoScaler, VideoScalerError, VideoScalerParams,
};
use br_agent::codecs::{Avif, Jxl};
use br_agent::native::{Rect, current_thread_cpu_ms};
use br_agent::privacy::{PixelBox, RedactionMasks};
use br_agent::thumbs::{AgentThumbnailer, ByteLruCache, KeyframeBlobStore};
use br_agent::util::RetryBackoff;
use br_contracts::{NewScreenCapture, Settings, ThumbData};
use br_core::{CancelToken, Thumbnailer};
use chrono::Utc;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE90,
    DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_UNSPECIFIED,
    DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use common::{
    FakeStore, TempDir, mean_abs_diff, screen_flat, screen_gradient, screen_rect, screen_text,
    screen_ui,
};

// ---------------- CaptureLogicTests: DHashTests ----------------

fn dhash_gradient(w: i32, h: i32, left_to_right: bool, noise_seed: u64) -> Vec<u8> {
    let mut px = vec![0u8; (w as usize) * (h as usize) * 4];
    let mut state = noise_seed.wrapping_add(1);
    for y in 0..h {
        for x in 0..w {
            let mut v = if left_to_right {
                x * 255 / (w - 1)
            } else {
                255 - x * 255 / (w - 1)
            };
            if noise_seed != 0 {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let delta = ((state >> 32) % 7) as i32 - 3;
                v = (v + delta).clamp(0, 255);
            }
            let i = ((y * w + x) as usize) * 4;
            px[i] = v as u8;
            px[i + 1] = v as u8;
            px[i + 2] = v as u8;
            px[i + 3] = 255;
        }
    }
    px
}

#[test]
fn identical_images_hash_equal() {
    let a = dhash_gradient(320, 200, true, 0);
    assert_eq!(
        DHash::compute(&a, 320, 200, 320 * 4).unwrap(),
        DHash::compute(&a, 320, 200, 320 * 4).unwrap()
    );
}

#[test]
fn small_noise_stays_within_threshold_and_inverse_is_far() {
    let a = dhash_gradient(320, 200, true, 0);
    let noisy = dhash_gradient(320, 200, true, 7);
    let inverse = dhash_gradient(320, 200, false, 0);
    let ha = DHash::compute(&a, 320, 200, 1280).unwrap();
    let hn = DHash::compute(&noisy, 320, 200, 1280).unwrap();
    let hi = DHash::compute(&inverse, 320, 200, 1280).unwrap();
    assert!(DHash::distance(ha, hn) < 6);
    assert_eq!(64, DHash::distance(ha, hi));
}

#[test]
fn respects_stride_padding() {
    let a = dhash_gradient(100, 80, true, 0);
    let stride = 100 * 4 + 64;
    let mut padded = vec![0u8; (stride as usize) * 80];
    for y in 0..80 {
        padded[y * (stride as usize)..y * (stride as usize) + 400]
            .copy_from_slice(&a[y * 400..(y + 1) * 400]);
    }
    assert_eq!(
        DHash::compute(&a, 100, 80, 400).unwrap(),
        DHash::compute(&padded, 100, 80, stride).unwrap()
    );
}

// ---------------- CaptureRecoveryTests: OutputRotationTests + GpuDeviceRecoveryTests ----------------

#[test]
fn degrees_from_dxgi() {
    assert_eq!(0, OutputRotation::degrees(DXGI_MODE_ROTATION_UNSPECIFIED));
    assert_eq!(0, OutputRotation::degrees(DXGI_MODE_ROTATION_IDENTITY));
    assert_eq!(90, OutputRotation::degrees(DXGI_MODE_ROTATION_ROTATE90));
    assert_eq!(180, OutputRotation::degrees(DXGI_MODE_ROTATION_ROTATE180));
    assert_eq!(270, OutputRotation::degrees(DXGI_MODE_ROTATION_ROTATE270));
}

#[test]
fn sizes_swap_for_quarter_turns() {
    assert_eq!((1080, 1920), OutputRotation::desktop_size(1920, 1080, 90));
    assert_eq!((1080, 1920), OutputRotation::desktop_size(1920, 1080, 270));
    assert_eq!((1920, 1080), OutputRotation::desktop_size(1920, 1080, 180));
    assert_eq!((1920, 1080), OutputRotation::texture_size(1080, 1920, 90));
    assert_eq!(
        (1920, 1080),
        OutputRotation::texture_size_for(1920, 1080, 1080, 1920, 90)
    );
    assert_eq!(
        (1920, 1080),
        OutputRotation::texture_size_for(1080, 1920, 1080, 1920, 90)
    );
    assert_eq!(
        (2560, 1440),
        OutputRotation::texture_size_for(2560, 1440, 2560, 1440, 0)
    );
}

#[test]
fn quarter_turn_corners_match_the_duplication_sample() {
    let (w, h) = (1920, 1080);
    assert_eq!((h - 1, 0), OutputRotation::texture_pixel_to_desktop(0, 0, w, h, 90));
    assert_eq!(
        (h - 1, w - 1),
        OutputRotation::texture_pixel_to_desktop(w - 1, 0, w, h, 90)
    );
    assert_eq!(
        (0, 0),
        OutputRotation::texture_pixel_to_desktop(0, h - 1, w, h, 90)
    );
    assert_eq!(
        (0, w - 1),
        OutputRotation::texture_pixel_to_desktop(0, 0, w, h, 270)
    );
    assert_eq!(
        (w - 1, h - 1),
        OutputRotation::texture_pixel_to_desktop(0, 0, w, h, 180)
    );
}

#[test]
fn rect_mapping_agrees_with_pixels_and_inverts() {
    let (w, h) = (7, 4);
    for deg in [0, 90, 180, 270] {
        let (dw, dh) = OutputRotation::desktop_size(w, h, deg);
        for y in 0..h {
            for x in 0..w {
                let (px, py) = OutputRotation::texture_pixel_to_desktop(x, y, w, h, deg);
                assert!((0..dw).contains(&px));
                assert!((0..dh).contains(&py));
                let d = OutputRotation::texture_to_desktop(
                    Rect::new(x, y, x + 1, y + 1),
                    w,
                    h,
                    deg,
                );
                assert_eq!(Rect::new(px, py, px + 1, py + 1), d);
                assert_eq!(
                    Rect::new(x, y, x + 1, y + 1),
                    OutputRotation::desktop_to_texture(d, w, h, deg)
                );
            }
        }
        let r = Rect::new(1, 0, 6, 3);
        let m = OutputRotation::texture_to_desktop(r, w, h, deg);
        assert_eq!(r.width() * r.height(), m.width() * m.height());
        assert!(m.left >= 0 && m.top >= 0 && m.right <= dw && m.bottom <= dh);
        assert_eq!(r, OutputRotation::desktop_to_texture(m, w, h, deg));
        assert_eq!(
            Rect::new(0, 0, dw, dh),
            OutputRotation::texture_to_desktop(Rect::new(0, 0, w, h), w, h, deg)
        );
    }
}

#[test]
fn portrait_output_masks_map_to_the_upright_keyframe_and_back() {
    let monitor = Rect::new(1920, -420, 1920 + 1080, -420 + 1920);
    let (kf_w, kf_h) = (540, 960);
    let field = Rect::new(1920 + 108, -420 + 192, 1920 + 108 + 216, -420 + 192 + 96);
    let boxes = RedactionMasks::to_keyframe(&[field], monitor, kf_w, kf_h);
    assert_eq!(1, boxes.len());
    assert_eq!(PixelBox::new(54, 96, 108, 48), boxes[0]);
    let back = RedactionMasks::to_desktop(&boxes, kf_w, kf_h, monitor);
    assert_eq!(vec![field], back);
    let wrong = RedactionMasks::to_keyframe(
        &[field],
        Rect::new(1920, -420, 1920 + 1920, -420 + 1080),
        kf_w,
        kf_h,
    );
    assert_eq!(1, wrong.len());
    assert_ne!(boxes[0], wrong[0]);
}

#[test]
fn video_processor_rotation_matches_the_mapping() {
    let Some(adapter) = GpuDevices::adapters().into_iter().find(|a| !a.software) else {
        return;
    };
    let Ok(dev) = GpuDevices::create_device(adapter.luid) else {
        return;
    };
    let (w, h) = (64i32, 32i32);
    let mut px = vec![0u32; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            px[(y * w + x) as usize] = match (x < w / 2, y < h / 2) {
                (true, true) => 0xFFFF_0000,
                (false, true) => 0xFF00_FF00,
                (true, false) => 0xFF00_00FF,
                _ => 0xFFFF_FFFF,
            };
        }
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w as u32,
        Height: h as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut src = None;
    unsafe { dev.device.CreateTexture2D(&desc, None, Some(&mut src)) }.unwrap();
    let src = src.unwrap();
    let src_res: ID3D11Resource = src.cast().unwrap();
    unsafe {
        dev.context
            .UpdateSubresource(&src_res, 0, None, px.as_ptr().cast(), (w * 4) as u32, 0);
    }

    for deg in [90, 180, 270] {
        let (dw, dh) = OutputRotation::desktop_size(w, h, deg);
        let mut scaler = match VideoScaler::new(
            dev.clone(),
            VideoScalerParams {
                src_w: w,
                src_h: h,
                dst_w: dw,
                dst_h: dh,
                dst_format: DXGI_FORMAT_B8G8R8A8_UNORM,
                with_staging: true,
                rotation_degrees: deg,
            },
        ) {
            Ok(s) => s,
            Err(VideoScalerError::RotationUnsupported) => return,
            Err(e) => panic!("VideoScaler failed: {e}"),
        };
        scaler.run(&src, true).unwrap();
        let st_res: ID3D11Resource = scaler.staging.as_ref().unwrap().cast().unwrap();
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { dev.context.Map(&st_res, 0, D3D11_MAP_READ, 0, Some(&mut m)) }.unwrap();
        for (tx, ty) in [
            (w / 4, h / 4),
            (3 * w / 4, h / 4),
            (w / 4, 3 * h / 4),
            (3 * w / 4, 3 * h / 4),
        ] {
            let (dx, dy) = OutputRotation::texture_pixel_to_desktop(tx, ty, w, h, deg);
            let got = unsafe {
                *(m.pData
                    .cast::<u8>()
                    .add((dy as usize) * (m.RowPitch as usize) + (dx as usize) * 4)
                    .cast::<u32>())
            };
            let want = px[(ty * w + tx) as usize];
            for shift in [0, 8, 16] {
                let diff = (((got >> shift) & 0xFF) as i32 - ((want >> shift) & 0xFF) as i32).abs();
                assert!(diff <= 24, "deg={deg} diff={diff}");
            }
        }
        unsafe { dev.context.Unmap(&st_res, 0) };
    }
}

#[test]
fn device_disposed_by_another_output_is_reported_gone_without_throwing() {
    let Some(adapter) = GpuDevices::adapters().into_iter().find(|a| !a.software) else {
        return;
    };
    let cache = GpuDeviceCache::new();
    let Ok(shared) = cache.get(adapter.luid) else {
        return;
    };
    assert!(cache.is_current(&shared));
    assert!(!cache.invalidate_if_removed(&shared));
    cache.invalidate(adapter.luid);
    assert!(shared.is_disposed());
    assert!(!cache.is_current(&shared));
    assert!(cache.invalidate_if_removed(&shared));
    shared.mark_disposed();
    let fresh = cache.get(adapter.luid).unwrap();
    assert!(!Arc::ptr_eq(&shared, &fresh));
    assert!(cache.is_current(&fresh));
}

// ---------------- DuplicationBackoffT3Tests ----------------

const T3_START: i64 = 10_000;

fn t3_ups(minutes: i64, lost_after_ms: i64, frames_before_loss: i64, backoff: bool) -> Vec<i64> {
    let barren = RetryBackoff::new(1000, OutputCapture::BARREN_MAX_MS);
    let mut now = T3_START;
    let mut last_try = 0i64;
    let mut ups = Vec::new();
    while now < T3_START + minutes * 60_000 {
        if OutputCapture::retry_due(now, last_try, if backoff { barren.next_at() } else { i64::MIN })
        {
            last_try = now;
            ups.push(now);
            now += lost_after_ms;
            if OutputCapture::barren(frames_before_loss, lost_after_ms) {
                barren.failed(now);
            } else {
                barren.succeeded();
            }
        } else {
            now += 500;
        }
    }
    ups
}

#[test]
fn lost_before_the_first_frame_is_barren_a_static_desktop_is_not() {
    assert!(OutputCapture::barren(0, 5));
    assert!(OutputCapture::barren(0, 255));
    assert!(!OutputCapture::barren(1, 5));
    assert!(!OutputCapture::barren(0, OutputCapture::BARREN_MS));
    assert!(!OutputCapture::barren(1200, 3_600_000));
}

#[test]
fn retry_waits_one_second_after_the_last_try_and_for_the_backoff() {
    assert!(!OutputCapture::retry_due(T3_START + 999, T3_START, i64::MIN));
    assert!(OutputCapture::retry_due(T3_START + 1000, T3_START, i64::MIN));
    assert!(!OutputCapture::retry_due(
        T3_START + 1000,
        T3_START,
        T3_START + 4000
    ));
    assert!(OutputCapture::retry_due(
        T3_START + 4000,
        T3_START,
        T3_START + 4000
    ));
}

#[test]
fn barren_duplications_back_off_to_the_cap() {
    let old = t3_ups(1, 5, 0, false);
    assert!((55..=61).contains(&old.len()));

    let ups = t3_ups(3, 5, 0, true);
    let first_min = ups.iter().filter(|&&t| t < T3_START + 60_000).count();
    assert!((6..=9).contains(&first_min));
    let gaps: Vec<i64> = ups.windows(2).map(|w| w[1] - w[0]).collect();
    for pair in gaps.windows(2) {
        assert!(pair[1] >= pair[0], "gap shrank: {} -> {}", pair[0], pair[1]);
    }
    for &g in gaps.iter().skip(5) {
        assert!((OutputCapture::BARREN_MAX_MS..=OutputCapture::BARREN_MAX_MS + 500).contains(&g));
    }
}

#[test]
fn a_duplication_that_delivered_frames_is_retried_after_one_second() {
    let ups = t3_ups(1, 300, 10, true);
    let gaps: Vec<i64> = ups.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(!gaps.is_empty());
    for g in gaps {
        assert!((1000..=1500).contains(&g));
    }
}

#[test]
fn a_frame_resets_the_backoff() {
    let barren = RetryBackoff::new(1000, OutputCapture::BARREN_MAX_MS);
    let mut now = T3_START;
    let mut delays = Vec::new();
    for _ in 0..6 {
        now += 5;
        if OutputCapture::barren(0, 5) {
            delays.push(barren.failed(now));
        }
        now = barren.next_at();
    }
    assert_eq!(
        vec![
            1000,
            2000,
            4000,
            8000,
            OutputCapture::BARREN_MAX_MS,
            OutputCapture::BARREN_MAX_MS
        ],
        delays
    );
    now += 20_000;
    if OutputCapture::barren(500, 20_000) {
        barren.failed(now);
    } else {
        barren.succeeded();
    }
    assert!(OutputCapture::retry_due(
        now + 1000,
        now - 20_000,
        barren.next_at()
    ));
}

// ---------------- EncodeGovernorTests ----------------

struct GovItem {
    id: i32,
    disposes: Arc<AtomicUsize>,
}

impl GovItem {
    fn new(id: i32) -> (Self, Arc<AtomicUsize>) {
        let d = Arc::new(AtomicUsize::new(0));
        (Self { id, disposes: d.clone() }, d)
    }
}

impl Drop for GovItem {
    fn drop(&mut self) {
        self.disposes.fetch_add(1, Ordering::AcqRel);
    }
}

type EncodeRecord = (i64, EncodeLevel, f64, i32);

struct GovHarness {
    clock: Arc<Mutex<i64>>,
    encodes: Arc<Mutex<Vec<EncodeRecord>>>,
    gov: EncodeGovernor<GovItem>,
}

impl GovHarness {
    fn new(costs: [f64; 4], initial: Option<[f64; 4]>, procs: usize) -> Self {
        let clock = Arc::new(Mutex::new(0i64));
        let encodes = Arc::new(Mutex::new(Vec::new()));
        let c1 = clock.clone();
        let c2 = clock.clone();
        let e1 = encodes.clone();
        let gov = EncodeGovernor::new(
            move || *c1.lock().unwrap(),
            procs,
            move |item: &mut GovItem, level| {
                let cost = costs[level.index()];
                let now = *c2.lock().unwrap();
                e1.lock().unwrap().push((now, level, cost, item.id));
                cost
            },
            |_item: &mut GovItem, _reason| {},
            initial.unwrap_or(costs),
        );
        Self {
            clock,
            encodes,
            gov,
        }
    }

    fn set_now(&self, t: i64) {
        *self.clock.lock().unwrap() = t;
    }

    fn now(&self) -> i64 {
        *self.clock.lock().unwrap()
    }
}

fn assert_within_budget(encodes: &[EncodeRecord], limit10: f64, limit60: f64) {
    for &(x, _, _, _) in encodes {
        let sum10: f64 = encodes
            .iter()
            .filter(|e| e.0 > x - 10_000 && e.0 <= x)
            .map(|e| e.2)
            .sum();
        let sum60: f64 = encodes
            .iter()
            .filter(|e| e.0 > x - 60_000 && e.0 <= x)
            .map(|e| e.2)
            .sum();
        assert!(sum10 <= limit10 + 1e-6, "10s budget exceeded at t={x}: {sum10} > {limit10}");
        assert!(sum60 <= limit60 + 1e-6, "60s budget exceeded at t={x}: {sum60} > {limit60}");
    }
}

#[test]
fn limits_calculate_correctly_from_processor_count() {
    let gov20 = EncodeGovernor::<GovItem>::new(|| 0, 20, |_, _| 0.0, |_, _| {}, [10.0; 4]);
    assert_eq!(3000.0, gov20.limit10_ms());
    assert_eq!(12000.0, gov20.limit60_ms());

    let gov8 = EncodeGovernor::<GovItem>::new(|| 0, 8, |_, _| 0.0, |_, _| {}, [10.0; 4]);
    assert_eq!(1200.0, gov8.limit10_ms());
    assert_eq!(4800.0, gov8.limit60_ms());
}

#[test]
fn light_load_stays_at_normal() {
    let mut h = GovHarness::new([100.0, 60.0, 30.0, 20.0], None, 20);
    let mut offered = 0;
    let mut t = 2000;
    while t <= 600_000 {
        h.set_now(t);
        offered += 1;
        h.gov.offer(GovItem::new(offered).0);
        t += 2000;
    }
    let enc = h.encodes.lock().unwrap().clone();
    assert_eq!(300, enc.len());
    assert!(enc.iter().all(|e| e.1 == EncodeLevel::Normal));
    assert_eq!(0, h.gov.deferred);
    assert_eq!(0, h.gov.dropped);
    assert_eq!(0, h.gov.waiting());
    assert_within_budget(&enc, 3000.0, 12000.0);
}

#[test]
fn heavy_load_steps_down_and_holds_the_budget() {
    let mut h = GovHarness::new([300.0, 150.0, 60.0, 30.0], None, 20);
    let mut offered = 0u64;
    let mut t = 2000;
    while t <= 600_000 {
        h.set_now(t);
        for _ in 0..3 {
            offered += 1;
            h.gov.offer(GovItem::new(offered as i32).0);
        }
        if t % 30_000 == 0 {
            for _ in 0..6 {
                offered += 1;
                h.gov.offer(GovItem::new(offered as i32).0);
            }
        }
        h.gov.pump();
        t += 2000;
    }
    let enc = h.encodes.lock().unwrap().clone();
    assert_within_budget(&enc, 3000.0, 12000.0);
    assert!(h.gov.downgraded > 0);
    assert!(enc.iter().any(|e| e.1 == EncodeLevel::Fast));
    assert!(enc.iter().any(|e| e.1 == EncodeLevel::Reduced));
    assert_eq!(offered, h.gov.encoded + h.gov.dropped + h.gov.waiting() as u64);
    let total_cost: f64 = enc.iter().map(|e| e.2).sum();
    assert!(total_cost / (600_000.0 * 20.0) < 0.01);
}

#[test]
fn newest_is_never_dropped() {
    let clock = Arc::new(Mutex::new(0i64));
    let last_offered = Arc::new(Mutex::new(0i32));
    let violations = Arc::new(Mutex::new(Vec::<(i32, i32)>::new()));
    let drops = Arc::new(Mutex::new(0usize));
    let costs = [800.0, 600.0, 500.0, 400.0];
    let c1 = clock.clone();
    let lo1 = last_offered.clone();
    let v1 = violations.clone();
    let d1 = drops.clone();
    let mut gov = EncodeGovernor::new(
        move || *c1.lock().unwrap(),
        20,
        move |_: &mut GovItem, level| costs[level.index()],
        move |item: &mut GovItem, _| {
            let lo = *lo1.lock().unwrap();
            if item.id == lo {
                v1.lock().unwrap().push((item.id, lo));
            }
            *d1.lock().unwrap() += 1;
        },
        costs,
    );

    let mut offered = 0i32;
    let mut t = 2000;
    while t <= 600_000 {
        *clock.lock().unwrap() = t;
        for _ in 0..3 {
            offered += 1;
            *last_offered.lock().unwrap() = offered;
            gov.offer(GovItem::new(offered).0);
        }
        if t % 30_000 == 0 {
            for _ in 0..6 {
                offered += 1;
                *last_offered.lock().unwrap() = offered;
                gov.offer(GovItem::new(offered).0);
            }
        }
        gov.pump();
        t += 2000;
    }
    assert!(*drops.lock().unwrap() > 0);
    assert!(violations.lock().unwrap().is_empty());
}

#[test]
fn burst_then_catch_up() {
    let mut h = GovHarness::new([200.0, 100.0, 50.0, 25.0], None, 20);
    h.set_now(0);
    for i in 1..=40 {
        h.gov.offer(GovItem::new(i).0);
    }
    let enc0 = h.encodes.lock().unwrap().clone();
    assert!(enc0.iter().any(|e| e.1 == EncodeLevel::Reduced));
    let sum_at_0: f64 = enc0.iter().filter(|e| e.0 == 0).map(|e| e.2).sum();
    assert!(sum_at_0 <= 0.8 * h.gov.limit10_ms());
    assert!(h.gov.waiting() > 0 || h.gov.webp_fallbacks > 0 || h.gov.dropped > 0);

    let mut t = 1000;
    while t <= 120_000 {
        h.set_now(t);
        h.gov.pump();
        t += 1000;
    }
    assert_eq!(0, h.gov.waiting());
    let enc = h.encodes.lock().unwrap().clone();
    assert_within_budget(&enc, 3000.0, 12000.0);
    assert_eq!(40, h.gov.encoded + h.gov.dropped);
}

#[test]
fn stale_frames_go_to_webp() {
    let mut h = GovHarness::new([5000.0, 5000.0, 5000.0, 10.0], None, 20);
    h.set_now(0);
    h.gov.offer(GovItem::new(1).0);
    assert_eq!(1, h.gov.waiting());
    assert_eq!(0, h.gov.encoded);

    h.set_now(EncodeGovernor::<GovItem>::MAX_WAIT_MS + 1000);
    h.gov.pump();
    assert_eq!(0, h.gov.waiting());
    assert_eq!(1, h.gov.encoded);
    assert_eq!(1, h.gov.webp_fallbacks);
    let enc = h.encodes.lock().unwrap().clone();
    assert_eq!(1, enc.len());
    assert_eq!(EncodeLevel::WebP, enc[0].1);
}

#[test]
fn estimates_follow_measured_costs() {
    let mut h = GovHarness::new([100.0; 4], Some([1000.0; 4]), 20);
    h.set_now(0);
    for i in 1..=10 {
        h.set_now(h.now() + 15_000);
        h.gov.offer(GovItem::new(i).0);
    }
    assert_eq!(10, h.gov.encoded);
    assert!(h.gov.estimate(EncodeLevel::Normal) < 200.0);
}

#[test]
fn every_item_is_disposed_exactly_once() {
    let mut h = GovHarness::new([300.0, 150.0, 60.0, 30.0], None, 20);
    let mut counters = Vec::new();
    let mut id = 0;
    let mut t = 2000;
    while t <= 600_000 {
        h.set_now(t);
        for _ in 0..3 {
            id += 1;
            let (item, d) = GovItem::new(id);
            counters.push(d);
            h.gov.offer(item);
        }
        if t % 30_000 == 0 {
            for _ in 0..6 {
                id += 1;
                let (item, d) = GovItem::new(id);
                counters.push(d);
                h.gov.offer(item);
            }
        }
        h.gov.pump();
        t += 2000;
    }
    h.gov.clear();
    assert!(!counters.is_empty());
    assert!(counters.iter().all(|c| c.load(Ordering::Acquire) == 1));
}

// ---------------- SmartCaptureTests: ChangeMapTests + ImageCodecTests ----------------

#[test]
fn identical_frames_score_zero_and_size_change_is_first() {
    let (w, h) = (1280, 720);
    let mut a = screen_flat(w, h, 240);
    screen_text(&mut a, w, 64, 64, 600, 300, 1);
    let ma = ChangeMap::compute(&a, w, h, w * 4).unwrap();
    let r = ChangeMap::compare(Some(&ma), &ma);
    assert!(r.comparable);
    assert_eq!(0.0, r.score);
    assert_eq!(ChangeResult::FIRST, ChangeMap::compare(None, &ma));
    let small = screen_flat(640, 360, 240);
    let ms = ChangeMap::compute(&small, 640, 360, 640 * 4).unwrap();
    assert!(!ChangeMap::compare(Some(&ms), &ma).comparable);
}

#[test]
fn flat_area_change_scores_its_share_of_the_screen() {
    let (w, h) = (1280, 720);
    let a = screen_flat(w, h, 240);
    let mut b = a.clone();
    screen_rect(&mut b, w, 0, 0, 640, 288, (30, 90, 200));
    let ma = ChangeMap::compute(&a, w, h, w * 4).unwrap();
    let mb = ChangeMap::compute(&b, w, h, w * 4).unwrap();
    let r = ChangeMap::compare(Some(&ma), &mb);
    assert!((19.5..=20.5).contains(&r.changed_percent));
    assert_eq!(0.0, r.text_percent);
    assert!((19.5..=20.5).contains(&r.score));
}

#[test]
fn text_like_change_counts_five_times() {
    let (w, h) = (1280, 720);
    let a = screen_flat(w, h, 240);
    let mut b = a.clone();
    screen_text(&mut b, w, 0, 0, 320, 96, 3);
    let ma = ChangeMap::compute(&a, w, h, w * 4).unwrap();
    let mb = ChangeMap::compute(&b, w, h, w * 4).unwrap();
    assert!(mb.text[0]);
    let r = ChangeMap::compare(Some(&ma), &mb);
    assert!((3.0..=3.5).contains(&r.text_percent));
    assert!((15.0..=17.5).contains(&r.score));
}

#[test]
fn flat_and_gradient_tiles_are_not_text() {
    let (w, h) = (1280, 720);
    let mut a = screen_flat(w, h, 240);
    screen_gradient(&mut a, w, 0, 0, 640, 360);
    let m = ChangeMap::compute(&a, w, h, w * 4).unwrap();
    assert!(!m.text.contains(&true));
}

#[test]
fn clear_zeroes_the_map() {
    let a = screen_flat(1280, 720, 240);
    let mut m = ChangeMap::compute(&a, 1280, 720, 1280 * 4).unwrap();
    m.clear();
    assert!(m.means.iter().all(|&b| b == 0));
}

#[test]
fn policy_table() {
    let cases = [
        (true, false, false, true, 0.0, 0, 6, 15.0, KeepReason::Force),
        (false, false, false, false, 100.0, 64, 6, 15.0, KeepReason::First),
        (false, false, true, true, 0.0, 0, 6, 15.0, KeepReason::Switch),
        (false, false, false, true, 14.9, 2, 6, 15.0, KeepReason::Skip),
        (false, false, false, true, 15.0, 2, 6, 15.0, KeepReason::Change),
        (false, false, false, true, 3.0, 7, 6, 15.0, KeepReason::Hash),
        (false, false, false, true, 3.0, 7, 0, 15.0, KeepReason::Skip),
        (false, true, false, true, 0.6, 0, 6, 15.0, KeepReason::Trigger),
        (false, true, false, true, 0.2, 1, 6, 15.0, KeepReason::Skip),
        (false, false, false, true, 0.0, 0, 6, 0.0, KeepReason::Skip),
        (false, false, false, true, 0.1, 0, 6, 0.0, KeepReason::Change),
    ];
    for (force, trigger, ctx, comp, score, hash, thr, dirty_min, expected) in cases {
        let change = ChangeResult::new(score, 0.0, score, comp);
        assert_eq!(
            expected,
            CapturePolicy::decide(force, trigger, ctx, change, hash, thr, dirty_min)
        );
    }
}

#[test]
fn dedup_filter_compares_with_the_last_stored_keyframe() {
    let (w, h) = (1280, 720);
    let mut d = DedupFilter::new();
    let a = screen_flat(w, h, 240);
    let req = KeyframeRequest {
        ts: Utc::now(),
        app: "app".to_string(),
        exe_path: String::new(),
        title: "title".to_string(),
        context: "app\u{0001}title".to_string(),
        force: false,
        max_width: w,
        dirty_min_percent: 0.5,
        site: String::new(),
        keyframe: true,
        replay_sample: false,
        trigger: false,
        secrets: None,
        bounds: Rect::default(),
        mask_only: false,
    };
    let ma = ChangeMap::compute(&a, w, h, w * 4).unwrap();
    let (r1, c1) = d.decide(&ma, 1, &req, 6, 15.0);
    assert_eq!(KeepReason::First, r1);
    assert!(!c1.comparable);
    d.accept(1, req.context.clone(), ma);

    let mut b = a.clone();
    screen_rect(&mut b, w, 0, 0, 1280, 64, (0, 0, 0));
    let mb1 = ChangeMap::compute(&b, w, h, w * 4).unwrap();
    assert_eq!(KeepReason::Skip, d.decide(&mb1, 1, &req, 6, 15.0).0);
    screen_rect(&mut b, w, 0, 64, 1280, 64, (0, 0, 0));
    let mb2 = ChangeMap::compute(&b, w, h, w * 4).unwrap();
    assert_eq!(KeepReason::Change, d.decide(&mb2, 1, &req, 6, 15.0).0);

    let mut req_other = req.clone();
    req_other.context = "other\u{0001}x".to_string();
    let ma2 = ChangeMap::compute(&a, w, h, w * 4).unwrap();
    assert_eq!(KeepReason::Switch, d.decide(&ma2, 1, &req_other, 6, 15.0).0);
    d.reset();
    assert!(d.last_hash().is_none());
    assert_eq!(KeepReason::First, d.decide(&ma2, 1, &req, 6, 15.0).0);
}

#[test]
fn round_trip_keeps_size_and_pixels_close() {
    let (w, h) = (640, 360);
    let px = screen_ui(w, h);
    for (format, mime) in [
        ("avif", "image/avif"),
        ("jxl", "image/jxl"),
        ("webp", "image/webp"),
        ("jpeg", "image/jpeg"),
    ] {
        assert_eq!(format, ImageCodec::resolve(format));
        let enc = unsafe { ImageCodec::encode_bgra(px.as_ptr(), w, h, w * 4, format, 75) }.unwrap();
        assert_eq!(mime, ImageCodec::mime(format));
        let (dec, dw, dh) = ImageCodec::decode_bgra(&enc, mime).unwrap();
        assert_eq!((w, h), (dw, dh));
        let mad = mean_abs_diff(&px, &dec);
        assert!(mad < 12.0, "{format} mean abs diff {mad:.2}");
    }
}

#[test]
fn transcode_makes_avif_and_jxl_readable_and_resize_works() {
    let (w, h) = (640, 360);
    let px = screen_ui(w, h);
    for format in ["avif", "jxl"] {
        let enc = unsafe { ImageCodec::encode_bgra(px.as_ptr(), w, h, w * 4, format, 75) }.unwrap();
        let (bytes, mime, tw, th) =
            ImageCodec::transcode(&enc, ImageCodec::mime(format), "webp", 90).unwrap();
        assert_eq!("image/webp", mime);
        assert_eq!((w, h), (tw, th));
        assert_eq!(b"RIFF", &bytes[..4]);
        let (thumb, rw, rh) =
            ImageCodec::resize_to_webp(&enc, ImageCodec::mime(format), 320, 70).unwrap();
        assert_eq!((320, 180), (rw, rh));
        let (_, dw, dh) = ImageCodec::decode_bgra(&thumb, "image/webp").unwrap();
        assert_eq!((320, 180), (dw, dh));
    }
    assert!(!ImageCodec::commonly_decodable("image/avif"));
    assert!(ImageCodec::commonly_decodable("image/webp"));
    assert_eq!("webp", ImageCodec::resolve("bogus"));
}

#[test]
fn thumbnailer_transcodes_avif_for_the_ui_and_jpeg_for_export() {
    let store = FakeStore::new();
    let (w, h) = (640, 360);
    let px = screen_ui(w, h);
    let avif = unsafe { ImageCodec::encode_bgra(px.as_ptr(), w, h, w * 4, "avif", 75) }.unwrap();
    let webp = unsafe { ImageCodec::encode_bgra(px.as_ptr(), w, h, w * 4, "webp", 75) }.unwrap();
    let s_for_t = store.clone();
    let thumbs = AgentThumbnailer::new(
        Arc::new(move || Some(s_for_t.clone() as Arc<dyn KeyframeBlobStore>)),
        Arc::new(Settings::default),
    );
    let a = ThumbData {
        id: "a".to_string(),
        mime: "image/avif".to_string(),
        width: w,
        height: h,
        bytes: avif,
    };
    let ui = thumbs.transcode(a, "ui");
    assert_eq!("image/webp", ui.mime);

    let plain = ThumbData {
        id: "b".to_string(),
        mime: "image/webp".to_string(),
        width: w,
        height: h,
        bytes: webp.clone(),
    };
    let ui_plain = thumbs.transcode(plain.clone(), "ui");
    assert_eq!(webp, ui_plain.bytes);
    let jpeg = thumbs.transcode(plain, "jpeg");
    assert_eq!("image/jpeg", jpeg.mime);
    assert_eq!(0xFF, jpeg.bytes[0]);
}

#[test]
#[ignore = "CPU heavy, run on request"]
fn spike_on_real_screenshots() {
    // Manual benchmark driven by BR_CODEC_SPIKE; kept ignored in automated runs.
}

#[test]
#[ignore = "CPU heavy, run on request"]
fn keyframe_cost_spike() {
    // Manual benchmark driven by BR_KF_SPIKE; kept ignored in automated runs.
}

// ---------------- AvifWriterTests ----------------

fn mdat_payload(file: &[u8]) -> &[u8] {
    let mut pos = 0usize;
    while pos + 8 <= file.len() {
        let len = u32::from_be_bytes(file[pos..pos + 4].try_into().unwrap()) as usize;
        if &file[pos + 4..pos + 8] == b"mdat" {
            return &file[pos + 8..pos + len];
        }
        pos += len;
    }
    panic!("no mdat");
}

fn has_box(file: &[u8], tag: &[u8]) -> bool {
    file.windows(tag.len()).any(|w| w == tag)
}

#[test]
fn writer_wraps_av1_obus_into_a_decodable_avif() {
    if !Avif::try_load() {
        return;
    }
    let (w, h) = (640, 360);
    let px = screen_ui(w, h);
    let reference =
        unsafe { ImageCodec::encode_bgra(px.as_ptr(), w, h, w * 4, "avif", 75) }.unwrap();
    let mut tu = Vec::new();
    tu.extend_from_slice(&[0x12, 0x00]); // temporal delimiter
    tu.extend_from_slice(mdat_payload(&reference));
    tu.extend_from_slice(&[0x7A, 0x02, 0xAA, 0xBB]); // padding

    let avif = AvifWriter::write(&tu, w, h).expect("AvifWriter::write");
    assert_eq!(b"ftyp", &avif[4..8]);
    for tag in [
        b"avif".as_slice(),
        b"hdlr",
        b"pict",
        b"pitm",
        b"iloc",
        b"iinf",
        b"infe",
        b"av01",
        b"ispe",
        b"av1C",
        b"pixi",
        b"colr",
        b"nclx",
        b"ipma",
        b"mdat",
    ] {
        assert!(has_box(&avif, tag), "missing box {}", String::from_utf8_lossy(tag));
    }
    assert!(!has_box(&avif, b"clap"));
    let mut expected = vec![0u8; tu.len()];
    let n = Av1::strip_for_mp4(&tu, &mut expected).unwrap();
    assert_eq!(&expected[..n], mdat_payload(&avif));
    assert!(
        !Av1::split(mdat_payload(&avif))
            .unwrap()
            .iter()
            .any(|o| matches!(o.obu_type, 2 | 15))
    );

    let (dec, dw, dh) = ImageCodec::decode_bgra(&avif, "image/avif").unwrap();
    assert_eq!((w, h), (dw, dh));
    let mad = mean_abs_diff(&px, &dec);
    assert!(mad < 12.0, "mad {mad:.2}");
}

#[test]
fn writer_rejects_units_without_a_sequence_header_or_larger_pictures() {
    assert!(AvifWriter::write(&[0x12, 0x00], 64, 64).is_none());
    if !Avif::try_load() {
        return;
    }
    let px = screen_ui(128, 128);
    let reference =
        unsafe { ImageCodec::encode_bgra(px.as_ptr(), 128, 128, 512, "avif", 75) }.unwrap();
    assert!(AvifWriter::write(mdat_payload(&reference), 130, 128).is_none());
    let cropped = AvifWriter::write(mdat_payload(&reference), 126, 120).expect("cropped");
    assert!(has_box(&cropped, b"clap"));
}

// ---------------- ReliabilitySweep2Tests (A8, A12, A19) ----------------

#[test]
fn sample_grab_does_not_reset_the_keyframe_gate() {
    let mut kf = ChangeGate::new();
    let mut sample = ChangeGate::new();
    kf.reset();
    sample.reset();
    kf.frame(20.0);
    sample.frame(20.0);
    assert!(sample.changed(0.5));
    sample.reset();
    assert!(kf.changed(0.5));
    assert!(!sample.changed(0.5));
    kf.reset();
    assert!(!kf.changed(0.5));
    kf.invalidate();
    assert!(kf.changed(100.0));
}

fn out_info(index: i32, name: &str) -> OutputInfo {
    OutputInfo {
        index,
        device_name: name.to_string(),
        adapter_luid: 1,
        vendor_id: 0x8086,
        adapter_name: "gpu".to_string(),
        left: index * 1920,
        top: 0,
        width: 1920,
        height: 1080,
        monitor: 0,
        refresh_hz: 60,
    }
}

#[test]
fn monitor_selection_follows_monitor_ids_across_a_hot_plug() {
    let before = [out_info(0, "A"), out_info(1, "B"), out_info(2, "C")];
    let (sel, pin) = MonitorSelection::resolve(&[0, 1], &before, |o| o.device_name.clone(), None);
    let names: Vec<&str> = sel.iter().map(|o| o.device_name.as_str()).collect();
    assert_eq!(vec!["A", "B"], names);
    let pin = pin.expect("pin");
    assert_eq!(vec!["A", "B"], pin.ids);

    let after = [out_info(0, "B"), out_info(1, "C")];
    let (sel2, pin2) =
        MonitorSelection::resolve(&[0, 1], &after, |o| o.device_name.clone(), Some(&pin));
    assert_eq!(
        vec!["B"],
        sel2.iter().map(|o| o.device_name.as_str()).collect::<Vec<_>>()
    );
    assert!(pin2.is_none());

    let (sel3, pin3) =
        MonitorSelection::resolve(&[1], &after, |o| o.device_name.clone(), Some(&pin));
    assert_eq!(
        vec!["C"],
        sel3.iter().map(|o| o.device_name.as_str()).collect::<Vec<_>>()
    );
    assert_eq!(vec!["C"], pin3.unwrap().ids);
}

#[test]
fn monitor_selection_empty_means_all_and_old_values_read_as_indices() {
    let outs = [out_info(0, "A"), out_info(1, "B")];
    let existing = MonitorPin {
        indices: vec![0],
        ids: vec!["A".to_string()],
    };
    let (all, pin) =
        MonitorSelection::resolve(&[], &outs, |o| o.device_name.clone(), Some(&existing));
    assert_eq!(2, all.len());
    assert!(pin.is_none());

    let (sel, pin2) = MonitorSelection::resolve(&[1], &outs, |o| o.device_name.clone(), None);
    assert_eq!(
        vec!["B"],
        sel.iter().map(|o| o.device_name.as_str()).collect::<Vec<_>>()
    );
    assert_eq!(vec![1], pin2.unwrap().indices);
}

#[test]
fn monitor_pin_round_trips_through_its_file() {
    let dir = TempDir::new("monpin");
    let path = dir.path().join(MonitorSelection::FILE_NAME);
    assert!(MonitorSelection::load(Some(&path)).is_none());
    let pin = MonitorPin {
        indices: vec![0, 2],
        ids: vec![
            r"\\?\DISPLAY#DEL41A8#5&1&0&UID4352#{e6f07b5f}".to_string(),
            r"\\.\DISPLAY3".to_string(),
        ],
    };
    MonitorSelection::save(Some(&path), &pin);
    let back = MonitorSelection::load(Some(&path)).expect("loaded pin");
    assert_eq!(vec![0, 2], back.indices);
    assert_eq!(r"\\.\DISPLAY3", back.ids[1]);
}

#[test]
fn sample_encodes_are_deferred_when_the_budget_is_spent() {
    let clock = Arc::new(Mutex::new(0i64));
    let c1 = clock.clone();
    let mut g = EncodeGovernor::<()>::new(
        move || *c1.lock().unwrap(),
        4,
        |_, _| 10.0,
        |_, _| {},
        [100.0, 60.0, 30.0, 200.0],
    );
    assert!(g.try_admit_sample());
    g.charge_sample(200.0);
    *clock.lock().unwrap() += 100;
    assert!(g.try_admit_sample());
    g.charge_sample(200.0);
    *clock.lock().unwrap() += 100;
    assert!(!g.try_admit_sample());
    assert_eq!(1, g.samples_deferred);
    *clock.lock().unwrap() += 10_000;
    assert!(g.try_admit_sample());
}

#[test]
fn sample_waits_while_a_keyframe_waits_for_budget() {
    let mut g = EncodeGovernor::<()>::new(|| 0, 4, |_, _| 10.0, |_, _| {}, [500.0, 500.0, 500.0, 5.0]);
    g.offer(());
    assert_eq!(1, g.waiting());
    assert!(!g.try_admit_sample());
}

// ---------------- ServiceTests: ThumbnailCacheTests ----------------

fn thumb_of(id: &str, size: usize) -> ThumbData {
    ThumbData {
        id: id.to_string(),
        mime: "image/webp".to_string(),
        width: 1,
        height: 1,
        bytes: vec![1u8; size],
    }
}

#[test]
fn lru_is_bounded_by_bytes_and_evicts_least_recent() {
    let c = ByteLruCache::<String>::new(1000);
    c.put("a".to_string(), thumb_of("a", 400));
    c.put("b".to_string(), thumb_of("b", 400));
    assert!(c.try_get(&"a".to_string()).is_some());
    c.put("c".to_string(), thumb_of("c", 400));
    assert!(c.try_get(&"b".to_string()).is_none());
    assert!(c.try_get(&"a".to_string()).is_some());
    assert!(c.try_get(&"c".to_string()).is_some());
    assert!(c.bytes() <= 1000);
    c.put("huge".to_string(), thumb_of("huge", 5000));
    assert!(c.try_get(&"huge".to_string()).is_none());
    c.resize(500);
    assert_eq!(1, c.count());
    assert!(c.bytes() <= 500);
}

#[test]
fn clear_zeroes_cached_bytes() {
    let c = ByteLruCache::<String>::new(1000);
    c.put(
        "a".to_string(),
        ThumbData {
            id: "a".to_string(),
            mime: "image/webp".to_string(),
            width: 1,
            height: 1,
            bytes: vec![1, 2, 3],
        },
    );
    c.clear();
    assert_eq!(0, c.count());
    assert_eq!(0, c.bytes());
}

#[test]
fn thumbnailer_resizes_to_webp_and_caches_in_ram() {
    let store = FakeStore::new();
    let mut settings = Settings::default();
    settings.keyframes.thumb_cache_mb = 8;
    let mut px = vec![0u8; 640 * 360 * 4];
    for (idx, chunk) in px.chunks_exact_mut(4).enumerate() {
        chunk[0] = ((idx * 4) % 251) as u8;
        chunk[1] = 80;
        chunk[2] = 160;
        chunk[3] = 255;
    }
    let webp =
        unsafe { ImageCodec::encode_bgra(px.as_ptr(), 640, 360, 640 * 4, "webp", 75) }.unwrap();
    let id = store.add_screen(NewScreenCapture {
        ts: Utc::now(),
        app: "app".to_string(),
        exe_path: String::new(),
        title: "t".to_string(),
        monitor: 0,
        width: 640,
        height: 360,
        d_hash: 0,
        image: webp,
        image_mime: "image/webp".to_string(),
        keep_keyframe: true,
        site: String::new(),
        change: None,
        switch: false,
        text: None,
    });
    let s_for_t = store.clone();
    let thumbs = AgentThumbnailer::new(
        Arc::new(move || Some(s_for_t.clone() as Arc<dyn KeyframeBlobStore>)),
        Arc::new(move || settings.clone()),
    );
    let t = thumbs.get_thumb(&id, 320, &CancelToken::none()).expect("thumb");
    assert_eq!(320, t.width);
    assert_eq!(180, t.height);
    assert_eq!(b"RIFF", &t.bytes[..4]);
    let t2 = thumbs.get_thumb(&id, 320, &CancelToken::none()).expect("cached");
    assert_eq!(t.bytes, t2.bytes);
    assert_eq!(1, thumbs.cache().count());
    assert!(thumbs.get_thumb("missing", 320, &CancelToken::none()).is_none());
    thumbs.clear_cache();
    assert_eq!(0, thumbs.cache().count());
}

// ---------------- Sweep3KTests ----------------

#[test]
fn one_slow_encode_does_not_disable_a_level_for_good() {
    let clock = Arc::new(Mutex::new(0i64));
    let c1 = clock.clone();
    let costs = Arc::new(Mutex::new(VecDeque::from([1500.0])));
    let cs = costs.clone();
    let mut g = EncodeGovernor::<()>::new(
        move || *c1.lock().unwrap(),
        4,
        move |_, _| cs.lock().unwrap().pop_front().unwrap_or(130.0),
        |_, _| {},
        KeyframeEncoder::INITIAL_ESTIMATE_MS,
    );
    g.offer(());
    assert!(g.estimate(EncodeLevel::Normal) > 0.8 * g.limit10_ms());
    let mut levels = Vec::new();
    for _ in 0..5 {
        *clock.lock().unwrap() += 120_000;
        g.offer(());
        levels.push(g.level());
    }
    assert!(levels.contains(&EncodeLevel::Normal));
    assert_eq!(Some(&EncodeLevel::Normal), levels.last());
}

#[test]
fn relax_only_lowers_estimates_of_unused_levels_toward_their_initial_value() {
    let clock = Arc::new(Mutex::new(0i64));
    let c1 = clock.clone();
    let mut g = EncodeGovernor::<()>::new(
        move || *c1.lock().unwrap(),
        20,
        |_, _| 50.0,
        |_, _| {},
        [130.0, 65.0, 15.0, 35.0],
    );
    g.offer(());
    let learned = g.estimate(EncodeLevel::Normal);
    assert!(learned < 130.0);
    *clock.lock().unwrap() += 10 * EncodeGovernor::<()>::RELAX_AFTER_MS;
    let _ = g.choose();
    assert_eq!(learned, g.estimate(EncodeLevel::Normal));
}

#[test]
#[ignore = "CPU heavy, run on request"]
fn jxl_encode_cpu_is_on_the_calling_thread() {
    if !Jxl::try_load() {
        return;
    }
    let (w, h) = (1280, 720);
    let mut px = screen_flat(w, h, 128);
    screen_text(&mut px, w, 20, 20, 1200, 680, 1);
    let _ = unsafe { Jxl::encode(px.as_ptr(), w, h, w * 4, 75, 3) }.unwrap();
    let t0 = current_thread_cpu_ms();
    for _ in 0..5 {
        let _ = unsafe { Jxl::encode(px.as_ptr(), w, h, w * 4, 75, 3) }.unwrap();
    }
    let thread_ms = current_thread_cpu_ms() - t0;
    assert!(thread_ms > 0.0);
}

#[test]
fn same_sizes_and_rotation_reuse_the_frames() {
    assert!(OutputCapture::same_frames(
        true,
        (1520, 823, 1520, 823, 0),
        (1520, 823, 1520, 823, 0)
    ));
    assert!(!OutputCapture::same_frames(
        false,
        (1520, 823, 1520, 823, 0),
        (1520, 823, 1520, 823, 0)
    ));
    assert!(!OutputCapture::same_frames(
        true,
        (1520, 823, 1520, 823, 0),
        (1920, 1080, 1920, 1080, 0)
    ));
    assert!(!OutputCapture::same_frames(
        true,
        (1080, 1920, 1920, 1080, 90),
        (1080, 1920, 1920, 1080, 0)
    ));
}
