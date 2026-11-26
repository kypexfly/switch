use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;

use block2::{DynBlock, RcBlock};
use jiff::Timestamp;
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{AnyThread, define_class, msg_send};
use objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString, NSUUID};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use switch_protocol::{Account, AccountStatus, ProviderState, Snapshot, UsageWindow};

use crate::preferences::{AccountScope, NotificationPreferences, ResetTrigger, WindowScope};

const RESET_ADVANCE_TOLERANCE_SECS: i64 = 60;
const FALLBACK_RESET_DROP_PERCENT: f64 = 50.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements and `NotificationDelegate` has no ivars or `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = ()]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[allow(non_snake_case)]
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn userNotificationCenter_willPresentNotification_withCompletionHandler(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion_handler.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }
    }
);

impl NotificationDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

struct RetainedDelegate(Retained<NotificationDelegate>);

// SAFETY: `NotificationDelegate` is stateless (`ivars = ()`) and inherits directly from `NSObject`.
unsafe impl Send for RetainedDelegate {}
unsafe impl Sync for RetainedDelegate {}

static DELEGATE: OnceLock<RetainedDelegate> = OnceLock::new();

fn account_eligible(prefs: &NotificationPreferences, provider: &ProviderState, account: &Account) -> bool {
    prefs.account_on(&account.id) && (prefs.accounts == AccountScope::All || account.active || !provider.switchable)
}

fn window_matches(prefs: &NotificationPreferences, window: &UsageWindow) -> bool {
    prefs.windows == WindowScope::All || window.primary
}

pub fn detect(previous: &Snapshot, current: &Snapshot, prefs: &NotificationPreferences) -> Vec<Notification> {
    if !prefs.enabled {
        return Vec::new();
    }
    let warning_threshold = f64::from(prefs.warning_threshold.clamp(1, 99));
    let mut notes = Vec::new();
    for curr_provider in &current.providers {
        let Some(prev_provider) = previous.providers.iter().find(|provider| provider.id == curr_provider.id) else {
            continue;
        };
        if prefs.auto_switch
            && curr_provider.switchable
            && let (Some(prev_active), Some(curr_active)) = (prev_provider.active(), curr_provider.active())
            && prev_active.id != curr_active.id
            && curr_provider.last_auto_switch_at.is_some()
            && curr_provider.last_auto_switch_at != prev_provider.last_auto_switch_at
            && (prefs.account_on(&curr_active.id) || prefs.account_on(&prev_active.id))
        {
            notes.push(Notification {
                title: format!("Switched {} to {}", curr_provider.name, curr_active.label()),
                body: format!(
                    "New {} sessions use this account. Restart running ones to switch them now.",
                    curr_provider.name
                ),
            });
        }
        for curr_account in &curr_provider.accounts {
            let Some(prev_account) = prev_provider.accounts.iter().find(|account| account.id == curr_account.id) else {
                continue;
            };
            if !account_eligible(prefs, curr_provider, curr_account) {
                continue;
            }
            if prefs.signed_out
                && prev_account.status != AccountStatus::NeedsLogin
                && curr_account.status == AccountStatus::NeedsLogin
            {
                notes.push(Notification {
                    title: format!("{} signed out", curr_provider.name),
                    body: format!("{} needs to sign in again.", curr_account.label()),
                });
            }
            if prev_account.usage == curr_account.usage {
                continue;
            }
            let curr_fetched_at = curr_account.usage.as_ref().map(|usage| usage.fetched_at);
            let curr_windows = curr_account.usage.as_ref().map(|usage| usage.windows.as_slice()).unwrap_or(&[]);
            let prev_windows = prev_account.usage.as_ref().map(|usage| usage.windows.as_slice()).unwrap_or(&[]);
            let mut reset_labels = Vec::new();
            for curr_win in curr_windows {
                if !window_matches(prefs, curr_win) {
                    continue;
                }
                let prev_win = prev_windows.iter().find(|window| window.id == curr_win.id);
                let prev_used = prev_win.and_then(|window| window.used_percent);
                if prefs.limit_reached
                    && curr_win.used_percent.is_some_and(|used| used >= 100.0)
                    && prev_used.is_none_or(|used| used < 100.0)
                {
                    notes.push(Notification {
                        title: format!("{} limit reached", curr_provider.name),
                        body: format!("{} reached 100% on {}.", curr_account.label(), curr_win.label),
                    });
                }
                if prefs.usage_warning
                    && let Some(curr_used) = curr_win.used_percent.filter(|used| used.is_finite())
                    && curr_used >= warning_threshold
                    && (curr_used < 100.0 || !prefs.limit_reached)
                    && prev_used.is_none_or(|used| used < warning_threshold)
                {
                    notes.push(Notification {
                        title: format!("{} usage warning", curr_provider.name),
                        body: format!(
                            "{} reached {} on {}.",
                            curr_account.label(),
                            crate::format::percent(Some(curr_used)),
                            curr_win.label
                        ),
                    });
                }
                if prefs.limit_reset
                    && let Some(prev_win) = prev_win
                    && window_reset(prev_win, curr_win, curr_fetched_at)
                    && (prefs.reset_trigger == ResetTrigger::Any
                        || prev_win.used_percent.is_some_and(|used| used >= 100.0))
                {
                    reset_labels.push(curr_win.label.as_str());
                }
            }
            if !reset_labels.is_empty() {
                notes.push(Notification {
                    title: format!("{} limit reset", curr_provider.name),
                    body: format!("{} {} usage reset.", curr_account.label(), join_labels(&reset_labels)),
                });
            }
        }
    }
    notes
}

fn window_reset(prev: &UsageWindow, curr: &UsageWindow, curr_fetched_at: Option<Timestamp>) -> bool {
    let Some(prev_used) = prev.used_percent.filter(|used| used.is_finite() && *used > 0.0) else {
        return false;
    };
    let reset_advanced = match (prev.resets_at, curr.resets_at) {
        (Some(before), Some(after)) => after.duration_since(before).as_secs() > RESET_ADVANCE_TOLERANCE_SECS,
        _ => false,
    };
    let reset_cleared = prev.resets_at.is_some() && curr.resets_at.is_none();
    let prev_reset_elapsed = prev.resets_at.is_some_and(|before| curr_fetched_at.is_some_and(|at| at >= before));
    match curr.used_percent.filter(|used| used.is_finite()) {
        None => reset_cleared || reset_advanced || prev_reset_elapsed,
        Some(curr_used) if curr_used >= 100.0 => false,
        Some(curr_used) => {
            if reset_advanced {
                curr_used < prev_used || prev_reset_elapsed
            } else if reset_cleared {
                curr_used < prev_used && (curr_used <= 0.0 || prev_reset_elapsed)
            } else if prev_reset_elapsed {
                curr_used < prev_used && (curr_used <= 0.0 || prev_used - curr_used >= FALLBACK_RESET_DROP_PERCENT)
            } else {
                curr_used < prev_used
                    && (prev_used - curr_used >= FALLBACK_RESET_DROP_PERCENT
                        || (prev.resets_at.is_none() && curr_used <= 0.0))
            }
        }
    }
}

fn join_labels(labels: &[&str]) -> String {
    match labels {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [first, second] => format!("{first} and {second}"),
        [init @ .., last] => format!("{}, and {last}", init.join(", ")),
    }
}

fn is_bundled_app() -> bool {
    let has_bundle_id = NSBundle::mainBundle().bundleIdentifier().is_some_and(|id| !id.to_string().is_empty());
    let in_app_bundle =
        std::env::current_exe().ok().is_some_and(|exe| exe.to_string_lossy().contains(".app/Contents/MacOS/"));
    has_bundle_id && in_app_bundle
}

fn ensure_center() -> Option<Retained<UNUserNotificationCenter>> {
    if !is_bundled_app() {
        return None;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let delegate = DELEGATE.get_or_init(|| RetainedDelegate(NotificationDelegate::new()));
    center.setDelegate(Some(ProtocolObject::from_ref(&*delegate.0)));
    Some(center)
}

pub fn prepare() {
    let Some(center) = ensure_center() else {
        return;
    };
    let handler = RcBlock::new(|_: Bool, error: *mut NSError| {
        if !error.is_null() {
            let message = unsafe { &*error }.localizedDescription();
            eprintln!("switch-desktop: notification authorization failed: {message}");
        }
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &handler,
    );
}

pub fn test_notification() -> Notification {
    Notification {
        title: "Switch notifications".to_owned(),
        body: "Test notification from Switch. Alerts are working.".to_owned(),
    }
}

pub fn send(notification: &Notification, sound: bool) {
    let Some(center) = ensure_center() else {
        send_osascript(notification, sound);
        return;
    };
    let title = notification.title.clone();
    let body = notification.body.clone();
    let handler = RcBlock::new(move |granted: Bool, error: *mut NSError| {
        if !error.is_null() {
            let message = unsafe { &*error }.localizedDescription();
            eprintln!("switch-desktop: notification authorization failed: {message}");
            return;
        }
        if !granted.as_bool() {
            return;
        }
        deliver_native(&title, &body, sound);
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &handler,
    );
}

fn deliver_native(title: &str, body: &str, sound: bool) {
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    if sound {
        content.setSound(Some(&UNNotificationSound::defaultSound()));
    }
    let id = NSUUID::UUID().UUIDString();
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
    let handler = RcBlock::new(|error: *mut NSError| {
        if !error.is_null() {
            let message = unsafe { &*error }.localizedDescription();
            eprintln!("switch-desktop: could not deliver notification: {message}");
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, Some(&handler));
}

fn send_osascript(notification: &Notification, sound: bool) {
    let title = escape_applescript(&notification.title);
    let body = escape_applescript(&notification.body);
    let sound_clause = if sound { " sound name \"default\"" } else { "" };
    let script = format!("display notification \"{body}\" with title \"{title}\"{sound_clause}");
    thread::spawn(move || {
        Command::new("osascript")
            .args(["-e", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
    });
}

fn escape_applescript(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use switch_protocol::{Account, Identity, ProviderState, Settings, Usage, UsageWindow};

    use super::*;

    fn window(id: &str, label: &str, used: Option<f64>, reset_sec: Option<i64>, primary: bool) -> UsageWindow {
        UsageWindow {
            id: id.into(),
            label: label.into(),
            used_percent: used,
            resets_at: reset_sec.map(|sec| Timestamp::from_second(sec).unwrap()),
            primary,
            period_seconds: None,
        }
    }

    fn account_with(
        id: &str,
        active: bool,
        status: AccountStatus,
        windows: Vec<UsageWindow>,
        fetched_sec: i64,
    ) -> Account {
        Account {
            id: id.into(),
            identity: Identity {
                account_id: id.into(),
                email: format!("{id}@example.com"),
                organization_id: None,
                organization: None,
            },
            alias: None,
            plan: None,
            active,
            enabled: true,
            status,
            usage: Some(Usage { windows, fetched_at: Timestamp::from_second(fetched_sec).unwrap() }),
            resets: Vec::new(),
        }
    }

    fn account(id: &str, active: bool, used: f64, reset_sec: i64, status: AccountStatus) -> Account {
        account_with(id, active, status, vec![window("five_hour", "Session", Some(used), Some(reset_sec), true)], 1000)
    }

    fn snapshot(accounts: Vec<Account>, auto_switch: bool, last_auto_switch_at: Option<Timestamp>) -> Snapshot {
        snapshot_for("claude", "Claude", true, accounts, auto_switch, last_auto_switch_at)
    }

    fn snapshot_for(
        id: &str,
        name: &str,
        switchable: bool,
        accounts: Vec<Account>,
        auto_switch: bool,
        last_auto_switch_at: Option<Timestamp>,
    ) -> Snapshot {
        Snapshot {
            providers: vec![ProviderState {
                id: id.into(),
                name: name.into(),
                accounts,
                unsaved_login: None,
                switchable,
                sign_in: !switchable,
                last_auto_switch_at,
            }],
            settings: Settings { auto_switch, ..Settings::default() },
            warnings: Vec::new(),
        }
    }

    #[test]
    fn detects_auto_switch_limit_reset_and_sign_out() {
        let prev = snapshot(
            vec![
                account("a1", true, 90.0, 2000, AccountStatus::Ready),
                account("a2", false, 100.0, 2000, AccountStatus::Ready),
            ],
            true,
            None,
        );
        let curr = snapshot(
            vec![
                account("a1", false, 90.0, 2000, AccountStatus::NeedsLogin),
                account("a2", true, 10.0, 5000, AccountStatus::Ready),
            ],
            true,
            Some(Timestamp::from_second(1500).unwrap()),
        );
        let prefs = NotificationPreferences::default();
        let notes = detect(&prev, &curr, &prefs);
        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0].title, "Switched Claude to a2@example.com");
        assert_eq!(notes[1].title, "Claude signed out");
        assert_eq!(notes[2].title, "Claude limit reset");
        assert_eq!(notes[2].body, "a2@example.com Session usage reset.");

        // Manual switches (no new auto-switch timestamp) never emit the auto-switch notice.
        let manual_same = snapshot(
            vec![
                account("a1", false, 90.0, 2000, AccountStatus::Ready),
                account("a2", true, 10.0, 5000, AccountStatus::Ready),
            ],
            true,
            None,
        );
        assert!(detect(&prev, &manual_same, &prefs).iter().all(|note| !note.title.starts_with("Switched")));

        let prev_ready = snapshot(vec![account("a1", true, 95.0, 2000, AccountStatus::Ready)], true, None);
        let curr_full = snapshot(vec![account("a1", true, 100.0, 2000, AccountStatus::Ready)], true, None);
        let reached = detect(&prev_ready, &curr_full, &prefs);
        assert_eq!(reached.len(), 1);
        assert_eq!(reached[0].title, "Claude limit reached");

        // Tiny reset jitter with high remaining usage does not trigger a limit reset notification.
        let curr_jitter = snapshot(vec![account("a1", true, 95.0, 2010, AccountStatus::Ready)], true, None);
        assert!(detect(&curr_full, &curr_jitter, &prefs).is_empty());

        let curr_reset = snapshot(vec![account("a1", true, 95.0, 2120, AccountStatus::Ready)], true, None);
        let reset = detect(&curr_full, &curr_reset, &prefs);
        assert_eq!(reset.len(), 1);
        assert_eq!(reset[0].title, "Claude limit reset");
    }

    #[test]
    fn detects_resets_from_below_full_and_cleared_measurements() {
        let prefs = NotificationPreferences::default();
        let prev = snapshot(
            vec![account_with(
                "a1",
                false,
                AccountStatus::Ready,
                vec![
                    window("five_hour", "Session", Some(90.0), Some(2000), true),
                    window("seven_day", "Weekly", Some(25.0), Some(10_000), true),
                ],
                1000,
            )],
            true,
            None,
        );

        let session_reset = snapshot(
            vec![account_with(
                "a1",
                false,
                AccountStatus::Ready,
                vec![
                    window("five_hour", "Session", Some(0.0), Some(20_000), true),
                    window("seven_day", "Weekly", Some(25.0), Some(10_000), true),
                ],
                2010,
            )],
            true,
            None,
        );
        assert_eq!(
            detect(&prev, &session_reset, &prefs),
            vec![Notification {
                title: "Claude limit reset".into(),
                body: "a1@example.com Session usage reset.".into(),
            }]
        );

        let weekly_reset = snapshot(
            vec![account_with(
                "a1",
                false,
                AccountStatus::Ready,
                vec![
                    window("five_hour", "Session", Some(90.0), Some(2000), true),
                    window("seven_day", "Weekly", Some(0.0), Some(614_800), true),
                ],
                10_010,
            )],
            true,
            None,
        );
        assert_eq!(
            detect(&prev, &weekly_reset, &prefs),
            vec![Notification {
                title: "Claude limit reset".into(),
                body: "a1@example.com Weekly usage reset.".into(),
            }]
        );

        let cleared_to_zero = snapshot(
            vec![account_with(
                "a1",
                false,
                AccountStatus::Ready,
                vec![
                    window("five_hour", "Session", Some(0.0), None, true),
                    window("seven_day", "Weekly", Some(25.0), Some(10_000), true),
                ],
                2010,
            )],
            true,
            None,
        );
        assert_eq!(
            detect(&prev, &cleared_to_zero, &prefs),
            vec![Notification {
                title: "Claude limit reset".into(),
                body: "a1@example.com Session usage reset.".into(),
            }]
        );

        let cleared_to_null = snapshot(
            vec![account_with(
                "a1",
                false,
                AccountStatus::Ready,
                vec![
                    window("five_hour", "Session", None, None, true),
                    window("seven_day", "Weekly", Some(25.0), Some(10_000), true),
                ],
                2010,
            )],
            true,
            None,
        );
        assert_eq!(
            detect(&prev, &cleared_to_null, &prefs),
            vec![Notification {
                title: "Claude limit reset".into(),
                body: "a1@example.com Session usage reset.".into(),
            }]
        );

        let null_same_reset = snapshot(
            vec![account_with(
                "a1",
                false,
                AccountStatus::Ready,
                vec![
                    window("five_hour", "Session", None, Some(2000), true),
                    window("seven_day", "Weekly", Some(25.0), Some(10_000), true),
                ],
                1500,
            )],
            true,
            None,
        );
        assert!(detect(&prev, &null_same_reset, &prefs).is_empty());
    }

    #[test]
    fn detects_non_primary_window_resets_fast_reuse_and_coalesces_labels() {
        let prefs = NotificationPreferences::default();
        let prev_ag = snapshot_for(
            "antigravity",
            "Antigravity",
            false,
            vec![account_with(
                "g1",
                false,
                AccountStatus::Ready,
                vec![
                    window("gemini-5h", "Gemini 5h", Some(45.0), Some(2000), true),
                    window("gemini-weekly", "Gemini 7d", Some(15.0), Some(10_000), true),
                    window("argon-weekly", "Argon 7d", Some(40.0), None, false),
                    window("3p-5h", "Claude 5h", Some(60.0), Some(2000), false),
                    window("3p-weekly", "Claude 7d", Some(30.0), Some(10_000), false),
                ],
                1000,
            )],
            false,
            None,
        );

        // Non-primary windows only (with and without resets_at).
        let curr_non_primary = snapshot_for(
            "antigravity",
            "Antigravity",
            false,
            vec![account_with(
                "g1",
                false,
                AccountStatus::Ready,
                vec![
                    window("gemini-5h", "Gemini 5h", Some(45.0), Some(2000), true),
                    window("gemini-weekly", "Gemini 7d", Some(15.0), Some(10_000), true),
                    window("argon-weekly", "Argon 7d", Some(0.0), None, false),
                    window("3p-5h", "Claude 5h", Some(0.0), Some(20_000), false),
                    window("3p-weekly", "Claude 7d", Some(0.0), Some(614_800), false),
                ],
                10_050,
            )],
            false,
            None,
        );
        assert_eq!(
            detect(&prev_ag, &curr_non_primary, &prefs),
            vec![Notification {
                title: "Antigravity limit reset".into(),
                body: "g1@example.com Argon 7d, Claude 5h, and Claude 7d usage reset.".into(),
            }]
        );

        // Two primary windows resetting together coalesce with "and".
        let prev_claude = snapshot(
            vec![account_with(
                "a1",
                true,
                AccountStatus::Ready,
                vec![
                    window("five_hour", "Session", Some(85.0), Some(2000), true),
                    window("seven_day", "Weekly", Some(40.0), Some(2000), true),
                ],
                1000,
            )],
            false,
            None,
        );
        let curr_two = snapshot(
            vec![account_with(
                "a1",
                true,
                AccountStatus::Ready,
                vec![
                    window("five_hour", "Session", Some(0.0), Some(20_000), true),
                    window("seven_day", "Weekly", Some(0.0), Some(606_800), true),
                ],
                2100,
            )],
            false,
            None,
        );
        assert_eq!(
            detect(&prev_claude, &curr_two, &prefs),
            vec![Notification {
                title: "Claude limit reset".into(),
                body: "a1@example.com Session and Weekly usage reset.".into(),
            }]
        );

        // Three windows resetting together use an Oxford comma.
        let curr_three = snapshot_for(
            "antigravity",
            "Antigravity",
            false,
            vec![account_with(
                "g1",
                false,
                AccountStatus::Ready,
                vec![
                    window("gemini-5h", "Gemini 5h", Some(0.0), Some(20_000), true),
                    window("gemini-weekly", "Gemini 7d", Some(0.0), Some(614_800), true),
                    window("argon-weekly", "Argon 7d", Some(40.0), None, false),
                    window("3p-5h", "Claude 5h", Some(0.0), Some(20_000), false),
                    window("3p-weekly", "Claude 7d", Some(30.0), Some(10_000), false),
                ],
                10_050,
            )],
            false,
            None,
        );
        assert_eq!(
            detect(&prev_ag, &curr_three, &prefs),
            vec![Notification {
                title: "Antigravity limit reset".into(),
                body: "g1@example.com Gemini 5h, Gemini 7d, and Claude 5h usage reset.".into(),
            }]
        );

        // Fast reuse after reset: 5% -> 8% once the previous reset time has passed and resets_at moved forward.
        let prev_low = snapshot(
            vec![account_with(
                "a1",
                true,
                AccountStatus::Ready,
                vec![window("five_hour", "Session", Some(5.0), Some(2000), true)],
                1500,
            )],
            false,
            None,
        );
        let curr_reused = snapshot(
            vec![account_with(
                "a1",
                true,
                AccountStatus::Ready,
                vec![window("five_hour", "Session", Some(8.0), Some(20_000), true)],
                2010,
            )],
            false,
            None,
        );
        assert_eq!(
            detect(&prev_low, &curr_reused, &prefs),
            vec![Notification {
                title: "Claude limit reset".into(),
                body: "a1@example.com Session usage reset.".into(),
            }]
        );
    }

    #[test]
    fn ignores_non_resets_and_unbundled_prepare_is_safe() {
        assert!(!is_bundled_app());
        prepare();
        let prefs = NotificationPreferences::default();

        // 0% -> 0% with advanced reset does not fire.
        let zero_prev = snapshot(vec![account("a1", true, 0.0, 2000, AccountStatus::Ready)], false, None);
        let zero_curr = snapshot(vec![account("a1", true, 0.0, 20_000, AccountStatus::Ready)], false, None);
        assert!(detect(&zero_prev, &zero_curr, &prefs).is_empty());

        // None -> 10% does not fire.
        let null_prev = snapshot(
            vec![account_with(
                "a1",
                true,
                AccountStatus::Ready,
                vec![window("five_hour", "Session", None, None, true)],
                1000,
            )],
            false,
            None,
        );
        let measured_curr = snapshot(vec![account("a1", true, 10.0, 20_000, AccountStatus::Ready)], false, None);
        assert!(detect(&null_prev, &measured_curr, &prefs).is_empty());

        // 30% -> 29% with +5s jitter does not fire.
        let prev_30 = snapshot(vec![account("a1", true, 30.0, 2000, AccountStatus::Ready)], false, None);
        let curr_29_jitter = snapshot(vec![account("a1", true, 29.0, 2005, AccountStatus::Ready)], false, None);
        assert!(detect(&prev_30, &curr_29_jitter, &prefs).is_empty());

        // Demo climb 40% -> 43% with +180s before prev.resets_at elapses does not fire.
        let prev_40 = snapshot(vec![account("a1", true, 40.0, 2000, AccountStatus::Ready)], false, None);
        let curr_43 = snapshot(vec![account("a1", true, 43.0, 2180, AccountStatus::Ready)], false, None);
        assert!(detect(&prev_40, &curr_43, &prefs).is_empty());

        // Staying at 100% does not fire.
        let prev_full = snapshot(vec![account("a1", true, 100.0, 2000, AccountStatus::Ready)], false, None);
        let curr_full = snapshot(vec![account("a1", true, 100.0, 20_000, AccountStatus::Ready)], false, None);
        assert!(detect(&prev_full, &curr_full, &prefs).is_empty());

        // Unchanged usage does not fire.
        assert!(detect(&prev_40, &prev_40, &prefs).is_empty());
    }

    #[test]
    fn respects_master_and_event_toggles() {
        let prev = snapshot(
            vec![
                account("a1", true, 90.0, 2000, AccountStatus::Ready),
                account("a2", false, 100.0, 2000, AccountStatus::Ready),
            ],
            true,
            None,
        );
        let curr = snapshot(
            vec![
                account("a1", false, 100.0, 2000, AccountStatus::NeedsLogin),
                account("a2", true, 10.0, 5000, AccountStatus::Ready),
            ],
            true,
            Some(Timestamp::from_second(1500).unwrap()),
        );

        let disabled = NotificationPreferences { enabled: false, ..NotificationPreferences::default() };
        assert!(detect(&prev, &curr, &disabled).is_empty());

        let only_switch = NotificationPreferences {
            limit_reached: false,
            limit_reset: false,
            signed_out: false,
            ..NotificationPreferences::default()
        };
        assert_eq!(detect(&prev, &curr, &only_switch).len(), 1);
        assert_eq!(detect(&prev, &curr, &only_switch)[0].title, "Switched Claude to a2@example.com");

        let no_switch = NotificationPreferences { auto_switch: false, ..NotificationPreferences::default() };
        assert!(detect(&prev, &curr, &no_switch).iter().all(|note| !note.title.starts_with("Switched")));
    }

    #[test]
    fn usage_warning_fires_once_on_crossing_and_avoids_double_alert_at_full() {
        let mut prefs = NotificationPreferences {
            usage_warning: true,
            warning_threshold: 80,
            ..NotificationPreferences::default()
        };
        let p70 = snapshot(vec![account("a1", true, 70.0, 2000, AccountStatus::Ready)], false, None);
        let p82 = snapshot(vec![account("a1", true, 82.0, 2000, AccountStatus::Ready)], false, None);
        let p90 = snapshot(vec![account("a1", true, 90.0, 2000, AccountStatus::Ready)], false, None);
        let p100 = snapshot(vec![account("a1", true, 100.0, 2000, AccountStatus::Ready)], false, None);
        let p0_reset = snapshot(vec![account("a1", true, 0.0, 20_000, AccountStatus::Ready)], false, None);

        let crossed = detect(&p70, &p82, &prefs);
        assert_eq!(
            crossed,
            vec![Notification {
                title: "Claude usage warning".into(),
                body: "a1@example.com reached 82% on Session.".into(),
            }]
        );
        assert!(detect(&p82, &p90, &prefs).is_empty());

        let jumped_full = detect(&p70, &p100, &prefs);
        assert_eq!(
            jumped_full,
            vec![Notification {
                title: "Claude limit reached".into(),
                body: "a1@example.com reached 100% on Session.".into(),
            }]
        );

        prefs.limit_reached = false;
        let jumped_full_warning_only = detect(&p70, &p100, &prefs);
        assert_eq!(
            jumped_full_warning_only,
            vec![Notification {
                title: "Claude usage warning".into(),
                body: "a1@example.com reached 100% on Session.".into(),
            }]
        );

        assert_eq!(detect(&p0_reset, &p82, &prefs).len(), 1);
    }

    #[test]
    fn reset_trigger_and_window_and_account_scopes_filter_correctly() {
        let prev_90 = snapshot(vec![account("a1", true, 90.0, 2000, AccountStatus::Ready)], false, None);
        let curr_0 = snapshot(vec![account("a1", true, 0.0, 20_000, AccountStatus::Ready)], false, None);
        assert_eq!(detect(&prev_90, &curr_0, &NotificationPreferences::default()).len(), 1);

        let exhausted_only =
            NotificationPreferences { reset_trigger: ResetTrigger::Exhausted, ..NotificationPreferences::default() };
        assert!(detect(&prev_90, &curr_0, &exhausted_only).is_empty());

        let prev_100 = snapshot(vec![account("a1", true, 100.0, 2000, AccountStatus::Ready)], false, None);
        assert_eq!(detect(&prev_100, &curr_0, &exhausted_only).len(), 1);

        // Non-primary windows are included by default and ignored under WindowScope::Primary.
        let prev_np = snapshot(
            vec![account_with(
                "a1",
                true,
                AccountStatus::Ready,
                vec![window("opus", "Opus 7d", Some(70.0), Some(2000), false)],
                1000,
            )],
            false,
            None,
        );
        let curr_np = snapshot(
            vec![account_with(
                "a1",
                true,
                AccountStatus::Ready,
                vec![window("opus", "Opus 7d", Some(100.0), Some(2000), false)],
                1500,
            )],
            false,
            None,
        );
        assert_eq!(detect(&prev_np, &curr_np, &NotificationPreferences::default()).len(), 1);
        let primary_only =
            NotificationPreferences { windows: WindowScope::Primary, ..NotificationPreferences::default() };
        assert!(detect(&prev_np, &curr_np, &primary_only).is_empty());

        // Inactive switchable accounts are filtered under AccountScope::Active, while usage-only providers stay.
        let prev_inactive = snapshot(vec![account("a1", false, 90.0, 2000, AccountStatus::Ready)], false, None);
        let curr_inactive = snapshot(vec![account("a1", false, 100.0, 2000, AccountStatus::Ready)], false, None);
        assert_eq!(detect(&prev_inactive, &curr_inactive, &NotificationPreferences::default()).len(), 1);
        let active_only =
            NotificationPreferences { accounts: AccountScope::Active, ..NotificationPreferences::default() };
        assert!(detect(&prev_inactive, &curr_inactive, &active_only).is_empty());

        let prev_ag = snapshot_for(
            "antigravity",
            "Antigravity",
            false,
            vec![account("g1", false, 90.0, 2000, AccountStatus::Ready)],
            false,
            None,
        );
        let curr_ag = snapshot_for(
            "antigravity",
            "Antigravity",
            false,
            vec![account("g1", false, 100.0, 2000, AccountStatus::Ready)],
            false,
            None,
        );
        assert_eq!(detect(&prev_ag, &curr_ag, &active_only).len(), 1);

        // Muted accounts suppress notifications.
        let mut muted = NotificationPreferences::default();
        muted.set_account("a1", false);
        assert!(detect(&prev_inactive, &curr_inactive, &muted).is_empty());
    }
}
