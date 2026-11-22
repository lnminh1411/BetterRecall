// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Watchdog detecting failed or stalled hardware replay encoders (`Replay\EncoderWatchdog.cs`).

/// Decides when a live replay encoder has failed (A7): it reported an error, or frames went in
/// (or waited on a busy encoder) for `stall_ms` with no packet coming out. Pure: clock passed in.
pub struct EncoderWatchdog {
    stall_ms: i64,
    last_packets: i64,
    last_progress_ms: i64,
    last_attempt_ms: i64,
}

impl EncoderWatchdog {
    pub const DEFAULT_STALL_MS: i64 = 3000;

    pub fn new(start_ms: i64, stall_ms: i64) -> Self {
        Self {
            stall_ms,
            last_packets: -1,
            last_progress_ms: start_ms,
            last_attempt_ms: i64::MIN,
        }
    }

    pub fn stall_ms(&self) -> i64 {
        self.stall_ms
    }

    /// A frame was submitted, or could not be because the encoder was not ready.
    pub fn attempt(&mut self, now_ms: i64) {
        if self.last_attempt_ms == i64::MIN || now_ms - self.last_attempt_ms > self.stall_ms {
            self.last_progress_ms = self.last_progress_ms.max(now_ms);
        }
        self.last_attempt_ms = now_ms;
    }

    /// Returns the failure reason, or `None` while the encoder is healthy.
    pub fn check(&mut self, now_ms: i64, packets_out: i64, error: Option<&str>) -> Option<String> {
        if let Some(err) = error
            && !err.is_empty()
        {
            return Some(err.to_string());
        }
        if packets_out != self.last_packets {
            self.last_packets = packets_out;
            self.last_progress_ms = now_ms;
            return None;
        }
        if self.last_attempt_ms >= self.last_progress_ms
            && now_ms - self.last_progress_ms >= self.stall_ms
        {
            return Some(format!(
                "no output for {} s",
                (now_ms - self.last_progress_ms) / 1000
            ));
        }
        None
    }
}
