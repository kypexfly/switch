use std::collections::HashMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use rune::{
    AppHandle, Color, Cursor, Cx, Drag, El, Key, KeyEvent, LoopSpec, Phase, PopupItem, PopupMenu, ScrollSpec, TextEdit,
    TextStyle, col, color, el, icon, oklch, oklcha, row, text, text_input,
};
use switch_client::{Client, oauth};
use switch_protocol::{
    Account, AccountStatus, AccountUpdate, Identity, LimitReset, OrderUpdate, ProviderState, Settings, SettingsUpdate,
    Snapshot, Strategy, UsageWindow,
};

use crate::app::TrayController;
use crate::connection::{SharedClient, Update};
use crate::format::{self, Severity, Tone};
use crate::icons;
use crate::launch_at_login;
use crate::notify;
use crate::preferences::{self, Preferences};
use crate::reorder::{self, List, Reorder};
use crate::settings::{self, Page};

pub const WIDTH: f32 = 360.0;
pub const RADIUS: f32 = 12.0;
const DRAG_START: f32 = 4.0;
const SECTION_GAP: f32 = 6.0;
const ACCOUNT_GAP: f32 = 2.0;

#[derive(Clone, Debug, PartialEq)]
pub enum Task {
    Refresh,
    Switch(String),
    Reset(String),
    Save,
    SignIn(String),
    Account,
    Order,
    Settings,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Prompt {
    Rename(String),
    Remove(String),
}

pub struct Panel {
    pub snapshot: Option<Snapshot>,
    pub connected: bool,
    pub connection_error: Option<String>,
    pub running: Vec<Task>,
    pub failed: Option<(Task, String)>,
    pub restart_hint: Option<String>,
    pub settings_before: Option<Settings>,
    pub prompt: Option<Prompt>,
    pub expanded_resets: Option<String>,
    pub confirming_reset: Option<(String, String)>,
    pub draft: TextEdit,
    pub saved_name: String,
    pub page: Page,
    pub preferences: Preferences,
    pub preferences_path: Option<PathBuf>,
    pub preferences_error: Option<String>,
    pub launch_at_login: bool,
    pub reorder: Option<Reorder>,
    pub heights: HashMap<String, f32>,
    pub now: Timestamp,
    pub zone: TimeZone,
    pub exact_resets: bool,
    pub max_height: f32,
    pub content_height: f32,
    pub shown: bool,
    pub dismissed_at: Option<Instant>,
    pub actions: Option<Actions>,
    pub tray: Option<TrayController>,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            snapshot: None,
            connected: false,
            connection_error: None,
            running: Vec::new(),
            failed: None,
            restart_hint: None,
            settings_before: None,
            prompt: None,
            expanded_resets: None,
            confirming_reset: None,
            draft: TextEdit::default(),
            saved_name: String::new(),
            page: Page::Usage,
            preferences: Preferences::default(),
            preferences_path: None,
            preferences_error: None,
            launch_at_login: false,
            reorder: None,
            heights: HashMap::new(),
            now: Timestamp::now(),
            zone: TimeZone::system(),
            exact_resets: false,
            max_height: 640.0,
            content_height: 0.0,
            shown: false,
            dismissed_at: None,
            actions: None,
            tray: None,
        }
    }
}

#[derive(Clone)]
pub struct Actions {
    pub client: SharedClient,
    pub handle: AppHandle<Panel>,
}

impl Panel {
    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Snapshot(snapshot) => {
                if self.preferences.notifications
                    && self.actions.is_some()
                    && let Some(previous) = &self.snapshot
                {
                    for notification in notify::detect(previous, &snapshot) {
                        notify::send(&notification);
                    }
                }
                self.snapshot = Some(snapshot);
                self.settings_before = None;
                self.connected = true;
                self.connection_error = None;
                if let Some(account_id) = &self.expanded_resets {
                    let has_available = self
                        .account(account_id)
                        .is_some_and(|account| account.resets.iter().any(|reset| reset.available(self.now)));
                    if !has_available {
                        self.expanded_resets = None;
                        self.confirming_reset = None;
                    }
                }
                if let Some((account_id, grant_id)) = &self.confirming_reset {
                    let still_usable = self.account(account_id).is_some_and(|account| {
                        account
                            .resets
                            .iter()
                            .any(|reset| reset.id == *grant_id && reset.available(self.now) && reset.usable)
                    });
                    if !still_usable {
                        self.confirming_reset = None;
                    }
                }
                let gone = match &self.page {
                    Page::Usage | Page::Settings | Page::AutoSwitch => false,
                    Page::Provider(id) => self.provider(id).is_none(),
                    Page::Account(id) => self.account(id).is_none(),
                };
                if gone {
                    self.open(Page::Settings);
                }
            }
            Update::Lost(message) => {
                self.connected = false;
                self.connection_error = Some(message);
            }
        }
        self.sync_tray();
    }

    pub fn sync_tray(&mut self) {
        if let Some(tray) = &mut self.tray {
            tray.sync(self.snapshot.as_ref(), &self.preferences, self.connected, self.now);
        }
    }

    /// Choices that could not be read are never saved over.
    pub fn use_preferences(&mut self, loaded: Result<Preferences>, path: PathBuf) {
        match loaded {
            Ok(preferences) => {
                self.preferences = preferences;
                self.preferences_path = Some(path);
            }
            Err(error) => self.preferences_error = Some(format!("Changes here won't be kept: {error:#}")),
        }
        if self.actions.is_some() && self.preferences.notifications {
            notify::prepare();
        }
        self.sync_tray();
    }

    pub fn edit_preferences(&mut self, edit: impl FnOnce(&mut Preferences)) {
        edit(&mut self.preferences);
        if let Some(path) = &self.preferences_path {
            let saved = preferences::save(path, &self.preferences);
            self.preferences_error = saved.err().map(|error| format!("{error:#}"));
        }
        self.sync_tray();
    }

    pub fn set_notifications(&mut self, on: bool) {
        self.edit_preferences(|all| all.notifications = on);
        if on && self.actions.is_some() {
            notify::prepare();
        }
    }

    pub fn toggle_tray_window(&mut self, provider: &str, window: &str) {
        if let Some(provider) = self.provider(provider).cloned() {
            self.edit_preferences(|preferences| preferences.toggle_tray_window(&provider, window));
        }
    }

    pub fn set_launch_at_login(&mut self, on: bool) {
        match launch_at_login::set_enabled(on) {
            Ok(()) => {
                self.launch_at_login = on;
                self.preferences_error = None;
            }
            Err(error) => self.preferences_error = Some(format!("{error:#}")),
        }
    }

    pub fn open(&mut self, page: Page) {
        self.save_name();
        self.cancel_prompt();
        self.expanded_resets = None;
        self.confirming_reset = None;
        self.cancel_drag();
        self.saved_name = match &page {
            Page::Account(id) => self.account(id).and_then(|account| account.alias.clone()).unwrap_or_default(),
            _ => String::new(),
        };
        self.draft = TextEdit::new(self.saved_name.clone());
        self.page = page;
    }

    pub fn go_back(&mut self) -> bool {
        match self.page {
            Page::Usage => return false,
            Page::Settings => self.open(Page::Usage),
            Page::AutoSwitch | Page::Provider(_) | Page::Account(_) => self.open(Page::Settings),
        }
        true
    }

    pub fn leave_unsaved(&mut self) -> bool {
        self.draft = TextEdit::new(self.saved_name.clone());
        self.cancel_prompt() || self.go_back()
    }

    pub fn clear_transient(&mut self) {
        self.failed = None;
        self.restart_hint = None;
        self.expanded_resets = None;
        self.confirming_reset = None;
    }

    pub fn save_name(&mut self) {
        let Page::Account(id) = &self.page else {
            return;
        };
        let (id, alias) = (id.clone(), self.draft.text.trim().to_owned());
        if self.connected && alias != self.saved_name && self.account(&id).is_some() {
            self.saved_name = alias.clone();
            self.update_account(id, AccountUpdate { alias: Some(alias), enabled: None });
        }
    }

    fn perform(&mut self, task: Task, request: impl FnOnce(&Client) -> Result<()> + Send + 'static) {
        let Some(actions) = self.actions.clone() else {
            return;
        };
        self.failed = None;
        self.restart_hint = None;
        self.running.push(task.clone());
        thread::spawn(move || {
            let result = actions.client.get().and_then(|client| request(&client));
            actions.handle.update(move |panel, _| panel.finish(task, result));
        });
    }

    fn finish(&mut self, task: Task, result: Result<()>) {
        if let Some(index) = self.running.iter().position(|running| *running == task) {
            self.running.remove(index);
        }
        if task == Task::Order {
            self.reorder = None;
        }
        match result {
            Ok(()) => {
                if task == Task::Settings {
                    self.settings_before = None;
                }
                if matches!(task, Task::Reset(_)) {
                    self.confirming_reset = None;
                }
                if let Task::Switch(id) = &task
                    && let Some(provider) = self.provider_of(id)
                {
                    self.restart_hint = Some(format!(
                        "New {} sessions use this account. Restart running ones to switch them now.",
                        provider.name
                    ));
                }
            }
            Err(error) => {
                if task == Task::Settings
                    && let Some(previous) = self.settings_before.take()
                    && let Some(snapshot) = &mut self.snapshot
                {
                    snapshot.settings = previous;
                    self.sync_tray();
                }
                self.failed = Some((task, format!("{error:#}")));
            }
        }
    }

    fn switching(&self) -> bool {
        self.running.iter().any(|task| matches!(task, Task::Switch(_)))
    }

    fn refreshing(&self) -> bool {
        self.running.contains(&Task::Refresh)
    }

    fn signing_in(&self) -> bool {
        self.running.iter().any(|task| matches!(task, Task::SignIn(_)))
    }

    fn resetting(&self, account: &str) -> bool {
        self.running.iter().any(|task| matches!(task, Task::Reset(id) if id == account))
    }

    fn any_resetting(&self) -> bool {
        self.running.iter().any(|task| matches!(task, Task::Reset(_)))
    }

    pub fn toggle_resets(&mut self, account: String) {
        if self.expanded_resets.as_deref() == Some(account.as_str()) {
            self.expanded_resets = None;
            self.confirming_reset = None;
        } else {
            self.expanded_resets = Some(account);
            self.confirming_reset = None;
        }
    }

    pub fn start_reset_confirm(&mut self, account: String, grant_id: String) {
        self.expanded_resets = Some(account.clone());
        self.confirming_reset = Some((account, grant_id));
    }

    pub fn confirm_reset(&mut self, account: String, grant_id: String) {
        if !self.any_resetting() {
            self.perform(Task::Reset(account.clone()), move |client| {
                client.redeem_reset(&account, &grant_id).map(drop)
            });
        }
    }

    pub fn dismiss_resets(&mut self) -> bool {
        if self.confirming_reset.take().is_some() {
            return true;
        }
        self.expanded_resets.take().is_some()
    }

    pub fn refresh(&mut self) {
        if !self.refreshing() {
            self.perform(Task::Refresh, |client| client.refresh().map(drop));
        }
    }

    pub fn activate(&mut self, account: String) {
        if !self.switching() {
            self.perform(Task::Switch(account.clone()), move |client| client.activate(&account).map(drop));
        }
    }

    fn save_login(&mut self, provider: String) {
        self.perform(Task::Save, move |client| client.add_account(&provider).map(drop));
    }

    pub fn sign_in(&mut self, provider: String) {
        if self.signing_in() {
            return;
        }
        self.perform(Task::SignIn(provider.clone()), move |client| {
            let redirect_uri = oauth::redirect_uri();
            let listener = TcpListener::bind(("127.0.0.1", oauth::PORT))
                .map_err(|error| anyhow::anyhow!("Could not listen on {redirect_uri} ({error})."))?;
            let sign_in = client.begin_sign_in(&provider, &redirect_uri)?;
            oauth::open_in_browser(&sign_in.url);
            let code = oauth::wait_for_callback(&listener, &sign_in.state, Some(Duration::from_secs(300)))?;
            client.finish_sign_in(&provider, &sign_in.state, &code)?;
            Ok(())
        });
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderState> {
        self.snapshot.iter().flat_map(|snapshot| &snapshot.providers).find(|provider| provider.id == id)
    }

    pub fn provider_of(&self, account: &str) -> Option<&ProviderState> {
        let mut providers = self.snapshot.iter().flat_map(|snapshot| &snapshot.providers);
        providers.find(|provider| provider.accounts.iter().any(|other| other.id == account))
    }

    pub fn account(&self, id: &str) -> Option<&Account> {
        self.snapshot
            .iter()
            .flat_map(|snapshot| &snapshot.providers)
            .flat_map(|provider| &provider.accounts)
            .find(|account| account.id == id)
    }

    fn on_account_menu(&mut self, account: String, picked: &str) {
        match picked {
            "rename" => self.start_rename(account),
            "pause" => self.update_account(account, AccountUpdate { alias: None, enabled: Some(false) }),
            "resume" => self.update_account(account, AccountUpdate { alias: None, enabled: Some(true) }),
            "remove" => self.prompt = Some(Prompt::Remove(account)),
            _ => {}
        }
    }

    fn start_rename(&mut self, id: String) {
        let alias = self.account(&id).and_then(|account| account.alias.clone()).unwrap_or_default();
        self.draft = TextEdit::new(alias);
        self.draft.anchor = 0;
        self.draft.focus = self.draft.text.len();
        self.draft.focused = true;
        self.prompt = Some(Prompt::Rename(id));
    }

    fn save_rename(&mut self) {
        if let Some(Prompt::Rename(id)) = self.prompt.take() {
            let alias = self.draft.text.trim().to_owned();
            self.update_account(id, AccountUpdate { alias: Some(alias), enabled: None });
        }
    }

    pub fn cancel_prompt(&mut self) -> bool {
        let prompt = self.prompt.take();
        if let Some(Prompt::Rename(_)) = prompt {
            self.draft = TextEdit::default();
        }
        prompt.is_some()
    }

    pub fn update_account(&mut self, account: String, update: AccountUpdate) {
        self.perform(Task::Account, move |client| client.update_account(&account, &update).map(drop));
    }

    fn remove_account(&mut self, account: String) {
        self.prompt = None;
        self.perform(Task::Account, move |client| client.remove_account(&account).map(drop));
    }

    /// What the usage page shows.
    fn visible(&self) -> Option<Snapshot> {
        self.snapshot.as_ref().map(|snapshot| self.preferences.panel_snapshot(snapshot))
    }

    pub fn ids(&self, list: &List) -> Vec<String> {
        let shown = if self.page == Page::Usage { self.visible() } else { self.snapshot.clone() };
        let providers = shown.as_ref().map_or(&[][..], |snapshot| &snapshot.providers);
        match list {
            List::Providers => providers.iter().map(|provider| provider.id.clone()).collect(),
            List::Accounts(provider) => {
                let provider = providers.iter().find(|other| other.id == *provider);
                provider.iter().flat_map(|provider| &provider.accounts).map(|account| account.id.clone()).collect()
            }
        }
    }

    fn on_drag(&mut self, list: &List, id: &str, drag: &Drag) {
        match drag.phase {
            Phase::Down => {}
            Phase::Move => self.drag_to(list, id, drag.dy),
            Phase::Up if drag.cancelled() => self.cancel_drag(),
            Phase::Up => self.drop_dragged(),
        }
    }

    fn saving_order(&self) -> bool {
        self.reorder.as_ref().is_some_and(|reorder| reorder.dropped)
    }

    fn drag_to(&mut self, list: &List, id: &str, travel: f32) {
        if !self.connected || self.saving_order() || (self.reorder.is_none() && travel.abs() < DRAG_START) {
            return;
        }
        let ids = self.ids(list);
        let Some(from) = ids.iter().position(|other| other == id).filter(|_| ids.len() > 1) else {
            self.reorder = None;
            return;
        };
        let gap = match (&self.page, list) {
            (Page::Usage, List::Providers) => SECTION_GAP,
            (Page::Usage, List::Accounts(_)) => ACCOUNT_GAP,
            (_, List::Providers) => settings::GROUP_GAP,
            (_, List::Accounts(_)) => 0.0,
        };
        let height = |id: &String| self.heights.get(&row_key(&self.page, list, id)).copied().unwrap_or_default();
        let heights: Vec<f32> = ids.iter().map(height).collect();
        let (to, offset) = reorder::place(&heights, gap, from, travel);
        self.reorder = Some(Reorder { list: list.clone(), id: id.to_owned(), to, offset, dropped: false });
    }

    pub fn move_by(&mut self, list: &List, id: &str, step: isize) {
        if !self.connected || self.reorder.is_some() {
            return;
        }
        let ids = self.ids(list);
        let place = ids.iter().position(|other| other == id).and_then(|from| from.checked_add_signed(step));
        if let Some(to) = place.filter(|to| *to < ids.len()) {
            self.reorder = Some(Reorder { list: list.clone(), id: id.to_owned(), to, offset: 0.0, dropped: false });
            self.drop_dragged();
        }
    }

    pub fn cancel_drag(&mut self) {
        if !self.saving_order() {
            self.reorder = None;
        }
    }

    fn drop_dragged(&mut self) {
        if self.saving_order() {
            return;
        }
        let Some(reorder) = self.reorder.take() else {
            return;
        };
        let before = self.ids(&reorder.list);
        let order: Vec<String> = reorder.arrange(&before, |id| id).into_iter().cloned().collect();
        if order == before {
            return;
        }
        let update = match reorder.list {
            List::Providers => OrderUpdate { providers: Some(order), accounts: None },
            List::Accounts(_) => OrderUpdate { providers: None, accounts: Some(order) },
        };
        self.perform(Task::Order, move |client| client.update_order(&update).map(drop));
        if self.running.contains(&Task::Order) {
            self.reorder = Some(Reorder { offset: 0.0, dropped: true, ..reorder });
        }
    }

    pub fn arranged<'a, T>(&self, list: &List, items: &'a [T], id: impl Fn(&T) -> &str) -> Vec<&'a T> {
        match self.reorder.as_ref().filter(|reorder| reorder.list == *list) {
            Some(reorder) => reorder.arrange(items, id),
            None => items.iter().collect(),
        }
    }

    fn apply_optimistic_settings(&mut self, edit: impl FnOnce(&mut Settings), update: SettingsUpdate) {
        if let Some(snapshot) = &mut self.snapshot {
            self.settings_before.get_or_insert_with(|| snapshot.settings.clone());
            edit(&mut snapshot.settings);
        }
        self.update_settings(update);
    }

    pub fn set_auto_switch(&mut self, on: bool) {
        self.apply_optimistic_settings(
            |settings| settings.auto_switch = on,
            SettingsUpdate { auto_switch: Some(on), ..SettingsUpdate::default() },
        );
    }

    pub fn set_threshold(&mut self, threshold: f64) {
        self.apply_optimistic_settings(
            |settings| settings.threshold = threshold,
            SettingsUpdate { threshold: Some(threshold), ..SettingsUpdate::default() },
        );
    }

    pub fn set_cooldown(&mut self, seconds: u64) {
        self.apply_optimistic_settings(
            |settings| settings.cooldown_seconds = seconds,
            SettingsUpdate { cooldown_seconds: Some(seconds), ..SettingsUpdate::default() },
        );
    }

    pub fn set_strategy(&mut self, strategy: Strategy) {
        self.apply_optimistic_settings(
            |settings| settings.strategy = strategy,
            SettingsUpdate { strategy: Some(strategy), ..SettingsUpdate::default() },
        );
    }

    fn update_settings(&mut self, update: SettingsUpdate) {
        self.perform(Task::Settings, move |client| client.update_settings(&update).map(drop));
    }

    pub fn on_menu(&mut self, id: &str) {
        match id {
            "refresh" => self.refresh(),
            "settings" => self.open(Page::Settings),
            "strategy:best" => self.set_strategy(Strategy::Best),
            "strategy:next" => self.set_strategy(Strategy::Next),
            "quit" => {
                if let Some(actions) = &self.actions {
                    actions.handle.quit();
                }
            }
            id => {
                if let Some(account) = id.strip_prefix("account:") {
                    self.activate(account.to_owned());
                } else if let Some(threshold) = id.strip_prefix("threshold:").and_then(|id| id.parse().ok()) {
                    self.set_threshold(threshold);
                } else if let Some(seconds) = id.strip_prefix("cooldown:").and_then(|id| id.parse().ok()) {
                    self.set_cooldown(seconds);
                }
            }
        }
    }

    fn last_update(&self) -> Option<Timestamp> {
        self.snapshot
            .iter()
            .flat_map(|snapshot| &snapshot.providers)
            .flat_map(|provider| &provider.accounts)
            .filter_map(|account| account.usage.as_ref().map(|usage| usage.fetched_at))
            .max()
    }
}

pub struct Palette {
    pub background: Color,
    pub border: Color,
    pub active: Color,
    pub active_border: Color,
    pub text: Color,
    pub secondary: Color,
    pub separator: Color,
    pub hover: Color,
    pub pace: Color,
    pub track: Color,
    pub knob: Color,
    pub button: Color,
    pub button_border: Color,
    pub accent: Color,
    pub on_accent: Color,
    pub warning: Color,
    pub danger: Color,
}

fn palette(dark: bool) -> Palette {
    let themed = |on_dark, on_light| color::hex(if dark { on_dark } else { on_light }).expect("valid hex color");
    let ink = |on_dark, on_light| {
        if dark { oklcha(1.0, 0.0, 0.0, on_dark) } else { oklcha(0.0, 0.0, 0.0, on_light) }
    };
    Palette {
        background: themed("#1e1f22", "#f4f4f6"),
        border: ink(0.12, 0.15),
        active: themed("#2c2d31", "#ffffff"),
        active_border: themed("#2e2f32", "#e3e3e5"),
        text: themed("#f5f5f7", "#1c1c1f"),
        secondary: themed("#a3a4aa", "#63646b"),
        separator: ink(0.08, 0.08),
        hover: ink(0.04, 0.04),
        pace: ink(0.7, 0.5),
        track: ink(0.12, 0.1),
        knob: oklch(1.0, 0.0, 0.0),
        button: if dark { oklcha(1.0, 0.0, 0.0, 0.09) } else { oklch(1.0, 0.0, 0.0) },
        button_border: ink(0.08, 0.15),
        accent: themed("#4796ff", "#1f6fe0"),
        on_accent: oklch(1.0, 0.0, 0.0),
        warning: themed("#f2a33c", "#b85f00"),
        danger: themed("#ff6f66", "#d0342c"),
    }
}

impl Palette {
    fn tone(&self, tone: Tone) -> Color {
        match tone {
            Tone::Muted => self.secondary,
            Tone::Warning => self.warning,
            Tone::Danger => self.danger,
        }
    }
}

pub const NAME: TextStyle = TextStyle::new(13.0, 600.0);
pub const SECTION: TextStyle = TextStyle::new(12.0, 600.0);
pub const BODY: TextStyle = TextStyle::new(12.0, 400.0);
pub const SMALL: TextStyle = TextStyle::new(11.0, 400.0);
const ACTIVE: TextStyle = TextStyle::new(11.0, 500.0);
const PERCENT: TextStyle = TextStyle::new(12.0, 600.0).features("'tnum' 1");
const RESET: TextStyle = TextStyle::new(11.0, 400.0).features("'tnum' 1");
const BUTTON: TextStyle = TextStyle::new(12.0, 500.0);
const DIMMED: f32 = 0.5;
pub const DISABLED: f32 = 0.45;

pub fn view(panel: &Panel, cx: &mut Cx<Panel>) -> El {
    let palette = palette(cx.system_dark());
    let in_settings = panel.page != Page::Usage;
    let mut view = col()
        .key("panel")
        .w_full()
        .max_h(panel.max_height)
        .fill(palette.background)
        .border(1.0, palette.border)
        .radius(RADIUS)
        .on_layout(|panel: &mut Panel, rect| panel.content_height = rect[3]);
    let content = col().shrink().min_h(0.0).scroll(ScrollSpec::default()).px(8.0);
    match &panel.snapshot {
        Some(snapshot) if in_settings => view.child(settings::header(panel, &palette)).child(
            content
                .key(format!("{:?}", panel.page))
                .pt(8.0)
                .pb(12.0)
                .gap(settings::GROUP_GAP)
                .children(settings::body(panel, snapshot, &palette)),
        ),
        snapshot => {
            view = view.child(content.key("body").pt(6.0).pb(8.0).gap(SECTION_GAP).children(body(panel, &palette)));
            match snapshot {
                Some(snapshot) => view.child(footer(panel, &snapshot.settings, &palette)),
                None => view,
            }
        }
    }
}

fn body(panel: &Panel, palette: &Palette) -> Vec<El> {
    let visible = panel.visible();
    let Some(snapshot) = &visible else {
        return vec![match &panel.connection_error {
            Some(error) => empty("Server offline, reconnecting", Some(error), palette),
            None => empty("Starting server", None, palette),
        }];
    };
    let mut items = Vec::new();
    if !panel.connected {
        items.push(notice("offline", "Server offline, reconnecting", palette.danger, palette));
    }
    if let Some((task, error)) = &panel.failed
        && !shown_on_account(task, snapshot)
    {
        items.push(dismissible_notice("error", error, palette.danger, palette, |panel| panel.failed = None));
    }
    if let Some(hint) = &panel.restart_hint {
        items.push(dismissible_notice("restart-hint", hint, palette.accent, palette, |panel| {
            panel.restart_hint = None;
        }));
    }
    for (index, warning) in snapshot.warnings.iter().enumerate() {
        items.push(notice(&format!("warning-{index}"), warning, palette.warning, palette));
    }
    let providers = panel.arranged(&List::Providers, &snapshot.providers, |provider| &provider.id);
    items.extend(providers.into_iter().map(|provider| section(provider, panel, palette)));
    if panel.snapshot.as_ref().is_some_and(|all| all.providers.is_empty()) {
        items.push(empty("No accounts yet", Some("Log in to a supported tool, then refresh."), palette));
    } else if snapshot.providers.is_empty() {
        items.push(empty("Everything is hidden", Some("Choose what to show in Settings."), palette));
    }
    items
}

fn shown_on_account(task: &Task, snapshot: &Snapshot) -> bool {
    match task {
        Task::Switch(id) => snapshot
            .providers
            .iter()
            .flat_map(|provider| &provider.accounts)
            .any(|account| account.id == *id && !account.active),
        Task::Reset(id) => {
            snapshot.providers.iter().flat_map(|provider| &provider.accounts).any(|account| account.id == *id)
        }
        _ => false,
    }
}

pub fn empty(title: &str, message: Option<&str>, palette: &Palette) -> El {
    let mut empty = col().px(24.0).py(32.0).gap(4.0).items_center().child(text(title).font(NAME).color(palette.text));
    if let Some(message) = message {
        empty = empty.child(text(message).font(SMALL).color(palette.secondary).wrap());
    }
    empty
}

pub fn notice(key: &str, message: &str, color: Color, palette: &Palette) -> El {
    row()
        .key(key.to_owned())
        .px(12.0)
        .py(8.0)
        .gap(8.0)
        .center_y()
        .radius(8.0)
        .fill(color.alpha(0.12))
        .child(icon(&icons::WARNING, 14.0).color(color))
        .child(text(message).font(SMALL).color(palette.text).wrap().grow(1.0).min_w(0.0))
}

pub fn dismissible_notice(
    key: &str,
    message: &str,
    color: Color,
    palette: &Palette,
    on_dismiss: impl Fn(&mut Panel) + 'static,
) -> El {
    let dismiss = row()
        .key(format!("{key}-dismiss"))
        .role("button")
        .label("Dismiss")
        .focusable(true)
        .cursor(Cursor::Pointer)
        .size(20.0, 20.0)
        .center()
        .radius(4.0)
        .hover_fill(palette.hover)
        .on_press(on_dismiss)
        .child(icon(&icons::CLOSE, 12.0).color(palette.secondary));
    notice(key, message, color, palette).child(dismiss)
}

fn row_key(page: &Page, list: &List, id: &str) -> String {
    let prefix = if *page == Page::Usage { "" } else { "settings-" };
    match list {
        List::Providers => format!("{prefix}provider-{id}"),
        List::Accounts(_) => format!("{prefix}account-{id}"),
    }
}

pub fn drag_handle(handle: El, list: List, id: &str) -> El {
    let id = id.to_owned();
    handle.on_drag(move |panel: &mut Panel, drag, _| panel.on_drag(&list, &id, drag))
}

pub fn movable(row: El, list: &List, id: &str, panel: &Panel, palette: &Palette) -> El {
    let key = row_key(&panel.page, list, id);
    let row = row.key(key.clone()).on_layout_tx(move |panel: &mut Panel, rect, tx| {
        panel.heights.insert(key.clone(), rect[3]);
        tx.live_only();
    });
    let section = panel.page == Page::Usage && *list == List::Providers;
    let fill = if section { palette.background } else { palette.active };
    let border = if panel.page == Page::Usage { palette.active_border } else { palette.border };
    let dragged = |reorder: &&Reorder| !reorder.dropped && reorder.list == *list && reorder.id == id;
    match panel.reorder.as_ref().filter(dragged) {
        Some(reorder) => row.fill(fill).border(1.0, border).radius(8.0).offset(0.0, reorder.offset).raise(),
        None => row,
    }
}

fn section(provider: &ProviderState, panel: &Panel, palette: &Palette) -> El {
    let unsaved = provider.unsaved_login.as_ref().filter(|_| provider.switchable);
    let header = section_header(provider, panel, palette).key(format!("header-{}", provider.id));
    let mut section = col().gap(ACCOUNT_GAP).child(drag_handle(header, List::Providers, &provider.id));
    if let Some(login) = unsaved {
        section = section.child(unsaved_login(provider, login, panel, palette));
    }
    if provider.accounts.is_empty() && unsaved.is_none() {
        section = section.child(no_accounts(provider, panel, palette));
    }
    let accounts = panel.arranged(&List::Accounts(provider.id.clone()), &provider.accounts, |account| &account.id);
    let section = section.children(accounts.into_iter().map(|account| account_row(account, provider, panel, palette)));
    movable(section, &List::Providers, &provider.id, panel, palette)
}

fn section_header(provider: &ProviderState, panel: &Panel, palette: &Palette) -> El {
    let mut header = row().h(28.0).px(12.0).gap(6.0).center_y();
    if let Some(mark) = icons::provider_mark(&provider.id) {
        header = header.child(icon(mark, 14.0).color(palette.text));
    }
    header = header
        .child(text(&provider.name).font(SECTION).color(palette.text).truncate().shrink().min_w(0.0))
        .child(el().grow(1.0));
    let has_resets = provider
        .accounts
        .iter()
        .flat_map(|account| &account.usage)
        .flat_map(|usage| &usage.windows)
        .any(|window| window.resets_at.is_some());
    if !has_resets {
        return header;
    }
    let label = if panel.exact_resets { "Resets at" } else { "Resets in" };
    header.child(
        row()
            .key(format!("resets-{}", provider.id))
            .role("button")
            .label("Switch between time left and reset time")
            .focusable(true)
            .cursor(Cursor::Pointer)
            .gap(2.0)
            .center_y()
            .on_press(|panel: &mut Panel| panel.exact_resets = !panel.exact_resets)
            .child(text(label).font(SMALL).color(palette.secondary).nowrap())
            .child(icon(&icons::UP_DOWN, 10.0).color(palette.secondary)),
    )
}

fn card(active: bool, palette: &Palette) -> El {
    let card = col().px(12.0).py(8.0).gap(4.0).radius(8.0);
    if active { card.fill(palette.active).border(1.0, palette.active_border) } else { card }
}

fn card_header(name: &str, name_color: Color, details: Vec<(String, Tone)>, palette: &Palette) -> El {
    let mut header =
        row().h(24.0).gap(6.0).center_y().child(text(name).font(NAME).color(name_color).truncate().shrink().min_w(0.0));
    for (detail, tone) in details {
        header = header.child(text(detail).font(SMALL).color(palette.tone(tone)).nowrap());
    }
    header.child(el().grow(1.0))
}

pub fn note(message: &str, color: Color) -> El {
    text(message).font(SMALL).color(color).wrap()
}

pub fn sign_in_hint(provider: &ProviderState, palette: &Palette) -> El {
    note(&format!("Sign in to {} through your browser.", provider.name), palette.secondary)
}

pub fn sign_in_button(key: String, provider: &ProviderState, panel: &Panel, palette: &Palette) -> El {
    let description = format!("Sign in to {}", provider.name);
    if panel.running.contains(&Task::SignIn(provider.id.clone())) {
        return button(key, "Signing in", &description, Look::Busy, palette).disabled(true);
    }
    let sign_in = button(key, "Sign in", &description, Look::Primary, palette);
    if !panel.connected || panel.signing_in() {
        return disable(sign_in);
    }
    let id = provider.id.clone();
    sign_in.on_press_tx(move |panel: &mut Panel, _| panel.sign_in(id.clone()))
}

fn no_accounts(provider: &ProviderState, panel: &Panel, palette: &Palette) -> El {
    let header = card_header("No account", palette.text, Vec::new(), palette);
    if provider.sign_in {
        return card(false, palette)
            .child(header.child(sign_in_button(format!("sign-in-{}", provider.id), provider, panel, palette)))
            .child(sign_in_hint(provider, palette));
    }
    card(false, palette)
        .child(header)
        .child(note(&format!("Log in to {}, then save it here.", provider.name), palette.secondary))
}

fn unsaved_login(provider: &ProviderState, login: &Identity, panel: &Panel, palette: &Palette) -> El {
    let shares_email = provider.accounts.iter().any(|account| account.identity.email == login.email);
    let detail = match login.organization.as_deref().filter(|_| shares_email) {
        Some(organization) => format!("Not saved, {organization}"),
        None => "Not saved".to_owned(),
    };
    let id = provider.id.clone();
    let save = button(format!("save-{}", provider.id), "Save", "Save this login as an account", Look::Primary, palette);
    let save = if !panel.connected || panel.running.contains(&Task::Save) {
        disable(save)
    } else {
        save.on_press_tx(move |panel: &mut Panel, _| panel.save_login(id.clone()))
    };
    card(true, palette)
        .key(format!("unsaved-{}", provider.id))
        .child(card_header(&login.email, palette.text, vec![(detail, Tone::Warning)], palette).child(save))
}

fn account_row(account: &Account, provider: &ProviderState, panel: &Panel, palette: &Palette) -> El {
    let name_color = if account.enabled { palette.text } else { palette.secondary };
    let details = format::account_details(account, provider, panel.now);
    let prompt = panel.prompt.as_ref().filter(|prompt| match prompt {
        Prompt::Rename(id) | Prompt::Remove(id) => *id == account.id,
    });
    let signed_out = account.status == AccountStatus::NeedsLogin;
    let failure = match &panel.failed {
        Some((Task::Switch(id), error)) if *id == account.id && !account.active => Some(error),
        Some((Task::Reset(id), error)) if *id == account.id => Some(error),
        _ => None,
    };
    let header = match prompt {
        Some(Prompt::Rename(_)) => rename_header(account, panel, palette),
        Some(Prompt::Remove(_)) => remove_header(account, name_color, panel, palette),
        None => card_header(account.label(), name_color, details, palette).children(account_control(
            account,
            provider,
            failure.is_some(),
            panel,
            palette,
        )),
    };
    let mut block = card(account.active, palette).child(header);
    if let Some(Prompt::Remove(_)) = prompt {
        block = block.child(note("This deletes its saved login from Switch.", palette.secondary));
    }
    if signed_out {
        block = block.child(if provider.sign_in {
            sign_in_hint(provider, palette)
        } else {
            note(&format!("Log in to {} with this account.", provider.name), palette.secondary)
        });
    }
    if let AccountStatus::Stale { message } = &account.status {
        block = block.child(note(message, palette.secondary));
    }
    if let Some(error) = failure {
        block = block.child(note(error, palette.danger));
    }
    if let Some(usage) = account.usage.as_ref().filter(|usage| !usage.windows.is_empty()) {
        let current = account.status == AccountStatus::Ready && account.enabled && panel.connected;
        block =
            block.child(col().children(usage.windows.iter().map(|window| usage_row(window, current, panel, palette))));
    }
    let available_resets: Vec<&LimitReset> = account.resets.iter().filter(|reset| reset.available(panel.now)).collect();
    if !signed_out && !available_resets.is_empty() {
        block = block.child(resets_row(account, &available_resets, panel, palette));
        if panel.expanded_resets.as_deref() == Some(account.id.as_str()) {
            block = block.child(resets_drawer(account, &available_resets, panel, palette));
        }
    }
    let id = account.id.clone();
    let (enabled, switchable, connected) = (account.enabled, provider.switchable, panel.connected);
    let list = List::Accounts(provider.id.clone());
    let block = movable(block, &list, &account.id, panel, palette);
    let resets_open = panel.expanded_resets.as_deref() == Some(account.id.as_str());
    let block = if prompt.is_some() || resets_open { block } else { drag_handle(block, list, &account.id) };
    block
        .role("group")
        .label(if account.active { format!("{}, active", account.label()) } else { account.label().to_owned() })
        .on_secondary_press(move |_: &mut Panel, press, tx| {
            let id = id.clone();
            tx.popup_menu(account_menu(enabled, switchable, connected), press, move |panel: &mut Panel, picked, _| {
                if let Some(picked) = picked {
                    panel.on_account_menu(id, picked);
                }
            });
        })
}

fn account_control(
    account: &Account,
    provider: &ProviderState,
    failed: bool,
    panel: &Panel,
    palette: &Palette,
) -> Option<El> {
    if account.active {
        return Some(
            row()
                .gap(3.0)
                .center_y()
                .child(icon(&icons::CHECK, 12.0).color(palette.accent))
                .child(text("Active").font(ACTIVE).color(palette.accent).nowrap()),
        );
    }
    if account.status == AccountStatus::NeedsLogin {
        return provider.sign_in.then(|| sign_in_button(format!("sign-in-{}", account.id), provider, panel, palette));
    }
    provider.switchable.then(|| switch_button(account, failed, panel, palette))
}

fn account_menu(enabled: bool, switchable: bool, connected: bool) -> PopupMenu {
    let item = |id: &str, title: &str| PopupItem::new(id, title).enabled(connected);
    let mut menu = PopupMenu::new().item(item("rename", "Rename"));
    if switchable {
        menu = menu.item(if enabled { item("pause", "Pause") } else { item("resume", "Resume") });
    }
    menu.separator().item(item("remove", "Remove..."))
}

fn rename_header(account: &Account, panel: &Panel, palette: &Palette) -> El {
    let field = text_input(&panel.draft, BODY)
        .single_line()
        .key("rename")
        .label(format!("Name for {}", account.identity.email))
        .placeholder(&account.identity.email, palette.secondary)
        .color(palette.text)
        .caret(palette.text, palette.accent.alpha(0.35))
        .grow(1.0)
        .min_w(0.0)
        .h(24.0)
        .px(6.0)
        .py(4.0)
        .radius(6.0)
        .fill(palette.button)
        .border(1.0, palette.accent)
        .on_edit(|panel: &mut Panel| &mut panel.draft)
        .on_submit(|panel: &mut Panel, _| panel.save_rename())
        .on_input_key(|panel: &mut Panel, key: &KeyEvent, _| key.key == Key::Escape && panel.cancel_prompt());
    let save = button("rename-save".to_owned(), "Save", "Save name", Look::Primary, palette);
    let save =
        if panel.connected { save.on_press_tx(|panel: &mut Panel, _| panel.save_rename()) } else { disable(save) };
    row().h(24.0).gap(6.0).center_y().child(field).child(cancel(palette)).child(save)
}

fn remove_header(account: &Account, name_color: Color, panel: &Panel, palette: &Palette) -> El {
    card_header(account.label(), name_color, Vec::new(), palette)
        .child(cancel(palette))
        .child(remove_button(account, panel, palette))
}

pub fn remove_button(account: &Account, panel: &Panel, palette: &Palette) -> El {
    let id = account.id.clone();
    let description = format!("Remove {}", account.label());
    let remove = button("remove-confirm".to_owned(), "Remove", &description, Look::Danger, palette);
    if panel.connected {
        remove.on_press_tx(move |panel: &mut Panel, _| panel.remove_account(id.clone()))
    } else {
        disable(remove)
    }
}

pub fn cancel(palette: &Palette) -> El {
    button("cancel".to_owned(), "Cancel", "Cancel", Look::Plain, palette).on_press_tx(|panel: &mut Panel, _| {
        panel.cancel_prompt();
    })
}

fn resets_row(account: &Account, resets: &[&LimitReset], panel: &Panel, palette: &Palette) -> El {
    let total: u32 = resets.iter().map(|reset| reset.remaining).sum();
    let expanded = panel.expanded_resets.as_deref() == Some(account.id.as_str());
    let chevron = if expanded { &icons::CHEVRON_UP } else { &icons::CHEVRON_DOWN };
    let id = account.id.clone();
    let label = format!("{total} available");
    let pill = row()
        .key(format!("resets-toggle-{}", account.id))
        .role("button")
        .label(format!("Limit resets, {label}"))
        .focusable(true)
        .cursor(Cursor::Pointer)
        .h(20.0)
        .px(8.0)
        .gap(4.0)
        .center_y()
        .radius(10.0)
        .fill(palette.button)
        .border(1.0, palette.button_border)
        .hover_fill(palette.hover)
        .on_press_tx(move |panel: &mut Panel, _| panel.toggle_resets(id.clone()))
        .child(text(label).font(SMALL).color(palette.text).nowrap())
        .child(icon(chevron, 10.0).color(palette.secondary));
    row()
        .h(24.0)
        .mt(2.0)
        .gap(8.0)
        .center_y()
        .child(text("Limit resets").font(BODY).color(palette.secondary).nowrap())
        .child(el().grow(1.0))
        .child(pill)
}

fn resets_drawer(account: &Account, resets: &[&LimitReset], panel: &Panel, palette: &Palette) -> El {
    col()
        .key(format!("resets-drawer-{}", account.id))
        .mt(4.0)
        .px(8.0)
        .py(8.0)
        .gap(8.0)
        .radius(6.0)
        .fill(palette.hover)
        .border(1.0, palette.separator)
        .children(resets.iter().map(|reset| reset_grant_row(account, reset, panel, palette)))
}

fn reset_grant_row(account: &Account, reset: &LimitReset, panel: &Panel, palette: &Palette) -> El {
    let confirming = panel.confirming_reset.as_ref() == Some(&(account.id.clone(), reset.id.clone()));
    let resetting = panel.resetting(&account.id);
    let badge = row()
        .size(18.0, 18.0)
        .radius(9.0)
        .center()
        .fill(palette.accent.alpha(0.16))
        .child(text(reset.remaining.to_string()).font(ACTIVE).color(palette.accent).nowrap());
    let mut title_row = row()
        .gap(6.0)
        .center_y()
        .child(text(&reset.label).font(SECTION).color(palette.text).truncate().shrink().min_w(0.0));
    if let Some(expires_at) = reset.expires_at {
        title_row = title_row.child(
            text(format::reset_expiry_countdown(expires_at, panel.now)).font(RESET).color(palette.secondary).nowrap(),
        );
    }
    let subtitle = if !reset.usable {
        "Unavailable right now".to_owned()
    } else if let Some(expires_at) = reset.expires_at {
        format!("Expires {}", format::reset_expiry_date(expires_at, &panel.zone))
    } else {
        "Available now".to_owned()
    };
    let info = col()
        .grow(1.0)
        .min_w(0.0)
        .gap(1.0)
        .child(title_row)
        .child(text(subtitle).font(SMALL).color(palette.secondary).truncate());
    let mut top = row().gap(8.0).center_y().child(badge).child(info);
    if !confirming && reset.usable {
        let key = format!("reset-use-{}-{}", account.id, reset.id);
        let description = format!("Use {} on {}", reset.label, account.label());
        let use_button = if resetting {
            button(key, "Resetting", &description, Look::Busy, palette).disabled(true)
        } else {
            let btn = button(key, "Use", &description, Look::Plain, palette);
            if !panel.connected || panel.any_resetting() {
                disable(btn)
            } else {
                let (account_id, grant_id) = (account.id.clone(), reset.id.clone());
                btn.on_press_tx(move |panel: &mut Panel, _| {
                    panel.start_reset_confirm(account_id.clone(), grant_id.clone());
                })
            }
        };
        top = top.child(use_button);
    }
    let mut item = col().gap(6.0).child(top);
    if confirming {
        let detail = reset.detail.as_deref().unwrap_or("Refills your usage limit now. This cannot be undone.");
        let cancel_btn = button("reset-cancel".to_owned(), "Cancel", "Cancel limit reset", Look::Plain, palette)
            .on_press_tx(|panel: &mut Panel, _| {
                panel.confirming_reset = None;
            });
        let confirm_btn = if resetting {
            button("reset-confirm".to_owned(), "Resetting", "Applying limit reset", Look::Busy, palette).disabled(true)
        } else {
            let btn = button(
                "reset-confirm".to_owned(),
                "Reset",
                &format!("Confirm {}", reset.label),
                Look::Primary,
                palette,
            );
            if !panel.connected || panel.any_resetting() {
                disable(btn)
            } else {
                let (account_id, grant_id) = (account.id.clone(), reset.id.clone());
                btn.on_press_tx(move |panel: &mut Panel, _| {
                    panel.confirm_reset(account_id.clone(), grant_id.clone());
                })
            }
        };
        item = item
            .child(note(detail, palette.secondary))
            .child(row().gap(6.0).justify_end().child(cancel_btn).child(confirm_btn));
    }
    item
}

fn switch_button(account: &Account, failed: bool, panel: &Panel, palette: &Palette) -> El {
    let key = format!("switch-{}", account.id);
    let description = format!("Switch to {}", account.label());
    if panel.running.contains(&Task::Switch(account.id.clone())) {
        return button(key, "Switching", &description, Look::Busy, palette).disabled(true);
    }
    let label = if failed { "Try again" } else { "Switch" };
    let button = button(key, label, &description, Look::Plain, palette);
    if panel.switching() || !panel.connected {
        return disable(button);
    }
    let id = account.id.clone();
    button.on_press_tx(move |panel: &mut Panel, _| panel.activate(id.clone()))
}

fn usage_row(window: &UsageWindow, current: bool, panel: &Panel, palette: &Palette) -> El {
    let used = window.used_percent.filter(|used| used.is_finite()).map(|used| used.clamp(0.0, 100.0));
    let mut bar = row()
        .grow(1.0)
        .min_w(0.0)
        .h(6.0)
        .radius(3.0)
        .fill(palette.track)
        .opacity(if current { 1.0 } else { DIMMED })
        .role("progressbar")
        .label(format!("{} used", window.label));
    if let Some(used) = used {
        bar = bar.range_value(used, 0.0, 100.0);
        let color = match format::severity(used) {
            Severity::Normal => palette.accent,
            Severity::Warning => palette.warning,
            Severity::Danger => palette.danger,
        };
        if used > 0.0 {
            bar = bar.child(el().w_frac((used / 100.0) as f32).min_w(6.0).h_full().radius(3.0).fill(color));
        }
    }
    if let Some(pace) = format::pace(window, panel.now) {
        bar = bar.child(
            row()
                .pin(Some(0.0), Some(-2.0), Some(0.0), Some(-2.0))
                .child(el().w_frac(pace as f32).h_full())
                .child(el().w(2.0).ml(-1.0).h_full().radius(1.0).fill(palette.pace)),
        );
    }
    let reset = format::reset_time(window, panel.now, &panel.zone, panel.exact_resets)
        .or_else(|| used.is_none().then(|| "No data".to_owned()))
        .unwrap_or_default();
    let percent_color = if used.is_some() { palette.text } else { palette.secondary };
    row()
        .h(20.0)
        .gap(8.0)
        .center_y()
        .child(text(&window.label).font(BODY).color(palette.secondary).truncate().w(64.0))
        .child(bar)
        .child(right(36.0, text(format::percent(used)).font(PERCENT).color(percent_color).nowrap()))
        .child(right(74.0, text(reset).font(RESET).color(palette.secondary).nowrap()))
}

fn right(width: f32, child: El) -> El {
    row().w(width).justify_end().child(child)
}

fn footer(panel: &Panel, settings: &Settings, palette: &Palette) -> El {
    let mut controls = row().h(44.0).px(14.0).gap(8.0).center_y().child(refresh(panel, palette)).child(el().grow(1.0));
    if panel.connected {
        let label = format!("Auto-switch at {}", format::percent(Some(settings.threshold)));
        controls = controls.child(text(&label).font(BODY).color(palette.text).nowrap()).child(auto_switch(
            &label,
            settings.auto_switch,
            palette,
        ));
    }
    col().child(el().w_full().h(1.0).fill(palette.separator)).child(controls.child(more(
        settings.threshold,
        panel.connected,
        palette,
    )))
}

fn refresh(panel: &Panel, palette: &Palette) -> El {
    let (label, mark) = if panel.refreshing() {
        ("Updating".to_owned(), icon(&icons::SPINNER, 14.0).looping(LoopSpec::rotation(0.9)))
    } else {
        let age = panel.last_update().map(|updated| format::age(updated, panel.now));
        (age.unwrap_or_else(|| "Refresh".to_owned()), icon(&icons::REFRESH, 14.0))
    };
    let control = row()
        .key("refresh")
        .role("button")
        .label(format!("Refresh usage, {label}"))
        .h(24.0)
        .px(6.0)
        .gap(6.0)
        .center_y()
        .radius(6.0)
        .child(mark.color(palette.secondary))
        .child(text(label).font(BODY).color(palette.secondary).nowrap());
    if panel.refreshing() {
        return control.disabled(true);
    }
    if !panel.connected {
        return disable(control);
    }
    control.focusable(true).hover_fill(palette.hover).on_press_tx(|panel: &mut Panel, _| panel.refresh())
}

fn auto_switch(label: &str, on: bool, palette: &Palette) -> El {
    toggle("auto-switch", label, on, palette).on_press_tx(move |panel: &mut Panel, _| panel.set_auto_switch(!on))
}

pub fn toggle(key: &str, label: &str, on: bool, palette: &Palette) -> El {
    let knob = el().size(12.0, 12.0).radius(6.0).fill(palette.knob).absolute(if on { 14.0 } else { 2.0 }, 2.0);
    el().key(key.to_owned())
        .role("switch")
        .label(label)
        .checked(on)
        .focusable(true)
        .size(28.0, 16.0)
        .radius(8.0)
        .fill(if on { palette.accent } else { palette.track })
        .child(knob)
}

const THRESHOLDS: [f64; 5] = [75.0, 80.0, 85.0, 90.0, 95.0];

fn more_menu(threshold: f64, connected: bool) -> PopupMenu {
    let mut values = THRESHOLDS.to_vec();
    if !values.contains(&threshold) {
        values.push(threshold);
        values.sort_by(f64::total_cmp);
    }
    let thresholds = values.into_iter().map(|value| {
        PopupItem::new(format!("threshold:{value}"), format!("{} used", format::percent(Some(value))))
            .checked(value == threshold)
            .enabled(connected)
    });
    PopupMenu::new()
        .item(PopupItem::new("auto-switch", "Auto-switch at").enabled(false))
        .items(thresholds)
        .separator()
        .item(PopupItem::new("settings", "Settings..."))
        .item(PopupItem::new("quit", "Quit Switch"))
}

fn more(threshold: f64, connected: bool, palette: &Palette) -> El {
    row()
        .key("more")
        .role("button")
        .label("More")
        .focusable(true)
        .size(24.0, 24.0)
        .center()
        .radius(6.0)
        .hover_fill(palette.hover)
        .on_press_tx(move |_: &mut Panel, tx| {
            tx.popup_menu(more_menu(threshold, connected), "more", |panel: &mut Panel, picked, _| {
                if let Some(id) = picked {
                    panel.on_menu(id);
                }
            });
        })
        .child(icon(&icons::MORE, 16.0).color(palette.secondary))
}

pub enum Look {
    Plain,
    Primary,
    Danger,
    Busy,
}

pub fn disable(control: El) -> El {
    control.disabled(true).opacity(DISABLED)
}

pub fn button(key: String, label: &str, description: &str, look: Look, palette: &Palette) -> El {
    let button =
        row().key(key).role("button").label(description).focusable(true).h(24.0).px(10.0).gap(5.0).center().radius(6.0);
    let plain = |button: El| button.fill(palette.button).border(1.0, palette.button_border);
    let (button, color) = match look {
        Look::Plain => (plain(button), palette.text),
        Look::Danger => (plain(button), palette.danger),
        Look::Primary => (button.fill(palette.accent), palette.on_accent),
        Look::Busy => (
            plain(button).child(icon(&icons::SPINNER, 12.0).color(palette.secondary).looping(LoopSpec::rotation(0.9))),
            palette.secondary,
        ),
    };
    button.child(text(label).font(BUTTON).color(color).nowrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::preferences::Place;

    #[test]
    fn a_failed_switch_is_shown_on_its_account() {
        let switch = Task::Switch("a2".into());
        let mut panel = Panel { running: vec![Task::Refresh, switch.clone()], ..Panel::default() };
        panel.finish(switch.clone(), Err(anyhow::anyhow!("not saved")));
        assert_eq!(panel.running, [Task::Refresh]);
        assert_eq!(panel.failed, Some((switch.clone(), "not saved".to_owned())));
        assert!(shown_on_account(&switch, &fixtures::two_providers()));
        assert!(!shown_on_account(&Task::Switch("gone".into()), &fixtures::two_providers()));
        assert!(!shown_on_account(&Task::Refresh, &fixtures::two_providers()));
    }

    #[test]
    fn the_more_menu_checks_and_sets_the_threshold() {
        let menu = more_menu(90.0, true);
        assert!(menu.find("threshold:90").unwrap().is_checked());
        assert!(!menu.find("threshold:80").unwrap().is_checked());
        assert!(!more_menu(90.0, false).find("threshold:80").unwrap().is_enabled());
        let mut panel = Panel { snapshot: Some(fixtures::two_providers()), ..Panel::default() };
        panel.on_menu("threshold:80");
        assert_eq!(panel.snapshot.unwrap().settings.threshold, 80.0);
    }

    #[test]
    fn the_account_menu_offers_pausing_only_where_switching_works() {
        assert!(account_menu(true, true, true).find("pause").is_some());
        assert!(account_menu(false, true, true).find("resume").is_some());
        assert!(account_menu(true, false, true).find("pause").is_none());
        assert!(!account_menu(true, true, false).find("remove").unwrap().is_enabled());
    }

    fn on_page(page: Page) -> Panel {
        let mut panel = Panel::default();
        panel.apply(Update::Snapshot(fixtures::two_providers()));
        panel.open(page);
        panel
    }

    #[test]
    fn back_leads_from_an_account_to_settings_to_usage() {
        let mut panel = on_page(Page::Account("a1".into()));
        assert!(panel.go_back());
        assert_eq!(panel.page, Page::Settings);
        assert!(panel.go_back());
        assert_eq!(panel.page, Page::Usage);
        assert!(!panel.go_back());
    }

    #[test]
    fn a_removed_account_returns_to_settings() {
        let mut panel = on_page(Page::Account("a2".into()));
        let mut snapshot = fixtures::two_providers();
        snapshot.providers[0].accounts.remove(1);
        panel.apply(Update::Snapshot(snapshot));
        assert_eq!(panel.page, Page::Settings);
    }

    #[test]
    fn cancelling_a_removal_keeps_the_name_being_edited() {
        let mut panel = on_page(Page::Account("a1".into()));
        assert_eq!(panel.draft.text, "Work");
        panel.prompt = Some(Prompt::Remove("a1".into()));
        assert!(panel.cancel_prompt());
        assert_eq!(panel.draft.text, "Work");
        panel.draft = TextEdit::new("Job");
        panel.save_name();
        assert_eq!(panel.saved_name, "Job");
    }

    #[test]
    fn dragging_on_the_usage_page_moves_only_what_it_shows() {
        let mut panel = on_page(Page::Settings);
        panel.edit_preferences(|preferences| preferences.set_account(Place::Panel, "a2", false));
        let accounts = List::Accounts("claude".into());
        assert_eq!(panel.ids(&accounts), ["a1", "a2"]);
        panel.open(Page::Usage);
        assert_eq!(panel.ids(&accounts), ["a1"]);
    }

    #[test]
    fn unreadable_preferences_are_not_saved_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = preferences::path(dir.path());
        std::fs::write(&path, "{").unwrap();
        let mut panel = on_page(Page::Settings);
        panel.use_preferences(preferences::load(&path), path.clone());
        panel.edit_preferences(|preferences| preferences.set_provider(Place::Panel, "claude", false));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{");
        assert!(panel.preferences_error.is_some());
        assert!(!panel.preferences.provider_on(Place::Panel, "claude"));
    }

    #[test]
    fn renaming_starts_from_the_alias_and_removing_asks_first() {
        let mut panel = Panel { snapshot: Some(fixtures::two_providers()), ..Panel::default() };
        panel.on_account_menu("a1".into(), "rename");
        assert_eq!(panel.prompt, Some(Prompt::Rename("a1".into())));
        assert_eq!(panel.draft.text, "Work");
        assert!(panel.cancel_prompt());
        assert!(!panel.cancel_prompt());
        panel.on_account_menu("a2".into(), "remove");
        assert_eq!(panel.prompt, Some(Prompt::Remove("a2".into())));
        assert!(panel.account("a2").is_some());
    }

    #[test]
    fn failed_settings_update_restores_previous_values() {
        let mut panel = Panel { snapshot: Some(fixtures::two_providers()), ..Panel::default() };
        let initial = panel.snapshot.as_ref().unwrap().settings.clone();
        panel.set_auto_switch(!initial.auto_switch);
        panel.set_threshold(80.0);
        assert_ne!(panel.snapshot.as_ref().unwrap().settings, initial);

        panel.finish(Task::Settings, Err(anyhow::anyhow!("save failed")));

        assert_eq!(panel.snapshot.as_ref().unwrap().settings, initial);
        assert!(panel.settings_before.is_none());
    }
}
