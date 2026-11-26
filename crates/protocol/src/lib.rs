use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ServerInfo {
    pub version: String,
    pub pid: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Snapshot {
    pub providers: Vec<ProviderState>,
    pub settings: Settings,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProviderState {
    pub id: String,
    pub name: String,
    pub accounts: Vec<Account>,
    /// The live login when it has not been saved as an account yet.
    pub unsaved_login: Option<Identity>,
    /// Switch can make a saved account the live login. Otherwise it only reads usage.
    #[serde(default = "yes")]
    pub switchable: bool,
    /// Accounts are added by signing in through Switch rather than from the live login.
    #[serde(default)]
    pub sign_in: bool,
    /// When Switch last automatically switched this provider's active account.
    #[serde(default)]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_auto_switch_at: Option<Timestamp>,
}

fn yes() -> bool {
    true
}

impl ProviderState {
    pub fn active(&self) -> Option<&Account> {
        self.accounts.iter().find(|account| account.active)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Identity {
    pub account_id: String,
    pub email: String,
    pub organization_id: Option<String>,
    pub organization: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Account {
    pub id: String,
    pub identity: Identity,
    pub alias: Option<String>,
    pub plan: Option<String>,
    pub active: bool,
    /// Disabled accounts are skipped by automatic and strategy switching.
    pub enabled: bool,
    pub status: AccountStatus,
    pub usage: Option<Usage>,
    #[serde(default)]
    pub resets: Vec<LimitReset>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct LimitReset {
    pub id: String,
    pub label: String,
    pub remaining: u32,
    #[serde(default)]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub expires_at: Option<Timestamp>,
    #[serde(default)]
    pub usable: bool,
    #[serde(default)]
    pub detail: Option<String>,
}

impl LimitReset {
    pub fn available(&self, now: Timestamp) -> bool {
        self.remaining > 0 && self.expires_at.is_none_or(|at| at > now)
    }
}

impl Account {
    pub fn label(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.identity.email)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AccountStatus {
    Pending,
    Ready,
    /// The last refresh failed, so `usage` may be out of date.
    Stale {
        message: String,
    },
    /// The saved login was rejected and has to be added again.
    NeedsLogin,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Usage {
    pub windows: Vec<UsageWindow>,
    #[schema(value_type = String, format = DateTime)]
    pub fetched_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UsageWindow {
    pub id: String,
    pub label: String,
    /// `None` when the provider reports the window without a measurement.
    pub used_percent: Option<f64>,
    #[schema(value_type = Option<String>, format = DateTime)]
    pub resets_at: Option<Timestamp>,
    /// Account-wide windows, shown in compact places such as the menu bar.
    pub primary: bool,
    /// Length of a rolling window, used to judge the pace of usage. `None` when unknown.
    #[serde(default)]
    pub period_seconds: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Settings {
    pub auto_switch: bool,
    /// Used percentage of any primary window at which auto-switch moves on.
    pub threshold: f64,
    pub cooldown_seconds: u64,
    pub strategy: Strategy,
}

impl Default for Settings {
    fn default() -> Self {
        Self { auto_switch: false, threshold: 90.0, cooldown_seconds: 300, strategy: Strategy::Best }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SettingsUpdate {
    pub auto_switch: Option<bool>,
    pub threshold: Option<f64>,
    #[serde(default)]
    pub cooldown_seconds: Option<u64>,
    #[serde(default)]
    pub strategy: Option<Strategy>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AccountUpdate {
    /// An empty alias clears it.
    pub alias: Option<String>,
    pub enabled: Option<bool>,
}

/// Display order by id. Listed items trade places among themselves; the rest stay where they are.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct OrderUpdate {
    pub providers: Option<Vec<String>>,
    pub accounts: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Next,
    Best,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct StrategySwitch {
    pub strategy: Strategy,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RedeemReset {
    pub grant_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SignInStart {
    /// Where the browser is sent back to with the authorization code, such as `http://127.0.0.1:51121/oauth-callback`.
    pub redirect_uri: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SignIn {
    /// The page to open in a browser.
    pub url: String,
    /// Comes back with the code and identifies this sign-in when finishing it.
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SignInFinish {
    pub state: String,
    pub code: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "server.connected")]
    Connected { snapshot: Snapshot },
    #[serde(rename = "snapshot.updated")]
    Updated { snapshot: Snapshot },
}

impl Event {
    pub fn into_snapshot(self) -> Snapshot {
        match self {
            Self::Connected { snapshot } | Self::Updated { snapshot } => snapshot,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ErrorBody {
    pub error: String,
}

/// How a running server is found by clients on the same machine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Registration {
    pub url: String,
    pub version: String,
    pub pid: u32,
    pub password: String,
    /// Started on purpose, for example by systemd, rather than by a client. Clients use it as it is.
    #[serde(default)]
    pub managed: bool,
}
