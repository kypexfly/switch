//! When to ask a provider for usage again. Ported from claude-swap's measured policy: Claude's usage
//! endpoint admits about 30 requests per account in a trailing hour, so polls average one per
//! three minutes, tighten only while the active account burns toward the switch line, and back off
//! after rate limiting.

use std::collections::HashMap;

use jiff::{SignedDuration, Timestamp};
use switch_protocol::UsageWindow;

/// Usage younger than this is never fetched again, however often clients ask.
pub const FRESH_SECS: f64 = 180.0;
const MIN_INTERVAL: f64 = 180.0;
const URGENT_INTERVAL: f64 = 60.0;
const ACTIVE_MAX_INTERVAL: f64 = 300.0;
const IDLE_DEFAULT_INTERVAL: f64 = 300.0;
const IDLE_MAX_INTERVAL: f64 = 600.0;
const EXHAUSTED_INTERVAL: f64 = 600.0;
const MOVEMENT_PERCENT: f64 = 1.0;
const ESCALATION_MARGIN_PERCENT: f64 = 15.0;
const RESET_SLACK: f64 = 60.0;
const JITTER: f64 = 0.1;
const LIMITED_MIN_INTERVAL: f64 = 360.0;
const LIMITED_MAX_INTERVAL: f64 = 1800.0;
const LIMITED_GROWTH: f64 = 1.5;
const RECENTLY_LIMITED: f64 = 3600.0;
const BACKOFF_BASE: f64 = 30.0;
const BACKOFF_CAP: f64 = 600.0;
const EDGE_BACKOFF: f64 = 300.0;
const LONG_BLOCK_MARGIN: f64 = 900.0;
const LONG_BLOCK_CAP: f64 = 4500.0;

#[derive(Clone, Debug, Default)]
pub struct Schedule {
    pub next_at: Option<Timestamp>,
    pub interval: Option<f64>,
    pub limited_at: Option<Timestamp>,
    pub failures: u32,
}

impl Schedule {
    pub fn due(&self, now: Timestamp) -> bool {
        self.next_at.is_none_or(|next_at| now >= next_at)
    }

    /// For a fetch the user asked for: still never sooner than the freshness floor or a backoff.
    pub fn due_on_request(&self, fetched_at: Option<Timestamp>, now: Timestamp) -> bool {
        let fresh = fetched_at.is_some_and(|fetched_at| seconds(now, fetched_at) < FRESH_SECS);
        let backing_off = self.failures > 0 && !self.due(now);
        !fresh && !backing_off
    }

    pub(crate) fn recently_limited(&self, now: Timestamp) -> bool {
        self.limited_at.is_some_and(|limited_at| seconds(now, limited_at) < RECENTLY_LIMITED)
    }
}

pub struct Fetched<'a> {
    pub previous: Option<&'a [UsageWindow]>,
    pub windows: &'a [UsageWindow],
    pub active: bool,
    pub threshold: f64,
}

/// `jitter` is in -1..=1 so that separate processes drift apart.
pub fn after_success(schedule: &mut Schedule, fetched: Fetched, now: Timestamp, jitter: f64) {
    let default = if fetched.active { MIN_INTERVAL } else { IDLE_DEFAULT_INTERVAL };
    let ceiling = if fetched.active { ACTIVE_MAX_INTERVAL } else { IDLE_MAX_INTERVAL };
    let base = schedule.interval.unwrap_or(default);
    let previous_peak = fetched.previous.and_then(peak);
    let mut interval = match (previous_peak, peak(fetched.windows)) {
        (Some(before), Some(after)) if (after - before).abs() >= MOVEMENT_PERCENT => MIN_INTERVAL.max(base / 2.0),
        (Some(_), Some(_)) => ceiling.min(MIN_INTERVAL.max(base * 1.5)),
        _ => default,
    };
    let limited = schedule.recently_limited(now);
    if fetched.active && !limited && rising_near_line(fetched.previous, fetched.windows, fetched.threshold) {
        interval = URGENT_INTERVAL;
    }
    if limited {
        let grown = (base * LIMITED_GROWTH).max(LIMITED_MIN_INTERVAL);
        interval = LIMITED_MAX_INTERVAL.min(interval.max(grown));
    }
    let exhausted = peak(fetched.windows).is_some_and(|peak| peak >= 100.0);
    if exhausted {
        interval = interval.max(EXHAUSTED_INTERVAL);
    }

    let mut next_at = after(now, interval * (1.0 + JITTER * jitter));
    let reset = if exhausted { last_exhausted_reset(fetched.windows) } else { next_reset(fetched.windows, now) };
    if let Some(reset) = reset.filter(|reset| *reset > now) {
        next_at = next_at.min(after(reset, RESET_SLACK));
    }
    schedule.next_at = Some(next_at);
    schedule.interval = Some(interval);
    schedule.failures = 0;
}

pub fn after_failure(schedule: &mut Schedule, rate_limited: bool, retry_after: Option<f64>, now: Timestamp) {
    schedule.failures += 1;
    if rate_limited {
        schedule.limited_at = Some(now);
    }
    let computed = (BACKOFF_BASE * 2f64.powi(schedule.failures.min(32) as i32 - 1)).min(BACKOFF_CAP);
    let wait = match retry_after {
        Some(asked) if rate_limited && asked > BACKOFF_CAP => (asked + LONG_BLOCK_MARGIN).min(LONG_BLOCK_CAP),
        Some(asked) if rate_limited && asked == 0.0 => computed.clamp(EDGE_BACKOFF, BACKOFF_CAP),
        Some(asked) => computed.max(asked).min(LONG_BLOCK_CAP),
        None => computed,
    };
    schedule.next_at = Some(after(now, wait));
}

/// Windows that just rose into the band below the switch line; a window that fell into it rolled over.
fn rising_near_line(previous: Option<&[UsageWindow]>, windows: &[UsageWindow], threshold: f64) -> bool {
    let Some(previous) = previous else { return false };
    let before: HashMap<&str, f64> = primary(previous).filter_map(|w| Some((w.id.as_str(), w.used_percent?))).collect();
    primary(windows).any(|window| {
        let (Some(now), Some(before)) = (window.used_percent, before.get(window.id.as_str())) else { return false };
        now - before >= MOVEMENT_PERCENT && now >= threshold - ESCALATION_MARGIN_PERCENT
    })
}

pub fn fresh_until(fetched_at: Timestamp) -> Timestamp {
    after(fetched_at, FRESH_SECS)
}

pub fn peak(windows: &[UsageWindow]) -> Option<f64> {
    primary(windows).filter_map(|window| window.used_percent).reduce(f64::max)
}

pub fn current_peak(windows: &[UsageWindow], now: Timestamp) -> Option<f64> {
    primary(windows)
        .filter(|window| window.resets_at.is_none_or(|reset| reset > now))
        .filter_map(|window| window.used_percent)
        .reduce(f64::max)
}

fn next_reset(windows: &[UsageWindow], now: Timestamp) -> Option<Timestamp> {
    windows
        .iter()
        .filter(|window| window.primary || window.used_percent.is_some_and(|percent| percent > 0.0))
        .filter_map(|window| window.resets_at)
        .filter(|reset| *reset > now)
        .min()
}

fn last_exhausted_reset(windows: &[UsageWindow]) -> Option<Timestamp> {
    primary(windows)
        .filter(|window| window.used_percent.is_some_and(|percent| percent >= 100.0))
        .filter_map(|window| window.resets_at)
        .max()
}

fn primary(windows: &[UsageWindow]) -> impl Iterator<Item = &UsageWindow> {
    windows.iter().filter(|window| window.primary)
}

fn after(time: Timestamp, secs: f64) -> Timestamp {
    time.saturating_add(SignedDuration::from_secs_f64(secs)).expect("durations are in range")
}

fn seconds(later: Timestamp, earlier: Timestamp) -> f64 {
    later.duration_since(earlier).as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_second(1_000_000).unwrap()
    }

    fn windows(five_hour: f64, seven_day: f64) -> Vec<UsageWindow> {
        [("session", five_hour), ("weekly", seven_day)]
            .map(|(id, percent)| UsageWindow {
                id: id.into(),
                label: id.into(),
                used_percent: Some(percent),
                resets_at: None,
                primary: true,
                period_seconds: None,
            })
            .to_vec()
    }

    fn plan(schedule: &mut Schedule, previous: Option<&[UsageWindow]>, windows: &[UsageWindow], active: bool) -> f64 {
        after_success(schedule, Fetched { previous, windows, active, threshold: 90.0 }, now(), 0.0);
        seconds(schedule.next_at.unwrap(), now())
    }

    #[test]
    fn idle_accounts_back_off_to_ten_minutes() {
        let mut schedule = Schedule::default();
        let usage = windows(10.0, 10.0);
        let intervals: Vec<f64> = (0..4).map(|_| plan(&mut schedule, Some(&usage), &usage, false)).collect();
        assert_eq!(intervals, [450.0, 600.0, 600.0, 600.0]);
    }

    #[test]
    fn the_active_account_polls_every_minute_while_burning_toward_the_line() {
        let mut schedule = Schedule::default();
        assert_eq!(plan(&mut schedule, Some(&windows(70.0, 10.0)), &windows(78.0, 10.0), true), 60.0);
        assert_eq!(plan(&mut schedule, Some(&windows(78.0, 10.0)), &windows(78.0, 10.0), true), 180.0);
        assert_eq!(plan(&mut schedule, Some(&windows(10.0, 10.0)), &windows(20.0, 10.0), true), 180.0);
    }

    #[test]
    fn a_window_reset_pulls_the_next_poll_forward() {
        let mut schedule = Schedule::default();
        let mut usage = windows(10.0, 10.0);
        usage[0].resets_at = Some(after(now(), 30.0));
        assert_eq!(plan(&mut schedule, None, &usage, false), 90.0);
    }

    #[test]
    fn used_non_primary_window_reset_pulls_the_next_poll_forward() {
        let mut usage = windows(10.0, 10.0);
        usage.push(UsageWindow {
            id: "model:fable".into(),
            label: "Fable".into(),
            used_percent: Some(0.0),
            resets_at: Some(after(now(), 30.0)),
            primary: false,
            period_seconds: None,
        });
        assert_eq!(plan(&mut Schedule::default(), None, &usage, false), 300.0);

        usage[2].used_percent = Some(25.0);
        assert_eq!(plan(&mut Schedule::default(), None, &usage, false), 90.0);

        usage[2].used_percent = None;
        assert_eq!(plan(&mut Schedule::default(), None, &usage, false), 300.0);
    }

    #[test]
    fn rate_limits_back_off_and_slow_later_polls() {
        let mut schedule = Schedule::default();
        after_failure(&mut schedule, true, Some(0.0), now());
        assert_eq!(seconds(schedule.next_at.unwrap(), now()), 300.0);
        after_failure(&mut schedule, true, Some(3000.0), now());
        assert_eq!(seconds(schedule.next_at.unwrap(), now()), 3900.0);
        after_failure(&mut schedule, false, None, now());
        assert_eq!(seconds(schedule.next_at.unwrap(), now()), 120.0);

        let usage = windows(80.0, 10.0);
        assert_eq!(plan(&mut schedule, Some(&windows(70.0, 10.0)), &usage, true), 360.0);
    }

    #[test]
    fn requests_respect_freshness_and_backoff() {
        let mut schedule = Schedule::default();
        assert!(schedule.due_on_request(None, now()));
        assert!(!schedule.due_on_request(Some(after(now(), -60.0)), now()));
        assert!(schedule.due_on_request(Some(after(now(), -200.0)), now()));
        after_failure(&mut schedule, true, None, now());
        assert!(!schedule.due_on_request(None, now()));
    }

    #[test]
    fn current_peak_ignores_expired_windows() {
        let mut usage = windows(95.0, 20.0);
        usage[0].resets_at = Some(after(now(), -10.0));
        usage[1].resets_at = Some(after(now(), 60.0));
        assert_eq!(current_peak(&usage, now()), Some(20.0));

        usage[1].resets_at = Some(after(now(), -5.0));
        assert_eq!(current_peak(&usage, now()), None);
    }
}
