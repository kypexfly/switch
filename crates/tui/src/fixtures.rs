use jiff::{SignedDuration, Timestamp};
use switch_protocol::{
    Account, AccountStatus, Identity, LimitReset, ProviderState, Settings, Snapshot, Usage, UsageWindow,
};

pub fn now() -> Timestamp {
    "2026-01-10T12:00:00Z".parse().unwrap()
}

pub fn identity(email: &str) -> Identity {
    Identity { account_id: email.to_owned(), email: email.to_owned(), organization_id: None, organization: None }
}

pub fn account(id: &str, email: &str) -> Account {
    Account {
        id: id.to_owned(),
        identity: identity(email),
        alias: None,
        plan: Some("Pro".to_owned()),
        active: false,
        enabled: true,
        status: AccountStatus::Ready,
        usage: None,
        resets: Vec::new(),
    }
}

fn window(label: &str, used_percent: Option<f64>, resets_in_minutes: i64, primary: bool) -> UsageWindow {
    UsageWindow {
        id: label.to_owned(),
        label: label.to_owned(),
        used_percent,
        resets_at: Some(now() + SignedDuration::from_mins(resets_in_minutes)),
        primary,
        period_seconds: None,
    }
}

fn usage(windows: Vec<UsageWindow>) -> Option<Usage> {
    Some(Usage { windows, fetched_at: now() - SignedDuration::from_mins(2) })
}

pub fn with_resets() -> Snapshot {
    let mut snap = snapshot();
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

pub fn empty() -> Snapshot {
    let provider = |id: &str, name: &str| ProviderState {
        id: id.to_owned(),
        name: name.to_owned(),
        accounts: Vec::new(),
        unsaved_login: None,
        switchable: true,
        sign_in: false,
        last_auto_switch_at: None,
    };
    Snapshot {
        providers: vec![provider("claude", "Claude")],
        settings: Settings { auto_switch: true, threshold: 90.0, ..Settings::default() },
        warnings: Vec::new(),
    }
}

pub fn snapshot() -> Snapshot {
    let mut work = account("claude-work", "ada@example.com");
    work.alias = Some("work".to_owned());
    work.identity.organization = Some("Example Incorporated".to_owned());
    work.plan = Some("Max 20x".to_owned());
    work.active = true;
    work.usage = usage(vec![
        window("Session", Some(42.0), 119, true),
        window("Weekly", Some(91.5), 5 * 1_440 + 720, true),
        window("Weekly model", Some(78.0), 5 * 1_440 + 720, false),
    ]);

    let mut personal = account("claude-personal", "ada.lovelace@personal.example.com");
    personal.enabled = false;
    personal.status = AccountStatus::Stale { message: "Usage request timed out after 10 seconds".to_owned() };
    personal.usage = usage(vec![window("Session", Some(76.0), 30, true), window("Weekly", None, 2_000, true)]);

    let mut expired = account("claude-old", "old@example.com");
    expired.status = AccountStatus::NeedsLogin;

    let mut pending = account("codex-main", "ada@example.org");
    pending.status = AccountStatus::Pending;
    pending.active = true;

    let mut snapshot = empty();
    snapshot.providers[0].accounts = vec![work, personal, expired];
    snapshot.providers.push(ProviderState {
        id: "codex".to_owned(),
        name: "Codex".to_owned(),
        accounts: vec![pending],
        unsaved_login: Some(identity("grace@example.net")),
        switchable: true,
        sign_in: false,
        last_auto_switch_at: None,
    });
    snapshot
}

/// One usage-only provider with an account that has to sign in again and one without accounts.
pub fn usage_only() -> Snapshot {
    let mut ada = account("antigravity-ada", "ada@example.com");
    ada.plan = Some("Google AI Pro".to_owned());
    ada.usage = usage(vec![window("Gemini 5h", Some(12.0), 200, true), window("Claude 7d", Some(40.0), 3_000, false)]);
    let mut old = account("antigravity-old", "old@example.com");
    old.status = AccountStatus::NeedsLogin;
    let provider = |id: &str, name: &str, accounts| ProviderState {
        id: id.to_owned(),
        name: name.to_owned(),
        accounts,
        unsaved_login: None,
        switchable: false,
        sign_in: true,
        last_auto_switch_at: None,
    };
    let mut snapshot = empty();
    snapshot.providers =
        vec![provider("antigravity", "Antigravity", vec![ada, old]), provider("gemini", "Gemini", Vec::new())];
    snapshot
}

pub fn showcase() -> Snapshot {
    let mut work = account("a1", "work@acme.dev");
    work.alias = Some("Work".to_owned());
    work.identity.organization = Some("Acme".to_owned());
    work.plan = Some("Max".to_owned());
    work.active = true;
    work.usage =
        usage(vec![window("Session", Some(92.0), 72, true), window("Weekly", Some(28.0), 3 * 1440 + 4 * 60, true)]);

    let mut personal = account("a2", "me@example.com");
    personal.alias = Some("Personal".to_owned());
    personal.usage =
        usage(vec![window("Session", Some(78.0), 138, true), window("Weekly", Some(45.0), 4 * 1440 + 10 * 60, true)]);

    let mut side = account("a3", "side@example.com");
    side.alias = Some("Side project".to_owned());
    side.usage =
        usage(vec![window("Session", Some(14.0), 220, true), window("Weekly", Some(19.0), 5 * 1440 + 18 * 60, true)]);

    let mut ag_personal = account("g1", "me@example.com");
    ag_personal.alias = Some("Personal".to_owned());
    ag_personal.plan = Some("Google AI Pro".to_owned());
    ag_personal.usage = usage(vec![
        window("Gemini 5h", Some(18.0), 200, true),
        window("Gemini 7d", Some(42.0), 4 * 1440, true),
        UsageWindow { resets_at: None, ..window("Claude 5h", Some(0.0), 0, false) },
        window("Claude 7d", Some(55.0), 2 * 1440, false),
    ]);

    let mut ag_work = account("g2", "work@acme.dev");
    ag_work.alias = Some("Work".to_owned());
    ag_work.plan = Some("Google AI Ultra".to_owned());
    ag_work.usage = usage(vec![
        window("Gemini 5h", Some(64.0), 105, true),
        window("Gemini 7d", Some(31.0), 5 * 1440 + 6 * 60, true),
    ]);

    let mut snapshot = empty();
    snapshot.providers = vec![
        ProviderState {
            id: "claude".to_owned(),
            name: "Claude".to_owned(),
            accounts: vec![work, personal, side],
            unsaved_login: None,
            switchable: true,
            sign_in: false,
            last_auto_switch_at: None,
        },
        ProviderState {
            id: "antigravity".to_owned(),
            name: "Antigravity".to_owned(),
            accounts: vec![ag_personal, ag_work],
            unsaved_login: None,
            switchable: false,
            sign_in: true,
            last_auto_switch_at: None,
        },
    ];
    snapshot
}
