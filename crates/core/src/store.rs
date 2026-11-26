use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use switch_protocol::{Account, AccountStatus, Identity, LimitReset, Settings, Usage};

use crate::files::{read_optional, write_atomic};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SavedAccount {
    pub id: String,
    pub provider: String,
    pub identity: Identity,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub plan_checked_at: Option<Timestamp>,
    pub enabled: bool,
    pub status: AccountStatus,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub resets: Vec<LimitReset>,
    #[serde(default)]
    pub resets_checked_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pending_reset_claims: BTreeMap<String, String>,
}

impl SavedAccount {
    pub fn view(&self, active: bool) -> Account {
        Account {
            id: self.id.clone(),
            identity: self.identity.clone(),
            alias: self.alias.clone(),
            plan: self.plan.clone(),
            active,
            enabled: self.enabled,
            status: self.status.clone(),
            usage: self.usage.clone(),
            resets: self.resets.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ManualHold {
    pub account_id: String,
    #[serde(default)]
    pub over_threshold_resets: BTreeMap<String, Option<Timestamp>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Saved {
    #[serde(default)]
    pub accounts: Vec<SavedAccount>,
    #[serde(default)]
    pub provider_order: Vec<String>,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub manual_holds: BTreeMap<String, ManualHold>,
    #[serde(default)]
    pub last_live: BTreeMap<String, String>,
    #[serde(default)]
    pub last_auto_switch: BTreeMap<String, Timestamp>,
}

/// Accounts and settings on disk. Holds no secrets.
pub struct Store {
    path: Option<PathBuf>,
}

impl Store {
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self { path: Some(path.into()) }
    }

    pub fn memory() -> Self {
        Self { path: None }
    }

    pub(crate) fn load(&self) -> Result<Saved> {
        let Some(path) = &self.path else { return Ok(Saved::default()) };
        match read_optional(path)? {
            Some(text) => serde_json::from_str(&text).with_context(|| format!("could not parse {}", path.display())),
            None => Ok(Saved::default()),
        }
    }

    pub(crate) fn save(&self, saved: &Saved) -> Result<()> {
        match &self.path {
            Some(path) => write_atomic(path, &serde_json::to_string_pretty(saved)?),
            None => Ok(()),
        }
    }
}
