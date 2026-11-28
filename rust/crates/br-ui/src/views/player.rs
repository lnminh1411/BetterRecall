// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, KeyDownEvent, ObjectFit,
    ParentElement as _, Render, RenderImage, Styled as _, Subscription, Window, div, img, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::platform;
use crate::platform::video::{MediaPlayer, PlayerSnapshot};

pub struct PlayerView {
    player: Option<MediaPlayer>,
    video_path: Option<PathBuf>,
    current_frame: Option<Arc<RenderImage>>,
    snapshot: PlayerSnapshot,
    seek_slider: Entity<SliderState>,
    dragging_slider: bool,
    show_stats: bool,
    stats_to_stderr: bool,
    shown_frames_window: u32,
    shown_fps: f32,
    fps_window_start: Instant,
    last_stderr_log: Instant,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl PlayerView {
    pub fn new(
        video_path: Option<PathBuf>,
        offset_ms: u64,
        stats_to_stderr: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let player = video_path
            .clone()
            .map(|p| MediaPlayer::open(p, offset_ms));

        let seek_slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(1000.0)
                .step(1.0)
                .default_value(0.0)
        });
        let focus_handle = cx.focus_handle().tab_stop(true);

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(
            &seek_slider,
            |this, _, ev: &SliderEvent, cx| match ev {
                SliderEvent::Change(_) => {
                    this.dragging_slider = true;
                }
                SliderEvent::Release(val) => {
                    this.dragging_slider = false;
                    let dur = this.snapshot.duration_ms;
                    if dur > 0
                        && let Some(ref p) = this.player
                    {
                        let target_ms = ((val.end() / 1000.0) * dur as f32) as i64;
                        p.seek_ms(target_ms);
                    }
                    cx.notify();
                }
            },
        ));

        // Poll decoded frames at ~60Hz on the UI thread, dropping old RenderImages from GPU memory.
        cx.spawn_in(window, async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let keep_going = weak.update_in(cx, |this, window, cx| {
                    this.tick(window, cx);
                });
                if keep_going.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            player,
            video_path,
            current_frame: None,
            snapshot: PlayerSnapshot::default(),
            seek_slider,
            dragging_slider: false,
            show_stats: stats_to_stderr,
            stats_to_stderr,
            shown_frames_window: 0,
            shown_fps: 0.0,
            fps_window_start: Instant::now(),
            last_stderr_log: Instant::now(),
            focus_handle,
            _subscriptions: subscriptions,
        }
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ref player) = self.player else {
            return;
        };

        let snap = player.snapshot();
        let mut changed = snap.position_ms != self.snapshot.position_ms
            || snap.duration_ms != self.snapshot.duration_ms
            || snap.playing != self.snapshot.playing
            || snap.error != self.snapshot.error;
        self.snapshot = snap;

        if let Some(frame) = player.take_due_frame() {
            if let Some(old) = self.current_frame.replace(frame.image) {
                let _ = window.drop_image(old);
            }
            self.shown_frames_window += 1;
            changed = true;
        }

        let elapsed = self.fps_window_start.elapsed().as_secs_f32();
        if elapsed >= 0.5 {
            self.shown_fps = self.shown_frames_window as f32 / elapsed;
            self.shown_frames_window = 0;
            self.fps_window_start = Instant::now();
            changed = true;
        }

        if !self.dragging_slider && self.snapshot.duration_ms > 0 {
            let ratio = (self.snapshot.position_ms as f32 / self.snapshot.duration_ms as f32)
                .clamp(0.0, 1.0);
            self.seek_slider.update(cx, |s, cx| {
                s.set_value(SliderValue::Single(ratio * 1000.0), window, cx);
            });
        }

        if self.stats_to_stderr && self.last_stderr_log.elapsed() >= Duration::from_secs(1) {
            self.last_stderr_log = Instant::now();
            let ws_mb = platform::working_set_mb();
            eprintln!(
                "stats fps={:.1} shown={:.1} dropped={} ws_mb={:.1}",
                self.snapshot.decoded_fps, self.shown_fps, self.snapshot.dropped_frames, ws_mb
            );
        }

        if changed {
            cx.notify();
        }
    }

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if let Some(ref player) = self.player {
            if self.snapshot.playing {
                player.pause();
            } else {
                player.play();
            }
            cx.notify();
        }
    }
}

fn format_time(ms: i64) -> String {
    let total_secs = (ms.max(0) / 1000) as u64;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins:02}:{secs:02}")
}

impl Render for PlayerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let ws_mb = if self.show_stats {
            platform::working_set_mb()
        } else {
            0.0
        };

        v_flex()
            .id("player-root")
            .size_full()
            .p_4()
            .gap_3()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _window, cx| {
                match ev.keystroke.key.as_str() {
                    "space" => this.toggle_play(cx),
                    "f2" => {
                        this.show_stats = !this.show_stats;
                        cx.notify();
                    }
                    _ => {}
                }
            }))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(gpui::black())
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(if let Some(ref err) = self.snapshot.error {
                        div()
                            .p_4()
                            .text_sm()
                            .text_color(theme.danger_foreground)
                            .bg(theme.danger)
                            .rounded(theme.radius)
                            .child(err.clone())
                            .into_any_element()
                    } else if let Some(ref frame) = self.current_frame {
                        img(frame.clone())
                            .size_full()
                            .object_fit(ObjectFit::Contain)
                            .into_any_element()
                    } else if self.video_path.is_none() {
                        div()
                            .text_sm()
                            .text_color(gpui::white().opacity(0.75))
                            .child("Pass --video <path> [--offset-ms <n>] [--stats] to play a replay.")
                            .into_any_element()
                    } else {
                        div()
                            .text_sm()
                            .text_color(gpui::white().opacity(0.75))
                            .child("Opening video stream...")
                            .into_any_element()
                    })
                    .children(self.show_stats.then(|| {
                        v_flex()
                            .absolute()
                            .top_3()
                            .left_3()
                            .p_2()
                            .gap_0p5()
                            .rounded(theme.radius)
                            .bg(gpui::black().opacity(0.72))
                            .text_color(gpui::white())
                            .text_xs()
                            .child(format!("decoded fps: {:.1}", self.snapshot.decoded_fps))
                            .child(format!("shown fps:   {:.1}", self.shown_fps))
                            .child(format!("dropped:     {}", self.snapshot.dropped_frames))
                            .child(format!("queue len:   {}", self.snapshot.queue_len))
                            .child(format!("working set: {:.1} MB", ws_mb))
                    })),
            )
            .child(
                h_flex()
                    .p_3()
                    .gap_3()
                    .items_center()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.group_box)
                    .child(
                        Button::new("player-play-pause")
                            .primary()
                            .label(if self.snapshot.playing { "Pause" } else { "Play" })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_play(cx))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .w(px(90.0))
                            .child(format!(
                                "{} / {}",
                                format_time(self.snapshot.position_ms),
                                format_time(self.snapshot.duration_ms)
                            )),
                    )
                    .child(div().flex_1().child(Slider::new(&self.seek_slider)))
                    .child(
                        Button::new("player-stats-toggle")
                            .ghost()
                            .label(if self.show_stats { "Hide Stats (F2)" } else { "Stats (F2)" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_stats = !this.show_stats;
                                cx.notify();
                            })),
                    ),
            )
    }
}
