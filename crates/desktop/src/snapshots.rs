use std::path::{Path, PathBuf};

use rune::App;

use crate::connection::Update;
use crate::fixtures;
use crate::panel::{Panel, Prompt, Task};
use crate::preferences::{Place, Preferences, UsageShown};
use crate::reorder::{List, Reorder};
use crate::settings::Page;
use crate::status::{self, TrayColumn, TrayGroup};
use crate::tray_image::{self, Bitmap};

fn output_dir() -> PathBuf {
    let dir = PathBuf::from(std::env::var_os("SWITCH_DESKTOP_SNAPSHOTS").expect("set SWITCH_DESKTOP_SNAPSHOTS"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn panel(snapshot: Option<switch_protocol::Snapshot>) -> Panel {
    let mut panel = Panel { now: fixtures::now(), ..Panel::default() };
    if let Some(snapshot) = snapshot {
        panel.apply(Update::Snapshot(snapshot));
    }
    panel
}

fn snap(dir: &Path, name: &str, state: impl Fn() -> Panel, dark: bool) {
    let theme = if dark { "dark" } else { "light" };
    let tree = dir.join(format!("panel-{name}-{theme}.json"));
    headless(&state, &["--tree", tree.to_str().unwrap(), "--theme", theme, "--at", "2"]);
    let json = std::fs::read_to_string(&tree).unwrap();
    std::fs::remove_file(&tree).unwrap();
    let height = viewport_height(&json);
    let path = dir.join(format!("panel-{name}-{theme}.png"));
    let size = format!("{}x{}", crate::panel::WIDTH, height.ceil());
    headless(&state, &["--snap", path.to_str().unwrap(), "--theme", theme, "--scale", "2", "--size", &size]);
}

fn headless(state: &impl Fn() -> Panel, args: &[&str]) {
    let state = state();
    App::new(crate::app::spec())
        .init(move || state)
        .fit_content_height()
        .args(args.iter().map(|arg| arg.to_string()))
        .try_run()
        .unwrap();
}

fn viewport_height(tree: &str) -> f32 {
    let start = tree.find("\"viewport\"").expect("a viewport in the tree");
    let numbers = &tree[start..];
    let numbers = &numbers[numbers.find('[').unwrap() + 1..numbers.find(']').unwrap()];
    numbers.split(',').nth(1).unwrap().trim().parse().unwrap()
}

fn showcase(shown: UsageShown) -> Panel {
    let mut snapshot = fixtures::showcase();
    let accounts = &mut snapshot.providers[0].accounts;
    let weekly = &mut accounts[0].usage.as_mut().unwrap().windows[1];
    weekly.resets_at = Some(fixtures::now() + jiff::SignedDuration::from_hours(13 * 24 + 13));
    weekly.period_seconds = Some(30 * 86400);
    accounts[2].usage.as_mut().unwrap().windows[0].used_percent = None;
    let mut panel = panel(Some(snapshot));
    panel.preferences.usage_shown = shown;
    panel
}

fn lost(snapshot: Option<switch_protocol::Snapshot>) -> Panel {
    let mut panel = panel(snapshot);
    panel.apply(Update::Lost("Can't reach the server: connection refused".into()));
    panel
}

fn failed(task: Task) -> Panel {
    let error = "The current login is not saved yet.".to_owned();
    Panel { failed: Some((task, error)), ..panel(Some(fixtures::two_providers())) }
}

fn busy() -> Panel {
    let running = vec![Task::Refresh, Task::Switch("a2".into())];
    Panel { running, ..panel(Some(fixtures::several_accounts())) }
}

fn prompt(prompt: Prompt) -> Panel {
    let mut draft = rune::TextEdit::new("Work");
    draft.focused = true;
    Panel { prompt: Some(prompt), draft, ..panel(Some(fixtures::two_providers())) }
}

fn dragging(list: List, id: &str) -> Panel {
    let reorder = Reorder { list, id: id.into(), to: 1, offset: -30.0, dropped: false };
    Panel { reorder: Some(reorder), ..panel(Some(fixtures::two_providers())) }
}

fn warning_level() -> Panel {
    let mut snapshot = fixtures::two_providers();
    snapshot.providers[0].accounts[0].usage.as_mut().unwrap().windows[0].used_percent = Some(82.0);
    panel(Some(snapshot))
}

fn signed_out() -> Panel {
    let mut snapshot = fixtures::usage_only();
    snapshot.providers[1].accounts.clear();
    panel(Some(snapshot))
}

fn settings(page: Page, edit: impl FnOnce(&mut Preferences)) -> Panel {
    let mut snapshot = fixtures::usage_only();
    let mut claude = fixtures::several_accounts().providers.remove(0);
    claude.accounts.retain(|account| ["a1", "a2", "a5"].contains(&account.id.as_str()));
    snapshot.providers[0] = claude;
    let mut panel = panel(Some(snapshot));
    edit(&mut panel.preferences);
    panel.open(page);
    panel
}

fn settings_list() -> Panel {
    settings(Page::Settings, |preferences| {
        preferences.set_account(Place::MenuBar, "a2", false);
        preferences.set_account(Place::MenuBar, "a5", false);
        preferences.set_account(Place::Panel, "a5", false);
    })
}

fn nothing_in_the_menu_bar() -> Panel {
    settings(Page::Settings, |preferences| {
        preferences.set_provider(Place::MenuBar, "claude", false);
        preferences.set_provider(Place::MenuBar, "antigravity", false);
    })
}

fn settings_drag() -> Panel {
    let list = List::Accounts("claude".into());
    let reorder = Reorder { list, id: "a1".into(), to: 1, offset: -16.0, dropped: false };
    Panel { reorder: Some(reorder), ..settings_list() }
}

fn shows_first_after(events: &str, first: &str, second: &str) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let (script, tree) = (dir.path().join("script.json"), dir.path().join("tree.json"));
    std::fs::write(&script, events).unwrap();
    let args = ["--script", script.to_str().unwrap(), "--tree", tree.to_str().unwrap(), "--at", "2"];
    headless(&|| panel(Some(fixtures::two_providers())), &args);
    let tree = std::fs::read_to_string(&tree).unwrap();
    tree.find(first).expect(first) < tree.find(second).expect(second)
}

fn tree_after(state: impl Fn() -> Panel, events: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let (script, tree) = (dir.path().join("script.json"), dir.path().join("tree.json"));
    std::fs::write(&script, events).unwrap();
    headless(
        &state,
        &["--script", script.to_str().unwrap(), "--tree", tree.to_str().unwrap(), "--size", "360x640", "--at", "2"],
    );
    std::fs::read_to_string(&tree).unwrap()
}

#[test]
fn settings_rows_open_toggle_and_drag() {
    let click = |key: &str| format!(r#"[{{"t": 0.5, "type": "click", "key": "{key}"}}]"#);
    let opened = tree_after(settings_list, &click("open-a2"));
    assert!(opened.contains("Saved login"));
    assert!(tree_after(settings_list, &click("open-antigravity")).contains("Usage in menu bar"));
    assert!(tree_after(settings_list, &click("back")).contains("Auto-switch at"));
    let escape = r#"[{"t": 0.5, "type": "key", "name": "Escape"}]"#;
    assert!(tree_after(settings_list, escape).contains("Auto-switch at"));
    let before = tree_after(settings_list, "[]");
    assert!(before.contains("12%"));
    assert!(!tree_after(settings_list, &click("check-antigravity-MenuBar")).contains("12%"));
    let drag = r#"[{"t": 0.5, "type": "down", "key": "grip-a1"}, {"t": 0.6, "type": "move", "x": 45, "y": 210}]"#;
    let place = |tree: &str, key: &str| tree.find(&format!("settings-account-{key}")).unwrap();
    assert!(place(&before, "a1") < place(&before, "a2"));
    let dragged = tree_after(settings_list, drag);
    assert!(place(&dragged, "a2") < place(&dragged, "a1"));
}

#[test]
fn dragging_previews_the_new_place_and_escape_cancels_it() {
    const ACCOUNT: i32 = 50;
    const HEADER: i32 = 20;
    let hold = |y: i32, travel: i32| {
        format!(
            r#"{{"t": 0.5, "type": "down", "x": 120, "y": {y}}}, {{"t": 0.6, "type": "move", "x": 120, "y": {}}}"#,
            y + travel
        )
    };
    let (a1, a2) = ("account-a1", "account-a2");
    assert!(shows_first_after("[]", a1, a2));
    assert!(shows_first_after(&format!("[{}]", hold(ACCOUNT, 3)), a1, a2));
    assert!(shows_first_after(&format!("[{}]", hold(ACCOUNT, 120)), a2, a1));
    assert!(shows_first_after(&format!("[{}]", hold(HEADER, 250)), "provider-codex", "provider-claude"));
    let escaped = format!(r#"[{}, {{"t": 0.8, "type": "key", "name": "Escape"}}]"#, hold(ACCOUNT, 120));
    assert!(shows_first_after(&escaped, a1, a2));
}

#[test]
fn long_account_names_do_not_overlap_settings_checkboxes() {
    let long_email = "a.very.long.address@example.com";
    let tree = tree_after(
        || {
            let mut panel = settings_list();
            if let Some(snapshot) = &mut panel.snapshot {
                snapshot.providers[0].accounts[0].alias = None;
                snapshot.providers[0].accounts[0].identity.email = long_email.to_owned();
            }
            panel
        },
        "[]",
    );
    let parsed: serde_json::Value = serde_json::from_str(&tree).unwrap();
    let elements = parsed["elements"].as_array().unwrap();
    let rect_of = |pred: &dyn Fn(&serde_json::Value) -> bool| -> [f64; 4] {
        let el = elements.iter().find(|el| pred(el)).expect("matching element in tree");
        let rect = el["rect"].as_array().unwrap();
        [rect[0].as_f64().unwrap(), rect[1].as_f64().unwrap(), rect[2].as_f64().unwrap(), rect[3].as_f64().unwrap()]
    };
    let label_rect = rect_of(&|el| el["text"].as_str().is_some_and(|text| text.starts_with("a.very.long")));
    let check_rect = rect_of(&|el| el["key"].as_str() == Some("check-a1-Panel"));
    assert!(label_rect[0] + label_rect[2] <= check_rect[0], "label {label_rect:?} overlaps checkbox {check_rect:?}");
}

#[test]
fn banked_resets_expand_confirm_and_dismiss_with_escape() {
    let click = |key: &str| format!(r#"[{{"t": 0.5, "type": "click", "key": "{key}"}}]"#);
    let collapsed = tree_after(|| panel(Some(fixtures::with_resets())), "[]");
    assert!(collapsed.contains("Limit resets"));
    assert!(collapsed.contains("1 available"));
    assert!(!collapsed.contains("Full Reset"));

    let expanded = tree_after(|| panel(Some(fixtures::with_resets())), &click("resets-toggle-a1"));
    assert!(expanded.contains("Full Reset"));
    assert!(expanded.contains("resets-drawer-a1"));

    let confirming = tree_after(
        || panel(Some(fixtures::with_resets())),
        r#"[{"t": 0.4, "type": "click", "key": "resets-toggle-a1"}, {"t": 0.6, "type": "click", "key": "reset-use-a1-demo-full"}]"#,
    );
    assert!(confirming.contains("Refills your session and weekly limits now. This cannot be undone."));
    assert!(confirming.contains("reset-confirm"));

    let escaped_confirm = tree_after(
        || panel(Some(fixtures::with_resets())),
        r#"[{"t": 0.4, "type": "click", "key": "resets-toggle-a1"}, {"t": 0.6, "type": "click", "key": "reset-use-a1-demo-full"}, {"t": 0.8, "type": "key", "name": "Escape"}]"#,
    );
    assert!(escaped_confirm.contains("resets-drawer-a1"));
    assert!(!escaped_confirm.contains("reset-confirm"));

    let escaped_drawer = tree_after(
        || panel(Some(fixtures::with_resets())),
        r#"[{"t": 0.4, "type": "click", "key": "resets-toggle-a1"}, {"t": 0.8, "type": "key", "name": "Escape"}]"#,
    );
    assert!(!escaped_drawer.contains("resets-drawer-a1"));
}

#[test]
fn notifications_settings_open_and_toggle() {
    let opened = tree_after(settings_list, r#"[{"t": 0.4, "type": "click", "key": "open-notifications"}]"#);
    assert!(opened.contains("Reset trigger"));
    assert!(opened.contains("Any usage"));

    let warning_stepped = tree_after(
        settings_list,
        r#"[
            {"t": 0.3, "type": "click", "key": "open-notifications"},
            {"t": 0.5, "type": "click", "key": "notify-usage-warning"},
            {"t": 0.7, "type": "click", "key": "notify-warning-up"}
        ]"#,
    );
    assert!(warning_stepped.contains("85%"));

    let turned_off = tree_after(
        settings_list,
        r#"[
            {"t": 0.3, "type": "click", "key": "open-notifications"},
            {"t": 0.5, "type": "click", "key": "notifications-enabled"},
            {"t": 0.7, "type": "click", "key": "back"}
        ]"#,
    );
    assert!(turned_off.contains("Notification settings"));
    assert!(turned_off.contains("\"Off\""));
}

#[test]
#[ignore = "writes PNGs for review; run with SWITCH_DESKTOP_SNAPSHOTS=<dir>"]
fn render_snapshots() {
    let dir = output_dir();
    for dark in [false, true] {
        snap(&dir, "accounts", || panel(Some(fixtures::several_accounts())), dark);
        snap(&dir, "two-providers", || panel(Some(fixtures::two_providers())), dark);
        snap(&dir, "unsaved", || panel(Some(fixtures::unsaved_login())), dark);
        snap(&dir, "usage-only", || panel(Some(fixtures::usage_only())), dark);
        snap(&dir, "empty", || panel(Some(fixtures::empty())), dark);
        snap(&dir, "connecting", || panel(None), dark);
        snap(&dir, "offline", || lost(Some(fixtures::two_providers())), dark);
        snap(&dir, "unreachable", || lost(None), dark);
        snap(&dir, "switch-failed", || failed(Task::Switch("a2".into())), dark);
        snap(&dir, "refresh-failed", || failed(Task::Refresh), dark);
        snap(&dir, "busy", busy, dark);
        snap(&dir, "rename", || prompt(Prompt::Rename("a1".into())), dark);
        snap(&dir, "remove", || prompt(Prompt::Remove("a2".into())), dark);
        snap(&dir, "drag-account", || dragging(List::Accounts("claude".into()), "a1"), dark);
        snap(&dir, "drag-provider", || dragging(List::Providers, "claude"), dark);
        snap(&dir, "warning-level", warning_level, dark);
        snap(&dir, "signed-out", signed_out, dark);
        snap(&dir, "reset-times", || Panel { exact_resets: true, ..panel(Some(fixtures::usage_only())) }, dark);
        snap(&dir, "settings", settings_list, dark);
        snap(&dir, "settings-drag", settings_drag, dark);
        snap(&dir, "settings-nothing-in-menu-bar", nothing_in_the_menu_bar, dark);
        snap(&dir, "settings-provider", || settings(Page::Provider("antigravity".into()), |_| {}), dark);
        snap(&dir, "settings-notifications", || settings(Page::Notifications, |_| {}), dark);
        snap(
            &dir,
            "settings-notifications-off",
            || settings(Page::Notifications, |preferences| preferences.notifications.enabled = false),
            dark,
        );
        snap(&dir, "settings-account", || settings(Page::Account("a2".into()), |_| {}), dark);
        snap(&dir, "settings-usage-only-account", || settings(Page::Account("g1".into()), |_| {}), dark);
        let remove = Some(Prompt::Remove("a2".into()));
        let removing = || Panel { prompt: remove.clone(), ..settings(Page::Account("a2".into()), |_| {}) };
        snap(&dir, "settings-remove", removing, dark);
        let order_failed = Some((Task::Order, "connection refused".to_owned()));
        snap(&dir, "settings-order-failed", || Panel { failed: order_failed.clone(), ..settings_list() }, dark);
        snap(&dir, "settings-offline", || Panel { connected: false, ..settings_list() }, dark);
        snap(
            &dir,
            "settings-account-offline",
            || Panel { connected: false, ..settings(Page::Account("a2".into()), |_| {}) },
            dark,
        );
        snap(&dir, "scrolling", || Panel { max_height: 360.0, ..panel(Some(fixtures::several_accounts())) }, dark);
        snap(&dir, "showcase", || panel(Some(fixtures::showcase())), dark);
        snap(&dir, "showcase-bars-left", || showcase(UsageShown::Left), dark);
        snap(&dir, "resets-collapsed", || panel(Some(fixtures::with_resets())), dark);
        snap(
            &dir,
            "resets-expanded",
            || Panel { expanded_resets: Some("a2".into()), ..panel(Some(fixtures::with_resets())) },
            dark,
        );
        snap(
            &dir,
            "resets-confirm",
            || Panel {
                expanded_resets: Some("a1".into()),
                confirming_reset: Some(("a1".into(), "demo-full".into())),
                ..panel(Some(fixtures::with_resets()))
            },
            dark,
        );
    }
    let thickness = 24.0;
    let trays = [
        ("accounts", status::tray_groups(Some(&fixtures::several_accounts()), &Preferences::default())),
        ("two-providers", status::tray_groups(Some(&fixtures::two_providers()), &Preferences::default())),
        ("usage-only", status::tray_groups(Some(&fixtures::usage_only()), &Preferences::default())),
        ("showcase", status::tray_groups(Some(&fixtures::showcase()), &Preferences::default())),
        ("logo-only", status::tray_groups(Some(&fixtures::unsaved_login()), &Preferences::default())),
        ("no-data", status::tray_groups(None, &Preferences::default())),
        (
            "single",
            vec![TrayGroup {
                provider: "claude".into(),
                columns: vec![TrayColumn { values: vec!["42%".into()], active: true }],
            }],
        ),
    ];
    for (name, groups) in trays {
        let bitmap = tray_image::render(&groups, thickness);
        write_png(&dir.join(format!("tray-{name}.png")), bitmap.width, bitmap.height, &bitmap.rgba);
        write_preview(&dir.join(format!("tray-{name}-preview.png")), &bitmap);
    }
}

fn write_preview(path: &Path, bitmap: &Bitmap) {
    const ZOOM: u32 = 4;
    let (width, height) = (bitmap.width * ZOOM, bitmap.height * ZOOM * 2);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        let dark = y >= height / 2;
        let source_y = (y % (height / 2)) / ZOOM;
        for x in 0..width {
            let alpha = f32::from(bitmap.rgba[((source_y * bitmap.width + x / ZOOM) * 4 + 3) as usize]) / 255.0;
            let (background, ink) = if dark { (40.0, 245.0) } else { (236.0, 20.0) };
            let value = (background + (ink - background) * alpha) as u8;
            rgba.extend_from_slice(&[value, value, value, 255]);
        }
    }
    write_png(path, width, height, &rgba);
}

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) {
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(path).unwrap()), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().unwrap().write_image_data(rgba).unwrap();
}
