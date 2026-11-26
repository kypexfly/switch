use jiff::Timestamp;
use jiff::tz::TimeZone;
use switch_protocol::{Account, AccountStatus, ProviderState, UsageWindow};

pub fn percent(used: Option<f64>) -> String {
    match used {
        Some(used) if used.is_finite() => format!("{}%", used.max(0.0).round() as i64),
        _ => "--".to_owned(),
    }
}

#[derive(Debug, PartialEq)]
pub enum Severity {
    Normal,
    Warning,
    Danger,
}

pub fn severity(used: f64) -> Severity {
    if used >= 90.0 {
        Severity::Danger
    } else if used >= 75.0 {
        Severity::Warning
    } else {
        Severity::Normal
    }
}

pub fn duration(seconds: i64) -> String {
    let minutes = seconds.max(0) / 60;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => "<1m".to_owned(),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

pub fn resets_in(window: &UsageWindow, now: Timestamp) -> Option<String> {
    let resets_at = window.resets_at?;
    Some(format!("resets in {}", duration(resets_at.as_second() - now.as_second())))
}

pub fn reset_time(window: &UsageWindow, now: Timestamp, zone: &TimeZone, exact: bool) -> Option<String> {
    let resets_at = window.resets_at?;
    let seconds = resets_at.as_second() - now.as_second();
    if !exact {
        return Some(duration(seconds));
    }
    let local = resets_at.to_zoned(zone.clone());
    let format = if local.date() == now.to_zoned(zone.clone()).date() {
        "%H:%M"
    } else if seconds < 6 * 86400 {
        "%a %H:%M"
    } else {
        "%b %-d"
    };
    Some(local.strftime(format).to_string())
}

pub fn pace(window: &UsageWindow, now: Timestamp) -> Option<f64> {
    let period = window.period_seconds.filter(|period| *period > 0)?;
    let remaining = window.resets_at?.as_second() - now.as_second();
    (remaining > 0).then(|| (period - remaining).clamp(0, period) as f64 / period as f64)
}

pub fn age(then: Timestamp, now: Timestamp) -> String {
    let seconds = now.as_second() - then.as_second();
    if seconds < 60 { "just now".to_owned() } else { format!("{} ago", duration(seconds)) }
}

pub fn reset_expiry_countdown(expires_at: Timestamp, now: Timestamp) -> String {
    let seconds = (expires_at.as_second() - now.as_second()).max(0);
    let days = seconds / 86_400;
    if days >= 30 {
        let months = days / 30;
        let rem_days = days % 30;
        if rem_days == 0 { format!("{months}mo") } else { format!("{months}mo {rem_days}d") }
    } else {
        duration(seconds)
    }
}

pub fn reset_expiry_date(expires_at: Timestamp, zone: &TimeZone) -> String {
    expires_at.to_zoned(zone.clone()).strftime("%b %-d, %H:%M").to_string()
}

pub fn window_summary(window: &UsageWindow, now: Timestamp) -> String {
    let mut summary = format!("{} {} used", window.label, percent(window.used_percent));
    if let Some(resets) = resets_in(window, now) {
        summary.push_str(", ");
        summary.push_str(&resets);
    }
    summary
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tone {
    Muted,
    Warning,
    Danger,
}

pub fn account_details(account: &Account, provider: &ProviderState, now: Timestamp) -> Vec<(String, Tone)> {
    if account.status == AccountStatus::NeedsLogin {
        return vec![("Signed out".to_owned(), Tone::Danger)];
    }
    plan(account, provider).into_iter().chain(state(account, provider, now)).collect()
}

pub fn account_summary(account: &Account, provider: &ProviderState) -> Option<String> {
    if account.status == AccountStatus::NeedsLogin {
        return Some("Signed out".to_owned());
    }
    let state = match account {
        Account { active: true, .. } => Some("active"),
        Account { enabled: false, .. } => Some("paused"),
        _ => None,
    };
    match (plan(account, provider), state) {
        (Some((plan, _)), Some(state)) => Some(format!("{plan}, {state}")),
        (Some((plan, _)), None) => Some(plan),
        (None, Some("active")) => Some("Active".to_owned()),
        (None, Some(_)) => Some("Paused".to_owned()),
        (None, None) => None,
    }
}

fn state(account: &Account, provider: &ProviderState, now: Timestamp) -> Option<(String, Tone)> {
    match &account.status {
        AccountStatus::NeedsLogin => return None,
        AccountStatus::Stale { .. } => {
            let detail = match &account.usage {
                Some(usage) => format!("{} old", duration(now.as_second() - usage.fetched_at.as_second())),
                None => "No usage yet".to_owned(),
            };
            return Some((detail, Tone::Warning));
        }
        AccountStatus::Pending => return Some(("Checking usage".to_owned(), Tone::Muted)),
        AccountStatus::Ready if !account.enabled => return Some(("Paused".to_owned(), Tone::Muted)),
        AccountStatus::Ready => {}
    }
    let in_use = account.active || !provider.switchable;
    in_use.then(|| reached_limit(account)).flatten()
}

fn reached_limit(account: &Account) -> Option<(String, Tone)> {
    let mut windows = account.usage.iter().flat_map(|usage| &usage.windows);
    let reached = windows.find(|window| window.used_percent.is_some_and(|used| used >= 100.0))?;
    Some((format!("{} limit reached", reached.label), Tone::Danger))
}

fn plan(account: &Account, provider: &ProviderState) -> Option<(String, Tone)> {
    let organization = account.identity.organization.as_deref().filter(|_| shares_email(account, provider));
    let parts: Vec<&str> =
        account.plan.as_deref().into_iter().chain(organization).filter(|part| !part.is_empty()).collect();
    (!parts.is_empty()).then(|| (parts.join(", "), Tone::Muted))
}

fn shares_email(account: &Account, provider: &ProviderState) -> bool {
    provider.accounts.iter().any(|other| other.id != account.id && other.identity.email == account.identity.email)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn percentages_round_and_handle_missing_values() {
        assert_eq!(percent(Some(96.6)), "97%");
        assert_eq!(percent(Some(-3.0)), "0%");
        assert_eq!(percent(None), "--");
        assert_eq!(percent(Some(f64::NAN)), "--");
    }

    #[test]
    fn severity_follows_the_used_percentage() {
        assert_eq!(severity(74.9), Severity::Normal);
        assert_eq!(severity(75.0), Severity::Warning);
        assert_eq!(severity(89.9), Severity::Warning);
        assert_eq!(severity(90.0), Severity::Danger);
    }

    #[test]
    fn durations_use_the_two_largest_units() {
        assert_eq!(duration(30), "<1m");
        assert_eq!(duration(-5), "<1m");
        assert_eq!(duration(12 * 60), "12m");
        assert_eq!(duration(3600), "1h");
        assert_eq!(duration(3720), "1h 2m");
        assert_eq!(duration(2 * 86400), "2d");
        assert_eq!(duration(3 * 86400 + 4 * 3600 + 59), "3d 4h");
    }

    #[test]
    fn ages_and_summaries_read_naturally() {
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        assert_eq!(age(now, now), "just now");
        assert_eq!(age("2026-11-24T11:58:00Z".parse().unwrap(), now), "2m ago");
        let window = UsageWindow {
            id: "five_hour".into(),
            label: "Session".into(),
            used_percent: Some(97.0),
            resets_at: Some("2026-11-24T13:02:00Z".parse().unwrap()),
            primary: true,
            period_seconds: None,
        };
        assert_eq!(window_summary(&window, now), "Session 97% used, resets in 1h 2m");
    }

    fn session(used: f64, resets_in_minutes: i64) -> UsageWindow {
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        UsageWindow {
            id: "five_hour".into(),
            label: "Session".into(),
            used_percent: Some(used),
            resets_at: Some(now + jiff::SignedDuration::from_mins(resets_in_minutes)),
            primary: true,
            period_seconds: Some(5 * 3600),
        }
    }

    #[test]
    fn reset_times_switch_between_countdown_and_local_time() {
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let zone = TimeZone::get("America/New_York").unwrap();
        let exact = |minutes| reset_time(&session(10.0, minutes), now, &zone, true).unwrap();
        assert_eq!(reset_time(&session(10.0, 103), now, &zone, false).unwrap(), "1h 43m");
        assert_eq!(exact(103), "08:43");
        assert_eq!(exact(20 * 60), "Wed 03:00");
        assert_eq!(exact(7 * 24 * 60), "Dec 1");
    }

    #[test]
    fn pace_is_the_elapsed_part_of_the_window() {
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        assert_eq!(pace(&session(50.0, 180), now), Some(0.4));
        let unknown = UsageWindow { period_seconds: None, ..session(50.0, 180) };
        assert!(pace(&unknown, now).is_none());
    }

    #[test]
    fn details_show_plan_then_status() {
        let now = fixtures::now();
        let snapshot = fixtures::several_accounts();
        let provider = &snapshot.providers[0];
        let details: Vec<_> = provider.accounts.iter().map(|account| account_details(account, provider, now)).collect();
        let expected = [
            vec![("Max", Tone::Muted)],
            vec![("Pro", Tone::Muted)],
            vec![("Pro", Tone::Muted), ("2m old", Tone::Warning)],
            vec![("Signed out", Tone::Danger)],
            vec![("Pro", Tone::Muted), ("Paused", Tone::Muted)],
            vec![("Pro", Tone::Muted), ("Checking usage", Tone::Muted)],
        ];
        let shown: Vec<Vec<_>> = details
            .iter()
            .map(|details: &Vec<(String, Tone)>| details.iter().map(|(text, tone)| (text.as_str(), *tone)).collect())
            .collect();
        assert_eq!(shown, expected);
    }

    #[test]
    fn reached_limits_show_only_for_accounts_in_use() {
        let now = fixtures::now();
        let mut provider = fixtures::two_providers().providers.remove(0);
        provider.accounts[1].usage.as_mut().unwrap().windows[0].used_percent = Some(100.0);
        let details = |provider: &ProviderState| account_details(&provider.accounts[1], provider, now);
        assert_eq!(details(&provider).len(), 1);
        provider.switchable = false;
        assert_eq!(details(&provider)[1], ("Session limit reached".to_owned(), Tone::Danger));
    }

    #[test]
    fn organizations_show_only_for_shared_emails() {
        let now = fixtures::now();
        let mut provider = fixtures::two_providers().providers.remove(0);
        provider.accounts[1].identity.organization = Some("Acme".into());
        assert_eq!(account_details(&provider.accounts[1], &provider, now)[0].0, "Pro");
        provider.accounts[0].identity.email = provider.accounts[1].identity.email.clone();
        assert_eq!(account_details(&provider.accounts[1], &provider, now)[0].0, "Pro, Acme");
    }

    #[test]
    fn reset_expiry_formats_countdown_and_local_date() {
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let zone = TimeZone::UTC;
        let in_two_weeks = now + jiff::SignedDuration::from_hours(14 * 24 + 23);
        assert_eq!(reset_expiry_countdown(in_two_weeks, now), "14d 23h");
        assert_eq!(reset_expiry_date(in_two_weeks, &zone), "Dec 9, 11:00");
        let in_hundred_days = now + jiff::SignedDuration::from_hours(100 * 24);
        assert_eq!(reset_expiry_countdown(in_hundred_days, now), "3mo 10d");
    }
}
