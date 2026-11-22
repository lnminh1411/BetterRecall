// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Per-canvas / per-monitor replay pipeline: dirty-rect compositing, masks, cursor overlay,
//! encoder submission, and watchdog (`Replay\ReplayPipeline.cs`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CPU_ACCESS_WRITE, D3D11_MAP_READ, D3D11_MAP_READ_WRITE, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Resource,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::core::Interface;

use crate::capture::gpu_devices::{GpuDevice, GpuDeviceCache};
use crate::capture::output_capture::{DirtyMapper, FramePacer, OutputCapture};
use crate::capture::video_scaler::{VideoScaler, VideoScalerParams};
use crate::native::{Rect, qpc, tick_count64};
use crate::replay::canvas_blit::{CanvasBlit, Nv12Mut, Nv12Ref};
use crate::replay::cursor_overlay::{
    CursorDrawParams, CursorPainter, CursorProbe, CursorShape, CursorState,
};
use crate::replay::encoder_watchdog::EncoderWatchdog;
use crate::replay::replay_layout::{CanvasLayout, MonitorPlacement, dirty_to_dest};
use crate::replay::video_encoders::{
    EncoderCandidate, EncoderCreateParams, EncoderFactory, VideoEncoder,
};
use crate::replay::video_stream::VideoStream;

#[derive(Clone)]
pub struct PipelineSpec {
    pub key: String,
    pub sources: Vec<Arc<OutputCapture>>,
    pub candidate: EncoderCandidate,
    pub fps: i32,
    pub kbps: i32,
    pub resolution: String,
    pub scale_percent: i32,
}

struct Slot {
    source: Arc<OutputCapture>,
    dest: Rect,
    scaler: VideoScaler,
    stagings: [ID3D11Texture2D; 2],
    generation: i32,
    source_width: i32,
    source_height: i32,
    pending: i32,
    pending_rect: Rect,
    next_staging: usize,
    masks: Vec<Rect>,
    force_full: bool,
}

pub struct ReplayPipeline {
    pub spec: PipelineSpec,
    pub stream: Arc<VideoStream>,
    pub encoder: Box<dyn VideoEncoder>,
    pub layout: CanvasLayout,
    pub pacer: FramePacer,
    enc_dev: Arc<GpuDevice>,
    mirror: ID3D11Texture2D,
    pool: Option<[ID3D11Texture2D; 4]>,
    slots: Vec<Slot>,
    pool_index: isize,
    canvas_changed: bool,
    last_submit_pts: i64,
    last_forced_pts: i64,
    painter: CursorPainter,
    cursor: CursorState,
    needs_rebuild: bool,
    encoder_failure: Option<String>,
    watchdog: EncoderWatchdog,
    pub synthetic_motion: AtomicBool,
    pub motion_ticks: AtomicI64,
    pub motion_ticks_at_warm: AtomicI64,
    motion_frame: i64,
    pub copy_ticks: i64,
    pub copy_count: i64,
    pub submit_count: i64,
    pub repeat_count: i64,
    pub busy_count: i64,
}

unsafe impl Send for ReplayPipeline {}

impl ReplayPipeline {
    pub fn create(
        spec: PipelineSpec,
        stream: Arc<VideoStream>,
        devices: &GpuDeviceCache,
    ) -> Result<Self, String> {
        for s in &spec.sources {
            if s.source_width() <= 0 || s.device().is_none() {
                return Err("source not ready".to_string());
            }
        }
        let placements: Vec<MonitorPlacement> = spec
            .sources
            .iter()
            .map(|o| MonitorPlacement {
                index: o.info().index,
                left: o.info().left,
                top: o.info().top,
                width: o.source_width(),
                height: o.source_height(),
            })
            .collect();

        let enc_dev = devices.get(spec.candidate.adapter.luid)?;
        let mut chosen: Option<(Box<dyn VideoEncoder>, CanvasLayout)> = None;
        let mut prev_w = -1;
        let mut prev_h = -1;
        let mut last_err = "encoder creation failed".to_string();

        for &(max_w, max_h) in EncoderFactory::SIZE_LIMITS {
            let candidate_layout = CanvasLayout::compute(
                &placements,
                Some(&spec.resolution),
                spec.scale_percent,
                max_w,
                max_h,
            )?;
            if candidate_layout.width == prev_w && candidate_layout.height == prev_h {
                continue;
            }
            prev_w = candidate_layout.width;
            prev_h = candidate_layout.height;

            let session = stream.begin_session(
                spec.candidate.codec,
                candidate_layout.width,
                candidate_layout.height,
                spec.fps,
            );
            let stream_cb = stream.clone();
            match EncoderFactory::create(
                &spec.candidate,
                enc_dev.clone(),
                EncoderCreateParams {
                    width: candidate_layout.width,
                    height: candidate_layout.height,
                    fps: spec.fps,
                    kbps: spec.kbps,
                },
                Arc::new(move |d, p, k| stream_cb.on_packet(session, d, p, k)),
            ) {
                Ok(enc) => {
                    chosen = Some((enc, candidate_layout));
                    break;
                }
                Err(e) => last_err = e,
            }
        }

        let Some((encoder, layout)) = chosen else {
            return Err(last_err);
        };

        let surf_w = encoder.surface_width();
        let surf_h = encoder.surface_height();

        let (mirror, pool) = {
            let _g = enc_dev.gate.lock().unwrap_or_else(|e| e.into_inner());
            let mirror_desc = D3D11_TEXTURE2D_DESC {
                Width: surf_w as u32,
                Height: surf_h as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32,
                MiscFlags: 0,
            };
            let mut mirror = None;
            unsafe {
                enc_dev
                    .device
                    .CreateTexture2D(&mirror_desc, None, Some(&mut mirror))
            }
            .map_err(|e| format!("CreateTexture2D mirror: {e}"))?;
            let mirror = mirror.ok_or_else(|| "mirror null".to_string())?;
            let mirror_res: ID3D11Resource = mirror.cast().map_err(|e| e.to_string())?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                enc_dev
                    .context
                    .Map(&mirror_res, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut mapped))
            }
            .map_err(|e| format!("Map mirror: {e}"))?;
            unsafe {
                CanvasBlit::clear(
                    Nv12Mut {
                        ptr: mapped.pData.cast::<u8>(),
                        pitch: mapped.RowPitch as usize,
                        plane_height: surf_h as usize,
                    },
                    surf_w as usize,
                );
                enc_dev.context.Unmap(&mirror_res, 0);
            }

            let pool = if !encoder.copies_input() {
                let pool_desc = D3D11_TEXTURE2D_DESC {
                    Width: surf_w as u32,
                    Height: surf_h as u32,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut arr = Vec::with_capacity(4);
                for _ in 0..4 {
                    let mut tex = None;
                    unsafe {
                        enc_dev
                            .device
                            .CreateTexture2D(&pool_desc, None, Some(&mut tex))
                    }
                    .map_err(|e| format!("CreateTexture2D pool: {e}"))?;
                    arr.push(tex.ok_or_else(|| "pool tex null".to_string())?);
                }
                Some(arr.try_into().map_err(|_| "pool len".to_string())?)
            } else {
                None
            };
            (mirror, pool)
        };

        let mut slots = Vec::with_capacity(spec.sources.len());
        for src in &spec.sources {
            let dest_dest = layout
                .for_monitor(src.info().index)
                .ok_or_else(|| format!("No destination for monitor {}", src.info().index))?;
            let dest_rect = dest_dest.rect;
            let dest_w = dest_rect.width();
            let dest_h = dest_rect.height();
            let src_dev = src
                .device()
                .ok_or_else(|| "source device missing".to_string())?;
            let (scaler, stagings) = {
                let _g = src_dev.gate.lock().unwrap_or_else(|e| e.into_inner());
                let scaler = VideoScaler::new(
                    src_dev.clone(),
                    VideoScalerParams {
                        src_w: src.source_width(),
                        src_h: src.source_height(),
                        dst_w: dest_w,
                        dst_h: dest_h,
                        dst_format: DXGI_FORMAT_NV12,
                        with_staging: false,
                        rotation_degrees: 0,
                    },
                )
                .map_err(|e| e.to_string())?;
                let st_desc = D3D11_TEXTURE2D_DESC {
                    Width: dest_w as u32,
                    Height: dest_h as u32,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut st0 = None;
                let mut st1 = None;
                unsafe {
                    src_dev
                        .device
                        .CreateTexture2D(&st_desc, None, Some(&mut st0))
                }
                .map_err(|e| format!("CreateTexture2D staging0: {e}"))?;
                unsafe {
                    src_dev
                        .device
                        .CreateTexture2D(&st_desc, None, Some(&mut st1))
                }
                .map_err(|e| format!("CreateTexture2D staging1: {e}"))?;
                (
                    scaler,
                    [
                        st0.ok_or_else(|| "st0 null".to_string())?,
                        st1.ok_or_else(|| "st1 null".to_string())?,
                    ],
                )
            };
            slots.push(Slot {
                source: src.clone(),
                dest: dest_rect,
                scaler,
                stagings,
                generation: src.feed_generation(),
                source_width: src.source_width(),
                source_height: src.source_height(),
                pending: -1,
                pending_rect: Rect::default(),
                next_staging: 0,
                masks: Vec::new(),
                force_full: true,
            });
        }

        let mut freq = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut freq);
        }
        let pacer = FramePacer::new(spec.fps, freq.max(1));

        Ok(Self {
            spec,
            stream,
            encoder,
            layout,
            pacer,
            enc_dev,
            mirror,
            pool,
            slots,
            pool_index: -1,
            canvas_changed: true,
            last_submit_pts: i64::MIN / 2,
            last_forced_pts: i64::MIN / 2,
            painter: CursorPainter::new(),
            cursor: CursorState::HIDDEN,
            needs_rebuild: false,
            encoder_failure: None,
            watchdog: EncoderWatchdog::new(tick_count64(), EncoderWatchdog::DEFAULT_STALL_MS),
            synthetic_motion: AtomicBool::new(false),
            motion_ticks: AtomicI64::new(0),
            motion_ticks_at_warm: AtomicI64::new(0),
            motion_frame: 0,
            copy_ticks: 0,
            copy_count: 0,
            submit_count: 0,
            repeat_count: 0,
            busy_count: 0,
        })
    }

    pub fn needs_rebuild(&self) -> bool {
        self.needs_rebuild
    }

    pub fn encoder_failure(&self) -> Option<&str> {
        self.encoder_failure.as_deref()
    }

    pub fn encoder_device(&self) -> &Arc<GpuDevice> {
        &self.enc_dev
    }

    pub fn tick(&mut self, pts_100ns: i64, desktop_masks: &[Rect], force_all: bool) {
        for slot in &self.slots {
            if slot.source.disposed()
                || slot.source.feed_generation() != slot.generation
                || slot.source.source_width() != slot.source_width
                || slot.source.source_height() != slot.source_height
            {
                self.needs_rebuild = true;
                return;
            }
        }

        let now_ms = tick_count64();
        let enc_err = self.encoder.error();
        if let Some(failure) =
            self.watchdog
                .check(now_ms, self.encoder.packets_out(), enc_err.as_deref())
        {
            self.encoder_failure = Some(failure);
            self.needs_rebuild = true;
            return;
        }

        let t0 = qpc();
        let mut copied_anything = false;
        let mut canvas_changed = false;

        let mut mirror_ptr: *mut u8 = std::ptr::null_mut();
        let mut mirror_pitch = 0usize;
        let mut mirror_mapped = false;
        let surf_h = self.encoder.surface_height() as usize;

        let mirror_res: Option<ID3D11Resource> = self.mirror.cast().ok();
        let ensure_mirror_mapped = |mapped_flag: &mut bool,
                                    ptr: &mut *mut u8,
                                    pitch: &mut usize| {
            if !*mapped_flag
                && let Some(ref res) = mirror_res
            {
                let _g = self.enc_dev.gate.lock().unwrap_or_else(|e| e.into_inner());
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                if unsafe {
                    self.enc_dev
                        .context
                        .Map(res, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut m))
                }
                .is_ok()
                {
                    *ptr = m.pData.cast::<u8>();
                    *pitch = m.RowPitch as usize;
                    *mapped_flag = true;
                }
            }
        };

        let cursor = CursorProbe::current();
        let cursor_moved = cursor != self.cursor;
        self.cursor = cursor;
        let any_pending = self.slots.iter().any(|s| s.pending >= 0);
        let mut restored = false;
        if self.painter.has_patch() && (cursor_moved || any_pending) {
            ensure_mirror_mapped(&mut mirror_mapped, &mut mirror_ptr, &mut mirror_pitch);
            if mirror_mapped {
                unsafe {
                    self.painter.restore(Nv12Mut {
                        ptr: mirror_ptr,
                        pitch: mirror_pitch,
                        plane_height: surf_h,
                    });
                }
                restored = true;
                canvas_changed = true;
            }
        }

        let mut slot_written = vec![false; self.slots.len()];
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.pending >= 0
                && let Some(src_dev) = slot.source.device()
            {
                let dest_h = slot.dest.height() as usize;
                let _g = src_dev.gate.lock().unwrap_or_else(|e| e.into_inner());
                let staging = &slot.stagings[slot.pending as usize];
                if let Ok(st_res) = staging.cast::<ID3D11Resource>() {
                    let mut sm = D3D11_MAPPED_SUBRESOURCE::default();
                    if unsafe {
                        src_dev
                            .context
                            .Map(&st_res, 0, D3D11_MAP_READ, 0, Some(&mut sm))
                    }
                    .is_ok()
                    {
                        ensure_mirror_mapped(
                            &mut mirror_mapped,
                            &mut mirror_ptr,
                            &mut mirror_pitch,
                        );
                        if mirror_mapped {
                            unsafe {
                                CanvasBlit::copy_rect(
                                    Nv12Ref {
                                        ptr: sm.pData.cast::<u8>(),
                                        pitch: sm.RowPitch as usize,
                                        plane_height: dest_h,
                                    },
                                    Nv12Mut {
                                        ptr: mirror_ptr,
                                        pitch: mirror_pitch,
                                        plane_height: surf_h,
                                    },
                                    slot.pending_rect,
                                    slot.dest.left,
                                    slot.dest.top,
                                );
                            }
                        }
                        unsafe {
                            src_dev.context.Unmap(&st_res, 0);
                        }
                    }
                }
                slot.pending = -1;
                slot_written[i] = true;
                canvas_changed = true;
                copied_anything = true;
            }
        }

        for (i, slot) in self.slots.iter_mut().enumerate() {
            let src_left = slot.source.info().left;
            let src_top = slot.source.info().top;
            let src_w = slot.source_width;
            let src_h = slot.source_height;
            let dest_w = slot.dest.width();
            let dest_h = slot.dest.height();

            let mut new_masks = Vec::new();
            for m in desktop_masks {
                let l = m.left.max(src_left);
                let t = m.top.max(src_top);
                let r = m.right.min(src_left + src_w);
                let b = m.bottom.min(src_top + src_h);
                if r > l && b > t {
                    let src_local = Rect::new(l - src_left, t - src_top, r - src_left, b - src_top);
                    let dest_local = dirty_to_dest(src_local, src_w, src_h, dest_w, dest_h, 0);
                    new_masks.push(Rect::new(
                        dest_local.left + slot.dest.left,
                        dest_local.top + slot.dest.top,
                        dest_local.right + slot.dest.left,
                        dest_local.bottom + slot.dest.top,
                    ));
                }
            }
            let mask_changed = slot.masks != new_masks;
            if mask_changed {
                slot.masks = new_masks;
                slot.force_full = true;
            }
            if (slot_written[i] || mask_changed) && !slot.masks.is_empty() {
                ensure_mirror_mapped(&mut mirror_mapped, &mut mirror_ptr, &mut mirror_pitch);
                if mirror_mapped {
                    for &mr in &slot.masks {
                        unsafe {
                            CanvasBlit::fill_black(
                                Nv12Mut {
                                    ptr: mirror_ptr,
                                    pitch: mirror_pitch,
                                    plane_height: surf_h,
                                },
                                mr,
                            );
                        }
                    }
                    canvas_changed = true;
                }
            }
        }

        if cursor.visible
            && !self.painter.has_patch()
            && (cursor_moved || restored)
            && let Some((shape, params)) = find_cursor_slot(&self.slots, cursor)
        {
            ensure_mirror_mapped(&mut mirror_mapped, &mut mirror_ptr, &mut mirror_pitch);
            if mirror_mapped {
                unsafe {
                    self.painter.draw(
                        Nv12Mut {
                            ptr: mirror_ptr,
                            pitch: mirror_pitch,
                            plane_height: surf_h,
                        },
                        &shape,
                        params,
                    );
                }
                canvas_changed = true;
            }
        }

        if self.synthetic_motion.load(Ordering::Acquire) {
            let m0 = qpc();
            ensure_mirror_mapped(&mut mirror_mapped, &mut mirror_ptr, &mut mirror_pitch);
            if mirror_mapped {
                self.motion_frame += 1;
                unsafe {
                    CanvasBlit::paint_motion(
                        Nv12Mut {
                            ptr: mirror_ptr,
                            pitch: mirror_pitch,
                            plane_height: surf_h,
                        },
                        self.layout.width as usize,
                        self.layout.height as usize,
                        self.motion_frame,
                    );
                }
            }
            self.motion_ticks.fetch_add(qpc() - m0, Ordering::Relaxed);
            canvas_changed = true;
        }

        for slot in &mut self.slots {
            let Some(src_dev) = slot.source.device() else {
                continue;
            };
            let dest_w = slot.dest.width();
            let dest_h = slot.dest.height();
            let src_w = slot.source_width;
            let src_h = slot.source_height;
            let _g = src_dev.gate.lock().unwrap_or_else(|e| e.into_inner());
            let mut dirty = slot.source.take_feed_dirty();
            if slot.force_full || force_all {
                dirty = Rect::new(0, 0, src_w, src_h);
            }
            slot.force_full = false;
            if !DirtyMapper::is_empty(dirty)
                && let Some(latest) = slot.source.latest()
            {
                let rect = dirty_to_dest(dirty, src_w, src_h, dest_w, dest_h, 2);
                if slot.scaler.run(&latest, false).is_ok() {
                    slot.next_staging ^= 1;
                    let k = slot.next_staging;
                    if let (Ok(st_res), Ok(out_res)) = (
                        slot.stagings[k].cast::<ID3D11Resource>(),
                        slot.scaler.output.cast::<ID3D11Resource>(),
                    ) {
                        unsafe {
                            src_dev.context.CopyResource(&st_res, &out_res);
                        }
                        slot.pending = k as i32;
                        slot.pending_rect = rect;
                        copied_anything = true;
                    }
                }
            }
        }

        if copied_anything {
            self.copy_ticks += qpc() - t0;
            self.copy_count += 1;
        }

        if mirror_mapped
            && let Some(ref res) = mirror_res
        {
            let _g = self.enc_dev.gate.lock().unwrap_or_else(|e| e.into_inner());
            unsafe {
                self.enc_dev.context.Unmap(res, 0);
            }
        }

        self.canvas_changed |= canvas_changed;

        let half_frame = 5_000_000 / self.spec.fps.max(1) as i64;
        let new_picture = self.canvas_changed || force_all;
        if !new_picture && pts_100ns - self.last_submit_pts < 10_000_000 - half_frame {
            self.repeat_count += 1;
            return;
        }

        self.watchdog.attempt(now_ms);
        if !self.encoder.ready_for_input() {
            self.busy_count += 1;
            return;
        }

        let force_key = self.submit_count == 0
            || pts_100ns - self.stream.last_key_pts().max(self.last_forced_pts)
                >= 10_000_000 - half_frame;

        let submitted = if self.encoder.copies_input() {
            self.encoder.try_submit(&self.mirror, pts_100ns, force_key)
        } else {
            let next_pool = ((self.pool_index + 1) % 4) as usize;
            let tex = &self.pool.as_ref().unwrap()[next_pool];
            {
                let _g = self.enc_dev.gate.lock().unwrap_or_else(|e| e.into_inner());
                if let (Ok(dst_res), Ok(src_res)) =
                    (tex.cast::<ID3D11Resource>(), self.mirror.cast::<ID3D11Resource>())
                {
                    unsafe {
                        self.enc_dev.context.CopyResource(&dst_res, &src_res);
                    }
                }
            }
            self.pool_index = next_pool as isize;
            self.encoder.try_submit(tex, pts_100ns, force_key)
        };

        if submitted {
            self.canvas_changed = false;
            self.last_submit_pts = pts_100ns;
            if force_key {
                self.last_forced_pts = pts_100ns;
            }
            self.submit_count += 1;
        }
    }
}

impl Drop for ReplayPipeline {
    fn drop(&mut self) {
        self.encoder.dispose();
    }
}

fn find_cursor_slot(
    slots: &[Slot],
    cursor: CursorState,
) -> Option<(Arc<CursorShape>, CursorDrawParams)> {
    for slot in slots {
        let info = slot.source.info();
        let lx = cursor.x - info.left;
        let ly = cursor.y - info.top;
        if lx < 0 || ly < 0 || lx >= slot.source_width || ly >= slot.source_height {
            continue;
        }
        let shape = CursorProbe::shape(cursor.handle)?;
        let sx = slot.dest.width() as f64 / slot.source_width as f64;
        let sy = slot.dest.height() as f64 / slot.source_height as f64;
        return Some((
            shape,
            CursorDrawParams {
                cx: slot.dest.left as f64 + lx as f64 * sx,
                cy: slot.dest.top as f64 + ly as f64 * sy,
                sx,
                sy,
                clip: slot.dest,
            },
        ));
    }
    None
}
