use rune::{Cursor, El, Key, KeyEvent, PopupItem, PopupMenu, TextStyle, col, el, icon, row, text, text_input};
use switch_protocol::{Account, AccountUpdate, ProviderState, Snapshot, Strategy, UsageWindow};

use crate::format;
use crate::icons;
use crate::panel::{
    BODY, DISABLED, Look, NAME, Palette, Panel, Prompt, SECTION, SMALL, Task, button, cancel, disable,
    dismissible_notice, drag_handle, empty, movable, note, notice, remove_button, sign_in_button, sign_in_hint, toggle,
};
use crate::preferences::{
    AccountScope, NotificationPreferences, Place, ResetTrigger, UsageShown, WindowScope, provider_windows,
};
use crate::reorder::List;
use crate::status::{self, TrayGroup};

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Page {
    #[default]
    Usage,
    Settings,
    AutoSwitch,
    Notifications,
    Provider(String),
    Account(String),
}

pub const GROUP_GAP: f32 = 8.0;
const ROW: f32 = 36.0;
const ACCOUNT_ROW: f32 = 40.0;
const HANDLE: f32 = 20.0;
const PANEL_COLUMN: f32 = 48.0;
const MENU_BAR_COLUMN: f32 = 64.0;
const OPEN_COLUMN: f32 = 20.0;
const TRAY_ROW: TextStyle = TextStyle::new(9.0, 600.0).lh(10.0).features("'tnum' 1");
const TRAY_SINGLE: TextStyle = TextStyle::new(12.0, 700.0).features("'tnum' 1");
const TRAY_INACTIVE: f32 = 0.45;

pub fn header(panel: &Panel, palette: &Palette) -> El {
    let title = match &panel.page {
        Page::Usage | Page::Settings => "Settings",
        Page::AutoSwitch => "Auto-switch",
        Page::Notifications => "Notifications",
        Page::Provider(id) => panel.provider(id).map_or("", |provider| &provider.name),
        Page::Account(id) => panel.account(id).map_or("", |account| account.label()),
    };
    let back = row()
        .key("back")
        .role("button")
        .label("Back")
        .focusable(true)
        .size(24.0, 24.0)
        .center()
        .radius(6.0)
        .hover_fill(palette.hover)
        .on_press_tx(|panel: &mut Panel, _| {
            panel.go_back();
        })
        .child(icon(&icons::BACK, 16.0).color(palette.text));
    let bar = row()
        .h(43.0)
        .pl(10.0)
        .pr(14.0)
        .gap(4.0)
        .center_y()
        .child(back)
        .child(text(title).font(NAME).color(palette.text).truncate().shrink().min_w(0.0));
    col().child(bar).child(el().w_full().h(1.0).fill(palette.separator))
}

pub fn body(panel: &Panel, snapshot: &Snapshot, palette: &Palette) -> Vec<El> {
    let mut items = notices(panel, palette);
    match &panel.page {
        Page::Usage | Page::Settings => items.extend(list(panel, snapshot, palette)),
        Page::AutoSwitch => items.extend(auto_switch_page(snapshot, panel, palette)),
        Page::Notifications => items.extend(notifications_page(snapshot, panel, palette)),
        Page::Provider(id) => {
            items.extend(panel.provider(id).into_iter().flat_map(|provider| provider_page(provider, panel, palette)));
        }
        Page::Account(id) => {
            let found = panel.provider_of(id).zip(panel.account(id));
            items.extend(
                found.into_iter().flat_map(|(provider, account)| account_page(account, provider, panel, palette)),
            );
        }
    }
    items
}

fn notices(panel: &Panel, palette: &Palette) -> Vec<El> {
    let mut items = Vec::new();
    if !panel.connected {
        items.push(notice("offline", "Server offline, reconnecting", palette.danger, palette));
    }
    match &panel.failed {
        Some((Task::Order, error)) => {
            let message = format!("Could not save the new order: {error}");
            items.push(dismissible_notice("error", &message, palette.danger, palette, |panel| panel.failed = None));
        }
        Some((Task::Account | Task::Settings | Task::SignIn(_), error)) => {
            items.push(dismissible_notice("error", error, palette.danger, palette, |panel| panel.failed = None));
        }
        _ => {}
    }
    if let Some(error) = &panel.preferences_error {
        items.push(notice("preferences", error, palette.danger, palette));
    }
    items
}

fn group(palette: &Palette) -> El {
    col().radius(8.0).fill(palette.active).border(1.0, palette.active_border)
}

fn list(panel: &Panel, snapshot: &Snapshot, palette: &Palette) -> Vec<El> {
    let mut items = if snapshot.providers.is_empty() {
        vec![empty("No accounts yet", Some("Log in to a supported tool, then refresh."), palette)]
    } else {
        let providers = panel.arranged(&List::Providers, &snapshot.providers, |provider| &provider.id);
        let mut items = vec![preview(panel, snapshot, palette), columns(palette)];
        items.extend(providers.into_iter().map(|provider| provider_group(provider, panel, palette)));
        items.push(notes(note(
            "Only affects this app. Hidden accounts still refresh and can be used by auto-switch. The active one always shows in the panel.",
            palette.secondary,
        )));
        items
    };
    items.push(usage_display(panel, palette));
    items.push(auto_switch_row(snapshot, palette));
    items.push(app_settings(panel, palette));
    items
}

fn usage_shown_label(shown: UsageShown) -> &'static str {
    match shown {
        UsageShown::Used => "Used",
        UsageShown::Left => "Left",
    }
}

fn usage_display(panel: &Panel, palette: &Palette) -> El {
    let shown = panel.preferences.usage_shown;
    let shown_button =
        menu_button("usage-shown", usage_shown_label(shown), "Show usage as", true, palette, move || {
            let item = |id: &str, choice: UsageShown| {
                PopupItem::new(format!("usage-shown:{id}"), usage_shown_label(choice)).checked(shown == choice)
            };
            PopupMenu::new().item(item("used", UsageShown::Used)).item(item("left", UsageShown::Left))
        });
    group(palette).child(setting("Show usage as", shown_button, palette))
}

fn auto_switch_row(snapshot: &Snapshot, palette: &Palette) -> El {
    let summary = if snapshot.settings.auto_switch {
        format!("On, {}", format::percent(Some(snapshot.settings.threshold)))
    } else {
        "Off".to_owned()
    };
    let control = row()
        .gap(4.0)
        .center_y()
        .child(text(summary).font(BODY).color(palette.secondary).nowrap())
        .child(icon(&icons::OPEN, 14.0).color(palette.secondary));
    let row = setting("Auto-switch", control, palette)
        .key("open-auto-switch")
        .role("button")
        .label("Auto-switch settings")
        .focusable(true)
        .hover_fill(palette.hover)
        .on_press_tx(|panel: &mut Panel, _| panel.open(Page::AutoSwitch));
    group(palette).child(row)
}

const COOLDOWNS: [u64; 6] = [0, 60, 300, 900, 1800, 3600];

fn cooldown_label(seconds: u64) -> String {
    if seconds == 0 {
        "Off".to_owned()
    } else if seconds < 60 {
        format!("{seconds}s")
    } else {
        format::duration(seconds as i64)
    }
}

fn strategy_label(strategy: Strategy) -> &'static str {
    match strategy {
        Strategy::Best => "Best",
        Strategy::Next => "Next",
    }
}

fn auto_switch_page(snapshot: &Snapshot, panel: &Panel, palette: &Palette) -> Vec<El> {
    let settings = &snapshot.settings;
    let connected = panel.connected;

    let enabled = settings.auto_switch;
    let auto_toggle = toggle("auto-switch-setting", "Auto-switch", enabled, palette);
    let auto_toggle = if connected {
        auto_toggle.on_press_tx(move |panel: &mut Panel, _| panel.set_auto_switch(!enabled))
    } else {
        disable(auto_toggle)
    };

    let threshold = settings.threshold;
    let lower = button("threshold-down".to_owned(), "-", "Decrease threshold", Look::Plain, palette);
    let lower = if connected && threshold > 50.0 {
        lower.on_press_tx(move |panel: &mut Panel, _| panel.set_threshold((threshold - 5.0).max(50.0)))
    } else {
        disable(lower)
    };
    let raise = button("threshold-up".to_owned(), "+", "Increase threshold", Look::Plain, palette);
    let raise = if connected && threshold < 100.0 {
        raise.on_press_tx(move |panel: &mut Panel, _| panel.set_threshold((threshold + 5.0).min(100.0)))
    } else {
        disable(raise)
    };
    let threshold_controls = row()
        .gap(6.0)
        .center_y()
        .child(lower)
        .child(text(format::percent(Some(threshold))).font(BODY).color(palette.text).nowrap())
        .child(raise);

    let cooldown = settings.cooldown_seconds;
    let cooldown_btn =
        button("cooldown".to_owned(), &cooldown_label(cooldown), "Switch cooldown", Look::Plain, palette);
    let cooldown_btn = if connected {
        cooldown_btn.on_press_tx(move |_: &mut Panel, tx| {
            let mut menu = PopupMenu::new();
            for value in COOLDOWNS {
                menu = menu.item(
                    PopupItem::new(format!("cooldown:{value}"), cooldown_label(value)).checked(value == cooldown),
                );
            }
            tx.popup_menu(menu, "cooldown", |panel: &mut Panel, picked, _| {
                if let Some(id) = picked {
                    panel.on_menu(id);
                }
            });
        })
    } else {
        disable(cooldown_btn)
    };

    let strategy = settings.strategy;
    let strategy_btn =
        button("strategy".to_owned(), strategy_label(strategy), "Auto-switch strategy", Look::Plain, palette);
    let strategy_btn = if connected {
        strategy_btn.on_press_tx(move |_: &mut Panel, tx| {
            let menu = PopupMenu::new()
                .item(PopupItem::new("strategy:best", "Best").checked(strategy == Strategy::Best))
                .item(PopupItem::new("strategy:next", "Next").checked(strategy == Strategy::Next));
            tx.popup_menu(menu, "strategy", |panel: &mut Panel, picked, _| {
                if let Some(id) = picked {
                    panel.on_menu(id);
                }
            });
        })
    } else {
        disable(strategy_btn)
    };

    let mut items = vec![
        group(palette)
            .child(setting("Auto-switch", auto_toggle, palette))
            .child(divider(palette))
            .child(setting("Threshold", threshold_controls, palette))
            .child(divider(palette))
            .child(setting("Cooldown", cooldown_btn, palette))
            .child(divider(palette))
            .child(setting("Strategy", strategy_btn, palette)),
    ];

    let switchable: Vec<&ProviderState> = snapshot.providers.iter().filter(|p| p.switchable).collect();
    let multi_provider = switchable.len() > 1;
    let candidates: Vec<(&ProviderState, &Account)> =
        switchable.into_iter().flat_map(|p| p.accounts.iter().map(move |a| (p, a))).collect();
    if !candidates.is_empty() {
        let heading = text("Candidate accounts").font(BODY).color(palette.secondary).nowrap();
        items.push(row().h(20.0).px(12.0).center_y().child(heading));
        let mut group_el = group(palette);
        for (index, (provider, account)) in candidates.into_iter().enumerate() {
            if index > 0 {
                group_el = group_el.child(divider(palette));
            }
            let label = if multi_provider {
                format!("{} ({})", account.label(), provider.name)
            } else {
                account.label().to_owned()
            };
            let (id, account_enabled) = (account.id.clone(), account.enabled);
            let control = toggle(&format!("candidate-{}", account.id), &label, account_enabled, palette);
            let control = if connected {
                control.on_press_tx(move |panel: &mut Panel, _| {
                    panel.update_account(id.clone(), AccountUpdate { alias: None, enabled: Some(!account_enabled) });
                })
            } else {
                disable(control)
            };
            group_el = group_el.child(setting(&label, control, palette));
        }
        items.push(group_el);
    }
    items
}

fn app_settings(panel: &Panel, palette: &Palette) -> El {
    let summary = if panel.preferences.notifications.enabled { "On" } else { "Off" };
    let control = row()
        .gap(4.0)
        .center_y()
        .child(text(summary).font(BODY).color(palette.secondary).nowrap())
        .child(icon(&icons::OPEN, 14.0).color(palette.secondary));
    let notifications = setting("Notifications", control, palette)
        .key("open-notifications")
        .role("button")
        .label("Notification settings")
        .focusable(true)
        .hover_fill(palette.hover)
        .on_press_tx(|panel: &mut Panel, _| panel.open(Page::Notifications));
    let launch_on = panel.launch_at_login;
    let launch = toggle("launch-at-login", "Launch at login", launch_on, palette)
        .on_press(move |panel: &mut Panel| panel.set_launch_at_login(!launch_on));
    group(palette).child(notifications).child(divider(palette)).child(setting("Launch at login", launch, palette))
}

fn disable_if(el: El, off: bool) -> El {
    if off { disable(el) } else { el }
}

fn notify_toggle(
    key: &str,
    label: &str,
    on: bool,
    enabled: bool,
    palette: &Palette,
    edit: impl Fn(&mut NotificationPreferences, bool) + 'static,
) -> El {
    let control = toggle(key, label, on, palette);
    let control = if enabled {
        control.on_press(move |panel: &mut Panel| panel.edit_preferences(|all| edit(&mut all.notifications, !on)))
    } else {
        control.disabled(true)
    };
    setting(label, control, palette)
}

fn menu_button(
    key: &str,
    value_label: &str,
    description: &str,
    enabled: bool,
    palette: &Palette,
    build_menu: impl Fn() -> PopupMenu + 'static,
) -> El {
    let btn = button(key.to_owned(), value_label, description, Look::Plain, palette);
    if !enabled {
        return btn.disabled(true);
    }
    let menu_key = key.to_owned();
    btn.on_press_tx(move |_: &mut Panel, tx| {
        tx.popup_menu(build_menu(), menu_key.clone(), |panel: &mut Panel, picked, _| {
            if let Some(id) = picked {
                panel.on_menu(id);
            }
        });
    })
}

fn reset_trigger_label(trigger: ResetTrigger) -> &'static str {
    match trigger {
        ResetTrigger::Exhausted => "After 100% only",
        ResetTrigger::Any => "Any usage",
    }
}

fn window_scope_label(scope: WindowScope) -> &'static str {
    match scope {
        WindowScope::Primary => "Primary only",
        WindowScope::All => "All windows",
    }
}

fn account_scope_label(scope: AccountScope) -> &'static str {
    match scope {
        AccountScope::All => "All accounts",
        AccountScope::Active => "Active only",
    }
}

fn section_label(title: &str, enabled: bool, palette: &Palette) -> El {
    let heading = text(title).font(BODY).color(palette.secondary).nowrap();
    disable_if(row().h(20.0).px(12.0).center_y().child(heading), !enabled)
}

fn notifications_page(snapshot: &Snapshot, panel: &Panel, palette: &Palette) -> Vec<El> {
    let prefs = &panel.preferences.notifications;
    let enabled = prefs.enabled;

    let master = toggle("notifications-enabled", "Notifications", enabled, palette)
        .on_press(move |panel: &mut Panel| panel.set_notifications(!enabled));
    let sound = notify_toggle("notifications-sound", "Sound", prefs.sound, enabled, palette, |n, v| n.sound = v);
    let test_btn = button("notifications-test".to_owned(), "Send", "Send test notification", Look::Plain, palette);
    let test_btn = if enabled {
        test_btn.on_press(|panel: &mut Panel| panel.send_test_notification())
    } else {
        test_btn.disabled(true)
    };
    let general = group(palette)
        .child(setting("Notifications", master, palette))
        .child(divider(palette))
        .child(disable_if(sound, !enabled))
        .child(divider(palette))
        .child(disable_if(setting("Test notification", test_btn, palette), !enabled));

    let threshold = prefs.warning_threshold;
    let warn_enabled = enabled && prefs.usage_warning;
    let warn_down = button("notify-warning-down".to_owned(), "-", "Decrease warning threshold", Look::Plain, palette);
    let warn_down = if warn_enabled {
        if threshold > 50 {
            warn_down.on_press(move |panel: &mut Panel| {
                panel.edit_preferences(|all| all.notifications.warning_threshold = threshold.saturating_sub(5).max(50));
            })
        } else {
            disable(warn_down)
        }
    } else {
        warn_down.disabled(true)
    };
    let warn_up = button("notify-warning-up".to_owned(), "+", "Increase warning threshold", Look::Plain, palette);
    let warn_up = if warn_enabled {
        if threshold < 95 {
            warn_up.on_press(move |panel: &mut Panel| {
                panel.edit_preferences(|all| all.notifications.warning_threshold = (threshold + 5).min(95));
            })
        } else {
            disable(warn_up)
        }
    } else {
        warn_up.disabled(true)
    };
    let threshold_controls = row()
        .gap(6.0)
        .center_y()
        .child(warn_down)
        .child(text(format!("{threshold}%")).font(BODY).color(palette.text).nowrap())
        .child(warn_up);
    let threshold_row =
        disable_if(setting("Warning threshold", threshold_controls, palette), enabled && !prefs.usage_warning);

    let reset_trigger = prefs.reset_trigger;
    let reset_enabled = enabled && prefs.limit_reset;
    let reset_btn = menu_button(
        "notify-reset-trigger",
        reset_trigger_label(reset_trigger),
        "Reset trigger",
        reset_enabled,
        palette,
        move || {
            PopupMenu::new()
                .item(
                    PopupItem::new("notify-reset:exhausted", "After 100% only")
                        .checked(reset_trigger == ResetTrigger::Exhausted),
                )
                .item(PopupItem::new("notify-reset:any", "Any usage").checked(reset_trigger == ResetTrigger::Any))
        },
    );
    let reset_trigger_row = disable_if(setting("Reset trigger", reset_btn, palette), enabled && !prefs.limit_reset);

    let events = group(palette)
        .child(notify_toggle("notify-auto-switch", "Auto-switch", prefs.auto_switch, enabled, palette, |n, v| {
            n.auto_switch = v
        }))
        .child(divider(palette))
        .child(notify_toggle("notify-usage-warning", "Usage warning", prefs.usage_warning, enabled, palette, |n, v| {
            n.usage_warning = v
        }))
        .child(divider(palette))
        .child(threshold_row)
        .child(divider(palette))
        .child(notify_toggle("notify-limit-reached", "Limit reached", prefs.limit_reached, enabled, palette, |n, v| {
            n.limit_reached = v
        }))
        .child(divider(palette))
        .child(notify_toggle("notify-limit-reset", "Limit reset", prefs.limit_reset, enabled, palette, |n, v| {
            n.limit_reset = v
        }))
        .child(divider(palette))
        .child(reset_trigger_row)
        .child(divider(palette))
        .child(notify_toggle("notify-signed-out", "Signed out", prefs.signed_out, enabled, palette, |n, v| {
            n.signed_out = v
        }));

    let windows_scope = prefs.windows;
    let windows_btn = menu_button(
        "notify-windows",
        window_scope_label(windows_scope),
        "Windows scope",
        enabled,
        palette,
        move || {
            PopupMenu::new()
                .item(
                    PopupItem::new("notify-windows:primary", "Primary only")
                        .checked(windows_scope == WindowScope::Primary),
                )
                .item(PopupItem::new("notify-windows:all", "All windows").checked(windows_scope == WindowScope::All))
        },
    );
    let accounts_scope = prefs.accounts;
    let accounts_btn = menu_button(
        "notify-accounts",
        account_scope_label(accounts_scope),
        "Accounts scope",
        enabled,
        palette,
        move || {
            PopupMenu::new()
                .item(
                    PopupItem::new("notify-accounts:all", "All accounts").checked(accounts_scope == AccountScope::All),
                )
                .item(
                    PopupItem::new("notify-accounts:active", "Active only")
                        .checked(accounts_scope == AccountScope::Active),
                )
        },
    );
    let scope = group(palette).child(setting("Windows", windows_btn, palette)).child(divider(palette)).child(setting(
        "Accounts",
        accounts_btn,
        palette,
    ));

    let mut items = vec![
        general,
        section_label("Events", enabled, palette),
        disable_if(events, !enabled),
        disable_if(
            notes(note("After 100% only alerts when a blocked window becomes usable again.", palette.secondary)),
            !enabled,
        ),
        section_label("Scope", enabled, palette),
        disable_if(scope, !enabled),
        disable_if(
            notes(note("Usage-only providers are always included under Active only.", palette.secondary)),
            !enabled,
        ),
    ];

    let multi_provider = snapshot.providers.len() > 1;
    let accounts: Vec<(&ProviderState, &Account)> =
        snapshot.providers.iter().flat_map(|p| p.accounts.iter().map(move |a| (p, a))).collect();
    if !accounts.is_empty() {
        items.push(section_label("Notify for accounts", enabled, palette));
        let mut group_el = group(palette);
        for (index, (provider, account)) in accounts.into_iter().enumerate() {
            if index > 0 {
                group_el = group_el.child(divider(palette));
            }
            let label = if multi_provider {
                format!("{} ({})", account.label(), provider.name)
            } else {
                account.label().to_owned()
            };
            let id = account.id.clone();
            let on = prefs.account_on(&id);
            let key = format!("notify-account-{id}");
            group_el = group_el.child(notify_toggle(&key, &label, on, enabled, palette, move |n, v| {
                n.set_account(&id, v);
            }));
        }
        items.push(disable_if(group_el, !enabled));
    }

    items
}

fn notes(note: El) -> El {
    col().px(12.0).child(note)
}

fn preview(panel: &Panel, snapshot: &Snapshot, palette: &Palette) -> El {
    let groups = status::tray_groups(Some(snapshot), &panel.preferences);
    row()
        .key("preview")
        .h(40.0)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .radius(8.0)
        .fill(palette.active)
        .border(1.0, palette.active_border)
        .child(text("Menu bar preview").font(BODY).color(palette.secondary).nowrap())
        .child(
            row()
                .grow(1.0)
                .min_w(0.0)
                .clip_rounded(0.0)
                .justify_end()
                .center_y()
                .gap(11.0)
                .children(groups.iter().map(|group| tray_group(group, palette))),
        )
}

fn tray_group(group: &TrayGroup, palette: &Palette) -> El {
    let mark = match icons::provider_mark(&group.provider) {
        Some(mark) => icon(mark, 16.0).color(palette.text),
        None if group.provider.is_empty() => icon(&icons::SWITCH, 20.0).color(palette.text),
        None => el().size(15.0, 15.0).radius(7.5).fill(palette.text),
    };
    let columns = group.columns.iter().map(|column| {
        let font = if column.values.len() == 1 { TRAY_SINGLE } else { TRAY_ROW };
        let values = column.values.iter().map(|value| text(value).font(font).color(palette.text).nowrap());
        col().items_end().opacity(if column.active { 1.0 } else { TRAY_INACTIVE }).children(values)
    });
    let mut shown = row().gap(4.0).center_y().child(mark);
    if !group.columns.is_empty() {
        shown = shown.child(row().gap(7.0).center_y().children(columns));
    }
    shown
}

fn columns(palette: &Palette) -> El {
    let label = |label: &str| text(label).font(SMALL).color(palette.secondary).nowrap();
    row()
        .h(20.0)
        .px(12.0)
        .center_y()
        .child(row().grow(1.0).child(label("Show in")))
        .child(row().w(PANEL_COLUMN).justify_center().child(label("Panel")))
        .child(row().w(MENU_BAR_COLUMN).justify_center().child(label("Menu bar")))
        .child(el().w(OPEN_COLUMN))
}

fn provider_group(provider: &ProviderState, panel: &Panel, palette: &Palette) -> El {
    let list = List::Accounts(provider.id.clone());
    let accounts = panel.arranged(&list, &provider.accounts, |account| &account.id);
    let group = group(palette)
        .child(provider_row(provider, panel, palette))
        .children(accounts.into_iter().map(|account| account_row(account, provider, &list, panel, palette)));
    movable(group, &List::Providers, &provider.id, panel, palette)
}

fn provider_row(provider: &ProviderState, panel: &Panel, palette: &Palette) -> El {
    let mark = match icons::provider_mark(&provider.id) {
        Some(mark) => icon(mark, 14.0).color(palette.text),
        None => el(),
    };
    let page = Page::Provider(provider.id.clone());
    let check = |place: Place, width: f32| {
        let id = provider.id.clone();
        let on = panel.preferences.provider_on(place, &provider.id);
        let label = format!("Show {} in {}", provider.name, place_name(place));
        check_cell(format!("{}-{place:?}", provider.id), &label, on, width, palette)
            .on_press(move |panel: &mut Panel| panel.edit_preferences(|all| all.set_provider(place, &id, !on)))
    };
    let row = row()
        .key(format!("settings-header-{}", provider.id))
        .h(ROW)
        .px(12.0)
        .center_y()
        .child(grip(List::Providers, &provider.id, &provider.name, panel, palette))
        .child(row().w(HANDLE).center_y().child(mark))
        .child(opener(&page, text(&provider.name).font(SECTION).color(palette.text).truncate().grow(1.0).min_w(0.0)))
        .child(check(Place::Panel, PANEL_COLUMN))
        .child(check(Place::MenuBar, MENU_BAR_COLUMN))
        .child(open_button(&provider.id, &provider.name, page, palette));
    moved_by_keys(row, List::Providers, &provider.id)
}

fn account_row(account: &Account, provider: &ProviderState, list: &List, panel: &Panel, palette: &Palette) -> El {
    let page = Page::Account(account.id.clone());
    let name = if account.enabled { palette.text } else { palette.secondary };
    let mut names = col()
        .grow(1.0)
        .min_w(0.0)
        .gap(1.0)
        .child(text(account.label()).font(NAME).color(name).truncate().w_full().min_w(0.0));
    if let Some(summary) = format::account_summary(account, provider) {
        names = names.child(text(summary).font(SMALL).color(palette.secondary).truncate().w_full().min_w(0.0));
    }
    let check = |place: Place, width: f32| {
        let id = account.id.clone();
        let on = panel.preferences.account_on(place, &account.id);
        let label = format!("Show {} in {}", account.label(), place_name(place));
        let cell = check_cell(format!("{}-{place:?}", account.id), &label, on, width, palette);
        if !panel.preferences.provider_on(place, &provider.id) {
            return disable(cell);
        }
        cell.on_press(move |panel: &mut Panel| panel.edit_preferences(|all| all.set_account(place, &id, !on)))
    };
    let row = row()
        .h(ACCOUNT_ROW)
        .px(12.0)
        .center_y()
        .child(el().w(HANDLE))
        .child(grip(list.clone(), &account.id, account.label(), panel, palette))
        .child(opener(&page, names))
        .child(check(Place::Panel, PANEL_COLUMN))
        .child(check(Place::MenuBar, MENU_BAR_COLUMN))
        .child(open_button(&account.id, account.label(), page, palette));
    moved_by_keys(movable(row, list, &account.id, panel, palette), list.clone(), &account.id)
}

fn place_name(place: Place) -> &'static str {
    match place {
        Place::Panel => "the panel",
        Place::MenuBar => "the menu bar",
    }
}

fn grip(list: List, id: &str, name: &str, panel: &Panel, palette: &Palette) -> El {
    let slot = row().w(HANDLE).h_full().center_y();
    if panel.ids(&list).len() < 2 {
        return slot;
    }
    let slot = slot
        .key(format!("grip-{id}"))
        .role("button")
        .label(format!("Move {name}, Option and arrow keys"))
        .child(icon(&icons::GRIP, 14.0).color(palette.secondary));
    if !panel.connected {
        return disable(slot);
    }
    drag_handle(slot.focusable(true).cursor(Cursor::Grab), list, id)
}

fn moved_by_keys(row: El, list: List, id: &str) -> El {
    let id = id.to_owned();
    row.on_key(move |panel: &mut Panel, key: &KeyEvent, _| {
        let step = match key.key {
            Key::Up if key.mods.alt => -1,
            Key::Down if key.mods.alt => 1,
            _ => return false,
        };
        panel.move_by(&list, &id, step);
        true
    })
}

fn opener(page: &Page, content: El) -> El {
    let page = page.clone();
    row()
        .grow(1.0)
        .min_w(0.0)
        .pr(6.0)
        .h_full()
        .center_y()
        .on_background_press_tx(move |panel: &mut Panel, _| panel.open(page.clone()))
        .child(content)
}

fn open_button(id: &str, name: &str, page: Page, palette: &Palette) -> El {
    row()
        .key(format!("open-{id}"))
        .role("button")
        .label(format!("Settings for {name}"))
        .focusable(true)
        .w(OPEN_COLUMN)
        .h_full()
        .center_y()
        .justify_end()
        .on_press_tx(move |panel: &mut Panel, _| panel.open(page.clone()))
        .child(icon(&icons::OPEN, 14.0).color(palette.secondary))
}

fn check_cell(key: String, label: &str, on: bool, width: f32, palette: &Palette) -> El {
    row()
        .key(format!("check-{key}"))
        .role("checkbox")
        .label(label)
        .checked(on)
        .focusable(true)
        .w(width)
        .h_full()
        .center()
        .child(check_mark(on, palette))
}

fn check_mark(on: bool, palette: &Palette) -> El {
    let mark = row().size(16.0, 16.0).radius(4.0).center();
    if on {
        mark.fill(palette.accent).child(icon(&icons::CHECK, 12.0).color(palette.on_accent))
    } else {
        mark.fill(palette.button).border(1.0, palette.border)
    }
}

fn setting(label: &str, control: El, palette: &Palette) -> El {
    row()
        .h(ROW)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .child(text(label).font(BODY).color(palette.text).truncate().grow(1.0).min_w(0.0))
        .child(control)
}

fn divider(palette: &Palette) -> El {
    el().w_full().h(1.0).fill(palette.separator)
}

fn provider_page(provider: &ProviderState, panel: &Panel, palette: &Palette) -> Vec<El> {
    let show = |place: Place, label: &str| {
        let id = provider.id.clone();
        let on = panel.preferences.provider_on(place, &provider.id);
        let control = toggle(&format!("show-{place:?}"), label, on, palette)
            .on_press(move |panel: &mut Panel| panel.edit_preferences(|all| all.set_provider(place, &id, !on)));
        setting(label, control, palette)
    };
    let mut items = vec![
        group(palette)
            .child(show(Place::Panel, "Show in panel"))
            .child(divider(palette))
            .child(show(Place::MenuBar, "Show in menu bar")),
    ];
    let windows = provider_windows(provider);
    if !windows.is_empty() {
        let chosen = panel.preferences.tray_windows(provider);
        let heading = text("Usage in menu bar").font(BODY).color(palette.secondary).nowrap();
        let mut usage = group(palette);
        for (index, window) in windows.into_iter().enumerate() {
            if index > 0 {
                usage = usage.child(divider(palette));
            }
            usage = usage.child(window_row(window, chosen.contains(&window.id), provider, panel, palette));
        }
        items.push(row().h(20.0).px(12.0).center_y().child(heading));
        items.push(usage);
        let every = "Pick up to two. Every account of this provider uses them.";
        items.push(notes(note(every, palette.secondary)));
    }
    if provider.sign_in {
        let sign_in = sign_in_button(format!("sign-in-{}", provider.id), provider, panel, palette);
        items.push(group(palette).child(setting("Add an account", sign_in, palette)));
        items.push(notes(sign_in_hint(provider, palette)));
    }
    items
}

fn window_row(window: &UsageWindow, on: bool, provider: &ProviderState, panel: &Panel, palette: &Palette) -> El {
    let label = format!("Show {} in the menu bar", window.label);
    let row = setting(&window.label, check_mark(on, palette), palette)
        .key(format!("window-{}", window.id))
        .role("checkbox")
        .label(label)
        .checked(on);
    if !panel.preferences.can_toggle_tray_window(provider, &window.id) {
        return if on { row.disabled(true) } else { disable(row) };
    }
    let (provider, window) = (provider.id.clone(), window.id.clone());
    row.focusable(true).on_press(move |panel: &mut Panel| panel.toggle_tray_window(&provider, &window))
}

fn account_page(account: &Account, provider: &ProviderState, panel: &Panel, palette: &Palette) -> Vec<El> {
    let show = |place: Place, label: &str| {
        let id = account.id.clone();
        let on = panel.preferences.account_on(place, &account.id);
        let control = toggle(&format!("show-{place:?}"), label, on, palette);
        let control = if panel.preferences.provider_on(place, &provider.id) {
            control.on_press(move |panel: &mut Panel| panel.edit_preferences(|all| all.set_account(place, &id, !on)))
        } else {
            disable(control)
        };
        setting(label, control, palette)
    };
    let mut items = vec![
        group(palette).child(setting("Name", name_field(account, panel, palette), palette)),
        group(palette)
            .child(show(Place::Panel, "Show in panel"))
            .child(divider(palette))
            .child(show(Place::MenuBar, "Show in menu bar")),
    ];
    if provider.switchable {
        let (id, enabled) = (account.id.clone(), account.enabled);
        let control = toggle("use-for-auto-switch", "Use for auto-switch", enabled, palette);
        let control = if panel.connected {
            control.on_press_tx(move |panel: &mut Panel, _| {
                panel.update_account(id.clone(), AccountUpdate { alias: None, enabled: Some(!enabled) });
            })
        } else {
            disable(control)
        };
        items.push(group(palette).child(setting("Use for auto-switch", control, palette)));
        let paused = "Turning this off pauses the account. You can still switch to it yourself.";
        items.push(notes(note(paused, palette.secondary)));
    }
    if panel.prompt == Some(Prompt::Remove(account.id.clone())) {
        let buttons = row().gap(8.0).child(cancel(palette)).child(remove_button(account, panel, palette));
        items.push(group(palette).child(setting("Remove this login?", buttons, palette)));
        items.push(notes(note("This deletes its saved login from Switch.", palette.secondary)));
    } else {
        let id = account.id.clone();
        let description = format!("Remove {}", account.label());
        let remove = button("remove".to_owned(), "Remove...", &description, Look::Danger, palette);
        let remove = if panel.connected {
            remove.on_press_tx(move |panel: &mut Panel, _| panel.prompt = Some(Prompt::Remove(id.clone())))
        } else {
            disable(remove)
        };
        items.push(group(palette).child(setting("Saved login", remove, palette)));
    }
    items
}

fn name_field(account: &Account, panel: &Panel, palette: &Palette) -> El {
    let frame = |field: El| {
        field.w(200.0).h(24.0).px(6.0).py(4.0).radius(6.0).fill(palette.button).border(1.0, palette.button_border)
    };
    if !panel.connected {
        return frame(
            row().child(text(account.label()).font(BODY).color(palette.text).truncate().grow(1.0).min_w(0.0)),
        )
        .opacity(DISABLED);
    }
    let border = if panel.draft.focused { palette.accent } else { palette.button_border };
    frame(text_input(&panel.draft, BODY).single_line().key("name"))
        .border(1.0, border)
        .label(format!("Name for {}", account.identity.email))
        .placeholder(&account.identity.email, palette.secondary)
        .color(palette.text)
        .caret(palette.text, palette.accent.alpha(0.35))
        .on_edit(|panel: &mut Panel| &mut panel.draft)
        .on_submit(|panel: &mut Panel, _| panel.save_name())
        .on_input_key(|panel: &mut Panel, key: &KeyEvent, _| key.key == Key::Escape && panel.leave_unsaved())
        .on_focus_within_change(|panel: &mut Panel, within, _| {
            if !within {
                panel.save_name();
            }
        })
}
