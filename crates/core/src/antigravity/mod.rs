mod api;

use std::any::Any;

use anyhow::{Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use switch_protocol::UsageWindow;

use self::api::{Api, Tokens};
use crate::provider::{Login, OAuthSignIn, Profile, Provider, RemoteError, Secret, digest};

const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

/// Reads Antigravity usage with a Google login from Switch's own sign-in. The Antigravity app's login is never touched.
pub struct Antigravity {
    api: Api,
}

impl Default for Antigravity {
    fn default() -> Self {
        Self { api: Api::new() }
    }
}

#[derive(Serialize, Deserialize)]
struct Stored {
    access_token: String,
    refresh_token: Option<String>,
    expires_at_ms: i64,
}

impl Provider for Antigravity {
    fn id(&self) -> &'static str {
        "antigravity"
    }

    fn name(&self) -> &'static str {
        "Antigravity"
    }

    fn live(&self) -> Result<Option<Login>> {
        Ok(None)
    }

    fn lock(&self) -> Result<Box<dyn Any>> {
        bail!("Switching Antigravity accounts is not supported yet.")
    }

    fn replace_live(&self, _: &Secret) -> Result<()> {
        bail!("Switching Antigravity accounts is not supported yet.")
    }

    fn profile(&self, secret: &Secret) -> Result<Profile, RemoteError> {
        let access = &decode(secret)?.access_token;
        let identity = self.api.user(access)?;
        let plan = self.api.code_assist(access).ok().and_then(|code_assist| code_assist.plan);
        Ok(Profile { identity, plan })
    }

    fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError> {
        let access = &decode(secret)?.access_token;
        let project = match self.api.code_assist(access) {
            Ok(code_assist) => code_assist.project,
            Err(RemoteError::Unauthorized) => return Err(RemoteError::Unauthorized),
            Err(_) => None,
        };
        self.api.quota(access, project.as_deref())
    }

    fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError> {
        let stored = decode(secret)?;
        let refresh_token = stored.refresh_token.filter(|token| !token.is_empty()).ok_or(RemoteError::Rejected)?;
        let tokens = self.api.refresh(&refresh_token)?;
        Ok(encode(tokens, Some(refresh_token)))
    }

    fn expired(&self, secret: &Secret) -> bool {
        decode(secret).is_ok_and(|stored| stored.expires_at_ms <= Timestamp::now().as_millisecond() + EXPIRY_MARGIN_MS)
    }

    fn lineage(&self, secret: &Secret) -> Option<String> {
        let stored = decode(secret).ok()?;
        let token = stored.refresh_token.filter(|token| !token.is_empty()).unwrap_or(stored.access_token);
        (!token.is_empty()).then(|| digest(&token))
    }

    fn switchable(&self) -> bool {
        false
    }

    fn sign_in(&self) -> Option<&dyn OAuthSignIn> {
        Some(self)
    }
}

impl OAuthSignIn for Antigravity {
    fn authorize_url(&self, redirect_uri: &str, state: &str, code_challenge: &str) -> String {
        self.api.authorize_url(redirect_uri, state, code_challenge)
    }

    fn exchange(&self, code: &str, redirect_uri: &str, code_verifier: &str) -> Result<Secret, RemoteError> {
        let tokens = self.api.exchange(code, redirect_uri, code_verifier)?;
        if tokens.refresh.is_none() {
            return Err(RemoteError::Other("Google did not grant offline access; sign in again".to_owned()));
        }
        Ok(encode(tokens, None))
    }
}

/// Google may leave out the refresh token when renewing; the one already held stays valid then.
fn encode(tokens: Tokens, previous_refresh: Option<String>) -> Secret {
    let stored = Stored {
        access_token: tokens.access,
        refresh_token: tokens.refresh.or(previous_refresh),
        expires_at_ms: tokens.expires_at_ms,
    };
    Secret::new(serde_json::to_string(&stored).expect("logins serialize"))
}

fn decode(secret: &Secret) -> Result<Stored, RemoteError> {
    serde_json::from_str(secret.expose()).map_err(|_| RemoteError::Rejected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(access: &str, refresh: Option<&str>, expires_at_ms: i64) -> Tokens {
        Tokens { access: access.to_owned(), refresh: refresh.map(str::to_owned), expires_at_ms }
    }

    #[test]
    fn a_renewal_without_a_refresh_token_keeps_the_old_one() {
        let provider = Antigravity::default();
        let first = encode(tokens("a1", Some("r1"), 0), None);
        let renewed = encode(tokens("a2", None, 0), Some("r1".to_owned()));
        assert_eq!(provider.lineage(&first), provider.lineage(&renewed));
        let rotated = encode(tokens("a3", Some("r2"), 0), Some("r1".to_owned()));
        assert_ne!(provider.lineage(&first), provider.lineage(&rotated));
    }

    #[test]
    fn expiry_is_read_from_the_saved_login() {
        let provider = Antigravity::default();
        let now = Timestamp::now().as_millisecond();
        assert!(provider.expired(&encode(tokens("a", Some("r"), now + 60_000), None)));
        assert!(!provider.expired(&encode(tokens("a", Some("r"), now + 3_600_000), None)));
        assert!(!provider.expired(&Secret::new("damaged")));
    }

    #[test]
    fn it_never_has_a_live_login_and_can_not_switch() {
        let provider = Antigravity::default();
        assert!(provider.live().unwrap().is_none());
        assert!(provider.lock().is_err());
        assert!(provider.replace_live(&Secret::new("{}")).is_err());
        assert!(!provider.switchable());
        assert!(provider.sign_in().is_some());
    }

    #[test]
    fn a_damaged_login_has_to_sign_in_again() {
        let provider = Antigravity::default();
        assert!(matches!(provider.renew(&Secret::new("damaged")), Err(RemoteError::Rejected)));
        let without_refresh = encode(tokens("a", None, 0), None);
        assert!(matches!(provider.renew(&without_refresh), Err(RemoteError::Rejected)));
    }
}
