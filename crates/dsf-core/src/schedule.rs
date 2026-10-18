//! When the factory may claim work: the daily window, usage-limit backoff
//! and worker capacity. Times are passed in, never read from a clock.

use std::cmp::Ordering;

use jiff::civil::Time;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

/// Waiting this long past a reported reset avoids racing it.
const RESET_GRACE: SignedDuration = SignedDuration::from_mins(2);
/// A reported reset further away than this is treated as a mistake. Weekly
/// subscription limits can legitimately be days away.
const MAX_RESET_WAIT: SignedDuration = SignedDuration::from_hours(8 * 24);
/// When no reset time is known: 30m, 1h, 2h, then 4h at most.
const FIRST_BACKOFF: SignedDuration = SignedDuration::from_mins(30);
const MAX_BACKOFF: SignedDuration = SignedDuration::from_hours(4);

/// A daily window of local wall-clock time in which new work may start.
/// Work already running when it closes carries on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: Time,
    pub end: Time,
}

impl Window {
    /// Open from `start` up to but not including `end`. A window whose end
    /// is earlier than its start spans midnight. Equal times mean always
    /// open.
    pub const fn new(start: Time, end: Time) -> Self {
        Self { start, end }
    }

    /// Whether the window is open at `now`, judged by the local wall clock.
    /// On the night clocks go back, an hour that falls inside the window
    /// happens twice and counts both times.
    pub fn is_open(&self, now: Timestamp, tz: &TimeZone) -> bool {
        let time = tz.to_datetime(now).time();
        match self.start.cmp(&self.end) {
            Ordering::Equal => true,
            Ordering::Less => self.start <= time && time < self.end,
            Ordering::Greater => time >= self.start || time < self.end,
        }
    }

    /// `now` if the window is open, otherwise when it next opens. If the
    /// start time falls in a daylight-saving gap that day, this is the
    /// wall-clock time the gap skips to; the scheduler's own polling
    /// catches the window opening any earlier than that.
    pub fn next_open(&self, now: Timestamp, tz: &TimeZone) -> Timestamp {
        if self.is_open(now, tz) {
            now
        } else {
            next_daily(self.start, now, tz)
        }
    }
}

/// The first instant strictly after `after` when the local wall clock reads
/// `at`, e.g. when tomorrow's 07:00 digest is due. Correct across
/// daylight-saving changes, which a fixed 24-hour step is not.
pub fn next_daily(at: Time, after: Timestamp, tz: &TimeZone) -> Timestamp {
    let mut date = tz.to_datetime(after).date();
    // Two days always suffice; a third covers zones that once skipped a
    // whole calendar day.
    for _ in 0..3 {
        if let Ok(candidate) = tz.to_ambiguous_timestamp(date.to_datetime(at)).compatible()
            && candidate > after
        {
            return candidate;
        }
        match date.tomorrow() {
            Ok(next) => date = next,
            Err(_) => break,
        }
    }
    Timestamp::MAX
}

/// When claiming may resume after a usage-limit hit. `consecutive` counts
/// limit hits in a row, starting at 1.
pub fn usage_limit_resume(
    now: Timestamp,
    reset_at: Option<Timestamp>,
    consecutive: u32,
) -> Timestamp {
    match reset_at {
        Some(reset) if reset > now => {
            let latest = add(now, MAX_RESET_WAIT);
            add(reset, RESET_GRACE).min(latest)
        }
        _ => {
            let doublings = consecutive.saturating_sub(1).min(8);
            let backoff = FIRST_BACKOFF
                .checked_mul(1 << doublings)
                .unwrap_or(MAX_BACKOFF)
                .min(MAX_BACKOFF);
            add(now, backoff)
        }
    }
}

fn add(timestamp: Timestamp, duration: SignedDuration) -> Timestamp {
    timestamp.checked_add(duration).unwrap_or(Timestamp::MAX)
}

/// Factory-wide state that affects claiming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimState {
    /// Set by the operator to stop all new work.
    pub paused: bool,
    pub backoff_until: Option<Timestamp>,
    /// Runs currently holding a worker slot.
    pub active_runs: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimDecision {
    /// Claim up to this many issues now.
    Claim {
        slots: usize,
    },
    Wait(Wait),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    Paused,
    BackingOff { until: Timestamp },
    WindowClosed { opens_at: Timestamp },
    AtCapacity,
}

/// Whether the scheduler may claim work now. Resumed runs go through the
/// same decision as new ones, so they also respect the window.
pub fn claim_decision(
    now: Timestamp,
    tz: &TimeZone,
    window: &Window,
    workers: usize,
    state: &ClaimState,
) -> ClaimDecision {
    if state.paused {
        return ClaimDecision::Wait(Wait::Paused);
    }
    if let Some(until) = state.backoff_until
        && until > now
    {
        return ClaimDecision::Wait(Wait::BackingOff { until });
    }
    if !window.is_open(now, tz) {
        return ClaimDecision::Wait(Wait::WindowClosed {
            opens_at: window.next_open(now, tz),
        });
    }
    match workers.saturating_sub(state.active_runs) {
        0 => ClaimDecision::Wait(Wait::AtCapacity),
        slots => ClaimDecision::Claim { slots },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::time;

    /// Europe/London's rules as a POSIX TZ string, so tests need no tz
    /// database: GMT in winter, BST from the last Sunday in March at 01:00
    /// to the last Sunday in October at 02:00.
    fn london() -> TimeZone {
        TimeZone::posix("GMT0BST,M3.5.0/1,M10.5.0").unwrap()
    }

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn overnight() -> Window {
        Window::new(time(23, 0, 0, 0), time(7, 0, 0, 0))
    }

    #[test]
    fn overnight_window_opens_at_23_and_closes_at_07_local() {
        let tz = london();
        let window = overnight();
        // Summer: local time is UTC+1.
        assert!(!window.is_open(at("2026-07-01T21:59:59Z"), &tz));
        assert!(window.is_open(at("2026-07-01T22:00:00Z"), &tz));
        assert!(window.is_open(at("2026-07-02T05:59:59Z"), &tz));
        assert!(!window.is_open(at("2026-07-02T06:00:00Z"), &tz));
        // Winter: local time is UTC.
        assert!(!window.is_open(at("2026-12-01T22:59:59Z"), &tz));
        assert!(window.is_open(at("2026-12-01T23:00:00Z"), &tz));
        assert!(!window.is_open(at("2026-12-02T07:00:00Z"), &tz));
    }

    #[test]
    fn daytime_and_always_open_windows() {
        let tz = TimeZone::UTC;
        let day = Window::new(time(9, 0, 0, 0), time(17, 0, 0, 0));
        assert!(day.is_open(at("2026-07-01T09:00:00Z"), &tz));
        assert!(!day.is_open(at("2026-07-01T17:00:00Z"), &tz));
        assert!(!day.is_open(at("2026-07-01T03:00:00Z"), &tz));
        let always = Window::new(time(0, 0, 0, 0), time(0, 0, 0, 0));
        assert!(always.is_open(at("2026-07-01T12:34:00Z"), &tz));
    }

    #[test]
    fn window_is_nine_hours_the_night_clocks_go_back() {
        // 2026-10-25: 02:00 BST becomes 01:00 GMT.
        let tz = london();
        let window = overnight();
        let opens = window.next_open(at("2026-10-24T12:00:00Z"), &tz);
        assert_eq!(opens, at("2026-10-24T22:00:00Z"));
        let closes = next_daily(window.end, opens, &tz);
        assert_eq!(closes, at("2026-10-25T07:00:00Z"));
        assert_eq!(closes.duration_since(opens), SignedDuration::from_hours(9));
        // The repeated hour is inside the window both times.
        assert!(window.is_open(at("2026-10-25T00:30:00Z"), &tz));
        assert!(window.is_open(at("2026-10-25T01:30:00Z"), &tz));
    }

    #[test]
    fn window_is_seven_hours_the_night_clocks_go_forward() {
        // 2027-03-28: 01:00 GMT becomes 02:00 BST.
        let tz = london();
        let window = overnight();
        let opens = window.next_open(at("2027-03-27T12:00:00Z"), &tz);
        assert_eq!(opens, at("2027-03-27T23:00:00Z"));
        let closes = next_daily(window.end, opens, &tz);
        assert_eq!(closes, at("2027-03-28T06:00:00Z"));
        assert_eq!(closes.duration_since(opens), SignedDuration::from_hours(7));
    }

    #[test]
    fn digest_time_is_strictly_after_now_and_tracks_dst() {
        let tz = london();
        let seven = time(7, 0, 0, 0);
        // Exactly 07:00 local: the next digest is tomorrow's.
        assert_eq!(
            next_daily(seven, at("2026-10-24T06:00:00Z"), &tz),
            at("2026-10-25T07:00:00Z")
        );
        // Just before: today's, at 07:00 BST.
        assert_eq!(
            next_daily(seven, at("2026-10-24T05:59:00Z"), &tz),
            at("2026-10-24T06:00:00Z")
        );
    }

    #[test]
    fn start_time_in_a_dst_gap_moves_to_after_the_gap() {
        // 01:30 doesn't exist in London on 2027-03-28.
        let tz = london();
        let window = Window::new(time(1, 30, 0, 0), time(5, 0, 0, 0));
        let opens = window.next_open(at("2027-03-27T12:00:00Z"), &tz);
        assert_eq!(opens, at("2027-03-28T01:30:00Z")); // 02:30 BST
        // The wall clock passes 01:30 at the gap, so polling sees it open.
        assert!(window.is_open(at("2027-03-28T01:00:00Z"), &tz)); // 02:00 BST
    }

    #[test]
    fn usage_limit_waits_for_a_known_reset_plus_grace() {
        let now = at("2026-10-18T23:00:00Z");
        assert_eq!(
            usage_limit_resume(now, Some(at("2026-10-19T02:00:00Z")), 1),
            at("2026-10-19T02:02:00Z")
        );
        // A reset in the past is no help; fall back to backoff.
        assert_eq!(
            usage_limit_resume(now, Some(at("2026-10-18T22:00:00Z")), 1),
            at("2026-10-18T23:30:00Z")
        );
        // An absurd reset is capped at eight days.
        assert_eq!(
            usage_limit_resume(now, Some(at("2027-10-18T00:00:00Z")), 1),
            at("2026-10-26T23:00:00Z")
        );
    }

    #[test]
    fn usage_limit_backoff_doubles_up_to_four_hours() {
        let now = at("2026-10-18T23:00:00Z");
        let waits: Vec<i64> = (1..=6)
            .map(|n| {
                usage_limit_resume(now, None, n)
                    .duration_since(now)
                    .as_mins()
            })
            .collect();
        assert_eq!(waits, [30, 60, 120, 240, 240, 240]);
        assert_eq!(
            usage_limit_resume(now, None, u32::MAX),
            at("2026-10-19T03:00:00Z")
        );
        assert_eq!(usage_limit_resume(now, None, 0), at("2026-10-18T23:30:00Z"));
    }

    #[test]
    fn claim_decision_checks_pause_backoff_window_then_capacity() {
        let tz = london();
        let window = overnight();
        let night = at("2026-12-01T23:30:00Z");
        let idle = ClaimState {
            paused: false,
            backoff_until: None,
            active_runs: 0,
        };

        assert_eq!(
            claim_decision(night, &tz, &window, 2, &idle),
            ClaimDecision::Claim { slots: 2 }
        );
        let busy = ClaimState {
            active_runs: 1,
            ..idle
        };
        assert_eq!(
            claim_decision(night, &tz, &window, 2, &busy),
            ClaimDecision::Claim { slots: 1 }
        );
        let full = ClaimState {
            active_runs: 2,
            ..idle
        };
        assert_eq!(
            claim_decision(night, &tz, &window, 2, &full),
            ClaimDecision::Wait(Wait::AtCapacity)
        );

        let until = at("2026-12-02T01:00:00Z");
        let backing_off = ClaimState {
            backoff_until: Some(until),
            ..idle
        };
        assert_eq!(
            claim_decision(night, &tz, &window, 2, &backing_off),
            ClaimDecision::Wait(Wait::BackingOff { until })
        );
        let expired = ClaimState {
            backoff_until: Some(at("2026-12-01T23:00:00Z")),
            ..idle
        };
        assert_eq!(
            claim_decision(night, &tz, &window, 2, &expired),
            ClaimDecision::Claim { slots: 2 }
        );

        let paused = ClaimState {
            paused: true,
            ..backing_off
        };
        assert_eq!(
            claim_decision(night, &tz, &window, 2, &paused),
            ClaimDecision::Wait(Wait::Paused)
        );

        let noon = at("2026-12-01T12:00:00Z");
        assert_eq!(
            claim_decision(noon, &tz, &window, 2, &idle),
            ClaimDecision::Wait(Wait::WindowClosed {
                opens_at: at("2026-12-01T23:00:00Z")
            })
        );
    }
}
