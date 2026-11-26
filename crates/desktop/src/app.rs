use std::fs::{self, File, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSScreen, NSStatusBar};
use objc2_foundation::NSRect;
use rune::{
    App, AppHandle, AppSpec, DisplayTarget, Key, KeyEvent, OverlayPlacement, Tray, TrayEvent, TrayIcon, TrayItem,
    TraySpec, Tx, WindowOptions,
};
use switch_client::Service;
use switch_protocol::Snapshot;

use crate::connection::{self, SharedClient};
use crate::launch_at_login;
use crate::panel::{self, Actions, Panel, WIDTH};
use crate::placement::{self, Rect, Screen};
use crate::preferences::{self, Preferences};
use crate::settings::Page;
use crate::status::{self, TrayGroup};
use crate::tray_image;

const BUNDLE_ID: &str = "dev.switch.desktop";
const REOPEN_GUARD: Duration = Duration::from_millis(400);

pub struct TrayController {
    tray: Tray,
    thickness: f64,
    groups: Vec<TrayGroup>,
    tooltip: String,
    menu: Vec<TrayItem>,
}

impl TrayController {
    fn new(on_event: impl Fn(TrayEvent) + Send + Sync + 'static) -> Result<Self, rune::TrayError> {
        let thickness = NSStatusBar::systemStatusBar().thickness();
        let groups = status::tray_groups(None, &Preferences::default());
        let tooltip = status::tooltip(None, &Preferences::default(), Timestamp::now());
        let menu = status::menu(None, &Preferences::default(), false);
        let spec = TraySpec::new(BUNDLE_ID, "Switch", icon(&groups, thickness)).tooltip(&tooltip).menu(menu.clone());
        Ok(Self { tray: Tray::new(spec, on_event)?, thickness, groups, tooltip, menu })
    }

    pub fn sync(&mut self, snapshot: Option<&Snapshot>, preferences: &Preferences, connected: bool, now: Timestamp) {
        let groups = status::tray_groups(snapshot, preferences);
        if groups != self.groups {
            if let Err(error) = self.tray.set_icon(icon(&groups, self.thickness)) {
                eprintln!("switch-desktop: could not update the menu bar icon: {error}");
            }
            self.groups = groups;
        }
        let tooltip = status::tooltip(snapshot, preferences, now);
        if tooltip != self.tooltip {
            self.tray.set_tooltip(&tooltip);
            self.tooltip = tooltip;
        }
        let menu = status::menu(snapshot, preferences, connected);
        if menu != self.menu {
            self.tray.set_menu(menu.clone());
            self.menu = menu;
        }
    }

    fn panel_frame(&self) -> Option<placement::PanelFrame> {
        placement::panel_frame(self.tray.rect()?, &screens(), self.thickness, f64::from(WIDTH))
    }
}

fn icon(groups: &[TrayGroup], thickness: f64) -> TrayIcon {
    let bitmap = tray_image::render(groups, thickness);
    TrayIcon::rgba(bitmap.width, bitmap.height, bitmap.rgba).template(true)
}

fn screens() -> Vec<Screen> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Vec::new();
    };
    let screens = NSScreen::screens(mtm);
    let Some(primary_height) = screens.iter().next().map(|screen| screen.frame().size.height) else {
        return Vec::new();
    };
    let flip = |rect: NSRect| Rect {
        x: rect.origin.x,
        y: primary_height - rect.origin.y - rect.size.height,
        width: rect.size.width,
        height: rect.size.height,
    };
    screens
        .iter()
        .map(|screen| Screen {
            frame: flip(screen.frame()),
            visible: flip(screen.visibleFrame()),
            scale: screen.backingScaleFactor(),
        })
        .collect()
}

fn toggle(handle: &AppHandle<Panel>) {
    let window = handle.clone();
    handle.update(move |panel, _| {
        if panel.dismissed_at.take().is_some_and(|at| at.elapsed() < REOPEN_GUARD) {
            return;
        }
        if panel.shown {
            panel.shown = false;
            panel.clear_transient();
            window.hide();
            return;
        }
        if let Some(frame) = panel.tray.as_ref().and_then(TrayController::panel_frame) {
            panel.max_height = frame.max_height as f32;
            let height = if panel.content_height > 0.0 { panel.content_height.min(panel.max_height) } else { 320.0 };
            window.place(
                DisplayTarget::Primary,
                OverlayPlacement::Rect { x: frame.x as f32, y: frame.y as f32, width: WIDTH, height },
            );
        }
        panel.shown = true;
        panel.now = Timestamp::now();
        window.open();
    });
}

fn on_focus(panel: &mut Panel, focused: bool, tx: &mut Tx) {
    if !focused && panel.shown {
        panel.open(Page::Usage);
        panel.shown = false;
        panel.clear_transient();
        panel.dismissed_at = Some(Instant::now());
        tx.close_window();
    }
}

fn on_key(panel: &mut Panel, key: &KeyEvent, tx: &mut Tx) -> bool {
    if key.key != Key::Escape {
        return false;
    }
    if panel.cancel_prompt() || panel.dismiss_resets() || panel.go_back() {
        return true;
    }
    panel.shown = false;
    panel.clear_transient();
    tx.close_window();
    true
}

pub fn spec() -> AppSpec<Panel> {
    AppSpec {
        families: ("System Font", "ui-monospace"),
        on_key: Some(on_key),
        ..AppSpec::new("Switch", (WIDTH, 320.0), Panel::default, panel::view)
    }
}

fn single_instance() -> Option<File> {
    let home = switch_client::home_dir();
    fs::create_dir_all(&home).ok()?;
    let lock = File::create(home.join("desktop.lock")).ok()?;
    match lock.try_lock() {
        Ok(()) => Some(lock),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Error(error)) => {
            eprintln!("switch-desktop: could not take the instance lock: {error}");
            Some(lock)
        }
    }
}

pub fn run() {
    let Some(_instance) = single_instance() else {
        eprintln!("switch-desktop: Switch is already running");
        return;
    };
    let app = App::new(spec())
        .windowless()
        .accessory()
        .fit_content_height()
        .configured_options(
            WindowOptions::launcher_panel().macos(|native| native.corner_radius(panel::RADIUS).shadow(true)),
        )
        .on_focus(on_focus);
    let handle = app.handle();
    let client = SharedClient::default();

    let tray_handle = handle.clone();
    let preferences_path = preferences::path(&switch_client::home_dir());
    let preferences = preferences::load(&preferences_path);
    let launch_at_login = launch_at_login::enabled();
    let actions = Actions { client: client.clone(), handle: handle.clone() };
    handle.update(move |panel, tx| {
        let tray = TrayController::new(move |event| match event {
            TrayEvent::Activate { .. } => toggle(&tray_handle),
            TrayEvent::Menu(id) => tray_handle.update(move |panel, _| panel.on_menu(&id)),
            _ => {}
        });
        let tray = match tray {
            Ok(tray) => tray,
            Err(error) => {
                eprintln!("switch-desktop: could not add the menu bar item: {error}");
                tx.quit();
                return;
            }
        };
        panel.tray = Some(tray);
        panel.actions = Some(actions);
        panel.launch_at_login = launch_at_login;
        panel.use_preferences(preferences, preferences_path);
    });

    let updates = handle.clone();
    connection::spawn_events(Service::new(connection::sw_path()), client, move |update| {
        updates.update(move |panel, _| panel.apply(update));
    });
    let clock = handle.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(30));
            clock.update(|panel, _| {
                panel.now = Timestamp::now();
                panel.sync_tray();
            });
        }
    });
    app.run();
}
