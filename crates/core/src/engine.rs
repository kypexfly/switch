use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use switch_protocol::{
    AccountStatus, AccountUpdate, Identity, OrderUpdate, ProviderState, Settings, SettingsUpdate, SignIn, Snapshot,
    Strategy, Usage, UsageWindow,
};

use crate::poll::{self, Fetched, Schedule};
use crate::provider::{Login, Provider, RedeemError, RedeemOutcome, RemoteError, Secret};
use crate::sign_in::SignIns;
use crate::store::{ManualHold, Saved, SavedAccount, Store};
use crate::vault::Vault;

const AUTO_SWITCH_MARGIN: f64 = 5.0;
const MAX_COOLDOWN_SECONDS: u64 = 86_400;
const MAX_SWITCH_USAGE_AGE_SECS: i64 = 15 * 60;
const SWITCH_BACKOFF_BASE_SECS: i64 = 60;
const SWITCH_BACKOFF_CAP_SECS: i64 = 1800;
const PLAN_REFRESH_SECS: i64 = 24 * 60 * 60;
const RESETS_REFRESH_SECS: i64 = 60 * 60;
const RESETS_FORCED_MIN_SECS: i64 = 15 * 60;
const RESET_ADVANCE_TOLERANCE_SECS: i64 = 60;
const LIVE_CACHE_TTL: Duration = Duration::from_secs(12);

#[derive(Clone, Copy)]
enum SwitchSource {
    Manual,
    Auto,
}

enum SwitchFailure {
    Target(Error),
    Outgoing(Error),
}

impl SwitchFailure {
    fn into_error(self) -> Error {
        match self {
            Self::Target(error) | Self::Outgoing(error) => error,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct SwitchBackoff {
    failures: u32,
    retry_at: Option<Timestamp>,
}

impl SwitchBackoff {
    fn record_failure(&mut self, now: Timestamp) {
        self.failures += 1;
        let shift = (self.failures - 1).min(30);
        let wait = (SWITCH_BACKOFF_BASE_SECS.saturating_mul(1_i64 << shift)).min(SWITCH_BACKOFF_CAP_SECS);
        self.retry_at = now.checked_add(jiff::SignedDuration::from_secs(wait)).ok();
    }

    fn blocked(&self, now: Timestamp) -> bool {
        self.retry_at.is_some_and(|at| now < at)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Failed(String),
}

impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(format!("{error:#}"))
    }
}

/// A waiter holds `turn`, so a thread that releases and immediately relocks can not jump ahead of it.
#[derive(Default)]
struct WorkLock {
    turn: Mutex<()>,
    work: Mutex<()>,
}

impl WorkLock {
    fn lock(&self) -> MutexGuard<'_, ()> {
        let _turn = self.turn.lock().unwrap();
        self.work.lock().unwrap()
    }
}

/// Owns the saved accounts and every change to the providers' live logins.
pub struct Engine {
    providers: Vec<Arc<dyn Provider>>,
    store: Store,
    vault: Vault,
    state: Mutex<State>,
    store_write: Mutex<()>,
    work: HashMap<&'static str, WorkLock>,
    fallback_work: WorkLock,
    sign_ins: Mutex<SignIns>,
}

struct State {
    saved: Saved,
    live: BTreeMap<&'static str, Identity>,
    live_cache: HashMap<&'static str, (Option<Login>, Instant)>,
    live_secrets: HashMap<String, String>,
    problems: BTreeMap<String, String>,
    schedules: HashMap<String, Schedule>,
    target_backoffs: HashMap<String, SwitchBackoff>,
    provider_backoffs: HashMap<String, SwitchBackoff>,
    next_reset_grants: HashMap<String, Option<String>>,
}

impl Engine {
    pub fn new(providers: Vec<Arc<dyn Provider>>, store: Store, vault: Vault) -> anyhow::Result<Self> {
        let saved = store.load()?;
        let state = State {
            saved,
            live: BTreeMap::new(),
            live_cache: HashMap::new(),
            live_secrets: HashMap::new(),
            problems: BTreeMap::new(),
            schedules: HashMap::new(),
            target_backoffs: HashMap::new(),
            provider_backoffs: HashMap::new(),
            next_reset_grants: HashMap::new(),
        };
        let work = providers.iter().map(|provider| (provider.id(), WorkLock::default())).collect();
        let engine = Self {
            providers,
            store,
            vault,
            state: Mutex::new(state),
            store_write: Mutex::default(),
            work,
            fallback_work: WorkLock::default(),
            sign_ins: Mutex::default(),
        };
        engine.sync_live();
        Ok(engine)
    }

    pub fn snapshot(&self) -> Snapshot {
        let state = self.state();
        let providers = self
            .ordered_providers(&state.saved)
            .into_iter()
            .map(|provider| {
                let live = state.live.get(provider.id());
                let accounts: Vec<_> = state
                    .saved
                    .accounts
                    .iter()
                    .filter(|account| account.provider == provider.id())
                    .map(|account| account.view(live.is_some_and(|live| same(live, &account.identity))))
                    .collect();
                let unsaved_login = live.filter(|_| !accounts.iter().any(|account| account.active)).cloned();
                ProviderState {
                    id: provider.id().to_owned(),
                    name: provider.name().to_owned(),
                    accounts,
                    unsaved_login,
                    switchable: provider.switchable(),
                    sign_in: provider.sign_in().is_some(),
                    last_auto_switch_at: state.saved.last_auto_switch.get(provider.id()).copied(),
                }
            })
            .collect();
        Snapshot {
            providers,
            settings: state.saved.settings.clone(),
            warnings: state.problems.values().cloned().collect(),
        }
    }

    fn ordered_providers(&self, saved: &Saved) -> Vec<&Arc<dyn Provider>> {
        let mut providers: Vec<_> = self.providers.iter().collect();
        providers.sort_by_key(|provider| {
            let position = saved.provider_order.iter().position(|id| id == provider.id());
            position.unwrap_or(usize::MAX)
        });
        providers
    }

    fn read_live(&self, provider: &dyn Provider, max_age: Option<Duration>) -> anyhow::Result<Option<Login>> {
        if let Some(limit) = max_age
            && let Some((cached, at)) = self.state().live_cache.get(provider.id())
            && at.elapsed() < limit
        {
            return Ok(cached.clone());
        }
        let live = provider.live()?;
        self.state().live_cache.insert(provider.id(), (live.clone(), Instant::now()));
        Ok(live)
    }

    /// Notices logins changed by the providers' own tools. Never touches the network.
    pub fn sync_live(&self) {
        for provider in &self.providers {
            let _work = self.work(provider.id());
            self.sync_live_provider_locked(&**provider, None);
        }
    }

    fn sync_live_provider_locked(&self, provider: &dyn Provider, max_age: Option<Duration>) {
        let now = Timestamp::now();
        let live = self.read_live(provider, max_age);
        let _write = self.store_write.lock().unwrap();
        let to_save = {
            let mut state = self.state();
            let mut changed = false;
            match live {
                Ok(Some(login)) => {
                    let matched = state
                        .saved
                        .accounts
                        .iter()
                        .find(|account| account.provider == provider.id() && same(&login.identity, &account.identity))
                        .cloned();
                    state.live.insert(provider.id(), login.identity);
                    let recovered_live = state.problems.remove(provider.id()).is_some();
                    if let Some(account) = matched {
                        let secret_digest = crate::provider::digest(login.secret.expose());
                        let secret_changed = state
                            .live_secrets
                            .insert(account.id.clone(), secret_digest.clone())
                            .is_some_and(|previous| previous != secret_digest);
                        let became_live = match state.saved.last_live.get(provider.id()).map(String::as_str) {
                            None => {
                                state.saved.last_live.insert(provider.id().to_owned(), account.id.clone());
                                changed = true;
                                false
                            }
                            Some(previous) if previous != account.id => {
                                state.saved.last_live.insert(provider.id().to_owned(), account.id.clone());
                                state.saved.last_auto_switch.remove(provider.id());
                                state.provider_backoffs.remove(provider.id());
                                state.problems.remove(&auto_switch_problem_key(provider.id()));
                                record_manual_hold(&mut state.saved, provider.id(), &account, now);
                                changed = true;
                                true
                            }
                            Some(_) => false,
                        };
                        if (secret_changed || recovered_live || became_live)
                            && let Some(schedule) = state.schedules.get_mut(&account.id)
                        {
                            schedule.clear_failure_backoff(now);
                        }
                    }
                }
                Ok(None) => {
                    state.live.remove(provider.id());
                    state.problems.remove(provider.id());
                }
                Err(error) => {
                    let problem = format!("Could not read the current {} login: {error:#}", provider.name());
                    state.problems.insert(provider.id().to_owned(), problem);
                }
            }
            changed.then(|| state.saved.clone())
        };
        if let Some(saved) = to_save {
            self.store.save(&saved).ok();
        }
    }

    /// Fetches usage for the accounts the poll schedule says are due, then auto-switches if needed.
    pub fn poll(&self) {
        self.poll_with(|| {});
    }

    pub fn poll_with(&self, on_change: impl Fn() + Sync) {
        self.fetch_where(|schedule, _, now| schedule.due(now), true, on_change);
    }

    /// Fetches usage on request, except for accounts read recently or backing off.
    pub fn refresh(&self) {
        self.refresh_with(|| {});
    }

    pub fn refresh_with(&self, on_change: impl Fn() + Sync) {
        self.fetch_where(|schedule, fetched_at, now| schedule.due_on_request(fetched_at, now), false, on_change);
    }

    fn fetch_where(
        &self,
        due: impl Fn(&Schedule, Option<Timestamp>, Timestamp) -> bool + Sync,
        reuse_live: bool,
        on_change: impl Fn() + Sync,
    ) {
        let refresh_provider = |provider: &dyn Provider| {
            {
                let _work = self.work(provider.id());
                self.sync_live_provider_locked(provider, reuse_live.then_some(LIVE_CACHE_TTL));
            }
            on_change();
            let now = Timestamp::now();
            let ids: Vec<String> = {
                let state = self.state();
                let fetched_at = |account: &SavedAccount| account.usage.as_ref().map(|usage| usage.fetched_at);
                let accounts = state.saved.accounts.iter().filter(|account| account.provider == provider.id());
                accounts
                    .filter(|account| due(&state.schedule(account), fetched_at(account), now))
                    .map(|account| account.id.clone())
                    .collect()
            };
            for id in ids {
                {
                    let _work = self.work(provider.id());
                    if let Ok(account) = self.account(&id) {
                        self.refresh_account_locked(&account, reuse_live);
                    }
                }
                on_change();
            }
        };
        std::thread::scope(|scope| {
            for provider in &self.providers {
                scope.spawn(|| refresh_provider(&**provider));
            }
        });
        self.auto_switch();
        on_change();
    }

    pub fn add_account(&self, provider: &str) -> Result<(), Error> {
        let provider = self.provider(provider)?;
        if !provider.switchable() {
            let how = match provider.sign_in() {
                Some(_) => {
                    format!("Add {} accounts by signing in with `swx login {}`.", provider.name(), provider.id())
                }
                None => format!("{} accounts can not be added from the current login.", provider.name()),
            };
            return Err(Error::Invalid(how));
        }
        let _work = self.work(provider.id());
        let login = provider
            .live()?
            .ok_or_else(|| Error::Invalid(format!("Sign in with {} first, then add the account.", provider.name())))?;
        self.save_verified(&*provider, &login)?;
        {
            let mut state = self.state();
            state.provider_backoffs.remove(provider.id());
            state.problems.remove(&auto_switch_problem_key(provider.id()));
        }
        self.sync_live_provider_locked(&*provider, None);
        Ok(())
    }

    pub fn activate(&self, id: &str) -> Result<(), Error> {
        let account = self.account(id)?;
        let _work = self.work(&account.provider);
        self.activate_locked(id, SwitchSource::Manual, Timestamp::now()).map_err(SwitchFailure::into_error)
    }

    pub fn redeem_reset(&self, id: &str, grant_id: &str) -> Result<(), Error> {
        let account = self.account(id)?;
        let provider = self.provider(&account.provider)?;
        if !provider.supports_resets() {
            return Err(Error::Invalid(format!("{} does not support limit resets.", provider.name())));
        }
        let _work = self.work(provider.id());
        let account = self.account(id)?;
        let now = Timestamp::now();
        let Some(grant) = account.resets.iter().find(|grant| grant.id == grant_id && grant.available(now)) else {
            return Err(Error::Invalid("That limit reset is no longer available.".to_owned()));
        };
        if !grant.usable {
            return Err(Error::Invalid("That limit reset cannot be used right now.".to_owned()));
        }

        let live = self
            .read_live(&*provider, None)
            .map_err(Error::from)?
            .filter(|live| same(&live.identity, &account.identity));
        let is_live = live.is_some();
        if !is_live && account.status == AccountStatus::NeedsLogin {
            return Err(sign_in_again_error(&account, &*provider));
        }

        let request_id = match account.pending_reset_claims.get(grant_id) {
            Some(existing) => existing.clone(),
            None => {
                let request_id = uuid::Uuid::new_v4().to_string();
                self.change(|saved| {
                    if let Some(saved_account) = saved.accounts.iter_mut().find(|a| a.id == id) {
                        saved_account.pending_reset_claims.insert(grant_id.to_owned(), request_id.clone());
                    }
                })?;
                request_id
            }
        };

        let outcome = match live {
            Some(live) => provider.redeem_reset(&live.secret, &account.identity, grant_id, &request_id),
            None => self.redeem_saved_locked(&*provider, &account, grant_id, &request_id),
        };

        match outcome {
            Ok(outcome) => {
                self.apply_redeemed_reset(&account, grant_id, outcome, is_live, now)?;
                if let Ok(updated) = self.account(id) {
                    self.refresh_account_locked(&updated, false);
                }
                Ok(())
            }
            Err(RedeemError::Unsettled(message)) => Err(Error::Failed(message)),
            Err(RedeemError::Exhausted(message)) => {
                self.change(|saved| {
                    if let Some(saved_account) = saved.accounts.iter_mut().find(|a| a.id == id) {
                        saved_account.resets.retain(|reset| reset.id != grant_id);
                        saved_account.pending_reset_claims.remove(grant_id);
                        saved_account.resets_checked_at = None;
                    }
                })
                .ok();
                Err(Error::Invalid(message))
            }
            Err(error) => {
                self.change(|saved| {
                    if let Some(saved_account) = saved.accounts.iter_mut().find(|a| a.id == id) {
                        saved_account.pending_reset_claims.remove(grant_id);
                    }
                })
                .ok();
                match error {
                    RedeemError::NotNextGrant => {
                        let preferred = self.preferred_reset_label(id, grant_id);
                        let message = match preferred {
                            Some(label) => format!("Use {label} before using this reset."),
                            None => "Another limit reset has to be used first.".to_owned(),
                        };
                        Err(Error::Invalid(message))
                    }
                    RedeemError::Refused(message) => Err(Error::Invalid(message)),
                    RedeemError::Remote(RemoteError::Unauthorized) => {
                        if is_live {
                            Err(Error::Invalid(format!(
                                "The current {} login has expired. Use {} once so it renews the login, then try again.",
                                provider.name(),
                                provider.name()
                            )))
                        } else {
                            Err(Error::Invalid(format!(
                                "This {} login could not be authorized to use resets. Sign in with {} again, then re-add the account.",
                                provider.name(),
                                provider.name()
                            )))
                        }
                    }
                    RedeemError::Remote(RemoteError::Rejected) => {
                        self.set_status(id, AccountStatus::NeedsLogin);
                        Err(sign_in_again_error(&account, &*provider))
                    }
                    RedeemError::Remote(RemoteError::RateLimited(_)) => {
                        Err(Error::Failed(format!("{} is rate limiting; try again later.", provider.name())))
                    }
                    RedeemError::Remote(error) => Err(Error::Failed(error.to_string())),
                    RedeemError::Exhausted(_) | RedeemError::Unsettled(_) => unreachable!(),
                }
            }
        }
    }

    pub fn switch(&self, provider: &str, strategy: Strategy) -> Result<(), Error> {
        let provider = self.provider(provider)?;
        if !provider.switchable() {
            return Err(not_switchable(&*provider));
        }
        let _work = self.work(provider.id());
        let now = Timestamp::now();
        let target = pick(&self.state(), provider.id(), strategy, now);
        let target = target.ok_or_else(|| Error::Invalid("There is no other account to switch to.".to_owned()))?;
        self.activate_locked(&target, SwitchSource::Manual, now).map_err(SwitchFailure::into_error)
    }

    /// Starts a browser sign-in. The browser comes back to `redirect_uri` with the state and a code.
    pub fn begin_sign_in(&self, provider: &str, redirect_uri: &str) -> Result<SignIn, Error> {
        let provider = self.provider(provider)?;
        let sign_in = provider.sign_in().ok_or_else(|| {
            Error::Invalid(format!("{} does not support signing in through Switch.", provider.name()))
        })?;
        if !(redirect_uri.starts_with("http://") || redirect_uri.starts_with("https://")) {
            return Err(Error::Invalid("The redirect URI has to be an http or https URL.".to_owned()));
        }
        let (state, challenge) = self.sign_ins.lock().unwrap().begin(provider.id(), redirect_uri, Timestamp::now());
        Ok(SignIn { url: sign_in.authorize_url(redirect_uri, &state, &challenge), state })
    }

    /// Trades the code for a login and saves it, updating the account when it was saved before.
    pub fn finish_sign_in(&self, provider: &str, state: &str, code: &str) -> Result<(), Error> {
        let provider = self.provider(provider)?;
        let name = provider.name();
        let sign_in = provider
            .sign_in()
            .ok_or_else(|| Error::Invalid(format!("{name} does not support signing in through Switch.")))?;
        let pending = self.sign_ins.lock().unwrap().take(provider.id(), state, Timestamp::now());
        let pending = pending
            .ok_or_else(|| Error::Invalid("This sign-in expired or was already used. Start again.".to_owned()))?;
        let secret =
            sign_in.exchange(code.trim(), &pending.redirect_uri, &pending.verifier).map_err(|error| match error {
                RemoteError::Rejected => Error::Invalid(format!("{name} did not accept the sign-in. Start again.")),
                error => Error::Failed(format!("Could not finish signing in to {name}: {error}")),
            })?;
        let profile = provider
            .profile(&secret)
            .map_err(|error| Error::Failed(format!("Could not read the {name} account: {error}")))?;
        let _work = self.work(provider.id());
        self.save_login(&*provider, &profile.identity, Some(profile.plan), &secret)?;
        if let Some(account) = self.find_account(provider.id(), &profile.identity) {
            self.refresh_account_locked(&account, false);
        }
        Ok(())
    }

    pub fn update_account(&self, id: &str, update: AccountUpdate) -> Result<(), Error> {
        self.account(id)?;
        self.change(|saved| {
            let Some(account) = saved.accounts.iter_mut().find(|account| account.id == id) else { return };
            if let Some(alias) = update.alias {
                let alias = alias.trim();
                account.alias = (!alias.is_empty()).then(|| alias.to_owned());
            }
            if let Some(enabled) = update.enabled {
                account.enabled = enabled;
            }
        })?;
        Ok(())
    }

    pub fn remove_account(&self, id: &str) -> Result<(), Error> {
        let account = self.account(id)?;
        let _work = self.work(&account.provider);
        self.account(id)?;
        self.vault.remove(id)?;
        self.change(|saved| {
            saved.accounts.retain(|account| account.id != id);
            saved.manual_holds.retain(|_, hold| hold.account_id != id);
            saved.last_live.retain(|_, account_id| account_id != id);
        })?;
        let mut state = self.state();
        state.schedules.remove(id);
        state.live_secrets.remove(id);
        state.target_backoffs.remove(id);
        state.next_reset_grants.remove(id);
        Ok(())
    }

    pub fn update_order(&self, update: OrderUpdate) -> Result<(), Error> {
        self.change(|saved| {
            if let Some(order) = update.providers {
                let mut ids: Vec<String> =
                    self.ordered_providers(saved).into_iter().map(|provider| provider.id().to_owned()).collect();
                reorder(&mut ids, &order, |id| id);
                saved.provider_order = ids;
            }
            if let Some(order) = update.accounts {
                reorder(&mut saved.accounts, &order, |account| &account.id);
            }
        })?;
        Ok(())
    }

    pub fn update_settings(&self, update: SettingsUpdate) -> Result<Settings, Error> {
        if update.threshold.is_some_and(|threshold| !threshold.is_finite() || !(1.0..=100.0).contains(&threshold)) {
            return Err(Error::Invalid("The threshold must be between 1 and 100.".to_owned()));
        }
        if update.cooldown_seconds.is_some_and(|cooldown| cooldown > MAX_COOLDOWN_SECONDS) {
            return Err(Error::Invalid(format!("The cooldown must be at most {MAX_COOLDOWN_SECONDS} seconds.")));
        }
        let settings = self.change(|saved| {
            if let Some(auto_switch) = update.auto_switch {
                saved.settings.auto_switch = auto_switch;
            }
            if let Some(threshold) = update.threshold {
                saved.settings.threshold = threshold;
            }
            if let Some(cooldown_seconds) = update.cooldown_seconds {
                saved.settings.cooldown_seconds = cooldown_seconds;
            }
            if let Some(strategy) = update.strategy {
                saved.settings.strategy = strategy;
            }
            saved.settings.clone()
        })?;
        if update.auto_switch == Some(false) {
            self.state().problems.retain(|key, _| !key.starts_with("auto_switch:"));
        }
        Ok(settings)
    }

    fn activate_locked(&self, id: &str, source: SwitchSource, now: Timestamp) -> Result<(), SwitchFailure> {
        let account = self.account(id).map_err(SwitchFailure::Target)?;
        let provider = self.provider(&account.provider).map_err(SwitchFailure::Target)?;
        if !provider.switchable() {
            return Err(SwitchFailure::Target(not_switchable(&*provider)));
        }
        let label = account.alias.as_deref().unwrap_or(&account.identity.email);
        let sign_in_again = || {
            Error::Invalid(format!(
                "{label} has to sign in again. Sign in with {}, then add the account.",
                provider.name()
            ))
        };
        if account.status == AccountStatus::NeedsLogin {
            return Err(SwitchFailure::Target(sign_in_again()));
        }
        let live = self.read_live(&*provider, None).map_err(|error| SwitchFailure::Outgoing(error.into()))?;
        if live.is_some_and(|live| same(&live.identity, &account.identity)) {
            self.record_switch(provider.id(), &account, source, now);
            self.sync_live_provider_locked(&*provider, None);
            return Ok(());
        }
        let mut target = self
            .vault
            .get(id)
            .map_err(|error| SwitchFailure::Target(error.into()))?
            .ok_or_else(|| SwitchFailure::Target(sign_in_again()))?;
        if provider.expired(&target) {
            target = match self.renew_saved_locked(&*provider, id, &target) {
                Ok(renewed) => renewed,
                Err(RemoteError::Rejected) => {
                    self.set_status(id, AccountStatus::NeedsLogin);
                    return Err(SwitchFailure::Target(sign_in_again()));
                }
                Err(error) => {
                    return Err(SwitchFailure::Target(Error::Failed(format!(
                        "Could not renew the login for {label}: {error}"
                    ))));
                }
            };
        }

        let guard = provider.lock().map_err(|error| SwitchFailure::Outgoing(error.into()))?;
        if let Some(live) = provider.live().map_err(|error| SwitchFailure::Outgoing(error.into()))? {
            if same(&live.identity, &account.identity) {
                drop(guard);
                self.record_switch(provider.id(), &account, source, now);
                self.sync_live_provider_locked(&*provider, None);
                return Ok(());
            }
            self.keep_outgoing(&*provider, &live).map_err(SwitchFailure::Outgoing)?;
        }
        provider.replace_live(&target).map_err(|error| SwitchFailure::Target(error.into()))?;
        drop(guard);
        self.state().live_cache.remove(provider.id());
        self.record_switch(provider.id(), &account, source, now);
        self.sync_live_provider_locked(&*provider, None);
        Ok(())
    }

    fn record_switch(&self, provider_id: &str, account: &SavedAccount, source: SwitchSource, now: Timestamp) {
        self.change(|saved| {
            saved.last_live.insert(provider_id.to_owned(), account.id.clone());
            match source {
                SwitchSource::Manual => {
                    saved.last_auto_switch.remove(provider_id);
                    record_manual_hold(saved, provider_id, account, now);
                }
                SwitchSource::Auto => {
                    saved.manual_holds.remove(provider_id);
                    saved.last_auto_switch.insert(provider_id.to_owned(), now);
                }
            }
        })
        .ok();
        let mut state = self.state();
        if let Some(schedule) = state.schedules.get_mut(&account.id) {
            schedule.clear_failure_backoff(now);
        }
        state.target_backoffs.remove(&account.id);
        state.provider_backoffs.remove(provider_id);
        state.problems.remove(&auto_switch_problem_key(provider_id));
    }

    /// The live login can hold a refresh token nothing else has, so it is saved before it is replaced,
    /// but only into the account it provably belongs to.
    fn keep_outgoing(&self, provider: &dyn Provider, live: &Login) -> Result<(), Error> {
        let owner = self.find_account(provider.id(), &live.identity);
        let lineage = provider.lineage(&live.secret);
        if let Some(owner) = &owner {
            let stored = self.vault.get(&owner.id)?;
            if lineage.is_some() && stored.is_some_and(|stored| provider.lineage(&stored) == lineage) {
                self.vault.put(&owner.id, &live.secret)?;
                return Ok(());
            }
        }
        match provider.profile(&live.secret) {
            Ok(profile) if same(&profile.identity, &live.identity) => {
                self.save_login(provider, &live.identity, Some(profile.plan), &live.secret)?;
                Ok(())
            }
            Ok(_) => Err(mismatch(provider)),
            Err(_)
                if owner.as_ref().is_some_and(|owner| !self.belongs_elsewhere(provider, &live.secret, &owner.id)) =>
            {
                self.save_login(provider, &live.identity, None, &live.secret)?;
                Ok(())
            }
            Err(error) => Err(Error::Invalid(format!(
                "Could not confirm which account the current {} login belongs to ({error}), so it was left in place.",
                provider.name()
            ))),
        }
    }

    fn belongs_elsewhere(&self, provider: &dyn Provider, secret: &Secret, owner: &str) -> bool {
        let lineage = provider.lineage(secret);
        let others: Vec<String> = self
            .state()
            .saved
            .accounts
            .iter()
            .filter(|account| account.provider == provider.id() && account.id != owner)
            .map(|account| account.id.clone())
            .collect();
        others.iter().flat_map(|id| [id.clone(), format!("{id}.previous")]).any(|key| {
            // A vault that cannot be read proves nothing, so it counts as a match.
            self.vault
                .get(&key)
                .map_or(true, |stored| stored.is_some_and(|stored| provider.lineage(&stored) == lineage))
        })
    }

    fn save_verified(&self, provider: &dyn Provider, login: &Login) -> Result<(), Error> {
        let name = provider.name();
        match provider.profile(&login.secret) {
            Ok(profile) if same(&profile.identity, &login.identity) => {
                self.save_login(provider, &login.identity, Some(profile.plan), &login.secret)?;
                Ok(())
            }
            Ok(_) => Err(mismatch(provider)),
            Err(RemoteError::Unauthorized) => Err(Error::Invalid(format!(
                "The current {name} login has expired. Use {name} once so it renews the login, then add it again."
            ))),
            Err(error) => Err(Error::Failed(format!("Could not check the current {name} login: {error}"))),
        }
    }

    fn save_login(
        &self,
        provider: &dyn Provider,
        identity: &Identity,
        profile_plan: Option<Option<String>>,
        secret: &Secret,
    ) -> anyhow::Result<()> {
        let existing = self.find_account(provider.id(), identity).map(|account| account.id);
        let id = existing.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        self.vault.put(&id, secret)?;
        self.state().target_backoffs.remove(&id);
        let checked_at = Timestamp::now();
        self.change(|saved| match saved.accounts.iter_mut().find(|account| account.id == id) {
            Some(account) => {
                account.identity = identity.clone();
                if let Some(plan) = profile_plan {
                    account.plan = plan.or(account.plan.take());
                    account.plan_checked_at = Some(checked_at);
                }
                if account.status == AccountStatus::NeedsLogin {
                    account.status = AccountStatus::Pending;
                }
            }
            None => saved.accounts.push(SavedAccount {
                id,
                provider: provider.id().to_owned(),
                identity: identity.clone(),
                alias: None,
                plan_checked_at: profile_plan.as_ref().map(|_| checked_at),
                plan: profile_plan.flatten(),
                enabled: true,
                status: AccountStatus::Pending,
                usage: None,
                resets: Vec::new(),
                resets_checked_at: None,
                pending_reset_claims: BTreeMap::new(),
            }),
        })
    }

    fn refresh_account_locked(&self, account: &SavedAccount, reuse_live: bool) {
        let Ok(provider) = self.provider(&account.provider) else { return };
        let live = self
            .read_live(&*provider, reuse_live.then_some(LIVE_CACHE_TTL))
            .ok()
            .flatten()
            .filter(|live| same(&live.identity, &account.identity));
        let mut is_live = live.is_some();
        if !is_live && account.status == AccountStatus::NeedsLogin {
            return;
        }
        // The live login belongs to the provider's own tools, so only they renew it.
        let result = match live {
            Some(live) => provider.usage(&live.secret).map(|windows| (windows, live.secret)),
            None => self.fetch_saved_locked(&*provider, account, &mut is_live),
        };
        let now = Timestamp::now();
        let recently_limited = self.state().schedule(account).recently_limited(now);
        let plan_due = account.plan_checked_at.is_none_or(|at| now.duration_since(at).as_secs() >= PLAN_REFRESH_SECS);
        let (plan_checked, plan_update) = match &result {
            Ok((_, secret)) if plan_due && !recently_limited => {
                (true, provider.profile(secret).ok().and_then(|profile| profile.plan))
            }
            _ => (false, None),
        };
        let resets_min_age = if reuse_live { RESETS_REFRESH_SECS } else { RESETS_FORCED_MIN_SECS };
        let resets_due = provider.supports_resets()
            && !recently_limited
            && account.resets_checked_at.is_none_or(|at| now.duration_since(at).as_secs() >= resets_min_age);
        let resets_result = match &result {
            Ok((_, secret)) if resets_due => Some(provider.resets(secret)),
            _ => None,
        };
        let mut state = self.state();
        let threshold = state.saved.settings.threshold;
        let mut schedule = state.schedule(account);
        match &result {
            Ok((windows, _)) => {
                let previous = account.usage.as_ref().map(|usage| usage.windows.as_slice());
                let fetched = Fetched { previous, windows, active: is_live, threshold };
                poll::after_success(&mut schedule, fetched, now, jitter());
            }
            Err(RemoteError::RateLimited(retry_after)) => {
                poll::after_failure(&mut schedule, true, retry_after.map(|after| after.as_secs_f64()), now)
            }
            Err(_) => poll::after_failure(&mut schedule, false, None, now),
        }
        if let Some(Err(RemoteError::RateLimited(_))) = &resets_result {
            schedule.limited_at = Some(now);
        }
        if let Some(Ok(grants)) = &resets_result {
            state.next_reset_grants.insert(account.id.clone(), grants.next_grant_id.clone());
        }
        state.schedules.insert(account.id.clone(), schedule);
        drop(state);

        let stale = |message: String| (AccountStatus::Stale { message }, None);
        let (status, windows) = match result {
            Ok((windows, _)) => (AccountStatus::Ready, Some(windows)),
            Err(RemoteError::Rejected) => (AccountStatus::NeedsLogin, None),
            Err(RemoteError::Unauthorized) if is_live => {
                stale(format!("Waiting for {} to renew its login", provider.name()))
            }
            Err(RemoteError::RateLimited(_)) => {
                stale(format!("{} is rate limiting; trying again later", provider.name()))
            }
            Err(error) => stale(error.to_string()),
        };
        self.change(|saved| {
            let Some(saved) = saved.accounts.iter_mut().find(|saved| saved.id == account.id) else { return };
            saved.status = status;
            if let Some(windows) = windows {
                saved.usage = Some(Usage { windows, fetched_at: now });
            }
            if plan_update.is_some() {
                saved.plan = plan_update;
            }
            if plan_checked {
                saved.plan_checked_at = Some(now);
            }
            match resets_result {
                Some(Ok(grants)) => {
                    saved.resets = grants.resets;
                    saved
                        .pending_reset_claims
                        .retain(|grant_id, _| saved.resets.iter().any(|grant| grant.id == *grant_id));
                    saved.resets_checked_at = Some(now);
                }
                Some(Err(_)) => {
                    saved.resets_checked_at = Some(now);
                }
                None => {}
            }
        })
        .ok();
    }

    fn redeem_saved_locked(
        &self,
        provider: &dyn Provider,
        account: &SavedAccount,
        grant_id: &str,
        request_id: &str,
    ) -> Result<RedeemOutcome, RedeemError> {
        let mut secret = self.vault.get(&account.id).map_err(other)?.ok_or(RemoteError::Rejected)?;
        let mut renewed = false;
        let renew = |secret: &Secret| -> Result<Secret, RemoteError> {
            let live = self.read_live(provider, None).map_err(other)?;
            if live.is_some_and(|live| same(&live.identity, &account.identity)) {
                return Err(RemoteError::Unauthorized);
            }
            self.renew_saved_locked(provider, &account.id, secret)
        };
        if provider.expired(&secret) {
            secret = renew(&secret)?;
            renewed = true;
        }
        match provider.redeem_reset(&secret, &account.identity, grant_id, request_id) {
            Err(RedeemError::Remote(RemoteError::Unauthorized)) if !renewed => {
                let secret = renew(&secret)?;
                provider.redeem_reset(&secret, &account.identity, grant_id, request_id)
            }
            result => result,
        }
    }

    fn apply_redeemed_reset(
        &self,
        account: &SavedAccount,
        grant_id: &str,
        outcome: RedeemOutcome,
        is_live: bool,
        now: Timestamp,
    ) -> anyhow::Result<()> {
        self.change(|saved| {
            if is_live {
                saved.manual_holds.remove(&account.provider);
            }
            let Some(saved_account) = saved.accounts.iter_mut().find(|saved_account| saved_account.id == account.id)
            else {
                return;
            };
            saved_account.pending_reset_claims.remove(grant_id);
            saved_account.resets_checked_at = outcome.remaining.map(|_| now);
            if let Some(index) = saved_account.resets.iter().position(|grant| grant.id == grant_id) {
                let remaining =
                    outcome.remaining.unwrap_or_else(|| saved_account.resets[index].remaining.saturating_sub(1));
                if remaining == 0 {
                    saved_account.resets.remove(index);
                } else {
                    saved_account.resets[index].remaining = remaining;
                }
            }
        })
    }

    fn preferred_reset_label(&self, account_id: &str, chosen_grant_id: &str) -> Option<String> {
        let state = self.state();
        let next_id = state.next_reset_grants.get(account_id)?.as_deref()?;
        if next_id == chosen_grant_id {
            return None;
        }
        let account = state.saved.accounts.iter().find(|account| account.id == account_id)?;
        let chosen = account.resets.iter().find(|grant| grant.id == chosen_grant_id)?;
        let preferred = account.resets.iter().find(|grant| grant.id == next_id)?;
        (preferred.label != chosen.label).then(|| preferred.label.clone())
    }

    fn fetch_saved_locked(
        &self,
        provider: &dyn Provider,
        account: &SavedAccount,
        is_live: &mut bool,
    ) -> Result<(Vec<UsageWindow>, Secret), RemoteError> {
        let mut secret = self.vault.get(&account.id).map_err(other)?.ok_or(RemoteError::Rejected)?;
        let mut renewed = false;
        let renew = |secret: &Secret, is_live: &mut bool| -> Result<Secret, RemoteError> {
            let live = self.read_live(provider, None).map_err(other)?;
            if live.is_some_and(|live| same(&live.identity, &account.identity)) {
                *is_live = true;
                return Err(RemoteError::Unauthorized);
            }
            self.renew_saved_locked(provider, &account.id, secret)
        };
        if provider.expired(&secret) {
            secret = renew(&secret, is_live)?;
            renewed = true;
        }
        match provider.usage(&secret) {
            Err(RemoteError::Unauthorized) if !renewed => {
                let secret = renew(&secret, is_live)?;
                provider.usage(&secret).map(|windows| (windows, secret))
            }
            result => result.map(|windows| (windows, secret)),
        }
    }

    fn renew_saved_locked(&self, provider: &dyn Provider, id: &str, secret: &Secret) -> Result<Secret, RemoteError> {
        let renewed = provider.renew(secret)?;
        self.vault.put(id, &renewed).map_err(other)?;
        Ok(renewed)
    }

    fn auto_switch(&self) {
        self.auto_switch_at(Timestamp::now());
    }

    fn auto_switch_at(&self, now: Timestamp) {
        enum Plan {
            Switch(Vec<String>),
            Skip { clear_problem: bool },
        }

        {
            let mut state = self.state();
            if !state.saved.settings.auto_switch {
                state.problems.retain(|key, _| !key.starts_with("auto_switch:"));
                return;
            }
        }
        for provider in self.providers.iter().filter(|provider| provider.switchable()) {
            let _work = self.work(provider.id());
            let (plan, clear_hold) = {
                let mut state = self.state();
                let settings = &state.saved.settings;
                if !settings.auto_switch {
                    state.problems.retain(|key, _| !key.starts_with("auto_switch:"));
                    return;
                }
                if state.provider_backoffs.get(provider.id()).is_some_and(|backoff| backoff.blocked(now)) {
                    continue;
                }
                let threshold = settings.threshold;
                let max_candidate = (threshold - AUTO_SWITCH_MARGIN).max(0.0);
                let active = state.live.get(provider.id()).and_then(|live| {
                    state.saved.accounts.iter().find(|a| a.provider == provider.id() && same(live, &a.identity))
                });
                let Some(active) = active else {
                    state.problems.remove(&auto_switch_problem_key(provider.id()));
                    continue;
                };
                let mut clear_hold = false;
                let held = if let Some(hold) = state.saved.manual_holds.get(provider.id()) {
                    let still_held = hold_active(hold, active, threshold, now);
                    clear_hold = !still_held;
                    still_held
                } else {
                    false
                };
                let on_cooldown = settings.cooldown_seconds > 0
                    && state
                        .saved
                        .last_auto_switch
                        .get(provider.id())
                        .is_some_and(|last| now.duration_since(*last).as_secs() < settings.cooldown_seconds as i64);
                let needs_switch = recent_peak(active, now).is_some_and(|p| p >= threshold);
                if held || on_cooldown || !needs_switch {
                    (Plan::Skip { clear_problem: true }, clear_hold)
                } else {
                    let eligible_without_backoff =
                        ordered_candidates(&state, provider.id(), settings.strategy, now, |candidate| {
                            recent_peak(candidate, now).is_some_and(|p| p < threshold && p <= max_candidate)
                        });
                    let candidates: Vec<String> = eligible_without_backoff
                        .iter()
                        .filter(|id| !state.target_backoffs.get(*id).is_some_and(|backoff| backoff.blocked(now)))
                        .cloned()
                        .collect();
                    if candidates.is_empty() {
                        (Plan::Skip { clear_problem: eligible_without_backoff.is_empty() }, clear_hold)
                    } else {
                        (Plan::Switch(candidates), clear_hold)
                    }
                }
            };
            if clear_hold {
                self.change(|saved| {
                    saved.manual_holds.remove(provider.id());
                })
                .ok();
            }
            let candidates = match plan {
                Plan::Switch(candidates) => candidates,
                Plan::Skip { clear_problem } => {
                    if clear_problem {
                        self.state().problems.remove(&auto_switch_problem_key(provider.id()));
                    }
                    continue;
                }
            };
            let provider_id = provider.id();
            let provider_name = provider.name();
            let mut last_target_error: Option<Error> = None;
            let mut outgoing_failed = false;
            let mut switched = false;
            for candidate in candidates {
                match self.activate_locked(&candidate, SwitchSource::Auto, now) {
                    Ok(()) => {
                        switched = true;
                        break;
                    }
                    Err(SwitchFailure::Target(error)) => {
                        self.state().target_backoffs.entry(candidate).or_default().record_failure(now);
                        last_target_error = Some(error);
                    }
                    Err(SwitchFailure::Outgoing(error)) => {
                        let mut state = self.state();
                        state.provider_backoffs.entry(provider_id.to_owned()).or_default().record_failure(now);
                        state.problems.insert(
                            auto_switch_problem_key(provider_id),
                            format!("Auto-switch failed for {provider_name}: {error}"),
                        );
                        outgoing_failed = true;
                        break;
                    }
                }
            }
            if !switched
                && !outgoing_failed
                && let Some(error) = last_target_error
            {
                self.state().problems.insert(
                    auto_switch_problem_key(provider_id),
                    format!("Auto-switch failed for {provider_name}: {error}"),
                );
            }
        }
    }

    fn set_status(&self, id: &str, status: AccountStatus) {
        self.change(|saved| {
            if let Some(account) = saved.accounts.iter_mut().find(|account| account.id == id) {
                account.status = status;
            }
        })
        .ok();
    }

    fn change<T>(&self, apply: impl FnOnce(&mut Saved) -> T) -> anyhow::Result<T> {
        let _write = self.store_write.lock().unwrap();
        let (result, snapshot) = {
            let mut state = self.state();
            let result = apply(&mut state.saved);
            (result, state.saved.clone())
        };
        self.store.save(&snapshot)?;
        Ok(result)
    }

    fn provider(&self, id: &str) -> Result<Arc<dyn Provider>, Error> {
        self.providers
            .iter()
            .find(|provider| provider.id() == id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("Unknown provider {id}")))
    }

    fn account(&self, id: &str) -> Result<SavedAccount, Error> {
        let state = self.state();
        let account = state.saved.accounts.iter().find(|account| account.id == id);
        account.cloned().ok_or_else(|| Error::NotFound("That account no longer exists.".to_owned()))
    }

    fn find_account(&self, provider: &str, identity: &Identity) -> Option<SavedAccount> {
        let state = self.state();
        let mut accounts = state.saved.accounts.iter();
        accounts.find(|account| account.provider == provider && same(&account.identity, identity)).cloned()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn work(&self, provider: &str) -> MutexGuard<'_, ()> {
        self.work.get(provider).unwrap_or(&self.fallback_work).lock()
    }
}

fn reorder<T: Clone>(items: &mut [T], order: &[String], id: impl Fn(&T) -> &str) {
    let position = |item: &T| order.iter().position(|listed| listed == id(item));
    let slots: Vec<usize> = (0..items.len()).filter(|index| position(&items[*index]).is_some()).collect();
    let mut listed: Vec<T> = slots.iter().map(|index| items[*index].clone()).collect();
    listed.sort_by_key(position);
    for (slot, item) in slots.into_iter().zip(listed) {
        items[slot] = item;
    }
}

fn auto_switch_problem_key(provider_id: &str) -> String {
    format!("auto_switch:{provider_id}")
}

fn pick(state: &State, provider: &str, strategy: Strategy, now: Timestamp) -> Option<String> {
    ordered_candidates(state, provider, strategy, now, |_| true).into_iter().next()
}

fn ordered_candidates(
    state: &State,
    provider: &str,
    strategy: Strategy,
    now: Timestamp,
    eligible: impl Fn(&SavedAccount) -> bool,
) -> Vec<String> {
    let accounts: Vec<&SavedAccount> = state.saved.accounts.iter().filter(|a| a.provider == provider).collect();
    let live = state.live.get(provider);
    let active = accounts.iter().position(|account| live.is_some_and(|live| same(live, &account.identity)));
    let start = active.map_or(0, |index| index + 1);
    let mut candidates: Vec<&SavedAccount> = (0..accounts.len())
        .map(|offset| (start + offset) % accounts.len())
        .filter(|index| Some(*index) != active)
        .map(|index| accounts[index])
        .filter(|account| account.enabled && account.status != AccountStatus::NeedsLogin)
        .filter(|account| {
            strategy != Strategy::Next
                || account
                    .usage
                    .as_ref()
                    .and_then(|usage| poll::current_peak(&usage.windows, now))
                    .is_none_or(|peak| peak < 100.0)
        })
        .filter(|account| eligible(account))
        .collect();
    if strategy == Strategy::Best {
        candidates.sort_by(|a, b| load(a, now).total_cmp(&load(b, now)));
    }
    candidates.into_iter().map(|account| account.id.clone()).collect()
}

fn record_manual_hold(saved: &mut Saved, provider_id: &str, account: &SavedAccount, now: Timestamp) {
    let threshold = saved.settings.threshold;
    let over_threshold_resets: BTreeMap<String, Option<Timestamp>> = account
        .usage
        .as_ref()
        .into_iter()
        .flat_map(|usage| usage.windows.iter())
        .filter(|window| window.primary)
        .filter(|window| window.resets_at.is_none_or(|reset| reset > now))
        .filter(|window| window.used_percent.is_some_and(|percent| percent >= threshold))
        .map(|window| (window.id.clone(), window.resets_at))
        .collect();
    if over_threshold_resets.is_empty() {
        saved.manual_holds.remove(provider_id);
    } else {
        saved
            .manual_holds
            .insert(provider_id.to_owned(), ManualHold { account_id: account.id.clone(), over_threshold_resets });
    }
}

fn hold_active(hold: &ManualHold, active: &SavedAccount, threshold: f64, now: Timestamp) -> bool {
    if hold.account_id != active.id || hold.over_threshold_resets.is_empty() {
        return false;
    }
    let windows = active.usage.as_ref().map(|usage| usage.windows.as_slice()).unwrap_or(&[]);
    let any_reset = hold.over_threshold_resets.iter().any(|(win_id, recorded_reset)| {
        if recorded_reset.is_some_and(|reset| reset <= now) {
            return true;
        }
        windows.iter().find(|w| w.primary && &w.id == win_id).is_some_and(|window| {
            let later_reset = match (recorded_reset, window.resets_at) {
                (Some(before), Some(after)) => after.duration_since(*before).as_secs() > RESET_ADVANCE_TOLERANCE_SECS,
                (None, Some(_)) => true,
                _ => false,
            };
            let below_threshold = window.used_percent.is_some_and(|percent| percent < threshold);
            later_reset || below_threshold
        })
    });
    !any_reset
}

fn load(account: &SavedAccount, now: Timestamp) -> f64 {
    account.usage.as_ref().and_then(|usage| poll::current_peak(&usage.windows, now)).unwrap_or(f64::INFINITY)
}

fn recent_peak(account: &SavedAccount, now: Timestamp) -> Option<f64> {
    if account.status != AccountStatus::Ready {
        return None;
    }
    let usage = account.usage.as_ref()?;
    if now.duration_since(usage.fetched_at).as_secs() > MAX_SWITCH_USAGE_AGE_SECS {
        return None;
    }
    poll::current_peak(&usage.windows, now)
}

fn jitter() -> f64 {
    (uuid::Uuid::new_v4().as_u128() % 2001) as f64 / 1000.0 - 1.0
}

impl State {
    /// Usage read before a restart is not fetched again until it would have been due.
    fn schedule(&self, account: &SavedAccount) -> Schedule {
        self.schedules.get(&account.id).cloned().unwrap_or_else(|| Schedule {
            next_at: account.usage.as_ref().map(|usage| poll::fresh_until(usage.fetched_at)),
            ..Schedule::default()
        })
    }
}

/// Organizations are compared only when both sides know theirs.
fn same(a: &Identity, b: &Identity) -> bool {
    a.account_id == b.account_id
        && match (&a.organization_id, &b.organization_id) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
}

fn mismatch(provider: &dyn Provider) -> Error {
    let name = provider.name();
    Error::Invalid(format!(
        "The current {name} login does not belong to the account in {name}'s settings. Sign in again with {name}, then retry."
    ))
}

fn not_switchable(provider: &dyn Provider) -> Error {
    Error::Invalid(format!("Switching {} accounts is not supported yet.", provider.name()))
}

fn sign_in_again_error(account: &SavedAccount, provider: &dyn Provider) -> Error {
    let label = account.alias.as_deref().unwrap_or(&account.identity.email);
    Error::Invalid(format!("{label} has to sign in again. Sign in with {}, then add the account.", provider.name()))
}

fn other(error: anyhow::Error) -> RemoteError {
    RemoteError::Other(format!("{error:#}"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::demo::Demo;

    fn engine_with(demo: &Arc<Demo>) -> Engine {
        Engine::new(vec![demo.clone() as Arc<dyn Provider>], Store::memory(), Vault::memory()).unwrap()
    }

    fn add(engine: &Engine, demo: &Demo, email: &str) -> (Login, String) {
        let login = demo.login(email, "Pro");
        demo.set_live(Some(login.clone()));
        engine.add_account(demo.id()).unwrap();
        let id = engine.find_account(demo.id(), &login.identity).unwrap().id;
        (login, id)
    }

    fn live_email(demo: &Demo) -> String {
        demo.live().unwrap().unwrap().identity.email
    }

    #[test]
    fn the_demo_has_accounts_and_an_unsaved_login() {
        let snapshot = Demo::engine().unwrap().snapshot();
        let claude = &snapshot.providers[0];
        assert_eq!(claude.accounts.len(), 3);
        assert_eq!(claude.active().unwrap().identity.email, "ada@example.com");
        assert!(claude.unsaved_login.is_none());
        assert_eq!(snapshot.providers[1].unsaved_login.as_ref().unwrap().email, "linus@example.com");
        let usage_only = &snapshot.providers[2];
        assert!(claude.switchable && !claude.sign_in);
        assert!(!usage_only.switchable && usage_only.sign_in);
        assert_eq!(usage_only.accounts[0].identity.email, "ada@example.com");
        assert!(usage_only.unsaved_login.is_none());
    }

    #[test]
    fn switching_keeps_a_login_rotated_by_the_provider() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        let (_, grace) = add(&engine, &demo, "grace@example.com");
        let (ada, ada_id) = add(&engine, &demo, "ada@example.com");
        let rotated = Login { identity: ada.identity.clone(), secret: demo.renew(&ada.secret).unwrap() };
        demo.set_live(Some(rotated.clone()));

        engine.activate(&grace).unwrap();

        assert_eq!(live_email(&demo), "grace@example.com");
        assert_eq!(engine.vault.get(&ada_id).unwrap(), Some(rotated.secret));
        engine.activate(&ada_id).unwrap();
        assert_eq!(live_email(&demo), "ada@example.com");
    }

    #[test]
    fn a_login_that_belongs_to_someone_else_is_left_in_place() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        let (grace, grace_id) = add(&engine, &demo, "grace@example.com");
        let (ada, ada_id) = add(&engine, &demo, "ada@example.com");
        let crossed = Login { identity: ada.identity, secret: demo.renew(&grace.secret).unwrap() };
        demo.set_live(Some(crossed.clone()));

        assert!(matches!(engine.activate(&grace_id), Err(Error::Invalid(_))));
        assert_eq!(demo.live().unwrap().unwrap().secret, crossed.secret);
        assert_eq!(engine.vault.get(&ada_id).unwrap(), Some(ada.secret));
    }

    #[test]
    fn switching_away_saves_an_unsaved_login() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        let (_, ada) = add(&engine, &demo, "ada@example.com");
        demo.set_live(Some(demo.login("linus@example.com", "Pro")));
        engine.sync_live();
        assert!(engine.snapshot().providers[0].unsaved_login.is_some());

        engine.activate(&ada).unwrap();

        let claude = &engine.snapshot().providers[0];
        assert_eq!(claude.accounts.len(), 2);
        assert!(claude.unsaved_login.is_none());
    }

    fn set_peaks(engine: &Engine, peaks: &[f64]) {
        engine
            .change(|saved| {
                for (account, peak) in saved.accounts.iter_mut().zip(peaks) {
                    let window = UsageWindow {
                        id: "w".into(),
                        label: "w".into(),
                        used_percent: Some(*peak),
                        resets_at: None,
                        primary: true,
                        period_seconds: None,
                    };
                    account.status = AccountStatus::Ready;
                    account.usage = Some(Usage { windows: vec![window], fetched_at: Timestamp::now() });
                }
            })
            .unwrap();
    }

    #[test]
    fn next_and_best_skip_unusable_accounts() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        let ids: Vec<String> = ["a@x", "b@x", "c@x", "d@x"].iter().map(|email| add(&engine, &demo, email).1).collect();
        demo.set_live(Some(demo.login("a@x", "Pro")));
        engine.sync_live();
        set_peaks(&engine, &[90.0, 80.0, 10.0, 5.0]);
        engine.update_account(&ids[3], AccountUpdate { alias: None, enabled: Some(false) }).unwrap();

        let state = engine.state();
        let now = Timestamp::now();
        assert_eq!(pick(&state, "claude", Strategy::Next, now), Some(ids[1].clone()));
        assert_eq!(pick(&state, "claude", Strategy::Best, now), Some(ids[2].clone()));
    }

    #[test]
    fn auto_switch_moves_off_a_busy_account() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        add(&engine, &demo, "idle@example.com");
        add(&engine, &demo, "busy@example.com");
        set_peaks(&engine, &[10.0, 95.0]);
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(50.0),
                ..SettingsUpdate::default()
            })
            .unwrap();

        engine.auto_switch();

        assert_eq!(live_email(&demo), "idle@example.com");
    }

    #[test]
    fn auto_switch_stays_when_every_account_is_busy() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        add(&engine, &demo, "idle@example.com");
        add(&engine, &demo, "busy@example.com");
        set_peaks(&engine, &[60.0, 95.0]);
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(50.0),
                ..SettingsUpdate::default()
            })
            .unwrap();

        engine.auto_switch();

        assert_eq!(live_email(&demo), "busy@example.com");
    }

    #[test]
    fn refreshing_fills_in_usage() {
        let engine = Demo::engine().unwrap();
        engine.refresh();
        let snapshot = engine.snapshot();
        let account = &snapshot.providers[0].accounts[0];
        assert_eq!(account.status, AccountStatus::Ready);
        assert_eq!(account.usage.as_ref().unwrap().windows.len(), 2);
        assert!(snapshot.warnings.is_empty());
    }

    #[test]
    fn fresh_usage_is_not_fetched_again() {
        let engine = Demo::engine().unwrap();
        engine.refresh();
        let first = engine.snapshot();
        engine.refresh();
        engine.poll();
        assert_eq!(engine.snapshot(), first);
        assert!(engine.state().schedules.values().all(|schedule| !schedule.due(Timestamp::now())));
    }

    #[test]
    fn removing_forgets_the_login() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        let (_, id) = add(&engine, &demo, "ada@example.com");
        engine.remove_account(&id).unwrap();
        assert!(engine.vault.get(&id).unwrap().is_none());
        assert!(engine.snapshot().providers[0].unsaved_login.is_some());
        assert!(matches!(engine.remove_account(&id), Err(Error::NotFound(_))));
    }

    fn signed_in(engine: &Engine, provider: &str, code: &str) -> String {
        let sign_in = engine.begin_sign_in(provider, "http://127.0.0.1:1/callback").unwrap();
        engine.finish_sign_in(provider, &sign_in.state, code).unwrap();
        let state = engine.state();
        state.saved.accounts.iter().rev().find(|account| account.provider == provider).unwrap().id.clone()
    }

    #[test]
    fn signing_in_saves_a_usage_only_account_once() {
        let demo = Arc::new(Demo::usage_only("usage", "Usage"));
        let engine = engine_with(&demo);
        let first = signed_in(&engine, "usage", "sam");
        assert_eq!(signed_in(&engine, "usage", "sam"), first);

        let usage = &engine.snapshot().providers[0];
        assert!(!usage.switchable && usage.sign_in);
        assert_eq!(usage.accounts.len(), 1);
        assert_eq!(usage.accounts[0].identity.email, "sam@example.com");
        assert_eq!(usage.accounts[0].status, AccountStatus::Ready);
        assert!(engine.vault.get(&first).unwrap().is_some());
    }

    #[test]
    fn a_sign_in_needs_its_state_and_a_provider_that_supports_it() {
        let usage = Arc::new(Demo::usage_only("usage", "Usage"));
        let claude = Arc::new(Demo::new("claude", "Claude"));
        let providers: Vec<Arc<dyn Provider>> = vec![usage, claude];
        let engine = Engine::new(providers, Store::memory(), Vault::memory()).unwrap();
        let sign_in = engine.begin_sign_in("usage", "http://127.0.0.1:1/callback").unwrap();
        assert!(sign_in.url.contains(&sign_in.state));

        assert!(matches!(engine.finish_sign_in("usage", "forged", "sam"), Err(Error::Invalid(_))));
        assert!(matches!(engine.finish_sign_in("claude", &sign_in.state, "sam"), Err(Error::Invalid(_))));
        assert!(matches!(engine.finish_sign_in("usage", &sign_in.state, "bad code"), Err(Error::Invalid(_))));
        assert!(matches!(engine.finish_sign_in("usage", &sign_in.state, "sam"), Err(Error::Invalid(_))));
        assert!(matches!(engine.begin_sign_in("claude", "http://127.0.0.1:1/callback"), Err(Error::Invalid(_))));
        assert!(matches!(engine.begin_sign_in("usage", "javascript:alert(1)"), Err(Error::Invalid(_))));
        assert!(engine.snapshot().providers[0].accounts.is_empty());
    }

    #[test]
    fn usage_only_accounts_are_never_switched_to() {
        let usage = Arc::new(Demo::usage_only("usage", "Usage"));
        let engine = engine_with(&usage);
        let first = signed_in(&engine, "usage", "ada");
        signed_in(&engine, "usage", "sam");
        set_peaks(&engine, &[95.0, 5.0]);
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(50.0),
                ..SettingsUpdate::default()
            })
            .unwrap();

        assert!(matches!(engine.activate(&first), Err(Error::Invalid(_))));
        assert!(matches!(engine.switch("usage", Strategy::Best), Err(Error::Invalid(_))));
        assert!(
            matches!(engine.add_account("usage"), Err(Error::Invalid(message)) if message.contains("swx login usage"))
        );
        engine.auto_switch();
        assert!(usage.live().unwrap().is_none());
        assert!(engine.snapshot().providers[0].accounts.iter().all(|account| !account.active));
    }

    #[test]
    fn listed_accounts_trade_places_and_the_rest_stay() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        let ids: Vec<String> = ["a@x", "b@x", "c@x"].iter().map(|email| add(&engine, &demo, email).1).collect();
        let accounts = Some(vec![ids[2].clone(), "removed".to_owned(), ids[0].clone()]);

        engine.update_order(OrderUpdate { providers: None, accounts }).unwrap();

        let shown: Vec<String> = engine.snapshot().providers[0].accounts.iter().map(|a| a.id.clone()).collect();
        assert_eq!(shown, [ids[2].clone(), ids[1].clone(), ids[0].clone()]);
        assert_eq!(pick(&engine.state(), "claude", Strategy::Next, Timestamp::now()), Some(ids[1].clone()));
    }

    #[test]
    fn the_order_survives_a_restart_and_new_items_go_last() {
        let home = tempfile::tempdir().unwrap();
        let open = |names: &[&'static str]| {
            let providers =
                names.iter().map(|name| Arc::new(Demo::new(name, name)) as Arc<dyn Provider>).collect::<Vec<_>>();
            Engine::new(providers, Store::file(home.path().join("accounts.json")), Vault::memory()).unwrap()
        };
        let shown = |engine: &Engine| -> Vec<String> {
            engine.snapshot().providers.into_iter().map(|provider| provider.id).collect()
        };
        let providers = Some(vec!["two".to_owned(), "one".to_owned()]);
        open(&["one", "two"]).update_order(OrderUpdate { providers, accounts: None }).unwrap();

        assert_eq!(shown(&open(&["one", "new", "two"])), ["two", "one", "new"]);
    }

    #[test]
    fn settings_are_validated() {
        let engine = engine_with(&Arc::new(Demo::new("claude", "Claude")));
        let invalid = SettingsUpdate { threshold: Some(0.0), ..SettingsUpdate::default() };
        assert!(matches!(engine.update_settings(invalid), Err(Error::Invalid(_))));
        let too_long = SettingsUpdate { cooldown_seconds: Some(MAX_COOLDOWN_SECONDS + 1), ..SettingsUpdate::default() };
        assert!(matches!(engine.update_settings(too_long), Err(Error::Invalid(_))));
    }

    #[test]
    fn manual_activation_is_not_undone_until_over_threshold_window_resets() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        add(&engine, &demo, "idle@example.com");
        let (_, busy) = add(&engine, &demo, "busy@example.com");
        let now = Timestamp::now();
        let reset_at = now + jiff::SignedDuration::from_hours(1);
        engine
            .change(|saved| {
                saved.accounts[0].status = AccountStatus::Ready;
                saved.accounts[0].usage = Some(Usage {
                    windows: vec![UsageWindow {
                        id: "w".into(),
                        label: "w".into(),
                        used_percent: Some(10.0),
                        resets_at: None,
                        primary: true,
                        period_seconds: None,
                    }],
                    fetched_at: now,
                });
                saved.accounts[1].status = AccountStatus::Ready;
                saved.accounts[1].usage = Some(Usage {
                    windows: vec![UsageWindow {
                        id: "w".into(),
                        label: "w".into(),
                        used_percent: Some(95.0),
                        resets_at: Some(reset_at),
                        primary: true,
                        period_seconds: None,
                    }],
                    fetched_at: now,
                });
            })
            .unwrap();
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(50.0),
                ..SettingsUpdate::default()
            })
            .unwrap();

        engine.activate(&busy).unwrap();
        engine.auto_switch_at(now);
        assert_eq!(live_email(&demo), "busy@example.com");

        let after_reset = reset_at + jiff::SignedDuration::from_secs(10);
        engine
            .change(|saved| {
                saved.accounts[0].usage.as_mut().unwrap().fetched_at = after_reset;
                saved.accounts[1].usage = Some(Usage {
                    windows: vec![UsageWindow {
                        id: "w".into(),
                        label: "w".into(),
                        used_percent: Some(95.0),
                        resets_at: Some(after_reset + jiff::SignedDuration::from_hours(5)),
                        primary: true,
                        period_seconds: None,
                    }],
                    fetched_at: after_reset,
                });
            })
            .unwrap();
        engine.auto_switch_at(after_reset);
        assert_eq!(live_email(&demo), "idle@example.com");
    }

    #[test]
    fn manual_choice_survives_restart() {
        let home = tempfile::tempdir().unwrap();
        let store_path = home.path().join("accounts.json");
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine =
            Engine::new(vec![demo.clone() as Arc<dyn Provider>], Store::file(&store_path), Vault::memory()).unwrap();
        add(&engine, &demo, "idle@example.com");
        let (_, busy) = add(&engine, &demo, "busy@example.com");
        set_peaks(&engine, &[10.0, 95.0]);
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(50.0),
                ..SettingsUpdate::default()
            })
            .unwrap();
        engine.activate(&busy).unwrap();
        drop(engine);

        let reopened =
            Engine::new(vec![demo.clone() as Arc<dyn Provider>], Store::file(&store_path), Vault::memory()).unwrap();
        reopened.auto_switch_at(Timestamp::now());
        assert_eq!(live_email(&demo), "busy@example.com");
    }

    #[test]
    fn external_login_change_counts_as_manual() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        let (_, idle) = add(&engine, &demo, "idle@example.com");
        let (busy_login, _) = add(&engine, &demo, "busy@example.com");
        set_peaks(&engine, &[10.0, 95.0]);
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(50.0),
                ..SettingsUpdate::default()
            })
            .unwrap();
        engine.activate(&idle).unwrap();

        demo.set_live(Some(busy_login));
        engine.sync_live();
        engine.auto_switch_at(Timestamp::now());
        assert_eq!(live_email(&demo), "busy@example.com");
    }

    #[test]
    fn auto_switch_respects_cooldown_and_margin() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        add(&engine, &demo, "idle@example.com");
        add(&engine, &demo, "busy@example.com");
        set_peaks(&engine, &[87.0, 95.0]);
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(90.0),
                cooldown_seconds: Some(300),
                ..SettingsUpdate::default()
            })
            .unwrap();

        let t0 = Timestamp::now();
        engine.auto_switch_at(t0);
        assert_eq!(live_email(&demo), "busy@example.com");

        set_peaks(&engine, &[84.0, 95.0]);
        engine.auto_switch_at(t0);
        assert_eq!(live_email(&demo), "idle@example.com");

        set_peaks(&engine, &[95.0, 10.0]);
        engine.auto_switch_at(t0 + jiff::SignedDuration::from_secs(60));
        assert_eq!(live_email(&demo), "idle@example.com");

        engine.auto_switch_at(t0 + jiff::SignedDuration::from_secs(301));
        assert_eq!(live_email(&demo), "busy@example.com");
    }

    #[test]
    fn auto_switch_ignores_expired_windows() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        add(&engine, &demo, "idle@example.com");
        add(&engine, &demo, "busy@example.com");
        let now = Timestamp::now();
        engine
            .change(|saved| {
                saved.accounts[0].status = AccountStatus::Ready;
                saved.accounts[0].usage = Some(Usage {
                    windows: vec![UsageWindow {
                        id: "w".into(),
                        label: "w".into(),
                        used_percent: Some(10.0),
                        resets_at: None,
                        primary: true,
                        period_seconds: None,
                    }],
                    fetched_at: now,
                });
                saved.accounts[1].status = AccountStatus::Ready;
                saved.accounts[1].usage = Some(Usage {
                    windows: vec![UsageWindow {
                        id: "w".into(),
                        label: "w".into(),
                        used_percent: Some(95.0),
                        resets_at: Some(now - jiff::SignedDuration::from_secs(30)),
                        primary: true,
                        period_seconds: None,
                    }],
                    fetched_at: now,
                });
            })
            .unwrap();
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(50.0),
                ..SettingsUpdate::default()
            })
            .unwrap();

        engine.auto_switch_at(now);
        assert_eq!(live_email(&demo), "busy@example.com");
    }

    #[test]
    fn auto_switch_requires_recent_ready_usage() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        add(&engine, &demo, "idle@example.com");
        add(&engine, &demo, "busy@example.com");
        set_peaks(&engine, &[10.0, 95.0]);
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(50.0),
                ..SettingsUpdate::default()
            })
            .unwrap();
        let now = Timestamp::now();

        engine
            .change(|saved| {
                saved.accounts[1].status = AccountStatus::Stale { message: "offline".into() };
            })
            .unwrap();
        engine.auto_switch_at(now);
        assert_eq!(live_email(&demo), "busy@example.com");

        engine
            .change(|saved| {
                saved.accounts[1].status = AccountStatus::Ready;
                saved.accounts[1].usage.as_mut().unwrap().fetched_at = now - jiff::SignedDuration::from_mins(20);
            })
            .unwrap();
        engine.auto_switch_at(now);
        assert_eq!(live_email(&demo), "busy@example.com");

        engine
            .change(|saved| {
                saved.accounts[1].usage.as_mut().unwrap().fetched_at = now;
                saved.accounts[0].status = AccountStatus::Stale { message: "offline".into() };
            })
            .unwrap();
        engine.auto_switch_at(now);
        assert_eq!(live_email(&demo), "busy@example.com");

        engine
            .change(|saved| {
                saved.accounts[0].status = AccountStatus::Ready;
                saved.accounts[0].usage.as_mut().unwrap().fetched_at = now - jiff::SignedDuration::from_mins(20);
            })
            .unwrap();
        engine.auto_switch_at(now);
        assert_eq!(live_email(&demo), "busy@example.com");
    }

    #[test]
    fn auto_switch_falls_back_to_next_candidate_and_backs_off_failing_target() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        let (_, broken) = add(&engine, &demo, "broken@example.com");
        add(&engine, &demo, "second@example.com");
        add(&engine, &demo, "busy@example.com");
        set_peaks(&engine, &[10.0, 20.0, 95.0]);
        engine.vault.remove(&broken).unwrap();
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(50.0),
                ..SettingsUpdate::default()
            })
            .unwrap();

        let now = Timestamp::now();
        engine.auto_switch_at(now);

        assert_eq!(live_email(&demo), "second@example.com");
        assert!(engine.state().target_backoffs.get(&broken).is_some_and(|b| b.blocked(now)));
    }

    #[test]
    fn outgoing_failure_backs_off_provider_without_blocking_other_providers() {
        let p1 = Arc::new(Demo::new("p1", "P1"));
        let p2 = Arc::new(Demo::new("p2", "P2"));
        let engine = Engine::new(
            vec![p1.clone() as Arc<dyn Provider>, p2.clone() as Arc<dyn Provider>],
            Store::memory(),
            Vault::memory(),
        )
        .unwrap();
        let (p1_other, _) = add(&engine, &p1, "p1-idle@example.com");
        let (p1_busy, _) = add(&engine, &p1, "p1-busy@example.com");
        add(&engine, &p2, "p2-idle@example.com");
        add(&engine, &p2, "p2-busy@example.com");
        set_peaks(&engine, &[10.0, 95.0, 10.0, 95.0]);
        let crossed = Login { identity: p1_busy.identity, secret: p1.renew(&p1_other.secret).unwrap() };
        p1.set_live(Some(crossed));
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(50.0),
                ..SettingsUpdate::default()
            })
            .unwrap();

        let now = Timestamp::now();
        engine.auto_switch_at(now);

        assert_eq!(live_email(&p2), "p2-idle@example.com");
        assert!(engine.state().provider_backoffs.get("p1").is_some_and(|b| b.blocked(now)));
        assert!(engine.snapshot().warnings.iter().any(|warning| warning.contains("P1")));
    }

    struct ChangingPlanProvider {
        demo: Demo,
        plan: Mutex<String>,
        profile_calls: std::sync::atomic::AtomicUsize,
    }

    impl Provider for ChangingPlanProvider {
        fn id(&self) -> &'static str {
            self.demo.id()
        }

        fn name(&self) -> &'static str {
            self.demo.name()
        }

        fn live(&self) -> anyhow::Result<Option<Login>> {
            self.demo.live()
        }

        fn lock(&self) -> anyhow::Result<Box<dyn std::any::Any>> {
            self.demo.lock()
        }

        fn replace_live(&self, secret: &Secret) -> anyhow::Result<()> {
            self.demo.replace_live(secret)
        }

        fn profile(&self, secret: &Secret) -> Result<crate::provider::Profile, RemoteError> {
            self.profile_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut profile = self.demo.profile(secret)?;
            profile.plan = Some(self.plan.lock().unwrap().clone());
            Ok(profile)
        }

        fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError> {
            self.demo.usage(secret)
        }

        fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError> {
            self.demo.renew(secret)
        }

        fn expired(&self, secret: &Secret) -> bool {
            self.demo.expired(secret)
        }

        fn lineage(&self, secret: &Secret) -> Option<String> {
            self.demo.lineage(secret)
        }
    }

    #[test]
    fn plan_refreshes_daily_and_respects_rate_limit_backoff() {
        use std::sync::atomic::Ordering;

        let provider = Arc::new(ChangingPlanProvider {
            demo: Demo::new("claude", "Claude"),
            plan: Mutex::new("Pro".to_owned()),
            profile_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let engine =
            Engine::new(vec![provider.clone() as Arc<dyn Provider>], Store::memory(), Vault::memory()).unwrap();
        provider.demo.set_live(Some(provider.demo.login("ada@example.com", "Pro")));
        engine.add_account("claude").unwrap();
        let id = engine.snapshot().providers[0].accounts[0].id.clone();
        assert_eq!(provider.profile_calls.load(Ordering::SeqCst), 1);

        *provider.plan.lock().unwrap() = "Max 5x".to_owned();
        let now = Timestamp::now();
        engine
            .change(|saved| {
                saved.accounts[0].usage =
                    Some(Usage { windows: Vec::new(), fetched_at: now - jiff::SignedDuration::from_mins(5) });
            })
            .unwrap();
        engine.refresh();
        assert_eq!(provider.profile_calls.load(Ordering::SeqCst), 1);
        assert_eq!(engine.snapshot().providers[0].accounts[0].plan.as_deref(), Some("Pro"));

        engine
            .change(|saved| {
                saved.accounts[0].plan_checked_at = Some(now - jiff::SignedDuration::from_hours(25));
                saved.accounts[0].usage.as_mut().unwrap().fetched_at = now - jiff::SignedDuration::from_mins(5);
            })
            .unwrap();
        {
            let mut state = engine.state();
            let mut schedule = state.schedule(&state.saved.accounts[0]);
            schedule.limited_at = Some(now - jiff::SignedDuration::from_mins(10));
            state.schedules.insert(id.clone(), schedule);
        }
        engine.refresh();
        assert_eq!(provider.profile_calls.load(Ordering::SeqCst), 1);

        {
            let mut state = engine.state();
            let mut schedule = state.schedule(&state.saved.accounts[0]);
            schedule.limited_at = None;
            state.schedules.insert(id, schedule);
        }
        engine
            .change(|saved| {
                saved.accounts[0].usage.as_mut().unwrap().fetched_at = now - jiff::SignedDuration::from_mins(5);
            })
            .unwrap();
        engine.refresh();
        assert_eq!(provider.profile_calls.load(Ordering::SeqCst), 2);
        assert_eq!(engine.snapshot().providers[0].accounts[0].plan.as_deref(), Some("Max 5x"));
    }

    #[test]
    fn next_strategy_skips_exhausted_accounts_ignoring_expired_windows() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        let ids: Vec<String> = ["a@x", "b@x", "c@x"].iter().map(|email| add(&engine, &demo, email).1).collect();
        demo.set_live(Some(demo.login("a@x", "Pro")));
        engine.sync_live();
        let now = Timestamp::now();
        engine
            .change(|saved| {
                saved.accounts[1].usage = Some(Usage {
                    windows: vec![UsageWindow {
                        id: "w".into(),
                        label: "w".into(),
                        used_percent: Some(100.0),
                        resets_at: Some(now + jiff::SignedDuration::from_hours(1)),
                        primary: true,
                        period_seconds: None,
                    }],
                    fetched_at: now,
                });
                saved.accounts[2].usage = Some(Usage {
                    windows: vec![UsageWindow {
                        id: "w".into(),
                        label: "w".into(),
                        used_percent: Some(100.0),
                        resets_at: Some(now - jiff::SignedDuration::from_secs(10)),
                        primary: true,
                        period_seconds: None,
                    }],
                    fetched_at: now,
                });
            })
            .unwrap();

        assert_eq!(pick(&engine.state(), "claude", Strategy::Next, now), Some(ids[2].clone()));
    }

    #[test]
    fn finish_sign_in_does_not_deadlock_when_usage_is_unauthorized() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct UnauthThenOkProvider {
            demo: Demo,
            renew_calls: AtomicUsize,
            renewed: Mutex<bool>,
        }

        impl Provider for UnauthThenOkProvider {
            fn id(&self) -> &'static str {
                self.demo.id()
            }
            fn name(&self) -> &'static str {
                self.demo.name()
            }
            fn switchable(&self) -> bool {
                false
            }
            fn sign_in(&self) -> Option<&dyn crate::provider::OAuthSignIn> {
                self.demo.sign_in()
            }
            fn live(&self) -> anyhow::Result<Option<Login>> {
                Ok(None)
            }
            fn lock(&self) -> anyhow::Result<Box<dyn std::any::Any>> {
                self.demo.lock()
            }
            fn replace_live(&self, secret: &Secret) -> anyhow::Result<()> {
                self.demo.replace_live(secret)
            }
            fn profile(&self, secret: &Secret) -> Result<crate::provider::Profile, RemoteError> {
                self.demo.profile(secret)
            }
            fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError> {
                if !*self.renewed.lock().unwrap() {
                    return Err(RemoteError::Unauthorized);
                }
                self.demo.usage(secret)
            }
            fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError> {
                self.renew_calls.fetch_add(1, Ordering::SeqCst);
                *self.renewed.lock().unwrap() = true;
                self.demo.renew(secret)
            }
            fn expired(&self, _: &Secret) -> bool {
                false
            }
            fn lineage(&self, secret: &Secret) -> Option<String> {
                self.demo.lineage(secret)
            }
        }

        let provider = Arc::new(UnauthThenOkProvider {
            demo: Demo::usage_only("usage", "Usage"),
            renew_calls: AtomicUsize::new(0),
            renewed: Mutex::new(false),
        });
        let engine = Arc::new(engine_with_provider(provider.clone()));
        let sign_in = engine.begin_sign_in("usage", "http://127.0.0.1:1/callback").unwrap();

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let engine_bg = engine.clone();
        std::thread::spawn(move || {
            done_tx.send(engine_bg.finish_sign_in("usage", &sign_in.state, "sam")).ok();
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("finish_sign_in deadlocked")
            .expect("finish_sign_in succeeded");
        assert_eq!(provider.renew_calls.load(Ordering::SeqCst), 1);
        assert_eq!(engine.snapshot().providers[0].accounts[0].status, AccountStatus::Ready);
    }

    fn engine_with_provider(provider: Arc<dyn Provider>) -> Engine {
        Engine::new(vec![provider], Store::memory(), Vault::memory()).unwrap()
    }

    #[test]
    fn refresh_does_not_renew_live_account_and_serializes_with_activate() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct RaceProvider {
            demo: Demo,
            target_email: &'static str,
            on_target_usage: Mutex<Option<std::sync::mpsc::Sender<()>>>,
            activate_finished: Arc<AtomicBool>,
            overlap_detected: AtomicBool,
            renew_calls: AtomicUsize,
            switch_live_on_usage: AtomicBool,
            live_login: Mutex<Option<Login>>,
        }

        impl Provider for RaceProvider {
            fn id(&self) -> &'static str {
                self.demo.id()
            }
            fn name(&self) -> &'static str {
                self.demo.name()
            }
            fn live(&self) -> anyhow::Result<Option<Login>> {
                self.demo.live()
            }
            fn lock(&self) -> anyhow::Result<Box<dyn std::any::Any>> {
                self.demo.lock()
            }
            fn replace_live(&self, secret: &Secret) -> anyhow::Result<()> {
                self.demo.replace_live(secret)
            }
            fn profile(&self, secret: &Secret) -> Result<crate::provider::Profile, RemoteError> {
                self.demo.profile(secret)
            }
            fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError> {
                let is_target = self.demo.profile(secret).is_ok_and(|p| p.identity.email == self.target_email);
                if is_target {
                    if let Some(tx) = self.on_target_usage.lock().unwrap().take() {
                        tx.send(()).ok();
                        std::thread::sleep(Duration::from_millis(40));
                        if self.activate_finished.load(Ordering::SeqCst) {
                            self.overlap_detected.store(true, Ordering::SeqCst);
                        }
                    }
                    if self.switch_live_on_usage.load(Ordering::SeqCst)
                        && let Some(login) = self.live_login.lock().unwrap().clone()
                    {
                        self.demo.set_live(Some(login));
                    }
                    return Err(RemoteError::Unauthorized);
                }
                self.demo.usage(secret)
            }
            fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError> {
                self.renew_calls.fetch_add(1, Ordering::SeqCst);
                self.demo.renew(secret)
            }
            fn expired(&self, _: &Secret) -> bool {
                false
            }
            fn lineage(&self, secret: &Secret) -> Option<String> {
                self.demo.lineage(secret)
            }
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let activate_finished = Arc::new(AtomicBool::new(false));
        let provider = Arc::new(RaceProvider {
            demo: Demo::new("claude", "Claude"),
            target_email: "grace@example.com",
            on_target_usage: Mutex::new(Some(tx)),
            activate_finished: activate_finished.clone(),
            overlap_detected: AtomicBool::new(false),
            renew_calls: AtomicUsize::new(0),
            switch_live_on_usage: AtomicBool::new(false),
            live_login: Mutex::new(None),
        });
        let engine = Arc::new(engine_with_provider(provider.clone()));

        let grace_login = provider.demo.login("grace@example.com", "Pro");
        *provider.live_login.lock().unwrap() = Some(grace_login.clone());
        provider.demo.set_live(Some(grace_login));
        engine.add_account("claude").unwrap();
        let grace_id = engine.snapshot().providers[0].accounts[0].id.clone();

        provider.demo.set_live(Some(provider.demo.login("ada@example.com", "Pro")));
        engine.add_account("claude").unwrap();

        let engine_bg = engine.clone();
        let grace_for_bg = grace_id.clone();
        let handle = std::thread::spawn(move || {
            rx.recv().unwrap();
            engine_bg.activate(&grace_for_bg).unwrap();
            activate_finished.store(true, Ordering::SeqCst);
        });
        engine.refresh();
        handle.join().unwrap();

        assert!(!provider.overlap_detected.load(Ordering::SeqCst));
        assert_eq!(provider.renew_calls.load(Ordering::SeqCst), 1);

        // Now test external live takeover during an in-flight usage read with cached live login.
        let now = Timestamp::now();
        engine
            .change(|saved| {
                saved.accounts[0].usage = None;
                saved.accounts[0].status = AccountStatus::Pending;
                saved.accounts[1].usage =
                    Some(Usage { windows: Vec::new(), fetched_at: now - jiff::SignedDuration::from_mins(5) });
            })
            .unwrap();
        let ada_login = provider.demo.login("ada@example.com", "Pro");
        provider.demo.set_live(Some(ada_login));
        engine.sync_live();
        provider.switch_live_on_usage.store(true, Ordering::SeqCst);
        let expired_grace = provider.demo.login("grace@example.com", "Pro");
        engine.vault.put(&grace_id, &expired_grace.secret).unwrap();
        *provider.live_login.lock().unwrap() = Some(expired_grace);
        provider.renew_calls.store(0, Ordering::SeqCst);
        engine.state().schedules.remove(&grace_id);
        engine.poll();
        assert_eq!(provider.renew_calls.load(Ordering::SeqCst), 0);
        assert!(matches!(engine.account(&grace_id).unwrap().status, AccountStatus::Stale { .. }));
    }

    #[derive(Default)]
    struct Script {
        live_fails: AtomicBool,
        unauthorized: AtomicBool,
        no_plan: AtomicBool,
        renew_calls: AtomicUsize,
        on_usage: Mutex<Option<Box<dyn Fn() + Send>>>,
    }

    struct ScriptedProvider {
        demo: Demo,
        script: Script,
    }

    impl ScriptedProvider {
        fn new(id: &'static str) -> Arc<Self> {
            Arc::new(Self { demo: Demo::new(id, id), script: Script::default() })
        }

        fn on_usage(&self, hook: impl Fn() + Send + 'static) {
            *self.script.on_usage.lock().unwrap() = Some(Box::new(hook));
        }
    }

    impl Provider for ScriptedProvider {
        fn id(&self) -> &'static str {
            self.demo.id()
        }
        fn name(&self) -> &'static str {
            self.demo.name()
        }
        fn live(&self) -> anyhow::Result<Option<Login>> {
            anyhow::ensure!(!self.script.live_fails.load(Ordering::SeqCst), "unreadable");
            self.demo.live()
        }
        fn lock(&self) -> anyhow::Result<Box<dyn std::any::Any>> {
            self.demo.lock()
        }
        fn replace_live(&self, secret: &Secret) -> anyhow::Result<()> {
            self.demo.replace_live(secret)
        }
        fn profile(&self, secret: &Secret) -> Result<crate::provider::Profile, RemoteError> {
            let mut profile = self.demo.profile(secret)?;
            if self.script.no_plan.load(Ordering::SeqCst) {
                profile.plan = None;
            }
            Ok(profile)
        }
        fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError> {
            if let Some(hook) = &*self.script.on_usage.lock().unwrap() {
                hook();
            }
            if self.script.unauthorized.load(Ordering::SeqCst) {
                return Err(RemoteError::Unauthorized);
            }
            self.demo.usage(secret)
        }
        fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError> {
            self.script.renew_calls.fetch_add(1, Ordering::SeqCst);
            self.demo.renew(secret)
        }
        fn expired(&self, secret: &Secret) -> bool {
            self.script.unauthorized.load(Ordering::SeqCst) || self.demo.expired(secret)
        }
        fn lineage(&self, secret: &Secret) -> Option<String> {
            self.demo.lineage(secret)
        }
    }

    #[test]
    fn unreadable_live_login_blocks_renewal() {
        let provider = ScriptedProvider::new("claude");
        let engine = engine_with_provider(provider.clone());
        let (_, id) = add(&engine, &provider.demo, "ada@example.com");
        provider.script.live_fails.store(true, Ordering::SeqCst);
        provider.script.unauthorized.store(true, Ordering::SeqCst);

        engine.refresh();
        assert!(matches!(engine.account(&id).unwrap().status, AccountStatus::Stale { .. }));
        assert!(engine.activate(&id).is_err());
        assert_eq!(provider.script.renew_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn rejected_usage_does_not_renew_when_live_login_is_unreadable() {
        let provider = ScriptedProvider::new("claude");
        let engine = engine_with_provider(provider.clone());
        let (_, id) = add(&engine, &provider.demo, "ada@example.com");
        provider.script.live_fails.store(true, Ordering::SeqCst);
        provider.on_usage({
            let provider = Arc::downgrade(&provider);
            move || provider.upgrade().unwrap().script.unauthorized.store(true, Ordering::SeqCst)
        });

        engine.refresh();
        assert!(matches!(engine.account(&id).unwrap().status, AccountStatus::Stale { .. }));
        assert_eq!(provider.script.renew_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn refresh_does_not_restore_removed_account_secret() {
        let provider = ScriptedProvider::new("claude");
        let engine = engine_with_provider(provider.clone());
        let (_, id) = add(&engine, &provider.demo, "grace@example.com");
        add(&engine, &provider.demo, "ada@example.com");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        provider.on_usage({
            let provider = Arc::downgrade(&provider);
            move || {
                entered_tx.send(()).ok();
                release_rx.recv_timeout(Duration::from_millis(200)).ok();
                provider.upgrade().unwrap().script.unauthorized.store(true, Ordering::SeqCst);
            }
        });

        std::thread::scope(|scope| {
            scope.spawn(|| engine.refresh());
            entered_rx.recv().unwrap();
            engine.remove_account(&id).unwrap();
            release_tx.send(()).ok();
        });
        assert!(engine.vault.get(&id).unwrap().is_none());
        assert!(engine.account(&id).is_err());
    }

    #[test]
    fn a_profile_without_a_plan_keeps_the_known_plan() {
        let provider = ScriptedProvider::new("claude");
        let engine = engine_with_provider(provider.clone());
        let (_, id) = add(&engine, &provider.demo, "ada@example.com");
        provider.script.no_plan.store(true, Ordering::SeqCst);
        let checked_at = Timestamp::now() - jiff::SignedDuration::from_hours(25);
        engine.change(|saved| saved.accounts[0].plan_checked_at = Some(checked_at)).unwrap();

        engine.refresh();
        let refreshed = engine.account(&id).unwrap();
        assert_eq!(refreshed.plan.as_deref(), Some("Pro"));
        assert!(refreshed.plan_checked_at.unwrap() > checked_at);

        engine.add_account("claude").unwrap();
        assert_eq!(engine.account(&id).unwrap().plan.as_deref(), Some("Pro"));
    }

    #[test]
    fn activate_waits_for_one_account_rather_than_the_whole_refresh() {
        let provider = ScriptedProvider::new("claude");
        let engine = engine_with_provider(provider.clone());
        let (_, id) = add(&engine, &provider.demo, "a0@example.com");
        for index in 1..6 {
            add(&engine, &provider.demo, &format!("a{index}@example.com"));
        }
        let usage_calls = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        provider.on_usage({
            let usage_calls = usage_calls.clone();
            move || {
                usage_calls.fetch_add(1, Ordering::SeqCst);
                entered_tx.send(()).ok();
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        std::thread::scope(|scope| {
            scope.spawn(|| engine.refresh());
            entered_rx.recv().unwrap();
            engine.activate(&id).unwrap();
            assert!(usage_calls.load(Ordering::SeqCst) <= 2);
        });
        assert_eq!(usage_calls.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn a_busy_provider_does_not_delay_refreshing_another() {
        let slow = ScriptedProvider::new("slow");
        let other = ScriptedProvider::new("other");
        let engine = Engine::new(vec![slow.clone(), other.clone()], Store::memory(), Vault::memory()).unwrap();
        add(&engine, &slow.demo, "a@example.com");
        add(&engine, &other.demo, "b@example.com");
        let (refreshed_tx, refreshed_rx) = std::sync::mpsc::channel();
        other.on_usage(move || {
            refreshed_tx.send(()).ok();
        });

        std::thread::scope(|scope| {
            let busy = engine.work("slow");
            scope.spawn(|| engine.refresh());
            let refreshed = refreshed_rx.recv_timeout(Duration::from_secs(2));
            drop(busy);
            assert!(refreshed.is_ok());
        });
    }

    #[test]
    fn refresh_runs_providers_in_parallel() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct BarrierProvider {
            demo: Demo,
            in_flight: Arc<AtomicUsize>,
            both_entered: Arc<std::sync::atomic::AtomicBool>,
        }

        impl Provider for BarrierProvider {
            fn id(&self) -> &'static str {
                self.demo.id()
            }
            fn name(&self) -> &'static str {
                self.demo.name()
            }
            fn live(&self) -> anyhow::Result<Option<Login>> {
                self.demo.live()
            }
            fn lock(&self) -> anyhow::Result<Box<dyn std::any::Any>> {
                self.demo.lock()
            }
            fn replace_live(&self, secret: &Secret) -> anyhow::Result<()> {
                self.demo.replace_live(secret)
            }
            fn profile(&self, secret: &Secret) -> Result<crate::provider::Profile, RemoteError> {
                self.demo.profile(secret)
            }
            fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError> {
                if self.in_flight.fetch_add(1, Ordering::SeqCst) + 1 >= 2 {
                    self.both_entered.store(true, Ordering::SeqCst);
                }
                let start = Instant::now();
                while !self.both_entered.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(1) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                self.demo.usage(secret)
            }
            fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError> {
                self.demo.renew(secret)
            }
            fn expired(&self, secret: &Secret) -> bool {
                self.demo.expired(secret)
            }
            fn lineage(&self, secret: &Secret) -> Option<String> {
                self.demo.lineage(secret)
            }
        }

        let in_flight = Arc::new(AtomicUsize::new(0));
        let both_entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let p1 = Arc::new(BarrierProvider {
            demo: Demo::new("p1", "P1"),
            in_flight: in_flight.clone(),
            both_entered: both_entered.clone(),
        });
        let p2 =
            Arc::new(BarrierProvider { demo: Demo::new("p2", "P2"), in_flight, both_entered: both_entered.clone() });
        let engine = Engine::new(
            vec![p1.clone() as Arc<dyn Provider>, p2.clone() as Arc<dyn Provider>],
            Store::memory(),
            Vault::memory(),
        )
        .unwrap();
        p1.demo.set_live(Some(p1.demo.login("a@p1", "Pro")));
        engine.add_account("p1").unwrap();
        p2.demo.set_live(Some(p2.demo.login("a@p2", "Pro")));
        engine.add_account("p2").unwrap();

        engine.refresh();
        assert!(both_entered.load(Ordering::SeqCst));
    }

    #[test]
    fn tiny_reset_jitter_does_not_release_manual_hold() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        add(&engine, &demo, "idle@example.com");
        let (_, busy) = add(&engine, &demo, "busy@example.com");
        let now = Timestamp::now();
        let reset_at = now + jiff::SignedDuration::from_hours(1);
        let set_busy_reset = |resets_at: Timestamp| {
            engine
                .change(|saved| {
                    for (idx, peak) in [(0, 10.0), (1, 95.0)] {
                        saved.accounts[idx].status = AccountStatus::Ready;
                        saved.accounts[idx].usage = Some(Usage {
                            windows: vec![UsageWindow {
                                id: "w".into(),
                                label: "w".into(),
                                used_percent: Some(peak),
                                resets_at: Some(resets_at),
                                primary: true,
                                period_seconds: None,
                            }],
                            fetched_at: now,
                        });
                    }
                })
                .unwrap();
        };
        set_busy_reset(reset_at);
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(50.0),
                ..SettingsUpdate::default()
            })
            .unwrap();

        engine.activate(&busy).unwrap();
        set_busy_reset(reset_at + jiff::SignedDuration::from_secs(10));
        engine.auto_switch_at(now);
        assert_eq!(live_email(&demo), "busy@example.com");

        set_busy_reset(reset_at + jiff::SignedDuration::from_secs(120));
        engine.auto_switch_at(now);
        assert_eq!(live_email(&demo), "idle@example.com");
    }

    #[test]
    fn auto_switch_warning_clears_when_disabled_or_no_longer_needed() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        let (other, _) = add(&engine, &demo, "idle@example.com");
        let (busy, _) = add(&engine, &demo, "busy@example.com");
        set_peaks(&engine, &[10.0, 95.0]);
        let crossed = Login { identity: busy.identity, secret: demo.renew(&other.secret).unwrap() };
        demo.set_live(Some(crossed));
        engine
            .update_settings(SettingsUpdate {
                auto_switch: Some(true),
                threshold: Some(50.0),
                ..SettingsUpdate::default()
            })
            .unwrap();

        let t0 = Timestamp::now();
        engine.auto_switch_at(t0);
        assert!(!engine.snapshot().warnings.is_empty());

        engine.update_settings(SettingsUpdate { auto_switch: Some(false), ..SettingsUpdate::default() }).unwrap();
        assert!(engine.snapshot().warnings.is_empty());

        engine.update_settings(SettingsUpdate { auto_switch: Some(true), ..SettingsUpdate::default() }).unwrap();
        let t1 = t0 + jiff::SignedDuration::from_hours(1);
        engine
            .change(|saved| {
                saved.accounts[0].usage.as_mut().unwrap().fetched_at = t1;
                saved.accounts[1].usage.as_mut().unwrap().fetched_at = t1;
            })
            .unwrap();
        engine.auto_switch_at(t1);
        assert!(!engine.snapshot().warnings.is_empty());

        let t2 = t1 + jiff::SignedDuration::from_hours(1);
        engine
            .change(|saved| {
                saved.accounts[0].usage.as_mut().unwrap().fetched_at = t2;
                saved.accounts[1].usage.as_mut().unwrap().fetched_at = t2;
                saved.accounts[1].usage.as_mut().unwrap().windows[0].used_percent = Some(20.0);
            })
            .unwrap();
        engine.auto_switch_at(t2);
        assert!(engine.snapshot().warnings.is_empty());
    }

    #[test]
    fn accounts_without_plan_and_failed_profiles_are_throttled_daily() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct NoPlanProvider {
            demo: Demo,
            profile_calls: AtomicUsize,
            fail_profile: Mutex<bool>,
        }

        impl Provider for NoPlanProvider {
            fn id(&self) -> &'static str {
                self.demo.id()
            }
            fn name(&self) -> &'static str {
                self.demo.name()
            }
            fn live(&self) -> anyhow::Result<Option<Login>> {
                self.demo.live()
            }
            fn lock(&self) -> anyhow::Result<Box<dyn std::any::Any>> {
                self.demo.lock()
            }
            fn replace_live(&self, secret: &Secret) -> anyhow::Result<()> {
                self.demo.replace_live(secret)
            }
            fn profile(&self, secret: &Secret) -> Result<crate::provider::Profile, RemoteError> {
                self.profile_calls.fetch_add(1, Ordering::SeqCst);
                if *self.fail_profile.lock().unwrap() {
                    return Err(RemoteError::RateLimited(Some(Duration::from_secs(60))));
                }
                let mut profile = self.demo.profile(secret)?;
                profile.plan = None;
                Ok(profile)
            }
            fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError> {
                self.demo.usage(secret)
            }
            fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError> {
                self.demo.renew(secret)
            }
            fn expired(&self, secret: &Secret) -> bool {
                self.demo.expired(secret)
            }
            fn lineage(&self, secret: &Secret) -> Option<String> {
                self.demo.lineage(secret)
            }
        }

        let provider = Arc::new(NoPlanProvider {
            demo: Demo::new("claude", "Claude"),
            profile_calls: AtomicUsize::new(0),
            fail_profile: Mutex::new(false),
        });
        let engine = engine_with_provider(provider.clone());
        provider.demo.set_live(Some(provider.demo.login("ada@example.com", "Pro")));
        engine.add_account("claude").unwrap();
        assert_eq!(provider.profile_calls.load(Ordering::SeqCst), 1);

        let now = Timestamp::now();
        engine.refresh();
        assert_eq!(provider.profile_calls.load(Ordering::SeqCst), 1);
        engine
            .change(|saved| {
                saved.accounts[0].usage.as_mut().unwrap().fetched_at = now - jiff::SignedDuration::from_mins(5);
            })
            .unwrap();
        engine.refresh();
        assert_eq!(provider.profile_calls.load(Ordering::SeqCst), 1);

        // Even after a 25h-old failed check, do not retry profile immediately or back off usage polling.
        *provider.fail_profile.lock().unwrap() = true;
        engine
            .change(|saved| {
                saved.accounts[0].plan_checked_at = Some(now - jiff::SignedDuration::from_hours(25));
                saved.accounts[0].usage =
                    Some(Usage { windows: Vec::new(), fetched_at: now - jiff::SignedDuration::from_mins(5) });
            })
            .unwrap();
        engine.refresh();
        assert_eq!(provider.profile_calls.load(Ordering::SeqCst), 2);
        assert_eq!(engine.snapshot().providers[0].accounts[0].status, AccountStatus::Ready);
        let id = engine.snapshot().providers[0].accounts[0].id.clone();
        assert_eq!(engine.state().schedules[&id].failures, 0);

        engine
            .change(|saved| {
                saved.accounts[0].usage.as_mut().unwrap().fetched_at = now - jiff::SignedDuration::from_mins(5);
            })
            .unwrap();
        engine.refresh();
        assert_eq!(provider.profile_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn best_strategy_ignores_expired_windows() {
        let demo = Arc::new(Demo::new("claude", "Claude"));
        let engine = engine_with(&demo);
        let ids: Vec<String> = ["a@x", "b@x", "c@x", "d@x"].iter().map(|email| add(&engine, &demo, email).1).collect();
        demo.set_live(Some(demo.login("a@x", "Pro")));
        engine.sync_live();
        let now = Timestamp::now();
        engine
            .change(|saved| {
                saved.accounts[1].usage = Some(Usage {
                    windows: vec![
                        UsageWindow {
                            id: "expired".into(),
                            label: "Session".into(),
                            used_percent: Some(95.0),
                            resets_at: Some(now - jiff::SignedDuration::from_secs(10)),
                            primary: true,
                            period_seconds: None,
                        },
                        UsageWindow {
                            id: "current".into(),
                            label: "Weekly".into(),
                            used_percent: Some(10.0),
                            resets_at: Some(now + jiff::SignedDuration::from_hours(2)),
                            primary: true,
                            period_seconds: None,
                        },
                    ],
                    fetched_at: now,
                });
                saved.accounts[2].usage = Some(Usage {
                    windows: vec![UsageWindow {
                        id: "current".into(),
                        label: "Weekly".into(),
                        used_percent: Some(40.0),
                        resets_at: Some(now + jiff::SignedDuration::from_hours(2)),
                        primary: true,
                        period_seconds: None,
                    }],
                    fetched_at: now,
                });
                saved.accounts[3].usage = Some(Usage {
                    windows: vec![UsageWindow {
                        id: "expired".into(),
                        label: "Session".into(),
                        used_percent: Some(5.0),
                        resets_at: Some(now - jiff::SignedDuration::from_secs(10)),
                        primary: true,
                        period_seconds: None,
                    }],
                    fetched_at: now,
                });
            })
            .unwrap();

        assert_eq!(pick(&engine.state(), "claude", Strategy::Best, now), Some(ids[1].clone()));
    }

    #[test]
    fn reset_grants_throttle_and_redeem_handles_live_saved_and_unsettled_claims() {
        use crate::provider::ResetGrants;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use switch_protocol::LimitReset;

        struct ResetProvider {
            demo: Demo,
            resets_calls: AtomicUsize,
            renew_calls: AtomicUsize,
            redeem_request_ids: Mutex<Vec<String>>,
            fail_resets_once: AtomicBool,
            redeem_unsettled_once: AtomicBool,
            saved_unauthorized: AtomicBool,
            live_unauthorized: AtomicBool,
            next_grant_error: AtomicBool,
        }

        impl Provider for ResetProvider {
            fn id(&self) -> &'static str {
                self.demo.id()
            }
            fn name(&self) -> &'static str {
                self.demo.name()
            }
            fn live(&self) -> anyhow::Result<Option<Login>> {
                self.demo.live()
            }
            fn lock(&self) -> anyhow::Result<Box<dyn std::any::Any>> {
                self.demo.lock()
            }
            fn replace_live(&self, secret: &Secret) -> anyhow::Result<()> {
                self.demo.replace_live(secret)
            }
            fn profile(&self, secret: &Secret) -> Result<crate::provider::Profile, RemoteError> {
                self.demo.profile(secret)
            }
            fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError> {
                self.demo.usage(secret)
            }
            fn supports_resets(&self) -> bool {
                true
            }
            fn resets(&self, secret: &Secret) -> Result<ResetGrants, RemoteError> {
                self.resets_calls.fetch_add(1, Ordering::SeqCst);
                if self.fail_resets_once.swap(false, Ordering::SeqCst) {
                    return Err(RemoteError::Other("offline".to_owned()));
                }
                self.demo.resets(secret)
            }
            fn redeem_reset(
                &self,
                secret: &Secret,
                identity: &Identity,
                grant_id: &str,
                request_id: &str,
            ) -> Result<RedeemOutcome, RedeemError> {
                self.redeem_request_ids.lock().unwrap().push(request_id.to_owned());
                if self.redeem_unsettled_once.swap(false, Ordering::SeqCst) {
                    return Err(RedeemError::Unsettled("timeout".to_owned()));
                }
                if self.live_unauthorized.load(Ordering::SeqCst) {
                    return Err(RedeemError::Remote(RemoteError::Unauthorized));
                }
                if self.saved_unauthorized.load(Ordering::SeqCst) {
                    return Err(RedeemError::Remote(RemoteError::Unauthorized));
                }
                if self.next_grant_error.swap(false, Ordering::SeqCst) {
                    return Err(RedeemError::NotNextGrant);
                }
                self.demo.redeem_reset(secret, identity, grant_id, request_id)
            }
            fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError> {
                self.renew_calls.fetch_add(1, Ordering::SeqCst);
                self.saved_unauthorized.store(false, Ordering::SeqCst);
                self.demo.renew(secret)
            }
            fn expired(&self, _: &Secret) -> bool {
                false
            }
            fn lineage(&self, secret: &Secret) -> Option<String> {
                self.demo.lineage(secret)
            }
        }

        let provider = Arc::new(ResetProvider {
            demo: Demo::new("claude", "Claude"),
            resets_calls: AtomicUsize::new(0),
            renew_calls: AtomicUsize::new(0),
            redeem_request_ids: Mutex::new(Vec::new()),
            fail_resets_once: AtomicBool::new(true),
            redeem_unsettled_once: AtomicBool::new(false),
            saved_unauthorized: AtomicBool::new(false),
            live_unauthorized: AtomicBool::new(false),
            next_grant_error: AtomicBool::new(false),
        });
        let now = Timestamp::now();
        let future = Some(now + jiff::SignedDuration::from_hours(48));
        provider.demo.set_resets(
            "live@example.com",
            ResetGrants {
                resets: vec![
                    LimitReset {
                        id: "full".to_owned(),
                        label: "Full Reset".to_owned(),
                        remaining: 1,
                        expires_at: future,
                        usable: true,
                        detail: None,
                    },
                    LimitReset {
                        id: "fable".to_owned(),
                        label: "Fable Reset".to_owned(),
                        remaining: 2,
                        expires_at: future,
                        usable: true,
                        detail: None,
                    },
                ],
                next_grant_id: Some("full".to_owned()),
            },
        );
        provider.demo.set_resets(
            "saved@example.com",
            ResetGrants {
                resets: vec![LimitReset {
                    id: "weekly".to_owned(),
                    label: "Weekly Reset".to_owned(),
                    remaining: 1,
                    expires_at: future,
                    usable: true,
                    detail: None,
                }],
                next_grant_id: Some("weekly".to_owned()),
            },
        );

        let engine = engine_with_provider(provider.clone());
        let (_, saved_id) = add(&engine, &provider.demo, "saved@example.com");
        let (_, live_id) = add(&engine, &provider.demo, "live@example.com");

        // Initial resets failure still records resets_checked_at so polls do not hammer the endpoint.
        engine.refresh();
        assert_eq!(provider.resets_calls.load(Ordering::SeqCst), 2);
        assert!(engine.account(&saved_id).unwrap().resets_checked_at.is_some());
        assert!(engine.account(&live_id).unwrap().resets_checked_at.is_some());

        engine
            .change(|saved| {
                for account in &mut saved.accounts {
                    if let Some(usage) = &mut account.usage {
                        usage.fetched_at = now - jiff::SignedDuration::from_mins(5);
                    }
                }
            })
            .unwrap();
        engine.refresh();
        assert_eq!(provider.resets_calls.load(Ordering::SeqCst), 2);

        engine
            .change(|saved| {
                for account in &mut saved.accounts {
                    if account.id == saved_id {
                        account.resets_checked_at = Some(now - jiff::SignedDuration::from_mins(16));
                    }
                    if let Some(usage) = &mut account.usage {
                        usage.fetched_at = now - jiff::SignedDuration::from_mins(5);
                    }
                }
            })
            .unwrap();
        engine.refresh();
        assert_eq!(provider.resets_calls.load(Ordering::SeqCst), 3);
        assert_eq!(engine.account(&saved_id).unwrap().resets.len(), 1);
        assert_eq!(engine.account(&live_id).unwrap().resets.len(), 2);

        // Live account unauthorized never renews.
        provider.live_unauthorized.store(true, Ordering::SeqCst);
        assert!(matches!(engine.redeem_reset(&live_id, "full"), Err(Error::Invalid(_))));
        assert_eq!(provider.renew_calls.load(Ordering::SeqCst), 0);
        provider.live_unauthorized.store(false, Ordering::SeqCst);

        // NotNextGrant names the preferred grant label.
        provider.next_grant_error.store(true, Ordering::SeqCst);
        match engine.redeem_reset(&live_id, "fable") {
            Err(Error::Invalid(message)) => assert!(message.contains("Full Reset"), "{message}"),
            other => panic!("unexpected result: {other:?}"),
        }

        // Unsettled claim reuses the exact same request_id on retry, then succeeds and refreshes usage.
        provider.redeem_request_ids.lock().unwrap().clear();
        provider.redeem_unsettled_once.store(true, Ordering::SeqCst);
        assert!(matches!(engine.redeem_reset(&live_id, "full"), Err(Error::Failed(_))));
        engine.redeem_reset(&live_id, "full").unwrap();
        let reqs = provider.redeem_request_ids.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0], reqs[1]);
        assert_eq!(engine.account(&live_id).unwrap().resets.len(), 1);
        assert_eq!(engine.account(&live_id).unwrap().usage.unwrap().windows[0].used_percent, Some(0.0));

        // Saved account renews once on Unauthorized and persists the rotated token to vault.
        let vault_before = engine.vault.get(&saved_id).unwrap().unwrap();
        provider.saved_unauthorized.store(true, Ordering::SeqCst);
        engine.redeem_reset(&saved_id, "weekly").unwrap();
        assert_eq!(provider.renew_calls.load(Ordering::SeqCst), 1);
        assert_ne!(engine.vault.get(&saved_id).unwrap().unwrap(), vault_before);
        assert!(engine.account(&saved_id).unwrap().resets.is_empty());
    }

    #[test]
    fn unsettled_reset_claim_survives_restart_and_already_used_refreshes_grants() {
        use crate::provider::ResetGrants;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use switch_protocol::LimitReset;

        struct RestartProvider {
            demo: Demo,
            resets_calls: AtomicUsize,
            redeem_request_ids: Mutex<Vec<String>>,
            unsettled_once: AtomicBool,
            return_already_used: AtomicBool,
        }

        impl Provider for RestartProvider {
            fn id(&self) -> &'static str {
                self.demo.id()
            }
            fn name(&self) -> &'static str {
                self.demo.name()
            }
            fn live(&self) -> anyhow::Result<Option<Login>> {
                self.demo.live()
            }
            fn lock(&self) -> anyhow::Result<Box<dyn std::any::Any>> {
                self.demo.lock()
            }
            fn replace_live(&self, secret: &Secret) -> anyhow::Result<()> {
                self.demo.replace_live(secret)
            }
            fn profile(&self, secret: &Secret) -> Result<crate::provider::Profile, RemoteError> {
                self.demo.profile(secret)
            }
            fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError> {
                self.demo.usage(secret)
            }
            fn supports_resets(&self) -> bool {
                true
            }
            fn resets(&self, secret: &Secret) -> Result<ResetGrants, RemoteError> {
                self.resets_calls.fetch_add(1, Ordering::SeqCst);
                self.demo.resets(secret)
            }
            fn redeem_reset(
                &self,
                _secret: &Secret,
                _identity: &Identity,
                grant_id: &str,
                request_id: &str,
            ) -> Result<RedeemOutcome, RedeemError> {
                self.redeem_request_ids.lock().unwrap().push(request_id.to_owned());
                if self.unsettled_once.swap(false, Ordering::SeqCst) {
                    return Err(RedeemError::Unsettled("timeout".to_owned()));
                }
                if self.return_already_used.load(Ordering::SeqCst) {
                    self.demo.set_resets(
                        "ada@example.com",
                        ResetGrants {
                            resets: vec![LimitReset {
                                id: grant_id.to_owned(),
                                label: "Weekly Reset".to_owned(),
                                remaining: 1,
                                expires_at: None,
                                usable: true,
                                detail: None,
                            }],
                            next_grant_id: Some(grant_id.to_owned()),
                        },
                    );
                    return Ok(RedeemOutcome { remaining: None });
                }
                Ok(RedeemOutcome { remaining: Some(0) })
            }
            fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError> {
                self.demo.renew(secret)
            }
            fn expired(&self, _: &Secret) -> bool {
                false
            }
            fn lineage(&self, secret: &Secret) -> Option<String> {
                self.demo.lineage(secret)
            }
        }

        let home = tempfile::tempdir().unwrap();
        let store_path = home.path().join("accounts.json");
        let vault_dir = home.path().join("logins");
        let provider = Arc::new(RestartProvider {
            demo: Demo::new("claude", "Claude"),
            resets_calls: AtomicUsize::new(0),
            redeem_request_ids: Mutex::new(Vec::new()),
            unsettled_once: AtomicBool::new(true),
            return_already_used: AtomicBool::new(true),
        });
        provider.demo.set_resets(
            "ada@example.com",
            ResetGrants {
                resets: vec![LimitReset {
                    id: "weekly".to_owned(),
                    label: "Weekly Reset".to_owned(),
                    remaining: 3,
                    expires_at: None,
                    usable: true,
                    detail: None,
                }],
                next_grant_id: Some("weekly".to_owned()),
            },
        );

        let engine = Engine::new(
            vec![provider.clone() as Arc<dyn Provider>],
            Store::file(&store_path),
            Vault::files(&vault_dir),
        )
        .unwrap();
        let (_, account_id) = add(&engine, &provider.demo, "ada@example.com");
        engine.refresh();
        assert_eq!(provider.resets_calls.load(Ordering::SeqCst), 1);

        assert!(matches!(engine.redeem_reset(&account_id, "weekly"), Err(Error::Failed(_))));
        assert!(engine.account(&account_id).unwrap().pending_reset_claims.contains_key("weekly"));
        drop(engine);

        let reopened = Engine::new(
            vec![provider.clone() as Arc<dyn Provider>],
            Store::file(&store_path),
            Vault::files(&vault_dir),
        )
        .unwrap();
        reopened.redeem_reset(&account_id, "weekly").unwrap();

        let reqs = provider.redeem_request_ids.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0], reqs[1]);
        let updated = reopened.account(&account_id).unwrap();
        assert!(updated.pending_reset_claims.is_empty());
        assert_eq!(provider.resets_calls.load(Ordering::SeqCst), 2);
        assert_eq!(updated.resets.len(), 1);
        assert_eq!(updated.resets[0].remaining, 1);
    }

    #[test]
    fn renewed_live_login_clears_failure_backoff() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct LiveTokenProvider {
            demo: Demo,
            valid_access: Mutex<String>,
            renew_calls: AtomicUsize,
        }

        impl Provider for LiveTokenProvider {
            fn id(&self) -> &'static str {
                self.demo.id()
            }
            fn name(&self) -> &'static str {
                self.demo.name()
            }
            fn live(&self) -> anyhow::Result<Option<Login>> {
                self.demo.live()
            }
            fn lock(&self) -> anyhow::Result<Box<dyn std::any::Any>> {
                self.demo.lock()
            }
            fn replace_live(&self, secret: &Secret) -> anyhow::Result<()> {
                self.demo.replace_live(secret)
            }
            fn profile(&self, _secret: &Secret) -> Result<crate::provider::Profile, RemoteError> {
                Ok(crate::provider::Profile {
                    identity: Identity {
                        account_id: "ada".into(),
                        email: "ada@example.com".into(),
                        organization_id: None,
                        organization: None,
                    },
                    plan: Some("Pro".to_owned()),
                })
            }
            fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError> {
                let expected = self.valid_access.lock().unwrap().clone();
                let parsed: serde_json::Value = serde_json::from_str(secret.expose()).unwrap();
                if parsed["access"].as_str() != Some(expected.as_str()) {
                    return Err(RemoteError::Unauthorized);
                }
                Ok(vec![UsageWindow {
                    id: "session".into(),
                    label: "Session".into(),
                    used_percent: Some(10.0),
                    resets_at: None,
                    primary: true,
                    period_seconds: None,
                }])
            }
            fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError> {
                self.renew_calls.fetch_add(1, Ordering::SeqCst);
                self.demo.renew(secret)
            }
            fn expired(&self, _: &Secret) -> bool {
                false
            }
            fn lineage(&self, secret: &Secret) -> Option<String> {
                let parsed: serde_json::Value = serde_json::from_str(secret.expose()).ok()?;
                parsed["refresh"].as_str().map(crate::provider::digest)
            }
        }

        let make_login = |access: &str| Login {
            identity: Identity {
                account_id: "ada".into(),
                email: "ada@example.com".into(),
                organization_id: None,
                organization: None,
            },
            secret: Secret::new(serde_json::json!({ "access": access, "refresh": "shared-rt" }).to_string()),
        };

        let provider = Arc::new(LiveTokenProvider {
            demo: Demo::new("claude", "Claude"),
            valid_access: Mutex::new("at-1".to_owned()),
            renew_calls: AtomicUsize::new(0),
        });
        let engine = engine_with_provider(provider.clone());
        provider.demo.set_live(Some(make_login("at-1")));
        engine.add_account("claude").unwrap();
        let id = engine.snapshot().providers[0].accounts[0].id.clone();

        *provider.valid_access.lock().unwrap() = "at-2".to_owned();
        engine.poll();
        assert!(matches!(engine.account(&id).unwrap().status, AccountStatus::Stale { .. }));
        assert_eq!(engine.state().schedules[&id].failures, 1);

        engine.refresh();
        assert!(matches!(engine.account(&id).unwrap().status, AccountStatus::Stale { .. }));

        provider.demo.set_live(Some(make_login("at-2")));
        engine.sync_live();
        assert_eq!(engine.state().schedules[&id].failures, 0);
        engine.poll();
        assert_eq!(engine.account(&id).unwrap().status, AccountStatus::Ready);
        assert_eq!(provider.renew_calls.load(Ordering::SeqCst), 0);
    }
}
