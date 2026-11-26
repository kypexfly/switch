use jiff::{SignedDuration, Timestamp};
use switch_protocol::{
    Account, AccountStatus, Identity, LimitReset, ProviderState, Settings, Snapshot, Usage, UsageWindow,
};

pub fn now() -> Timestamp {
    "2026-11-24T12:00:00Z".parse().unwrap()
}

fn later(minutes: i64) -> Option<Timestamp> {
    Some(now() + SignedDuration::from_mins(minutes))
}

fn window(id: &str, label: &str, used: Option<f64>, resets_in_minutes: Option<i64>, primary: bool) -> UsageWindow {
    UsageWindow {
        id: id.into(),
        label: label.into(),
        used_percent: used,
        resets_at: resets_in_minutes.and_then(later),
        primary,
        period_seconds: None,
    }
}

fn claude_windows(five_hour: f64, seven_day: f64) -> Vec<UsageWindow> {
    let with_period =
        |window: UsageWindow, period_seconds| UsageWindow { period_seconds: Some(period_seconds), ..window };
    vec![
        with_period(window("five_hour", "Session", Some(five_hour), Some(62), true), 5 * 3600),
        with_period(window("seven_day", "Weekly", Some(seven_day), Some(3 * 1440 + 4 * 60), true), 7 * 86400),
    ]
}

fn account(id: &str, email: &str, alias: Option<&str>, windows: Option<Vec<UsageWindow>>) -> Account {
    Account {
        id: id.into(),
        identity: Identity {
            account_id: format!("acct-{id}"),
            email: email.into(),
            organization_id: None,
            organization: None,
        },
        alias: alias.map(Into::into),
        plan: Some("Pro".into()),
        active: false,
        enabled: true,
        status: AccountStatus::Ready,
        usage: windows.map(|windows| Usage { windows, fetched_at: now() - SignedDuration::from_mins(2) }),
        resets: Vec::new(),
    }
}

fn snapshot(providers: Vec<ProviderState>, warnings: Vec<String>) -> Snapshot {
    Snapshot { providers, settings: Settings::default(), warnings }
}

fn claude(accounts: Vec<Account>) -> ProviderState {
    ProviderState {
        id: "claude".into(),
        name: "Claude".into(),
        accounts,
        unsaved_login: None,
        switchable: true,
        sign_in: false,
        last_auto_switch_at: None,
    }
}

pub fn several_accounts() -> Snapshot {
    let mut work = account("a1", "work@acme.dev", Some("Work"), Some(claude_windows(97.0, 16.0)));
    work.active = true;
    work.plan = Some("Max".into());
    work.identity.organization = Some("Acme".into());
    if let Some(usage) = &mut work.usage {
        usage.windows.push(window("seven_day_opus", "Opus 7d", Some(40.0), Some(2 * 1440), false));
    }
    let personal = account("a2", "me@example.com", None, Some(claude_windows(20.0, 61.0)));
    let mut stale = account("a3", "side@example.com", Some("Side project"), Some(claude_windows(5.0, 3.0)));
    stale.status = AccountStatus::Stale { message: "Claude is rate limiting; trying again later".into() };
    let mut expired = account("a4", "old@example.com", None, None);
    expired.status = AccountStatus::NeedsLogin;
    let mut paused = account("a5", "spare@example.com", Some("Spare"), Some(claude_windows(0.0, 0.0)));
    paused.enabled = false;
    let mut pending = account("a6", "new@example.com", None, None);
    pending.status = AccountStatus::Pending;
    let mut snapshot = snapshot(vec![claude(vec![work, personal, stale, expired, paused, pending])], Vec::new());
    snapshot.settings.auto_switch = true;
    snapshot
}

pub fn two_providers() -> Snapshot {
    let mut work = account("a1", "work@acme.dev", Some("Work"), Some(claude_windows(97.0, 16.0)));
    work.active = true;
    let other = account("a2", "me@example.com", None, Some(claude_windows(20.0, 61.0)));
    let mut codex = account(
        "c1",
        "me@example.com",
        None,
        Some(vec![
            window("primary", "Session", None, None, true),
            window("secondary", "Week", Some(42.0), Some(5 * 1440), true),
        ]),
    );
    codex.active = true;
    codex.plan = Some("Plus".into());
    let codex = ProviderState {
        id: "codex".into(),
        name: "Codex".into(),
        accounts: vec![codex],
        unsaved_login: None,
        switchable: true,
        sign_in: false,
        last_auto_switch_at: None,
    };
    snapshot(vec![claude(vec![work, other]), codex], Vec::new())
}

/// A switchable provider next to a usage-only one, which has a signed-in account and no live login.
pub fn usage_only() -> Snapshot {
    let mut work = account("a1", "work@acme.dev", Some("Work"), Some(claude_windows(97.0, 16.0)));
    work.active = true;
    let with_period =
        |window: UsageWindow, period_seconds| UsageWindow { period_seconds: Some(period_seconds), ..window };
    let mut google = account(
        "g1",
        "me@gmail.example",
        None,
        Some(vec![
            with_period(window("gemini-5h", "Gemini 5h", Some(12.0), Some(200), true), 5 * 3600),
            with_period(window("gemini-weekly", "Gemini 7d", Some(40.0), Some(4 * 1440), true), 7 * 86400),
            with_period(window("3p-5h", "Claude 5h", Some(0.0), None, false), 5 * 3600),
            with_period(window("3p-weekly", "Claude 7d", Some(55.0), Some(2 * 1440), false), 7 * 86400),
        ]),
    );
    google.plan = Some("Google AI Pro".into());
    let antigravity = ProviderState {
        id: "antigravity".into(),
        name: "Antigravity".into(),
        accounts: vec![google],
        unsaved_login: None,
        switchable: false,
        sign_in: true,
        last_auto_switch_at: None,
    };
    snapshot(vec![claude(vec![work]), antigravity], Vec::new())
}

pub fn unsaved_login() -> Snapshot {
    let mut provider = claude(Vec::new());
    provider.unsaved_login = Some(Identity {
        account_id: "acct-x".into(),
        email: "fresh@example.com".into(),
        organization_id: Some("org-1".into()),
        organization: Some("Fresh Labs".into()),
    });
    snapshot(vec![provider], vec!["Could not read the current Claude login: permission denied".into()])
}

pub fn empty() -> Snapshot {
    snapshot(Vec::new(), Vec::new())
}

pub fn showcase() -> Snapshot {
    let timed = |id, label, used, resets_in_minutes, primary, period_seconds| UsageWindow {
        period_seconds: Some(period_seconds),
        ..window(id, label, Some(used), resets_in_minutes, primary)
    };
    let claude_usage = |five_h, five_h_m, seven_d, seven_d_m| {
        Some(vec![
            timed("five_hour", "Session", five_h, Some(five_h_m), true, 5 * 3600),
            timed("seven_day", "Weekly", seven_d, Some(seven_d_m), true, 7 * 86400),
        ])
    };
    let mut work = account("a1", "work@acme.dev", Some("Work"), claude_usage(92.0, 72, 28.0, 3 * 1440 + 4 * 60));
    work.active = true;
    work.plan = Some("Max".into());
    work.identity.organization = Some("Acme".into());
    let personal = account("a2", "me@example.com", Some("Personal"), claude_usage(78.0, 138, 45.0, 4 * 1440 + 10 * 60));
    let side =
        account("a3", "side@example.com", Some("Side project"), claude_usage(14.0, 220, 19.0, 5 * 1440 + 18 * 60));

    let mut ag_personal = account(
        "g1",
        "me@example.com",
        Some("Personal"),
        Some(vec![
            timed("gemini-5h", "Gemini 5h", 18.0, Some(200), true, 5 * 3600),
            timed("gemini-weekly", "Gemini 7d", 42.0, Some(4 * 1440), true, 7 * 86400),
            timed("3p-5h", "Claude 5h", 0.0, None, false, 5 * 3600),
            timed("3p-weekly", "Claude 7d", 55.0, Some(2 * 1440), false, 7 * 86400),
        ]),
    );
    ag_personal.plan = Some("Google AI Pro".into());
    let mut ag_work = account(
        "g2",
        "work@acme.dev",
        Some("Work"),
        Some(vec![
            timed("gemini-5h", "Gemini 5h", 64.0, Some(105), true, 5 * 3600),
            timed("gemini-weekly", "Gemini 7d", 31.0, Some(5 * 1440 + 6 * 60), true, 7 * 86400),
        ]),
    );
    ag_work.plan = Some("Google AI Ultra".into());
    let antigravity = ProviderState {
        id: "antigravity".into(),
        name: "Antigravity".into(),
        accounts: vec![ag_personal, ag_work],
        unsaved_login: None,
        switchable: false,
        sign_in: true,
        last_auto_switch_at: None,
    };

    let mut snap = snapshot(vec![claude(vec![work, personal, side]), antigravity], Vec::new());
    snap.settings = Settings { auto_switch: true, threshold: 90.0, ..Settings::default() };
    snap
}

pub fn with_resets() -> Snapshot {
    let mut snap = two_providers();
    snap.providers[0].accounts[0].resets = vec![LimitReset {
        id: "demo-full".to_owned(),
        label: "Full Reset".to_owned(),
        remaining: 1,
        expires_at: Some(now() + SignedDuration::from_hours(14 * 24 + 23)),
        usable: true,
        detail: Some("Refills your session and weekly limits now. This cannot be undone.".to_owned()),
    }];
    snap.providers[0].accounts[1].resets = vec![
        LimitReset {
            id: "demo-weekly".to_owned(),
            label: "Weekly Reset".to_owned(),
            remaining: 1,
            expires_at: Some(now() + SignedDuration::from_hours(10 * 24)),
            usable: true,
            detail: Some("Refills your weekly limit now. This cannot be undone.".to_owned()),
        },
        LimitReset {
            id: "demo-fable".to_owned(),
            label: "Fable Reset".to_owned(),
            remaining: 2,
            expires_at: Some(now() + SignedDuration::from_hours(30 * 24)),
            usable: false,
            detail: Some("Refills your Fable limit now. This cannot be undone.".to_owned()),
        },
    ];
    snap
}
