use std::io::IsTerminal;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use switch_protocol::{Account, AccountStatus, LimitReset, Snapshot, UsageWindow};

const BAR_WIDTH: usize = 10;

pub struct Style {
    color: bool,
}

impl Style {
    pub fn detect() -> Self {
        Self { color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color && !text.is_empty() { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() }
    }

    fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    fn green(&self, text: &str) -> String {
        self.paint("32", text)
    }

    fn yellow(&self, text: &str) -> String {
        self.paint("33", text)
    }

    fn red(&self, text: &str) -> String {
        self.paint("31", text)
    }

    fn by_usage(&self, used_percent: Option<f64>, text: &str) -> String {
        match used_percent {
            Some(used) if used >= 90.0 => self.red(text),
            Some(used) if used >= 75.0 => self.yellow(text),
            _ => text.to_owned(),
        }
    }
}

pub struct Clock {
    pub now: Timestamp,
    pub zone: TimeZone,
}

impl Clock {
    pub fn system() -> Self {
        Self { now: Timestamp::now(), zone: TimeZone::system() }
    }
}

pub fn switched(account: &Account, style: &Style) -> String {
    let who = match &account.alias {
        Some(alias) => format!("{alias} ({})", account.identity.email),
        None => account.identity.email.clone(),
    };
    format!("{} {}", style.green("Switched to"), style.bold(&who))
}

pub fn restart_hint(provider: &str, style: &Style) -> String {
    style.dim(&format!("New {provider} sessions use this account. Restart running ones to switch them now."))
}

pub fn reset_used(account: &Account, reset_label: &str, style: &Style) -> String {
    format!("{} {} on {}", style.green("Used"), style.bold(reset_label), style.bold(account.label()))
}

pub fn snapshot(snapshot: &Snapshot, style: &Style, clock: &Clock) -> String {
    let mut out = String::new();
    let mut number = 0;
    let accounts = snapshot.providers.iter().flat_map(|provider| &provider.accounts);
    let columns =
        Columns::measure(accounts.flat_map(|account| account.usage.iter().flat_map(|usage| &usage.windows)), clock);
    for provider in &snapshot.providers {
        out += &format!("{}\n", style.bold(&provider.name));
        for account in &provider.accounts {
            number += 1;
            account_lines(&mut out, number, account, &columns, style, clock);
            out.push('\n');
        }
        if let Some(login) = provider.unsaved_login.as_ref().filter(|_| provider.switchable) {
            let action = format!("not saved yet, run `swx add {}`", provider.id);
            out += &format!("   {}  {}  {}\n\n", style.yellow("+"), login.email, style.dim(&action));
        } else if provider.accounts.is_empty() {
            let action = if provider.sign_in {
                format!("No accounts. Run `swx login {}` to sign in.", provider.id)
            } else {
                format!("No accounts. Sign in with {}, then run `swx add {}`.", provider.name, provider.id)
            };
            out += &format!("   {}\n\n", style.dim(&action));
        }
    }
    for warning in &snapshot.warnings {
        out += &format!("{} {warning}\n", style.yellow("Warning:"));
    }
    out.trim_end().to_owned() + "\n"
}

struct Columns {
    label: usize,
    resets: usize,
    countdown: usize,
}

impl Columns {
    fn measure<'a>(windows: impl Iterator<Item = &'a UsageWindow>, clock: &Clock) -> Self {
        let mut columns = Self { label: 0, resets: 0, countdown: 0 };
        for window in windows {
            columns.label = columns.label.max(window.label.chars().count());
            columns.resets = columns.resets.max(resets_at(window, clock).chars().count());
            columns.countdown = columns.countdown.max(countdown(window, clock).chars().count());
        }
        columns
    }
}

fn account_lines(out: &mut String, number: usize, account: &Account, columns: &Columns, style: &Style, clock: &Clock) {
    let mut title = style.bold(account.label());
    if account.alias.is_some() {
        title += &format!(" {}", style.dim(&format!("({})", account.identity.email)));
    }
    if account.active {
        title += &format!(" {}", style.bold(&style.green("active")));
    }
    let details: Vec<String> = [
        account.plan.clone(),
        account.identity.organization.clone().filter(|organization| !organization.ends_with("'s Organization")),
    ]
    .into_iter()
    .flatten()
    .chain(status(account, style))
    .collect();
    let mut line = format!("  {:>2}  {title}", style.dim(&number.to_string()));
    if !details.is_empty() {
        line += &format!(" {} {}", style.dim("·"), details.join(&format!(" {} ", style.dim("·"))));
    }
    *out += &(line + "\n");

    let available_resets: Vec<&LimitReset> = account.resets.iter().filter(|reset| reset.available(clock.now)).collect();
    let Some(usage) = &account.usage else {
        *out += &format!("      {}\n", style.dim("no usage yet"));
        render_resets(out, &available_resets, style, clock);
        return;
    };
    let count = usage.windows.len();
    for (index, window) in usage.windows.iter().enumerate() {
        let last_window = index + 1 == count;
        let last_row = last_window && available_resets.is_empty();
        let branch = style.dim(if last_row { "└" } else { "├" });
        let percent = match window.used_percent {
            Some(used) => format!("{:>4}", format!("{}%", used.floor())),
            None => "  --".to_owned(),
        };
        let percent = style.by_usage(window.used_percent, &percent);
        let label = format!("{:<width$}", window.label, width = columns.label);
        let label = if window.primary { label } else { style.dim(&label) };
        let resets = format!("{:<width$}", resets_at(window, clock), width = columns.resets);
        let countdown = format!("{:<width$}", countdown(window, clock), width = columns.countdown);
        let mut line = format!("      {branch} {label}  {percent}  {}  {resets}  {countdown}", bar(window, style));
        if last_window {
            line += &format!("  {}", style.dim(&age(usage.fetched_at, clock.now)));
        }
        *out += &(line.trim_end().to_owned() + "\n");
    }
    render_resets(out, &available_resets, style, clock);
}

fn render_resets(out: &mut String, resets: &[&LimitReset], style: &Style, clock: &Clock) {
    let count = resets.len();
    for (index, reset) in resets.iter().enumerate() {
        let branch = style.dim(if index + 1 == count { "└" } else { "├" });
        let mut parts = vec![format!("{} {}", reset.remaining, reset.label)];
        if let Some(expires_at) = reset.expires_at {
            parts.push(format!("expires {} ({})", local_time(expires_at, clock), in_span(expires_at, clock)));
        }
        if !reset.usable {
            parts.push("unavailable".to_owned());
        }
        *out += &format!("      {branch} {}\n", style.dim(&parts.join(" · ")));
    }
}

fn status(account: &Account, style: &Style) -> Option<String> {
    let status = match &account.status {
        AccountStatus::Ready => None,
        AccountStatus::Pending => Some(style.dim("waiting for usage")),
        AccountStatus::Stale { message } => Some(style.yellow(message)),
        AccountStatus::NeedsLogin => Some(style.red("sign in again")),
    };
    if account.enabled { status } else { Some(style.dim("paused")) }
}

fn bar(window: &UsageWindow, style: &Style) -> String {
    let filled = match window.used_percent {
        Some(used) if used >= 100.0 => BAR_WIDTH,
        Some(used) if used > 0.0 => ((used / 100.0 * BAR_WIDTH as f64).round() as usize).clamp(1, BAR_WIDTH - 1),
        _ => 0,
    };
    let used = style.by_usage(window.used_percent, &"━".repeat(filled));
    format!("{used}{}", style.dim(&"─".repeat(BAR_WIDTH - filled)))
}

fn resets_at(window: &UsageWindow, clock: &Clock) -> String {
    let Some(resets_at) = window.resets_at else { return String::new() };
    format!("resets {}", local_time(resets_at, clock))
}

fn local_time(at: Timestamp, clock: &Clock) -> String {
    let local = at.to_zoned(clock.zone.clone());
    let format = if local.date() == clock.now.to_zoned(clock.zone.clone()).date() { "%H:%M" } else { "%b %-d %H:%M" };
    local.strftime(format).to_string()
}

fn countdown(window: &UsageWindow, clock: &Clock) -> String {
    match window.resets_at {
        Some(resets_at) => in_span(resets_at, clock),
        None => String::new(),
    }
}

fn in_span(at: Timestamp, clock: &Clock) -> String {
    if at > clock.now { format!("in {}", span(at.as_second() - clock.now.as_second())) } else { "now".to_owned() }
}

fn age(fetched_at: Timestamp, now: Timestamp) -> String {
    match now.as_second() - fetched_at.as_second() {
        ..60 => "just now".to_owned(),
        seconds => format!("{} ago", span(seconds)),
    }
}

fn span(seconds: i64) -> String {
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3_600 % 24, seconds / 60 % 60);
    match (days, hours) {
        (0, 0) => format!("{}m", minutes.max(1)),
        (0, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switch_protocol::{Identity, ProviderState, Settings, Usage};

    fn at(second: i64) -> Timestamp {
        Timestamp::from_second(second).unwrap()
    }

    fn window(label: &str, used: Option<f64>, resets_in: Option<i64>, primary: bool) -> UsageWindow {
        let now = 1_764_032_400;
        UsageWindow {
            id: label.into(),
            label: label.into(),
            used_percent: used,
            resets_at: resets_in.map(|seconds| at(now + seconds)),
            primary,
            period_seconds: None,
        }
    }

    fn account(email: &str, active: bool, windows: Option<Vec<UsageWindow>>) -> Account {
        Account {
            id: email.into(),
            identity: Identity {
                account_id: email.into(),
                email: email.into(),
                organization_id: None,
                organization: None,
            },
            alias: None,
            plan: Some("Pro".into()),
            active,
            enabled: true,
            status: if windows.is_some() { AccountStatus::Ready } else { AccountStatus::Pending },
            usage: windows.map(|windows| Usage { windows, fetched_at: at(1_764_032_400 - 240) }),
            resets: Vec::new(),
        }
    }

    #[test]
    fn renders_aligned_windows_without_color() {
        let mut work = account(
            "work@example.com",
            false,
            Some(vec![
                window("Session", Some(64.0), Some(7_740), true),
                window("Weekly", Some(72.0), Some(283_000), true),
            ]),
        );
        work.alias = Some("work".into());
        let snapshot = Snapshot {
            providers: vec![ProviderState {
                id: "claude".into(),
                name: "Claude".into(),
                accounts: vec![
                    work,
                    account(
                        "me@example.com",
                        true,
                        Some(vec![
                            window("Session", Some(100.0), Some(8_340), true),
                            window("Weekly", Some(90.0), None, true),
                            window("Fable", None, None, false),
                        ]),
                    ),
                    account("new@example.com", false, None),
                ],
                unsaved_login: None,
                switchable: true,
                sign_in: false,
                last_auto_switch_at: None,
            }],
            settings: Settings::default(),
            warnings: vec!["Keychain is locked".into()],
        };
        let clock = Clock { now: at(1_764_032_400), zone: TimeZone::UTC };

        let text = snapshot_text(&snapshot, &clock);

        let expected = "\
Claude
   1  work (work@example.com) · Pro
      ├ Session   64%  ━━━━━━────  resets 03:09         in 2h 9m
      └ Weekly    72%  ━━━━━━━───  resets Nov 28 07:36  in 3d 6h   4m ago

   2  me@example.com active · Pro
      ├ Session  100%  ━━━━━━━━━━  resets 03:19         in 2h 19m
      ├ Weekly    90%  ━━━━━━━━━─
      └ Fable      --  ──────────                                  4m ago

   3  new@example.com · Pro · waiting for usage
      no usage yet

Warning: Keychain is locked
";
        assert_eq!(text, expected, "\n{text}");
    }

    fn snapshot_text(value: &Snapshot, clock: &Clock) -> String {
        snapshot(value, &Style { color: false }, clock)
    }

    #[test]
    fn a_provider_without_accounts_says_how_to_add_one() {
        let provider = |id: &str, name: &str, sign_in: bool| ProviderState {
            id: id.into(),
            name: name.into(),
            accounts: Vec::new(),
            unsaved_login: None,
            switchable: !sign_in,
            sign_in,
            last_auto_switch_at: None,
        };
        let snapshot = Snapshot {
            providers: vec![provider("claude", "Claude", false), provider("antigravity", "Antigravity", true)],
            settings: Settings::default(),
            warnings: Vec::new(),
        };
        let expected = "\
Claude
   No accounts. Sign in with Claude, then run `swx add claude`.

Antigravity
   No accounts. Run `swx login antigravity` to sign in.
";
        assert_eq!(snapshot_text(&snapshot, &Clock { now: at(0), zone: TimeZone::UTC }), expected);
    }

    #[test]
    fn spans_use_the_two_largest_units() {
        assert_eq!(span(30), "1m");
        assert_eq!(span(7_740), "2h 9m");
        assert_eq!(span(283_000), "3d 6h");
    }

    #[test]
    fn renders_available_resets() {
        let now = 1_764_032_400;
        let mut work = account("work@example.com", true, Some(vec![window("Session", Some(80.0), Some(3_600), true)]));
        work.resets = vec![
            LimitReset {
                id: "full".into(),
                label: "Full Reset".into(),
                remaining: 1,
                expires_at: Some(at(now + 10 * 86_400)),
                usable: true,
                detail: None,
            },
            LimitReset {
                id: "fable".into(),
                label: "Fable Reset".into(),
                remaining: 2,
                expires_at: Some(at(now + 20 * 86_400)),
                usable: false,
                detail: None,
            },
            LimitReset {
                id: "expired".into(),
                label: "Expired Reset".into(),
                remaining: 1,
                expires_at: Some(at(now - 60)),
                usable: true,
                detail: None,
            },
        ];
        let snapshot = Snapshot {
            providers: vec![ProviderState {
                id: "claude".into(),
                name: "Claude".into(),
                accounts: vec![work],
                unsaved_login: None,
                switchable: true,
                sign_in: false,
                last_auto_switch_at: None,
            }],
            settings: Settings::default(),
            warnings: Vec::new(),
        };
        let text = snapshot_text(&snapshot, &Clock { now: at(now), zone: TimeZone::UTC });
        assert!(text.contains("1 Full Reset · expires Dec 5 01:00 (in 10d 0h)"), "\n{text}");
        assert!(text.contains("2 Fable Reset · expires Dec 15 01:00 (in 20d 0h) · unavailable"), "\n{text}");
        assert!(!text.contains("Expired Reset"), "\n{text}");
    }
}
