// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Sample clock for the replay audio track (`Audio\AudioTimeline.cs`).

/// Planned action for one captured audio packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimelineStep {
    pub end_run: bool,
    pub start_run: bool,
    pub fill_frames: i64,
    pub skip_frames: i32,
    pub feed_frames: i32,
    pub feed_silent: bool,
}

/// Sample clock for the replay audio track. A run is a stretch of continuous audio whose time is
/// `run_start (QPC) + frames_fed / sample_rate`, so AAC packet times never drift from video.
pub struct AudioTimeline {
    sample_rate: i32,
    run_start: i64,
    frames_fed: i64,
    last_loud_end: i64,
    runs: i64,
    filled_frames: i64,
    trimmed_frames: i64,
}

impl AudioTimeline {
    /// 2 seconds of silence ends the run.
    pub const MAX_SILENCE_100NS: i64 = 20_000_000;
    /// 2 ms jitter threshold: treat as continuous.
    pub const JITTER_100NS: i64 = 20_000;

    pub fn new(sample_rate: i32) -> Self {
        Self {
            sample_rate,
            run_start: i64::MIN,
            frames_fed: 0,
            last_loud_end: 0,
            runs: 0,
            filled_frames: 0,
            trimmed_frames: 0,
        }
    }

    pub fn in_run(&self) -> bool {
        self.run_start != i64::MIN
    }

    pub fn run_start(&self) -> i64 {
        self.run_start
    }

    pub fn runs(&self) -> i64 {
        self.runs
    }

    pub fn filled_frames(&self) -> i64 {
        self.filled_frames
    }

    pub fn trimmed_frames(&self) -> i64 {
        self.trimmed_frames
    }

    /// Time of the next frame to feed, in 100 ns QPC units.
    pub fn expected(&self) -> i64 {
        self.run_start + self.frames_fed * 10_000_000 / self.sample_rate as i64
    }

    /// Plans one captured packet. Call [`Self::commit`] with the step after acting on it.
    pub fn plan(&self, qpc: i64, frames: i32, silent: bool) -> TimelineStep {
        if frames <= 0 {
            return TimelineStep::default();
        }
        let mut end = false;
        if self.in_run() {
            let expected = self.expected();
            let gap = qpc - expected;
            if !(-Self::MAX_SILENCE_100NS..=Self::MAX_SILENCE_100NS).contains(&gap) {
                end = true;
            } else if silent && expected - self.last_loud_end >= Self::MAX_SILENCE_100NS {
                return TimelineStep {
                    end_run: true,
                    ..Default::default()
                };
            } else if gap > Self::JITTER_100NS {
                return TimelineStep {
                    fill_frames: gap * self.sample_rate as i64 / 10_000_000,
                    feed_frames: frames,
                    feed_silent: silent,
                    ..Default::default()
                };
            } else if gap < -Self::JITTER_100NS {
                let skip = (frames as i64).min((-gap) * self.sample_rate as i64 / 10_000_000) as i32;
                return TimelineStep {
                    skip_frames: skip,
                    feed_frames: frames - skip,
                    feed_silent: silent,
                    ..Default::default()
                };
            } else {
                return TimelineStep {
                    feed_frames: frames,
                    feed_silent: silent,
                    ..Default::default()
                };
            }
        }
        if silent {
            return TimelineStep {
                end_run: end,
                ..Default::default()
            };
        }
        TimelineStep {
            end_run: end,
            start_run: true,
            feed_frames: frames,
            ..Default::default()
        }
    }

    pub fn commit(&mut self, s: TimelineStep, qpc: i64) {
        if s.end_run {
            self.run_start = i64::MIN;
            self.frames_fed = 0;
        }
        if s.start_run {
            self.run_start = qpc;
            self.frames_fed = 0;
            self.last_loud_end = qpc;
            self.runs += 1;
        }
        if !self.in_run() {
            return;
        }
        self.frames_fed += s.fill_frames + s.feed_frames as i64;
        self.filled_frames += s.fill_frames;
        self.trimmed_frames += s.skip_frames as i64;
        if s.feed_frames > 0 && !s.feed_silent {
            self.last_loud_end = self.expected();
        }
    }
}
