use jiff::Timestamp;
use rune::TrayItem;
use switch_protocol::{Account, AccountStatus, ProviderState, Snapshot, UsageWindow};

use crate::format;
use crate::preferences::{Place, Preferences, UsageShown};

#[derive(Clone, Debug, PartialEq)]
pub struct TrayGroup {
    pub provider: String,
    pub columns: Vec<TrayColumn>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrayColumn {
    pub values: Vec<String>,
    pub active: bool,
}

pub fn tray_groups(snapshot: Option<&Snapshot>, preferences: &Preferences) -> Vec<TrayGroup> {
    let providers = snapshot.map_or(&[][..], |snapshot| &snapshot.providers);
    let without_accounts = providers.iter().all(|provider| provider.accounts.is_empty());
    let groups: Vec<TrayGroup> = providers
        .iter()
        .filter(|provider| preferences.provider_on(Place::MenuBar, &provider.id))
        .map(|provider| TrayGroup { provider: provider.id.clone(), columns: columns(provider, preferences) })
        .filter(|group| without_accounts || !group.columns.is_empty())
        .collect();
    if groups.is_empty() { vec![TrayGroup { provider: String::new(), columns: Vec::new() }] } else { groups }
}

fn columns(provider: &ProviderState, preferences: &Preferences) -> Vec<TrayColumn> {
    let windows = preferences.tray_windows(provider);
    preferences
        .tray_accounts(provider)
        .map(|account| {
            let used = |id: &String| account_windows(account).find(|window| window.id == *id)?.used_percent;
            let mut values: Vec<String> = windows.iter().map(|id| format::percent(used(id))).collect();
            values.resize(windows.len().max(1), format::percent(None));
            TrayColumn { values, active: account.active || !provider.switchable }
        })
        .collect()
}

fn account_windows(account: &Account) -> impl Iterator<Item = &UsageWindow> {
    account.usage.iter().flat_map(|usage| &usage.windows)
}

fn account_tray_tooltip(account: &Account, shown: &[String], usage_shown: UsageShown, now: Timestamp) -> String {
    let mut tags = Vec::new();
    if account.active {
        tags.push("active");
    }
    match account.status {
        AccountStatus::NeedsLogin => tags.push("signed out"),
        AccountStatus::Stale { .. } => tags.push("stale"),
        AccountStatus::Pending | AccountStatus::Ready => {}
    }
    if !account.enabled {
        tags.push("paused");
    }
    let label =
        if tags.is_empty() { account.label().to_owned() } else { format!("{} ({})", account.label(), tags.join(", ")) };
    let windows: Vec<String> = account_windows(account)
        .filter(|window| shown.contains(&window.id))
        .map(|window| format::window_summary(window, usage_shown, now))
        .collect();
    if windows.is_empty() { label } else { format!("{label} - {}", windows.join("; ")) }
}

pub fn tooltip(snapshot: Option<&Snapshot>, preferences: &Preferences, connected: bool, now: Timestamp) -> String {
    let providers = snapshot.map_or(&[][..], |snapshot| &snapshot.providers);
    let mut lines = Vec::new();
    for provider in providers {
        let shown = preferences.tray_windows(provider);
        for account in preferences.tray_accounts(provider) {
            lines.push(account_tray_tooltip(account, &shown, preferences.usage_shown, now));
        }
    }
    if !connected && snapshot.is_some() {
        lines.push("Server offline".to_owned());
    }
    if lines.is_empty() { "Switch".to_owned() } else { lines.join("\n") }
}

pub fn tray_preview_tooltips(snapshot: &Snapshot, preferences: &Preferences, now: Timestamp) -> Vec<Vec<String>> {
    let without_accounts = snapshot.providers.iter().all(|provider| provider.accounts.is_empty());
    let groups: Vec<Vec<String>> = snapshot
        .providers
        .iter()
        .filter(|provider| preferences.provider_on(Place::MenuBar, &provider.id))
        .map(|provider| {
            let shown = preferences.tray_windows(provider);
            preferences
                .tray_accounts(provider)
                .map(|account| account_tray_tooltip(account, &shown, preferences.usage_shown, now))
                .collect::<Vec<_>>()
        })
        .filter(|columns| without_accounts || !columns.is_empty())
        .collect();
    if groups.is_empty() { vec![Vec::new()] } else { groups }
}

pub fn menu(snapshot: Option<&Snapshot>, preferences: &Preferences, connected: bool) -> Vec<TrayItem> {
    let visible = snapshot.map(|snapshot| preferences.panel_snapshot(snapshot));
    let providers: Vec<&ProviderState> = visible
        .iter()
        .flat_map(|snapshot| &snapshot.providers)
        .filter(|provider| provider.switchable && !provider.accounts.is_empty())
        .collect();
    let mut items = Vec::new();
    for provider in &providers {
        if providers.len() > 1 {
            items.push(TrayItem::action(format!("provider:{}", provider.id), provider.name.clone()).enabled(false));
        }
        for account in &provider.accounts {
            items.push(
                TrayItem::check(format!("account:{}", account.id), account.label(), account.active).enabled(connected),
            );
        }
    }
    if !items.is_empty() {
        items.push(TrayItem::Separator);
    }
    items.extend([
        TrayItem::action("refresh", "Refresh").enabled(connected),
        TrayItem::Separator,
        TrayItem::action("quit", "Quit Switch"),
    ]);
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn column(values: &[&str], active: bool) -> TrayColumn {
        TrayColumn { values: values.iter().map(|value| value.to_string()).collect(), active }
    }

    #[test]
    fn groups_show_every_account_in_order_with_the_active_one_marked() {
        let groups = tray_groups(Some(&fixtures::several_accounts()), &Preferences::default());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].provider, "claude");
        assert_eq!(
            groups[0].columns,
            [
                column(&["97%", "16%"], true),
                column(&["20%", "61%"], false),
                column(&["5%", "3%"], false),
                column(&["--", "--"], false),
                column(&["0%", "0%"], false),
                column(&["--", "--"], false),
            ]
        );
    }

    #[test]
    fn groups_cover_every_provider_with_accounts() {
        let groups = tray_groups(Some(&fixtures::two_providers()), &Preferences::default());
        assert_eq!(groups.iter().map(|group| group.provider.as_str()).collect::<Vec<_>>(), ["claude", "codex"]);
        assert_eq!(groups[0].columns.len(), 2);
        assert_eq!(groups[1].columns, [column(&["--", "42%"], true)]);
    }

    #[test]
    fn without_accounts_only_logos_remain() {
        assert_eq!(
            tray_groups(None, &Preferences::default()),
            vec![TrayGroup { provider: String::new(), columns: vec![] }]
        );
        let groups = tray_groups(Some(&fixtures::unsaved_login()), &Preferences::default());
        assert_eq!(groups, vec![TrayGroup { provider: "claude".into(), columns: vec![] }]);
        assert_eq!(
            tray_groups(Some(&fixtures::empty()), &Preferences::default()),
            tray_groups(None, &Preferences::default())
        );
    }

    #[test]
    fn usage_only_accounts_show_at_full_strength_but_are_not_in_the_menu() {
        let snapshot = fixtures::usage_only();
        let groups = tray_groups(Some(&snapshot), &Preferences::default());
        assert_eq!(groups[1].provider, "antigravity");
        assert_eq!(groups[1].columns, [column(&["12%", "40%"], true)]);
        let items = menu(Some(&snapshot), &Preferences::default(), true);
        assert!(!items.iter().any(|item| matches!(item, TrayItem::Check { id, .. } if id == "account:g1")));
        assert_eq!(items[0], TrayItem::check("account:a1", "Work", true));
    }

    #[test]
    fn tooltip_lists_every_account_and_marks_the_active_one() {
        let tip = tooltip(Some(&fixtures::two_providers()), &Preferences::default(), true, fixtures::now());
        let lines: Vec<&str> = tip.lines().collect();
        assert_eq!(lines[0], "Work (active) - Session 97% used, resets in 1h 2m; Weekly 16% used, resets in 3d 4h");
        assert!(lines[1].starts_with("me@example.com - Session 20% used"));
        assert!(lines[2].starts_with("me@example.com (active) - "));
        assert_eq!(tooltip(None, &Preferences::default(), false, fixtures::now()), "Switch");
    }

    #[test]
    fn tooltip_respects_usage_shown_left_and_degraded_states() {
        let mut prefs = Preferences::default();
        prefs.usage_shown = UsageShown::Left;
        let tip = tooltip(Some(&fixtures::several_accounts()), &prefs, false, fixtures::now());
        let lines: Vec<&str> = tip.lines().collect();
        assert!(lines[0].contains("Session 3% left"));
        assert!(lines.iter().any(|line| line.starts_with("Side project (stale)")));
        assert!(lines.iter().any(|line| line == &"old@example.com (signed out)"));
        assert!(lines.iter().any(|line| line.starts_with("Spare (paused)")));
        assert_eq!(lines.last(), Some(&"Server offline"));

        let preview = tray_preview_tooltips(&fixtures::two_providers(), &prefs, fixtures::now());
        assert_eq!(preview.len(), 2);
        assert!(preview[0][0].starts_with("Work (active) - Session 3% left"));
    }

    #[test]
    fn menu_lists_accounts_with_the_active_one_checked() {
        let items = menu(Some(&fixtures::several_accounts()), &Preferences::default(), true);
        assert_eq!(items[0], TrayItem::check("account:a1", "Work", true));
        assert_eq!(items[1], TrayItem::check("account:a2", "me@example.com", false));
        assert!(items.contains(&TrayItem::action("quit", "Quit Switch")));
        let offline = menu(Some(&fixtures::several_accounts()), &Preferences::default(), false);
        assert_eq!(offline[0], TrayItem::check("account:a1", "Work", true).enabled(false));
    }

    #[test]
    fn the_menu_bar_and_its_menu_leave_out_what_is_hidden_there() {
        let snapshot = fixtures::two_providers();
        let mut preferences = Preferences::default();
        preferences.set_account(Place::MenuBar, "a2", false);
        preferences.set_account(Place::Panel, "a2", false);
        let groups = tray_groups(Some(&snapshot), &preferences);
        assert_eq!(groups[0].columns, [column(&["97%", "16%"], true)]);
        assert!(!tooltip(Some(&snapshot), &preferences, true, fixtures::now()).contains("20% used"));
        let menu_ids = |preferences: &Preferences| {
            let items = menu(Some(&snapshot), preferences, true);
            let checks = items.into_iter().filter_map(|item| match item {
                TrayItem::Check { id, .. } => Some(id),
                _ => None,
            });
            checks.collect::<Vec<_>>()
        };
        assert_eq!(menu_ids(&preferences), ["account:a1", "account:c1"]);
        preferences.set_provider(Place::MenuBar, "claude", false);
        preferences.set_account(Place::MenuBar, "c1", false);
        assert_eq!(tray_groups(Some(&snapshot), &preferences), tray_groups(None, &preferences));
        assert_eq!(tooltip(Some(&snapshot), &preferences, true, fixtures::now()), "Switch");
    }

    #[test]
    fn chosen_windows_show_in_the_menu_bar_and_missing_ones_keep_their_row() {
        let snapshot = fixtures::several_accounts();
        let mut preferences = Preferences::default();
        preferences.toggle_tray_window(&snapshot.providers[0], "five_hour");
        preferences.toggle_tray_window(&snapshot.providers[0], "seven_day_opus");
        let groups = tray_groups(Some(&snapshot), &preferences);
        assert_eq!(groups[0].columns[..2], [column(&["16%", "40%"], true), column(&["61%", "--"], false)]);
    }
}
