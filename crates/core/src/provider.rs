use std::any::Any;
use std::fmt;
use std::time::Duration;

use anyhow::Result;
use sha2::{Digest, Sha256};
use switch_protocol::{Identity, LimitReset, UsageWindow};

/// Everything a provider needs to restore a login, in a format only that provider reads.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(..)")
    }
}

#[derive(Clone, Debug)]
pub struct Login {
    /// As recorded locally. Only `Provider::profile` proves who the secret belongs to.
    pub identity: Identity,
    pub secret: Secret,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub identity: Identity,
    pub plan: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("the access token was not accepted")]
    Unauthorized,
    #[error("the saved login is no longer valid")]
    Rejected,
    #[error("too many requests")]
    RateLimited(Option<Duration>),
    #[error("{0}")]
    Other(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResetGrants {
    pub resets: Vec<LimitReset>,
    pub next_grant_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RedeemOutcome {
    pub remaining: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum RedeemError {
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Exhausted(String),
    #[error("another limit reset has to be used first")]
    NotNextGrant,
    #[error("{0}")]
    Unsettled(String),
}

pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;

    /// The login the provider's own tools use right now. Never touches the network.
    fn live(&self) -> Result<Option<Login>>;

    /// Keeps the provider's own tools from changing the live login until the guard is dropped.
    fn lock(&self) -> Result<Box<dyn Any>>;

    /// Makes `secret` the live login, or leaves the live login as it was. Call with the lock held.
    fn replace_live(&self, secret: &Secret) -> Result<()>;

    fn profile(&self, secret: &Secret) -> Result<Profile, RemoteError>;
    fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError>;

    fn supports_resets(&self) -> bool {
        false
    }

    fn resets(&self, _secret: &Secret) -> Result<ResetGrants, RemoteError> {
        Ok(ResetGrants::default())
    }

    fn redeem_reset(
        &self,
        _secret: &Secret,
        _identity: &Identity,
        _grant_id: &str,
        _request_id: &str,
    ) -> Result<RedeemOutcome, RedeemError> {
        Err(RedeemError::Refused(format!("{} does not support limit resets.", self.name())))
    }

    /// Consumes the refresh token in `secret`. The result has to be stored or the login is lost.
    fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError>;

    fn expired(&self, secret: &Secret) -> bool;

    /// Equal for two secrets exactly when they hold the same refresh token.
    fn lineage(&self, secret: &Secret) -> Option<String>;

    /// Whether a saved account can be made the live login. Without it, Switch only reads usage.
    fn switchable(&self) -> bool {
        true
    }

    /// How accounts are added by signing in through a browser, when the provider supports it.
    fn sign_in(&self) -> Option<&dyn OAuthSignIn> {
        None
    }
}

/// An OAuth authorization code sign-in with PKCE, owned by Switch rather than by the provider's tools.
pub trait OAuthSignIn: Send + Sync {
    fn authorize_url(&self, redirect_uri: &str, state: &str, code_challenge: &str) -> String;

    /// Trades the code from the browser for a secret that `Provider::profile` and `Provider::usage` accept.
    fn exchange(&self, code: &str, redirect_uri: &str, code_verifier: &str) -> Result<Secret, RemoteError>;
}

pub(crate) fn digest(value: &str) -> String {
    Sha256::digest(value).iter().map(|byte| format!("{byte:02x}")).collect()
}
