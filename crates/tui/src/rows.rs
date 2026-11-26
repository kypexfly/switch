use jiff::Timestamp;
use jiff::tz::TimeZone;
use opentui::prelude::*;
use switch_protocol::{Account, AccountStatus, LimitReset, ProviderState, Snapshot, UsageWindow};
use unicode_width::UnicodeWidthStr;

use crate::format::{
    Severity, age, countdown, filled_cells, pace_cell, pad, percent, reset_time, severity, truncate, wrap,
};
use crate::model::{Connection, Model, Target, has_own_row};

const INDENT: usize = 4;
const GAP: &str = "  ";
const PERCENT_WIDTH: usize = 4;
const RESET_WIDTH: usize = 18;
const WINDOW_LABEL_MAX: usize = 16;
const BAR_MIN: usize = 6;
const BAR_MAX: usize = 36;

pub struct Row {
    pub spans: Vec<Span<'static>>,
    pub target: Option<Target>,
}

impl Row {
    fn new(spans: Vec<Span<'static>>) -> Self {
        Self { spans, target: None }
    }

    fn blank() -> Self {
        Self::new(Vec::new())
    }
}

pub fn dashboard(model: &Model, theme: &Theme, now: Timestamp, zone: &TimeZone, width: usize) -> Vec<Row> {
    let Some(snapshot) = &model.snapshot else {
        let text = match model.connection {
            Connection::Connecting => "Connecting to the server",
            Connection::Connected | Connection::Reconnecting => "Waiting for the server",
        };
        return vec![Row::new(vec![muted(format!(" {text}"))])];
    };
    let page = Page { model, theme, now, zone, width, window_label_width: window_label_width(snapshot) };
    let mut rows: Vec<Row> = Vec::new();
    for warning in &snapshot.warnings {
        rows.extend(page.indented(warning, 1, Style::new().fg(theme.warning)));
    }
    if !snapshot.warnings.is_empty() {
        rows.push(Row::blank());
    }
    if snapshot.providers.iter().all(|provider| provider.accounts.is_empty()) {
        rows.push(Row::new(vec![" No accounts yet".bold()]));
        let hint = "Sign in with the provider's CLI, then press a to add that login.";
        rows.extend(page.indented(hint, 1, Style::new().dim()));
        rows.push(Row::blank());
    }
    for provider in &snapshot.providers {
        rows.extend(page.provider(provider));
    }
    while rows.last().is_some_and(|row| row.spans.is_empty()) {
        rows.pop();
    }
    rows
}

pub fn reset_picker(
    label: &str,
    grants: &[LimitReset],
    selected: usize,
    theme: &Theme,
    now: Timestamp,
    zone: &TimeZone,
    width: usize,
) -> Vec<Row> {
    let mut rows =
        vec![Row::new(vec![format!(" Limit resets for {}", truncate(label, width.saturating_sub(20))).bold()])];
    rows.push(Row::blank());
    for (index, grant) in grants.iter().enumerate() {
        let marker = if index == selected { "▌ ".fg(theme.accent) } else { Span::raw(GAP) };
        let title = format!("{} {}", grant.remaining, grant.label);
        let title_span = if grant.usable { title.bold() } else { muted(title) };
        let mut line = vec![marker, title_span];
        if let Some(expires_at) = grant.expires_at {
            let local = expires_at.to_zoned(zone.clone());
            let span = countdown(expires_at, now).replace("resets in ", "");
            line.push(muted(format!("  expires {} ({span})", local.strftime("%b %-d %-I:%M %p"))));
        }
        if !grant.usable {
            line.push("  unavailable".fg(theme.warning));
        }
        rows.push(Row::new(line));
        if let Some(detail) = &grant.detail {
            for wrapped in wrap(detail, width.saturating_sub(INDENT + 2)) {
                rows.push(Row::new(vec![Span::raw("    "), muted(wrapped)]));
            }
        }
    }
    rows
}

pub fn help() -> Vec<Row> {
    [
        ("up/down, j/k", "Select an account"),
        ("enter", "Switch to the selected account"),
        ("n", "Switch to the next account of this provider"),
        ("b", "Switch to the best account of this provider"),
        ("a", "Add this provider's current login"),
        ("r", "Refresh usage"),
        ("e", "Pause or resume the account for switching"),
        ("m", "Rename the account, empty clears the alias"),
        ("u", "Use a banked limit reset"),
        ("x", "Remove the account"),
        ("J / K", "Move the account down or up"),
        ("] / [", "Move this provider down or up"),
        ("s", "Turn auto switch on or off"),
        ("+ / -", "Change the auto switch threshold"),
        ("t", "Toggle reset countdown and local time"),
        ("esc, ?", "Close this help"),
        ("q, ctrl+c", "Quit"),
    ]
    .into_iter()
    .map(|(keys, description)| Row::new(vec![format!(" {}", pad(keys, 14)).bold(), description.into()]))
    .collect()
}

struct Page<'a> {
    model: &'a Model,
    theme: &'a Theme,
    now: Timestamp,
    zone: &'a TimeZone,
    width: usize,
    window_label_width: usize,
}

impl Page<'_> {
    fn provider(&self, provider: &ProviderState) -> Vec<Row> {
        let count = match provider.accounts.len() {
            1 => "1 account".to_owned(),
            count => format!("{count} accounts"),
        };
        let name = truncate(&provider.name, self.width.saturating_sub(count.len() + 4));
        let mut rows = vec![Row::new(vec![format!(" {name}").bold(), muted(format!("{GAP}{count}"))])];
        if has_own_row(provider) {
            let target = Target::Provider { id: provider.id.clone() };
            rows.extend(self.selectable(target, self.provider_note(provider)));
            rows.push(Row::blank());
        }
        for account in &provider.accounts {
            let target = Target::Account { id: account.id.clone() };
            rows.extend(self.selectable(target, self.account(account, provider)));
            rows.push(Row::blank());
        }
        rows
    }

    fn provider_note(&self, provider: &ProviderState) -> Vec<Row> {
        let Some(login) = provider.unsaved_login.as_ref().filter(|_| provider.switchable) else {
            let text = if provider.sign_in {
                format!("{GAP}No accounts saved - run swx login {}", provider.id)
            } else {
                format!("{GAP}No accounts saved")
            };
            return vec![Row::new(vec![muted(text)])];
        };
        let selected = self.model.selected_provider().is_some_and(|selected| selected.id == provider.id);
        let hint = if selected { "press a to save it" } else { "select it and press a to save it" };
        let text = format!("Unsaved login {} - {hint}", login.email);
        self.indented(&text, INDENT - 2, Style::new().fg(self.theme.warning))
    }

    fn account(&self, account: &Account, provider: &ProviderState) -> Vec<Row> {
        let mut rows = vec![self.title(account)];
        rows.extend(self.details(account));
        match &account.status {
            AccountStatus::Stale { message } => {
                rows.extend(self.indented(message, INDENT - 2, Style::new().fg(self.theme.warning)));
            }
            AccountStatus::NeedsLogin => {
                let text = if provider.sign_in {
                    format!("Sign in to {} again with swx login {}.", provider.name, provider.id)
                } else {
                    format!("Sign in to {} again with its CLI, then press a.", provider.name)
                };
                rows.extend(self.indented(&text, INDENT - 2, Style::new().fg(self.theme.error)));
            }
            AccountStatus::Pending | AccountStatus::Ready => {}
        }
        let windows = account.usage.as_ref().map(|usage| usage.windows.as_slice()).unwrap_or_default();
        if windows.is_empty() {
            rows.push(Row::new(vec![muted(format!("{GAP}No usage reported yet"))]));
        }
        rows.extend(windows.iter().map(|window| self.window(window)));
        let resets: Vec<&LimitReset> = account.resets.iter().filter(|reset| reset.available(self.now)).collect();
        if !resets.is_empty() {
            rows.push(self.resets_summary(&resets));
        }
        rows
    }

    fn resets_summary(&self, resets: &[&LimitReset]) -> Row {
        let total: u32 = resets.iter().map(|reset| reset.remaining).sum();
        let names: Vec<String> = resets
            .iter()
            .map(|reset| {
                let mut part = format!("{} {}", reset.remaining, reset.label);
                if let Some(expires_at) = reset.expires_at {
                    let span = countdown(expires_at, self.now).replace("resets in ", "expires in ");
                    part.push_str(&format!(" ({span})"));
                }
                if !reset.usable {
                    part.push_str(" [unavailable]");
                }
                part
            })
            .collect();
        let summary = format!("Limit resets: {total} available - {}", names.join(", "));
        Row::new(vec![muted(format!("{GAP}{}", truncate(&summary, self.width.saturating_sub(INDENT))))])
    }

    fn title(&self, account: &Account) -> Row {
        let theme = self.theme;
        let mut tags = Vec::new();
        if account.active {
            tags.push(format!("{GAP}active").fg(theme.success));
        }
        match account.status {
            AccountStatus::Ready => {}
            AccountStatus::Pending => tags.push(muted(format!("{GAP}pending"))),
            AccountStatus::Stale { .. } => tags.push(format!("{GAP}stale").fg(theme.warning)),
            AccountStatus::NeedsLogin => tags.push(format!("{GAP}needs login").fg(theme.error)),
        }
        if !account.enabled {
            tags.push(muted(format!("{GAP}paused")));
        }
        if let Some(plan) = &account.plan {
            tags.push(muted(format!("{GAP}{}", truncate(plan, WINDOW_LABEL_MAX))));
        }
        let marker = if account.active { "● ".fg(theme.success) } else { Span::raw(GAP) };
        let tags_width: usize = tags.iter().map(|tag| tag.content.width()).sum();
        let label = truncate(account.label(), self.width.saturating_sub(INDENT + tags_width));
        let mut spans = vec![marker, label.bold()];
        spans.append(&mut tags);
        Row::new(spans)
    }

    fn details(&self, account: &Account) -> Option<Row> {
        let email = account.alias.is_some().then_some(account.identity.email.as_str());
        let organization = account.identity.organization.as_deref();
        let owner = email.into_iter().chain(organization).collect::<Vec<_>>().join(" - ");
        let age = account.usage.as_ref().map(|usage| age(usage.fetched_at, self.now)).unwrap_or_default();
        let separator = if owner.is_empty() || age.is_empty() { "" } else { " - " };
        let room = self.width.saturating_sub(INDENT + separator.len() + age.len());
        let text = format!("{}{separator}{age}", truncate(&owner, room));
        (!text.is_empty()).then(|| Row::new(vec![muted(format!("{GAP}{text}"))]))
    }

    fn window(&self, window: &UsageWindow) -> Row {
        let taken = INDENT + self.window_label_width + 3 * GAP.len() + PERCENT_WIDTH + RESET_WIDTH;
        let bar_width = self.width.saturating_sub(taken).clamp(BAR_MIN, BAR_MAX);
        let filled = window.used_percent.map_or(0, |used| filled_cells(used, bar_width));
        let color = match window.used_percent.map_or(Severity::Normal, severity) {
            Severity::Normal => self.theme.accent,
            Severity::Warning => self.theme.warning,
            Severity::Danger => self.theme.error,
        };
        let quiet = if window.primary { Style::new() } else { Style::new().dim() };
        let tick = pace_cell(window, self.now, bar_width);
        let reset = reset_time(window, self.now, self.zone, self.model.exact_resets);
        let mut spans = vec![Span::styled(format!("{GAP}{}{GAP}", pad(&window.label, self.window_label_width)), quiet)];
        spans.extend(bar_spans(filled, bar_width, tick, quiet, color));
        spans.push(Span::styled(format!("{GAP}{:>PERCENT_WIDTH$}{GAP}", percent(window.used_percent)), quiet));
        spans.push(muted(reset));
        Row::new(spans)
    }

    fn selectable(&self, target: Target, rows: Vec<Row>) -> Vec<Row> {
        let selected = self.model.selected.as_ref() == Some(&target);
        rows.into_iter()
            .map(|row| {
                let gutter = if selected { "▌ ".fg(self.theme.accent) } else { Span::raw(GAP) };
                let spans = std::iter::once(gutter).chain(row.spans).collect();
                Row { spans, target: Some(target.clone()) }
            })
            .collect()
    }

    fn indented(&self, text: &str, indent: usize, style: Style) -> Vec<Row> {
        let gutter = INDENT + 1;
        wrap(text, self.width.saturating_sub(gutter))
            .into_iter()
            .map(|line| Row::new(vec![Span::styled(format!("{}{line}", " ".repeat(indent)), style)]))
            .collect()
    }
}

fn bar_spans(filled: usize, width: usize, tick: Option<usize>, quiet: Style, color: Color) -> Vec<Span<'static>> {
    let Some(pos) = tick.filter(|pos| *pos < width) else {
        return vec![Span::styled("━".repeat(filled), quiet.fg(color)), muted("─".repeat(width - filled))];
    };
    let mut spans = Vec::new();
    let left_filled = filled.min(pos);
    if left_filled > 0 {
        spans.push(Span::styled("━".repeat(left_filled), quiet.fg(color)));
    }
    if pos > left_filled {
        spans.push(muted("─".repeat(pos - left_filled)));
    }
    if pos < filled {
        spans.push(Span::styled("┃", quiet.fg(color).bold()));
    } else {
        spans.push(Span::styled("│", Style::new().bold()));
    }
    let right_start = pos + 1;
    if right_start < filled {
        spans.push(Span::styled("━".repeat(filled - right_start), quiet.fg(color)));
    }
    let right_unfilled_start = right_start.max(filled);
    if right_unfilled_start < width {
        spans.push(muted("─".repeat(width - right_unfilled_start)));
    }
    spans
}

fn muted(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), Style::new().dim())
}

fn window_label_width(snapshot: &Snapshot) -> usize {
    let accounts = snapshot.providers.iter().flat_map(|provider| &provider.accounts);
    let windows = accounts.filter_map(|account| account.usage.as_ref()).flat_map(|usage| &usage.windows);
    windows.map(|window| window.label.width()).max().unwrap_or(0).min(WINDOW_LABEL_MAX)
}
