use opentui::prelude::KeyEvent;
use switch_protocol::{
    Account, AccountUpdate, LimitReset, OrderUpdate, ProviderState, Settings, SettingsUpdate, Snapshot, Strategy,
};

use crate::connection::Update;

const THRESHOLD_STEP: f64 = 5.0;
const THRESHOLD_MIN: f64 = 50.0;
const THRESHOLD_MAX: f64 = 100.0;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Activate { account: String, label: String },
    RedeemReset { account: String, label: String, grant_id: String, reset_label: String },
    Switch { provider: String, strategy: Strategy },
    Add { provider: String },
    Refresh,
    Update { account: String, update: AccountUpdate },
    Remove { account: String, label: String },
    Order(OrderUpdate),
    Settings(SettingsUpdate),
}

impl Action {
    fn progress(&self) -> String {
        match self {
            Self::Activate { label, .. } => format!("Switching to {label}"),
            Self::RedeemReset { label, reset_label, .. } => format!("Using {reset_label} on {label}"),
            Self::Switch { strategy: Strategy::Next, .. } => "Switching to the next account".to_owned(),
            Self::Switch { strategy: Strategy::Best, .. } => "Switching to the best account".to_owned(),
            Self::Add { .. } => "Adding the current login".to_owned(),
            Self::Refresh => "Refreshing usage".to_owned(),
            Self::Update { .. } => "Saving the account".to_owned(),
            Self::Remove { label, .. } => format!("Removing {label}"),
            Self::Order(_) => "Saving the order".to_owned(),
            Self::Settings(_) => "Saving settings".to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Account {
        id: String,
    },
    /// A provider's own row: its unsaved login, or a note that it has no accounts.
    Provider {
        id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Connection {
    Connecting,
    Connected,
    Reconnecting,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Overlay {
    None,
    Help,
    Rename { account: String, alias: String },
    Remove { account: String, label: String },
    Reset { account: String, label: String, grants: Vec<LimitReset>, index: usize, confirming: bool },
}

#[derive(Debug, PartialEq)]
pub enum Response {
    Ignored,
    Changed,
    Run(Action),
    ScrollHelp(f64),
    Quit,
}

#[derive(Clone, Copy)]
enum Command {
    Up,
    Down,
    Activate,
    Next,
    Best,
    Add,
    Refresh,
    ToggleEnabled,
    Rename,
    Reset,
    Remove,
    MoveUp,
    MoveDown,
    MoveProviderUp,
    MoveProviderDown,
    ToggleAutoSwitch,
    RaiseThreshold,
    LowerThreshold,
    ToggleResetTime,
    Help,
    Quit,
}

const COMMANDS: [(&str, Command); 25] = [
    ("up", Command::Up),
    ("k", Command::Up),
    ("down", Command::Down),
    ("j", Command::Down),
    ("enter", Command::Activate),
    ("n", Command::Next),
    ("b", Command::Best),
    ("a", Command::Add),
    ("r", Command::Refresh),
    ("e", Command::ToggleEnabled),
    ("m", Command::Rename),
    ("u", Command::Reset),
    ("x", Command::Remove),
    ("K", Command::MoveUp),
    ("shift+up", Command::MoveUp),
    ("J", Command::MoveDown),
    ("shift+down", Command::MoveDown),
    ("[", Command::MoveProviderUp),
    ("]", Command::MoveProviderDown),
    ("s", Command::ToggleAutoSwitch),
    ("+", Command::RaiseThreshold),
    ("-", Command::LowerThreshold),
    ("t", Command::ToggleResetTime),
    ("?", Command::Help),
    ("q", Command::Quit),
];

pub struct Model {
    pub snapshot: Option<Snapshot>,
    pub connection: Connection,
    pub disconnect_reason: Option<String>,
    pub selected: Option<Target>,
    pub busy: Option<String>,
    pub error: Option<String>,
    pub notice: Option<String>,
    pending_notice: Option<String>,
    pub exact_resets: bool,
    pub overlay: Overlay,
}

impl Model {
    pub fn new() -> Self {
        Self {
            snapshot: None,
            connection: Connection::Connecting,
            disconnect_reason: None,
            selected: None,
            busy: None,
            error: None,
            notice: None,
            pending_notice: None,
            exact_resets: false,
            overlay: Overlay::None,
        }
    }

    pub fn apply(&mut self, update: &Update) {
        match update {
            Update::Snapshot(snapshot) => {
                self.selected = preserved_selection(self.selected.as_ref(), self.snapshot.as_ref(), snapshot);
                self.snapshot = Some(snapshot.clone());
                self.connection = Connection::Connected;
                self.disconnect_reason = None;
            }
            Update::Disconnected { reason } => {
                if self.connection == Connection::Connected {
                    self.connection = Connection::Reconnecting;
                }
                self.disconnect_reason = Some(reason.clone());
            }
            Update::Finished { error } => {
                self.busy = None;
                self.error = error.clone();
                if error.is_none() {
                    self.notice = self.pending_notice.take();
                } else {
                    self.pending_notice = None;
                    self.notice = None;
                }
            }
        }
    }

    pub fn press(&mut self, key: &KeyEvent) -> Response {
        if key.matches("escape") && self.overlay != Overlay::None {
            if let Overlay::Reset { grants, confirming: true, .. } = &mut self.overlay
                && grants.len() > 1
            {
                if let Overlay::Reset { confirming, .. } = &mut self.overlay {
                    *confirming = false;
                }
                self.error = None;
                return Response::Changed;
            }
            self.overlay = Overlay::None;
            return Response::Changed;
        }
        let command = COMMANDS.iter().find(|(shortcut, _)| key.matches(*shortcut)).map(|(_, command)| *command);
        if command.is_some() {
            self.notice = None;
        }
        match &mut self.overlay {
            Overlay::Rename { .. } => Response::Ignored,
            Overlay::Remove { account, label } => {
                if !key.matches("y") {
                    return Response::Ignored;
                }
                let action = Action::Remove { account: account.clone(), label: label.clone() };
                self.overlay = Overlay::None;
                self.start(action)
            }
            Overlay::Reset { account, label, grants, index, confirming } => {
                if *confirming {
                    if !key.matches("y") {
                        return Response::Ignored;
                    }
                    let chosen = grants[*index].clone();
                    let action = Action::RedeemReset {
                        account: account.clone(),
                        label: label.clone(),
                        grant_id: chosen.id,
                        reset_label: chosen.label,
                    };
                    self.overlay = Overlay::None;
                    self.start(action)
                } else {
                    match command {
                        Some(Command::Up) => {
                            *index = index.saturating_sub(1);
                            self.error = None;
                            Response::Changed
                        }
                        Some(Command::Down) => {
                            *index = (*index + 1).min(grants.len().saturating_sub(1));
                            self.error = None;
                            Response::Changed
                        }
                        Some(Command::Activate) => {
                            if grants[*index].usable {
                                *confirming = true;
                                self.error = None;
                            } else {
                                self.error = Some("That limit reset cannot be used right now.".to_owned());
                            }
                            Response::Changed
                        }
                        Some(Command::Quit) => Response::Quit,
                        _ => Response::Ignored,
                    }
                }
            }
            Overlay::Help => match command {
                Some(Command::Up) => Response::ScrollHelp(-1.0),
                Some(Command::Down) => Response::ScrollHelp(1.0),
                Some(Command::Help) => {
                    self.overlay = Overlay::None;
                    Response::Changed
                }
                Some(Command::Quit) => Response::Quit,
                _ => Response::Ignored,
            },
            Overlay::None => command.map_or(Response::Ignored, |command| self.execute(command)),
        }
    }

    pub fn submit_alias(&mut self, alias: &str) -> Response {
        let Overlay::Rename { account, .. } = &self.overlay else {
            return Response::Ignored;
        };
        let update = AccountUpdate { alias: Some(alias.trim().to_owned()), enabled: None };
        let action = Action::Update { account: account.clone(), update };
        self.overlay = Overlay::None;
        self.start(action)
    }

    pub fn select(&mut self, target: Target) {
        self.selected = Some(target);
    }

    pub fn settings(&self) -> Option<&Settings> {
        self.snapshot.as_ref().map(|snapshot| &snapshot.settings)
    }

    pub fn selected_provider(&self) -> Option<&ProviderState> {
        let providers = &self.snapshot.as_ref()?.providers;
        match &self.selected {
            Some(target) => providers.iter().find(|provider| targets_of(provider).contains(target)),
            None => providers.first(),
        }
    }

    fn selected_account(&self) -> Option<&Account> {
        let Some(Target::Account { id }) = &self.selected else {
            return None;
        };
        self.selected_provider()?.accounts.iter().find(|account| account.id == *id)
    }

    fn execute(&mut self, command: Command) -> Response {
        match command {
            Command::Up => self.move_selection(-1),
            Command::Down => self.move_selection(1),
            Command::Help => self.open(Overlay::Help),
            Command::Quit => Response::Quit,
            Command::ToggleResetTime => {
                self.exact_resets = !self.exact_resets;
                Response::Changed
            }
            Command::Rename => {
                let Some(account) = self.selected_account() else {
                    return Response::Ignored;
                };
                let alias = account.alias.clone().unwrap_or_default();
                let account = account.id.clone();
                self.open(Overlay::Rename { account, alias })
            }
            Command::Reset => {
                let Some(account) = self.selected_account() else {
                    return Response::Ignored;
                };
                let grants: Vec<LimitReset> =
                    account.resets.iter().filter(|reset| reset.remaining > 0).cloned().collect();
                if grants.is_empty() {
                    self.error = Some("No limit resets available for this account.".to_owned());
                    return Response::Changed;
                }
                let index = grants.iter().position(|grant| grant.usable).unwrap_or(0);
                let confirming = grants.len() == 1 && grants[0].usable;
                let (account, label) = (account.id.clone(), account.label().to_owned());
                self.error = None;
                self.open(Overlay::Reset { account, label, grants, index, confirming })
            }
            Command::Remove => {
                let Some(account) = self.selected_account() else {
                    return Response::Ignored;
                };
                let (account, label) = (account.id.clone(), account.label().to_owned());
                self.open(Overlay::Remove { account, label })
            }
            _ => {
                if let Some(refusal) = self.refusal(command) {
                    self.error = Some(refusal);
                    return Response::Changed;
                }
                match self.action(command) {
                    Some(action) => self.start(action),
                    None => Response::Ignored,
                }
            }
        }
    }

    /// Usage-only providers neither switch nor save their live login.
    fn refusal(&self, command: Command) -> Option<String> {
        let provider = self.selected_provider().filter(|provider| !provider.switchable)?;
        match command {
            Command::Activate | Command::Next | Command::Best => {
                Some(format!("Switching {} accounts is not supported yet.", provider.name))
            }
            Command::Add if provider.sign_in => {
                Some(format!("Add {} accounts by signing in with swx login {}.", provider.name, provider.id))
            }
            _ => None,
        }
    }

    fn action(&self, command: Command) -> Option<Action> {
        let provider = || self.selected_provider().map(|provider| provider.id.clone());
        Some(match command {
            Command::Activate => {
                let account = self.selected_account()?;
                Action::Activate { account: account.id.clone(), label: account.label().to_owned() }
            }
            Command::Next => Action::Switch { provider: provider()?, strategy: Strategy::Next },
            Command::Best => Action::Switch { provider: provider()?, strategy: Strategy::Best },
            Command::Add => Action::Add { provider: provider()? },
            Command::Refresh => Action::Refresh,
            Command::ToggleEnabled => {
                let account = self.selected_account()?;
                let update = AccountUpdate { alias: None, enabled: Some(!account.enabled) };
                Action::Update { account: account.id.clone(), update }
            }
            Command::MoveUp => self.account_move(-1)?,
            Command::MoveDown => self.account_move(1)?,
            Command::MoveProviderUp => self.provider_move(-1)?,
            Command::MoveProviderDown => self.provider_move(1)?,
            Command::ToggleAutoSwitch => {
                let auto_switch = Some(!self.settings()?.auto_switch);
                Action::Settings(SettingsUpdate { auto_switch, ..SettingsUpdate::default() })
            }
            Command::RaiseThreshold => self.threshold_change(THRESHOLD_STEP)?,
            Command::LowerThreshold => self.threshold_change(-THRESHOLD_STEP)?,
            Command::Up
            | Command::Down
            | Command::Rename
            | Command::Reset
            | Command::Remove
            | Command::ToggleResetTime
            | Command::Help
            | Command::Quit => {
                return None;
            }
        })
    }

    fn account_move(&self, step: isize) -> Option<Action> {
        let accounts = self.selected_provider()?.accounts.iter().map(|account| account.id.clone()).collect();
        let accounts = moved(accounts, &self.selected_account()?.id, step)?;
        Some(Action::Order(OrderUpdate { providers: None, accounts: Some(accounts) }))
    }

    fn provider_move(&self, step: isize) -> Option<Action> {
        let providers = self.snapshot.as_ref()?.providers.iter().map(|provider| provider.id.clone()).collect();
        let providers = moved(providers, &self.selected_provider()?.id, step)?;
        Some(Action::Order(OrderUpdate { providers: Some(providers), accounts: None }))
    }

    fn threshold_change(&self, step: f64) -> Option<Action> {
        let current = self.settings()?.threshold;
        let threshold = (current + step).clamp(THRESHOLD_MIN, THRESHOLD_MAX);
        (threshold != current)
            .then_some(Action::Settings(SettingsUpdate { threshold: Some(threshold), ..SettingsUpdate::default() }))
    }

    fn start(&mut self, action: Action) -> Response {
        if self.busy.is_some() {
            return Response::Changed;
        }
        if self.connection != Connection::Connected {
            self.error = Some("Not connected to the server".to_owned());
            return Response::Changed;
        }
        self.error = None;
        self.notice = None;
        self.pending_notice = match &action {
            Action::Activate { .. } | Action::Switch { .. } => self
                .selected_provider()
                .map(|p| format!("New {} sessions use this account. Restart running ones to switch them now.", p.name)),
            Action::RedeemReset { label, reset_label, .. } => Some(format!("Used {reset_label} on {label}.")),
            _ => None,
        };
        self.busy = Some(action.progress());
        Response::Run(action)
    }

    fn open(&mut self, overlay: Overlay) -> Response {
        self.overlay = overlay;
        Response::Changed
    }

    fn move_selection(&mut self, step: isize) -> Response {
        let Some(snapshot) = &self.snapshot else {
            return Response::Ignored;
        };
        let targets = targets(snapshot);
        let current = targets.iter().position(|target| Some(target) == self.selected.as_ref()).unwrap_or(0);
        let next = current.saturating_add_signed(step).min(targets.len().saturating_sub(1));
        self.selected = targets.get(next).cloned();
        Response::Changed
    }
}

fn moved(mut ids: Vec<String>, id: &str, step: isize) -> Option<Vec<String>> {
    let from = ids.iter().position(|other| other == id)?;
    let to = from.checked_add_signed(step).filter(|to| *to < ids.len())?;
    ids.swap(from, to);
    Some(ids)
}

pub fn has_own_row(provider: &ProviderState) -> bool {
    (provider.switchable && provider.unsaved_login.is_some()) || provider.accounts.is_empty()
}

fn targets_of(provider: &ProviderState) -> Vec<Target> {
    let own_row = has_own_row(provider).then(|| Target::Provider { id: provider.id.clone() });
    let accounts = provider.accounts.iter().map(|account| Target::Account { id: account.id.clone() });
    own_row.into_iter().chain(accounts).collect()
}

fn targets(snapshot: &Snapshot) -> Vec<Target> {
    snapshot.providers.iter().flat_map(targets_of).collect()
}

fn preserved_selection(selected: Option<&Target>, before: Option<&Snapshot>, after: &Snapshot) -> Option<Target> {
    let remaining = targets(after);
    let Some(selected) = selected else {
        let active = after.providers.iter().find_map(ProviderState::active);
        let active = active.map(|account| Target::Account { id: account.id.clone() });
        return active.or_else(|| remaining.first().cloned());
    };
    if remaining.contains(selected) {
        return Some(selected.clone());
    }
    let before = before.map(targets).unwrap_or_default();
    let index = before.iter().position(|target| target == selected).unwrap_or(0);
    remaining.get(index.min(remaining.len().saturating_sub(1))).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn connected(snapshot: Snapshot) -> Model {
        let mut model = Model::new();
        model.apply(&Update::Snapshot(snapshot));
        model
    }

    fn press(model: &mut Model, shortcut: &str) -> Response {
        model.press(&KeyEvent::from_shortcut(shortcut).unwrap())
    }

    fn account(id: &str) -> Option<Target> {
        Some(Target::Account { id: id.to_owned() })
    }

    #[test]
    fn selection_starts_at_the_active_account() {
        assert_eq!(connected(fixtures::snapshot()).selected, account("claude-work"));
    }

    #[test]
    fn selection_follows_its_account_when_others_come_and_go() {
        let mut model = connected(fixtures::snapshot());
        let mut changed = fixtures::snapshot();
        let added = fixtures::account("claude-new", "new@example.com");
        changed.providers[0].accounts.insert(0, added);
        model.apply(&Update::Snapshot(changed.clone()));
        assert_eq!(model.selected, account("claude-work"));
        changed.providers[0].accounts.remove(0);
        model.apply(&Update::Snapshot(changed));
        assert_eq!(model.selected, account("claude-work"));
    }

    #[test]
    fn selection_moves_to_a_neighbor_when_its_account_is_removed() {
        let mut model = connected(fixtures::snapshot());
        let mut changed = fixtures::snapshot();
        changed.providers[0].accounts.remove(0);
        model.apply(&Update::Snapshot(changed.clone()));
        assert_eq!(model.selected, account("claude-personal"));
        changed.providers.clear();
        model.apply(&Update::Snapshot(changed));
        assert_eq!(model.selected, None);
        model.apply(&Update::Snapshot(fixtures::snapshot()));
        assert_eq!(model.selected, account("claude-work"));
    }

    #[test]
    fn saving_an_unsaved_login_selects_the_saved_account() {
        let mut snapshot = fixtures::empty();
        snapshot.providers[0].unsaved_login = Some(fixtures::identity("new@example.com"));
        let mut model = connected(snapshot.clone());
        assert_eq!(model.selected, Some(Target::Provider { id: "claude".to_owned() }));
        snapshot.providers[0].unsaved_login = None;
        snapshot.providers[0].accounts.push(fixtures::account("claude-new", "new@example.com"));
        model.apply(&Update::Snapshot(snapshot));
        assert_eq!(model.selected, account("claude-new"));
    }

    #[test]
    fn provider_actions_use_the_provider_of_the_selection() {
        let mut model = connected(fixtures::snapshot());
        for _ in 0..10 {
            press(&mut model, "down");
        }
        assert_eq!(model.selected, account("codex-main"));
        let response = press(&mut model, "b");
        let expected = Action::Switch { provider: "codex".to_owned(), strategy: Strategy::Best };
        assert_eq!(response, Response::Run(expected));
    }

    #[test]
    fn an_action_in_flight_blocks_others_and_its_error_stays_until_the_next() {
        let mut model = connected(fixtures::snapshot());
        assert_eq!(press(&mut model, "r"), Response::Run(Action::Refresh));
        assert_eq!(press(&mut model, "n"), Response::Changed);
        model.apply(&Update::Finished { error: Some("rate limited".to_owned()) });
        press(&mut model, "j");
        assert_eq!(model.error.as_deref(), Some("rate limited"));
        assert_eq!(press(&mut model, "r"), Response::Run(Action::Refresh));
        assert_eq!(model.error, None);
    }

    #[test]
    fn actions_wait_for_a_connection() {
        let mut model = connected(fixtures::snapshot());
        model.apply(&Update::Disconnected { reason: "gone".to_owned() });
        assert_eq!(model.connection, Connection::Reconnecting);
        assert_eq!(press(&mut model, "r"), Response::Changed);
        assert_eq!(model.error.as_deref(), Some("Not connected to the server"));
        assert!(model.snapshot.is_some());
    }

    #[test]
    fn usage_only_providers_explain_why_they_do_not_switch() {
        let mut model = connected(fixtures::usage_only());
        assert_eq!(model.selected, account("antigravity-ada"));
        assert_eq!(press(&mut model, "enter"), Response::Changed);
        assert_eq!(model.error.as_deref(), Some("Switching Antigravity accounts is not supported yet."));
        assert_eq!(press(&mut model, "n"), Response::Changed);
        assert_eq!(press(&mut model, "a"), Response::Changed);
        assert_eq!(model.error.as_deref(), Some("Add Antigravity accounts by signing in with swx login antigravity."));
        assert_eq!(press(&mut model, "r"), Response::Run(Action::Refresh));
    }

    #[test]
    fn moving_swaps_neighbors_and_stops_at_the_ends() {
        let snapshot = fixtures::snapshot();
        let mut accounts: Vec<String> = snapshot.providers[0].accounts.iter().map(|a| a.id.clone()).collect();
        let mut providers: Vec<String> = snapshot.providers.iter().map(|provider| provider.id.clone()).collect();
        accounts.swap(0, 1);
        providers.swap(0, 1);
        let order = |providers, accounts| Response::Run(Action::Order(OrderUpdate { providers, accounts }));
        for key in ["J", "shift+down"] {
            let response = press(&mut connected(snapshot.clone()), key);
            assert_eq!(response, order(None, Some(accounts.clone())));
        }
        assert_eq!(press(&mut connected(snapshot.clone()), "]"), order(Some(providers), None));
        assert_eq!(press(&mut connected(snapshot.clone()), "K"), Response::Ignored);
        assert_eq!(press(&mut connected(snapshot), "["), Response::Ignored);
    }

    #[test]
    fn removing_needs_confirmation() {
        let mut model = connected(fixtures::snapshot());
        press(&mut model, "x");
        assert_eq!(press(&mut model, "r"), Response::Ignored);
        assert_eq!(press(&mut model, "escape"), Response::Changed);
        assert_eq!(press(&mut model, "y"), Response::Ignored);
        press(&mut model, "x");
        let expected = Action::Remove { account: "claude-work".to_owned(), label: "work".to_owned() };
        assert_eq!(press(&mut model, "y"), Response::Run(expected));
    }

    #[test]
    fn the_threshold_moves_in_steps_within_its_limits() {
        for (current, key, expected) in [
            (90.0, "+", Some(95.0)),
            (98.0, "+", Some(100.0)),
            (100.0, "+", None),
            (90.0, "-", Some(85.0)),
            (50.0, "-", None),
        ] {
            let mut snapshot = fixtures::snapshot();
            snapshot.settings.threshold = current;
            let response = press(&mut connected(snapshot), key);
            let action = expected.map_or(Response::Ignored, |threshold| {
                Response::Run(Action::Settings(SettingsUpdate {
                    threshold: Some(threshold),
                    ..SettingsUpdate::default()
                }))
            });
            assert_eq!(response, action);
        }
    }

    #[test]
    fn using_a_limit_reset_supports_single_multiple_and_unavailable_grants() {
        let mut no_resets = connected(fixtures::snapshot());
        assert_eq!(press(&mut no_resets, "u"), Response::Changed);
        assert_eq!(no_resets.error.as_deref(), Some("No limit resets available for this account."));

        let mut single = connected(fixtures::with_resets());
        assert_eq!(press(&mut single, "u"), Response::Changed);
        assert!(matches!(single.overlay, Overlay::Reset { confirming: true, .. }));
        assert_eq!(press(&mut single, "escape"), Response::Changed);
        assert_eq!(single.overlay, Overlay::None);
        press(&mut single, "u");
        assert_eq!(
            press(&mut single, "y"),
            Response::Run(Action::RedeemReset {
                account: "claude-work".to_owned(),
                label: "work".to_owned(),
                grant_id: "demo-full".to_owned(),
                reset_label: "Full Reset".to_owned(),
            })
        );

        let mut multi = connected(fixtures::with_resets());
        press(&mut multi, "down");
        assert_eq!(press(&mut multi, "u"), Response::Changed);
        assert!(matches!(multi.overlay, Overlay::Reset { index: 0, confirming: false, .. }));
        press(&mut multi, "down");
        assert!(matches!(multi.overlay, Overlay::Reset { index: 1, confirming: false, .. }));
        assert_eq!(press(&mut multi, "enter"), Response::Changed);
        assert!(matches!(multi.overlay, Overlay::Reset { index: 1, confirming: false, .. }));
        assert_eq!(multi.error.as_deref(), Some("That limit reset cannot be used right now."));
        press(&mut multi, "up");
        press(&mut multi, "enter");
        assert!(matches!(multi.overlay, Overlay::Reset { index: 0, confirming: true, .. }));
        press(&mut multi, "escape");
        assert!(matches!(multi.overlay, Overlay::Reset { index: 0, confirming: false, .. }));
        press(&mut multi, "enter");
        assert_eq!(
            press(&mut multi, "y"),
            Response::Run(Action::RedeemReset {
                account: "claude-personal".to_owned(),
                label: "ada.lovelace@personal.example.com".to_owned(),
                grant_id: "demo-weekly".to_owned(),
                reset_label: "Weekly Reset".to_owned(),
            })
        );
    }
}
