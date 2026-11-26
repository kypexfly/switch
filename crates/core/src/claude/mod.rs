mod api;
mod live;
mod locks;

use std::any::Any;

use anyhow::Result;
use jiff::Timestamp;
use serde_json::{Value, json};
use switch_protocol::{Identity, UsageWindow};

use self::api::Api;
use self::live::{LiveStore, pack, unpack};
use crate::provider::{Login, Profile, Provider, RedeemError, RedeemOutcome, RemoteError, ResetGrants, Secret, digest};

const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

pub struct Claude {
    store: LiveStore,
    api: Api,
}

impl Claude {
    pub fn for_user() -> Result<Self> {
        Ok(Self { store: LiveStore::for_user()?, api: Api::new() })
    }

    /// A Claude Code home directory that never touches the Keychain.
    pub fn at(home: &std::path::Path) -> Self {
        Self { store: LiveStore::at(home, None), api: Api::new() }
    }
}

impl Provider for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn name(&self) -> &'static str {
        "Claude"
    }

    fn live(&self) -> Result<Option<Login>> {
        self.store.read()
    }

    fn lock(&self) -> Result<Box<dyn Any>> {
        Ok(Box::new(self.store.lock()?))
    }

    fn replace_live(&self, secret: &Secret) -> Result<()> {
        self.store.replace(secret)
    }

    fn profile(&self, secret: &Secret) -> Result<Profile, RemoteError> {
        self.api.profile(&access_token(secret)?)
    }

    fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError> {
        self.api.usage(&access_token(secret)?)
    }

    fn supports_resets(&self) -> bool {
        true
    }

    fn resets(&self, secret: &Secret) -> Result<ResetGrants, RemoteError> {
        self.api.reset_grants(&access_token(secret)?)
    }

    fn redeem_reset(
        &self,
        secret: &Secret,
        identity: &Identity,
        grant_id: &str,
        request_id: &str,
    ) -> Result<RedeemOutcome, RedeemError> {
        let token = access_token(secret)?;
        let org_id = match identity.organization_id.as_deref().filter(|id| !id.is_empty()) {
            Some(org_id) => org_id.to_owned(),
            None => self
                .api
                .profile(&token)?
                .identity
                .organization_id
                .filter(|id| !id.is_empty())
                .ok_or_else(|| RedeemError::Refused("This Claude account has no organization id.".to_owned()))?,
        };
        self.api.redeem_reset(&token, &org_id, grant_id, request_id)
    }

    fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError> {
        let (mut login, account) = unpack(secret).map_err(|_| RemoteError::Rejected)?;
        let oauth = login.get_mut("claudeAiOauth").ok_or(RemoteError::Rejected)?;
        let refresh_token = oauth["refreshToken"].as_str().filter(|token| !token.is_empty());
        let tokens = self.api.refresh(refresh_token.ok_or(RemoteError::Rejected)?)?;
        oauth["accessToken"] = json!(tokens.access);
        oauth["expiresAt"] = json!(tokens.expires_at_ms);
        if let Some(refresh) = tokens.refresh {
            oauth["refreshToken"] = json!(refresh);
        }
        if let Some(scopes) = tokens.scopes {
            oauth["scopes"] = json!(scopes);
        }
        Ok(pack(login, account))
    }

    fn expired(&self, secret: &Secret) -> bool {
        oauth(secret)
            .and_then(|oauth| oauth["expiresAt"].as_f64())
            .is_some_and(|at| at as i64 <= Timestamp::now().as_millisecond() + EXPIRY_MARGIN_MS)
    }

    fn lineage(&self, secret: &Secret) -> Option<String> {
        let oauth = oauth(secret)?;
        ["refreshToken", "accessToken"]
            .iter()
            .find_map(|key| oauth[key].as_str().filter(|token| !token.is_empty()))
            .map(digest)
    }
}

fn oauth(secret: &Secret) -> Option<Value> {
    unpack(secret).ok()?.0.remove("claudeAiOauth")
}

fn access_token(secret: &Secret) -> Result<String, RemoteError> {
    oauth(secret)
        .and_then(|oauth| oauth["accessToken"].as_str().filter(|token| !token.is_empty()).map(str::to_owned))
        .ok_or(RemoteError::Unauthorized)
}
