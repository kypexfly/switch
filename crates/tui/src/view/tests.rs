use std::cell::RefCell;
use std::rc::Rc;

use opentui::testing::TestApp;
use switch_protocol::{AccountUpdate, Snapshot};

use super::*;
use crate::fixtures;

struct Harness {
    app: TestApp,
    screen: Rc<Screen>,
    actions: Rc<RefCell<Vec<Action>>>,
    elapsed: Rc<Cell<i64>>,
}

impl Harness {
    fn new(width: u16, height: u16, snapshot: Option<Snapshot>) -> Self {
        let mut options = AppOptions::default().with_theme(Theme::terminal());
        options.focus_on_click = false;
        let app = TestApp::with_options(width, height, options);
        let actions = Rc::new(RefCell::new(Vec::new()));
        let elapsed = Rc::new(Cell::new(0));
        let screen = Screen::build(
            &app,
            {
                let elapsed = Rc::clone(&elapsed);
                move || fixtures::now() + jiff::SignedDuration::from_secs(elapsed.get())
            },
            {
                let actions = Rc::clone(&actions);
                move |action| actions.borrow_mut().push(action)
            },
        );
        let harness = Self { app, screen, actions, elapsed };
        harness.frame();
        if let Some(snapshot) = snapshot {
            harness.screen.apply(&Update::Snapshot(snapshot));
        }
        harness
    }

    fn frame(&self) -> String {
        self.app.render();
        self.app.advance(Duration::ZERO);
        let frame = self.app.render().text();
        println!("{frame}\n{}", "=".repeat(usize::from(self.app.size().0)));
        frame
    }

    fn press_all(&self, keys: &[&str]) {
        for key in keys {
            self.app.press(key);
        }
    }
}

fn assert_shows(frame: &str, texts: &[&str]) {
    for text in texts {
        assert!(frame.contains(text), "missing {text:?} in\n{frame}");
    }
}

#[test]
fn shows_every_account_window_and_state() {
    let frame = Harness::new(100, 36, Some(fixtures::snapshot())).frame();
    assert_shows(
        &frame,
        &[
            "Switch",
            "Auto switch on at 90%",
            "Claude  3 accounts",
            "▌ ● work  active  Max 20x",
            "ada@example.com - Example Incorporated - updated 2m ago",
            "42%  resets in 1h 59m",
            "91%  resets in 5d 12h",
            "Weekly model",
            "stale  paused  Pro",
            "Usage request timed out after 10 seconds",
            "--  resets in 1d 9h",
            "old@example.com  needs login",
            "Sign in to Claude again with its CLI, then press a.",
            "Codex  1 account",
            "Unsaved login grace@example.net - select it and press a to save it",
            "ada@example.org  active  pending",
            "q quit",
        ],
    );
    let bar_column = |label: &str| {
        let line = frame.lines().find(|line| line.trim_start_matches(['▌', ' ']).starts_with(label)).unwrap();
        line.chars().position(|character| character == '━').unwrap()
    };
    assert_eq!(bar_column("Session"), bar_column("Weekly model"));
}

#[test]
fn colors_bars_by_usage_and_quiets_secondary_windows() {
    let harness = Harness::new(100, 36, Some(fixtures::snapshot()));
    harness.frame();
    let capture = harness.app.render();
    let theme = harness.app.theme();
    let bar_cell = |percent: &str| {
        let row = (0..capture.height).find(|row| capture.row(*row).contains(percent)).unwrap();
        (0..capture.width)
            .filter_map(|column| capture.cell(column, row))
            .find(|cell| cell.symbol == "━")
            .unwrap()
            .clone()
    };
    assert_eq!(bar_cell("42%").fg, theme.accent);
    assert_eq!(bar_cell("76%").fg, theme.warning);
    assert_eq!(bar_cell("91%").fg, theme.error);
    assert!(!bar_cell("76%").attributes.contains(Attributes::DIM));
    assert!(bar_cell("78%").attributes.contains(Attributes::DIM));
}

#[test]
fn explains_the_empty_state() {
    let frame = Harness::new(80, 24, Some(fixtures::empty())).frame();
    assert_shows(
        &frame,
        &["No accounts yet", "Sign in with the provider's CLI, then press a to add that login.", "No accounts saved"],
    );
}

#[test]
fn usage_only_providers_point_to_swx_login() {
    let frame = Harness::new(80, 24, Some(fixtures::usage_only())).frame();
    assert_shows(
        &frame,
        &[
            "Antigravity  2 accounts",
            "ada@example.com  Google AI Pro",
            "Gemini 5h",
            "Sign in to Antigravity again with swx login antigravity.",
            "No accounts saved - run swx login gemini",
        ],
    );
}

#[test]
fn the_unsaved_login_of_another_provider_can_be_selected_and_saved() {
    let harness = Harness::new(80, 40, Some(fixtures::snapshot()));
    harness.press_all(&["down", "down", "down"]);
    assert_shows(&harness.frame(), &["▌   Unsaved login grace@example.net - press a to save it"]);
    harness.app.press("a");
    assert_eq!(*harness.actions.borrow(), [Action::Add { provider: "codex".to_owned() }]);
    assert_shows(&harness.frame(), &["Adding the current login..."]);
}

#[test]
fn a_narrow_screen_scrolls_to_the_selection_and_keeps_every_hint() {
    let harness = Harness::new(60, 16, Some(fixtures::snapshot()));
    assert_shows(&harness.frame(), &["Claude", "▌ ● work", "resets in 1h 59m", "? help", "q quit"]);
    for label in ["ada.lovelace@", "old@example.com", "Unsaved login", "ada@example.org"] {
        harness.app.press("down");
        let frame = harness.frame();
        assert!(frame.lines().any(|line| line.starts_with('▌') && line.contains(label)), "{label} in\n{frame}");
    }
    harness.press_all(&["up", "up", "up", "up"]);
    assert_shows(&harness.frame(), &["Claude", "▌ ● work"]);
}

#[test]
fn resizing_keeps_the_selection_visible() {
    let harness = Harness::new(100, 40, Some(fixtures::snapshot()));
    harness.press_all(&["down", "down", "down", "down"]);
    harness.app.resize(60, 16);
    assert_shows(&harness.frame(), &["▌ ● ada@example.org"]);
    harness.app.resize(160, 50);
    assert_shows(&harness.frame(), &["▌ ● ada@example.org", "Claude  3 accounts"]);
}

#[test]
fn removing_asks_first() {
    let harness = Harness::new(80, 24, Some(fixtures::snapshot()));
    harness.app.press("x");
    assert_shows(&harness.frame(), &["Remove work? Press y to confirm.", "esc cancel"]);
    harness.app.press("escape");
    assert!(!harness.frame().contains("Remove work?"));
    assert!(harness.actions.borrow().is_empty());
    harness.press_all(&["x", "y"]);
    assert_eq!(harness.actions.borrow().len(), 1);
    assert_shows(&harness.frame(), &["Removing work..."]);
}

#[test]
fn renaming_edits_the_alias_inline() {
    let harness = Harness::new(80, 24, Some(fixtures::snapshot()));
    harness.app.press("m");
    assert_shows(&harness.frame(), &["Alias: work", "enter save"]);
    harness.app.type_text(" q");
    assert_shows(&harness.frame(), &["Alias: work q"]);
    assert!(!harness.app.is_quitting());
    harness.app.press("enter");
    let update = AccountUpdate { alias: Some("work q".to_owned()), enabled: None };
    assert_eq!(*harness.actions.borrow(), [Action::Update { account: "claude-work".to_owned(), update }]);
    harness.screen.apply(&Update::Finished { error: None });

    harness.app.press("m");
    harness.press_all(&["backspace", "backspace", "backspace", "backspace", "enter"]);
    let update = AccountUpdate { alias: Some(String::new()), enabled: None };
    assert_eq!(harness.actions.borrow()[1], Action::Update { account: "claude-work".to_owned(), update });
    harness.screen.apply(&Update::Finished { error: None });

    harness.press_all(&["m", "escape", "r"]);
    assert_eq!(harness.actions.borrow()[2], Action::Refresh);
}

#[test]
fn a_lost_connection_keeps_the_last_snapshot() {
    let harness = Harness::new(80, 24, Some(fixtures::snapshot()));
    harness.screen.apply(&Update::Disconnected { reason: "The server closed the connection".to_owned() });
    assert_shows(&harness.frame(), &["Reconnecting", "work", "The server closed the connection"]);
    harness.screen.apply(&Update::Snapshot(fixtures::snapshot()));
    assert!(!harness.frame().contains("Reconnecting"));
}

#[test]
fn explains_why_the_first_connection_failed() {
    let harness = Harness::new(60, 16, None);
    harness.screen.apply(&Update::Disconnected { reason: "could not reach the server".to_owned() });
    assert_shows(&harness.frame(), &["Connecting to the server", "could not reach the server"]);
}

#[test]
fn shows_warnings_and_server_errors() {
    let mut snapshot = fixtures::snapshot();
    snapshot.warnings = vec!["An environment variable overrides the active Claude login on this machine".to_owned()];
    let harness = Harness::new(60, 16, Some(snapshot));
    harness.app.press("n");
    harness.screen.apply(&Update::Finished { error: Some("every other account is paused or exhausted".to_owned()) });
    let frame = harness.frame();
    assert_shows(
        &frame,
        &["An environment variable overrides", "every other account is paused or exhausted", "▌ ● work"],
    );
}

#[test]
fn help_lists_the_keys_and_scrolls_when_short() {
    let harness = Harness::new(60, 16, Some(fixtures::snapshot()));
    harness.app.press("?");
    assert_shows(&harness.frame(), &["up/down, j/k", "Refresh usage", "esc close"]);
    harness.press_all(&["j", "j", "j", "j", "j"]);
    assert_shows(&harness.frame(), &["q, ctrl+c"]);
    harness.app.press("escape");
    assert_shows(&harness.frame(), &["▌ ● work"]);
}

#[test]
fn the_clock_updates_times_without_a_server_call() {
    let harness = Harness::new(80, 30, Some(fixtures::snapshot()));
    harness.frame();
    harness.elapsed.set(120);
    harness.app.advance(CLOCK_INTERVAL);
    assert_shows(&harness.frame(), &["updated 4m ago", "resets in 1h 57m"]);
    assert!(harness.actions.borrow().is_empty());
}

#[test]
fn clicking_selects_an_account() {
    let harness = Harness::new(80, 40, Some(fixtures::snapshot()));
    let row = harness.frame().lines().position(|line| line.contains("old@example.com")).unwrap();
    harness.app.click(10, row as u16);
    assert_shows(&harness.frame(), &["▌   old@example.com"]);
}

#[test]
fn q_and_ctrl_c_quit() {
    for keys in [["q"].as_slice(), &["ctrl+c"], &["m", "ctrl+c"], &["x", "ctrl+c"]] {
        let harness = Harness::new(80, 24, Some(fixtures::snapshot()));
        harness.press_all(keys);
        assert!(harness.app.is_quitting(), "{keys:?}");
    }
}

#[test]
fn long_names_do_not_push_the_status_off_a_narrow_screen() {
    let mut snapshot = fixtures::snapshot();
    snapshot.providers[0].accounts[1].alias = Some("a very long alias that keeps going well past the edge".to_owned());
    snapshot.providers[0].accounts[1].identity.organization = Some("日本語の組織名がとても長い場合の表示".to_owned());
    let harness = Harness::new(60, 30, Some(snapshot));
    assert_shows(&harness.frame(), &["stale  paused  Pro", "updated 2m ago"]);
}

#[test]
fn the_offset_moves_only_as_far_as_needed() {
    assert_eq!(revealing_offset(0, 10, 2, 8), 0);
    assert_eq!(revealing_offset(0, 10, 8, 14), 4);
    assert_eq!(revealing_offset(6, 10, 8, 14), 6);
    assert_eq!(revealing_offset(20, 10, 12, 16), 12);
    assert_eq!(revealing_offset(0, 10, 12, 30), 12);
}

#[test]
fn banked_resets_display_and_can_be_used_from_the_tui() {
    let harness = Harness::new(100, 36, Some(fixtures::with_resets()));
    assert_shows(&harness.frame(), &["Limit resets: 1 available - 1 Full Reset"]);
    harness.app.press("u");
    assert_shows(&harness.frame(), &["Use Full Reset on work? Press y to confirm.", "y reset"]);
    harness.app.press("y");
    assert_eq!(
        *harness.actions.borrow(),
        [Action::RedeemReset {
            account: "claude-work".to_owned(),
            label: "work".to_owned(),
            grant_id: "demo-full".to_owned(),
            reset_label: "Full Reset".to_owned(),
        }]
    );
    harness.screen.apply(&Update::Finished { error: None });
    assert_shows(&harness.frame(), &["Used Full Reset on work."]);

    harness.app.press("down");
    harness.app.press("u");
    assert_shows(
        &harness.frame(),
        &["Limit resets for ada.lovelace@personal.example.com", "Weekly Reset", "Fable Reset", "unavailable"],
    );
    harness.app.press("escape");
    assert_shows(&harness.frame(), &["Claude  3 accounts"]);
}

#[test]
#[ignore = "writes TUI cell JSON; run with SWITCH_TUI_SNAPSHOT=<file>"]
fn dump_tui_showcase() {
    let path = std::env::var_os("SWITCH_TUI_SNAPSHOT").expect("set SWITCH_TUI_SNAPSHOT");
    let harness = Harness::new(96, 36, Some(fixtures::showcase()));
    harness.frame();
    let capture = harness.app.render();
    let theme = harness.app.theme();
    let role = |color| {
        if color == theme.accent {
            "accent"
        } else if color == theme.success {
            "success"
        } else if color == theme.warning {
            "warning"
        } else if color == theme.error {
            "error"
        } else {
            "default"
        }
    };
    let rows: Vec<Vec<serde_json::Value>> = (0..capture.height)
        .map(|row| {
            (0..capture.width)
                .map(|col| {
                    let cell = capture.cell(col, row).unwrap();
                    serde_json::json!({
                        "s": cell.symbol,
                        "fg": role(cell.fg),
                        "bold": cell.attributes.contains(Attributes::BOLD),
                        "dim": cell.attributes.contains(Attributes::DIM),
                    })
                })
                .collect()
        })
        .collect();
    let payload = serde_json::json!({ "width": capture.width, "height": capture.height, "rows": rows });
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_string(&payload).unwrap()).unwrap();
}
