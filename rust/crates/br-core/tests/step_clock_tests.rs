// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Backward clock-step detection and boot-anchor fallback (`StepClock`, `ProcessInfo`).

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use br_core::{ProcessInfo, StepClock};
use chrono::{DateTime, Duration as ChronoDuration, Utc};

/// Regression T2.
#[test]
fn start_bound_follows_the_boot_clock_only_after_a_step_back() {
    let boot = DateTime::parse_from_rfc3339("2026-11-10T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let wall = boot + ChronoDuration::hours(2);
    // Normal clock: wall == boot + 2h -> bound is wall.
    assert_eq!(
        wall,
        ProcessInfo::start_bound_fresh(wall, 2 * 3600 * 1000, Some(boot))
    );
    // Clock stepped back 1 hour: boot + 2h is 1h ahead of wall -> bound is boot + 2h.
    let stepped = boot + ChronoDuration::hours(1);
    assert_eq!(
        boot + ChronoDuration::hours(2),
        ProcessInfo::start_bound_fresh(stepped, 2 * 3600 * 1000, Some(boot))
    );
}

/// Regression S6: the boot-clock bound lasts only for the catch-up period and each step is logged
/// once.
#[test]
fn boot_bound_lasts_only_for_the_catch_up_period_and_is_logged_once_per_step() {
    let logs = Arc::new(AtomicI32::new(0));
    let logs_cb = logs.clone();
    let clock = StepClock::new(Some(Box::new(move |_| {
        logs_cb.fetch_add(1, Ordering::AcqRel);
    })));
    let boot = DateTime::parse_from_rfc3339("2026-11-10T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let step_wall = boot + ChronoDuration::minutes(30);
    let b1 = clock.bound(step_wall, 60 * 60 * 1000, Some(boot));
    assert_eq!(boot + ChronoDuration::minutes(60), b1);
    assert_eq!(1, logs.load(Ordering::Acquire));

    // Still inside catch-up: no second log.
    let b2 = clock.bound(
        step_wall + ChronoDuration::minutes(10),
        70 * 60 * 1000,
        Some(boot),
    );
    assert_eq!(boot + ChronoDuration::minutes(60), b2);
    assert_eq!(1, logs.load(Ordering::Acquire));

    // After catch-up: wall clock alone.
    let after = boot + ChronoDuration::minutes(70);
    let b3 = clock.bound(after, 100 * 60 * 1000, Some(boot));
    assert_eq!(after, b3);
    assert_eq!(1, logs.load(Ordering::Acquire));
}

/// C3 (sweep 5): a second backward step inside catch-up keeps the earlier `until`.
#[test]
fn step_clock_preserves_until_across_multiple_backward_steps_and_sub_tolerance_steps() {
    let clock = StepClock::new(None);
    let boot = DateTime::parse_from_rfc3339("2026-11-10T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let w1 = boot + ChronoDuration::minutes(30);
    let b1 = clock.bound(w1, 60 * 60 * 1000, Some(boot));
    assert_eq!(boot + ChronoDuration::minutes(60), b1);
    // Second smaller step back preserves the later `until`.
    let w2 = boot + ChronoDuration::minutes(20);
    let b2 = clock.bound(w2, 70 * 60 * 1000, Some(boot));
    assert_eq!(b1, b2);
}

/// Regression N3 (sweep 6): a standard user cannot read pid 4's start time; the fallback anchor
/// (wall time minus uptime, taken once) still lets a later backward clock step be seen.
#[test]
fn boot_anchor_fallback_still_detects_a_clock_step() {
    let t0 = DateTime::parse_from_rfc3339("2026-11-22T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let pid4 = t0 - ChronoDuration::hours(3);
    assert_eq!(pid4, ProcessInfo::boot_anchor(Some(pid4), t0, 3 * 3600 * 1000));
    let up0 = 5 * 3600 * 1000;
    let anchor = ProcessInfo::boot_anchor(None, t0, up0);
    assert_eq!(t0 - ChronoDuration::hours(5), anchor);
    // One minute later the wall clock is stepped back 2 h.
    let up1 = up0 + 60_000;
    let wall1 = t0 + ChronoDuration::minutes(1) - ChronoDuration::hours(2);
    let bound = StepClock::new(None).bound(wall1, up1, Some(anchor));
    assert!(bound >= t0 + ChronoDuration::minutes(1) - ChronoDuration::seconds(1), "{bound}");
}

/// C3 (sweep 5): a backward step under the 10 s tolerance is left to the wall clock (no log); a
/// larger one switches to the boot clock and is logged once.
#[test]
fn step_clock_ignores_steps_under_the_tolerance() {
    let logs = Arc::new(AtomicI32::new(0));
    let logs_cb = logs.clone();
    let clock = StepClock::new(Some(Box::new(move |_| {
        logs_cb.fetch_add(1, Ordering::AcqRel);
    })));
    let boot = DateTime::parse_from_rfc3339("2026-11-10T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let uptime_ms = 60 * 60 * 1000;
    let boot_now = boot + ChronoDuration::milliseconds(uptime_ms);

    let small = boot_now - ChronoDuration::seconds(5);
    assert_eq!(small, clock.bound(small, uptime_ms, Some(boot)));
    assert_eq!(0, logs.load(Ordering::Acquire));

    let large = boot_now - ChronoDuration::seconds(15);
    assert_eq!(boot_now, clock.bound(large, uptime_ms, Some(boot)));
    assert_eq!(1, logs.load(Ordering::Acquire));
}
