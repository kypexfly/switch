use jiff::Timestamp;
use jiff::tz::TimeZone;
use switch_protocol::UsageWindow;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Normal,
    Warning,
    Danger,
}

pub fn severity(used_percent: f64) -> Severity {
    if used_percent >= 90.0 {
        Severity::Danger
    } else if used_percent >= 75.0 {
        Severity::Warning
    } else {
        Severity::Normal
    }
}

pub fn percent(used_percent: Option<f64>) -> String {
    used_percent.map_or_else(|| "--".to_owned(), |used| format!("{}%", used.floor()))
}

pub fn filled_cells(used_percent: f64, width: usize) -> usize {
    if used_percent <= 0.0 || width == 0 {
        return 0;
    }
    if used_percent >= 100.0 {
        return width;
    }
    let cells = (used_percent / 100.0 * width as f64).round() as usize;
    cells.clamp(1, width.saturating_sub(1).max(1))
}

pub fn pace_cell(window: &UsageWindow, now: Timestamp, width: usize) -> Option<usize> {
    if width == 0 {
        return None;
    }
    let period = window.period_seconds.filter(|period| *period > 0)?;
    let remaining = window.resets_at?.as_second() - now.as_second();
    if remaining <= 0 {
        return None;
    }
    let elapsed = (period - remaining).clamp(0, period) as f64 / period as f64;
    Some(((elapsed * width as f64).floor() as usize).min(width - 1))
}

pub fn reset_time(window: &UsageWindow, now: Timestamp, zone: &TimeZone, exact: bool) -> String {
    let Some(resets_at) = window.resets_at else {
        return String::new();
    };
    if !exact {
        return countdown(resets_at, now);
    }
    let seconds = resets_at.as_second() - now.as_second();
    let local = resets_at.to_zoned(zone.clone());
    let fmt = if local.date() == now.to_zoned(zone.clone()).date() {
        "%H:%M"
    } else if seconds < 6 * 86_400 {
        "%a %H:%M"
    } else {
        "%b %-d"
    };
    format!("resets {}", local.strftime(fmt))
}

pub fn countdown(resets_at: Timestamp, now: Timestamp) -> String {
    let seconds = resets_at.as_second() - now.as_second();
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3_600 % 24, seconds / 60 % 60);
    if seconds <= 0 {
        "reset due".to_owned()
    } else if days > 0 {
        format!("resets in {days}d {hours}h")
    } else if hours > 0 {
        format!("resets in {hours}h {minutes}m")
    } else if minutes > 0 {
        format!("resets in {minutes}m")
    } else {
        "resets in <1m".to_owned()
    }
}

pub fn age(fetched_at: Timestamp, now: Timestamp) -> String {
    let seconds = now.as_second() - fetched_at.as_second();
    match seconds {
        ..60 => "updated just now".to_owned(),
        60..3_600 => format!("updated {}m ago", seconds / 60),
        3_600..86_400 => format!("updated {}h ago", seconds / 3_600),
        _ => format!("updated {}d ago", seconds / 86_400),
    }
}

pub fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut room = width.saturating_sub(1);
    let mut kept: String = text
        .chars()
        .take_while(|character| {
            let cells = character.width().unwrap_or(0);
            let fits = cells <= room;
            room = room.saturating_sub(cells);
            fits
        })
        .collect();
    if width > 0 {
        kept.push('…');
    }
    kept
}

pub fn pad(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    let padding = " ".repeat(width - text.width());
    text + &padding
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.width() + 1 + word.width() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(truncate(word, width)),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(second: i64) -> Timestamp {
        Timestamp::from_second(second).unwrap()
    }

    #[test]
    fn countdown_uses_the_two_largest_units() {
        for (seconds, expected) in [
            (-5, "reset due"),
            (0, "reset due"),
            (59, "resets in <1m"),
            (60, "resets in 1m"),
            (3_599, "resets in 59m"),
            (3_600, "resets in 1h 0m"),
            (7_190, "resets in 1h 59m"),
            (86_400, "resets in 1d 0h"),
            (5 * 86_400 + 12 * 3_600 + 30, "resets in 5d 12h"),
        ] {
            assert_eq!(countdown(at(1_000 + seconds), at(1_000)), expected);
        }
    }

    #[test]
    fn reset_times_use_24_hour_clock() {
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let zone = TimeZone::get("America/New_York").unwrap();
        let exact = |minutes| {
            let window = UsageWindow {
                id: "five_hour".into(),
                label: "Session".into(),
                used_percent: Some(10.0),
                resets_at: Some(now + jiff::SignedDuration::from_mins(minutes)),
                primary: true,
                period_seconds: None,
            };
            reset_time(&window, now, &zone, true)
        };
        assert_eq!(exact(103), "resets 08:43");
        assert_eq!(exact(20 * 60), "resets Wed 03:00");
        assert_eq!(exact(7 * 24 * 60), "resets Dec 1");
    }

    #[test]
    fn age_uses_the_largest_unit() {
        for (seconds, expected) in [
            (-3, "updated just now"),
            (59, "updated just now"),
            (125, "updated 2m ago"),
            (3_600, "updated 1h ago"),
            (200_000, "updated 2d ago"),
        ] {
            assert_eq!(age(at(1_000), at(1_000 + seconds)), expected);
        }
    }

    #[test]
    fn a_bar_is_only_empty_or_full_at_the_limits() {
        for (used, expected) in [(-1.0, 0), (0.0, 0), (0.2, 1), (50.0, 10), (99.9, 19), (100.0, 20), (130.0, 20)] {
            assert_eq!(filled_cells(used, 20), expected, "{used}");
        }
        assert_eq!(filled_cells(50.0, 0), 0);
        assert_eq!(filled_cells(50.0, 1), 1);
    }

    #[test]
    fn the_shown_percent_agrees_with_its_severity() {
        for (used, text, expected) in [
            (74.9, "74%", Severity::Normal),
            (75.0, "75%", Severity::Warning),
            (89.9, "89%", Severity::Warning),
            (90.0, "90%", Severity::Danger),
        ] {
            assert_eq!((percent(Some(used)).as_str(), severity(used)), (text, expected));
        }
        assert_eq!(percent(None), "--");
    }

    #[test]
    fn truncate_and_pad_count_terminal_cells() {
        assert_eq!(truncate("account", 7), "account");
        assert_eq!(truncate("account", 4), "acc…");
        assert_eq!(truncate("日本語", 4), "日…");
        assert_eq!(pad("日本", 6), "日本  ");
        assert_eq!(truncate("account", 0), "");
    }

    #[test]
    fn wrap_breaks_between_words() {
        assert_eq!(wrap("one two\nthree", 7), ["one two", "three"]);
        assert_eq!(wrap("unbreakable", 6), ["unbre…"]);
        assert!(wrap("", 6).is_empty());
    }
}
