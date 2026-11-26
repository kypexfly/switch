use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use switch_protocol::{Identity, LimitReset, UsageWindow};

use crate::engine::Engine;
use crate::provider::{
    Login, OAuthSignIn, Profile, Provider, RedeemError, RedeemOutcome, RemoteError, ResetGrants, Secret, digest,
};
use crate::store::Store;
use crate::vault::Vault;

#[derive(Serialize, Deserialize)]
struct DemoSecret {
    identity: Identity,
    plan: String,
    token: u64,
}

/// Made-up accounts for trying the clients without touching real logins.
pub struct Demo {
    id: &'static str,
    name: &'static str,
    live: Mutex<Option<Login>>,
    started: Timestamp,
    switchable: bool,
    resets: Mutex<HashMap<String, ResetGrants>>,
    reset_windows: Mutex<HashSet<String>>,
}

impl Demo {
    pub fn new(id: &'static str, name: &'static str) -> Self {
        Self {
            id,
            name,
            live: Mutex::default(),
            started: Timestamp::now(),
            switchable: true,
            resets: Mutex::default(),
            reset_windows: Mutex::default(),
        }
    }

    /// A provider whose accounts are added by signing in and never become the live login.
    /// Its sign-in page sends the browser straight back with the code `sam`.
    pub fn usage_only(id: &'static str, name: &'static str) -> Self {
        Self { switchable: false, ..Self::new(id, name) }
    }

    /// An engine kept in memory with a few accounts for two switchable providers and one usage-only provider.
    pub fn engine() -> Result<Engine> {
        let claude = Arc::new(Demo::new("claude", "Claude"));
        let codex = Arc::new(Demo::new("codex", "Codex"));
        let antigravity = Arc::new(Demo::usage_only("antigravity", "Antigravity"));
        let providers: Vec<Arc<dyn Provider>> = vec![claude.clone(), codex.clone(), antigravity.clone()];
        let engine = Engine::new(providers, Store::memory(), Vault::memory())?;
        let saved = [
            (&claude, "grace@example.com", "Pro"),
            (&claude, "alan@example.com", "Max 5x"),
            (&claude, "ada@example.com", "Max 20x"),
            (&codex, "ada@example.com", "Plus"),
        ];
        for (demo, email, plan) in saved {
            demo.set_live(Some(demo.login(email, plan)));
            engine.add_account(demo.id)?;
        }
        let now = Timestamp::now();
        claude.set_resets(
            "grace@example.com",
            ResetGrants {
                resets: vec![LimitReset {
                    id: "demo-full-grace".to_owned(),
                    label: "Full Reset".to_owned(),
                    remaining: 1,
                    expires_at: Some(now + SignedDuration::from_hours(14 * 24 + 23)),
                    usable: true,
                    detail: Some("Refills your session and weekly limits now. This cannot be undone.".to_owned()),
                }],
                next_grant_id: Some("demo-full-grace".to_owned()),
            },
        );
        claude.set_resets(
            "alan@example.com",
            ResetGrants {
                resets: vec![
                    LimitReset {
                        id: "demo-weekly-alan".to_owned(),
                        label: "Weekly Reset".to_owned(),
                        remaining: 1,
                        expires_at: Some(now + SignedDuration::from_hours(10 * 24)),
                        usable: true,
                        detail: Some("Refills your weekly limit now. This cannot be undone.".to_owned()),
                    },
                    LimitReset {
                        id: "demo-fable-alan".to_owned(),
                        label: "Fable Reset".to_owned(),
                        remaining: 2,
                        expires_at: Some(now + SignedDuration::from_hours(30 * 24)),
                        usable: false,
                        detail: Some("Refills your Fable limit now. This cannot be undone.".to_owned()),
                    },
                ],
                next_grant_id: Some("demo-weekly-alan".to_owned()),
            },
        );
        claude.set_resets(
            "ada@example.com",
            ResetGrants {
                resets: vec![LimitReset {
                    id: "demo-full-ada".to_owned(),
                    label: "Full Reset".to_owned(),
                    remaining: 1,
                    expires_at: Some(now + SignedDuration::from_hours(14 * 24 + 23)),
                    usable: true,
                    detail: Some("Refills your session and weekly limits now. This cannot be undone.".to_owned()),
                }],
                next_grant_id: Some("demo-full-ada".to_owned()),
            },
        );
        codex.set_live(Some(codex.login("linus@example.com", "Pro")));
        let sign_in = engine.begin_sign_in(antigravity.id, "http://127.0.0.1/demo")?;
        engine.finish_sign_in(antigravity.id, &sign_in.state, "ada")?;
        engine.sync_live();
        Ok(engine)
    }

    pub fn login(&self, email: &str, plan: &str) -> Login {
        let identity = Identity {
            account_id: format!("{}:{email}", self.id),
            email: email.to_owned(),
            organization_id: None,
            organization: None,
        };
        let secret = DemoSecret { identity: identity.clone(), plan: plan.to_owned(), token: 1 };
        Login { identity, secret: encode(&secret) }
    }

    pub fn set_live(&self, login: Option<Login>) {
        *self.live.lock().unwrap() = login;
    }

    pub fn set_resets(&self, email: &str, grants: ResetGrants) {
        self.resets.lock().unwrap().insert(format!("{}:{email}", self.id), grants);
    }
}

impl Provider for Demo {
    fn id(&self) -> &'static str {
        self.id
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn live(&self) -> Result<Option<Login>> {
        Ok(self.live.lock().unwrap().clone())
    }

    fn lock(&self) -> Result<Box<dyn Any>> {
        anyhow::ensure!(self.switchable, "{} accounts can not be switched", self.name);
        Ok(Box::new(()))
    }

    fn replace_live(&self, secret: &Secret) -> Result<()> {
        anyhow::ensure!(self.switchable, "{} accounts can not be switched", self.name);
        let identity = decode(secret).map_err(|_| anyhow::anyhow!("the saved login is damaged"))?.identity;
        self.set_live(Some(Login { identity, secret: secret.clone() }));
        Ok(())
    }

    fn profile(&self, secret: &Secret) -> Result<Profile, RemoteError> {
        let secret = decode(secret)?;
        Ok(Profile { identity: secret.identity, plan: Some(secret.plan) })
    }

    fn usage(&self, secret: &Secret) -> Result<Vec<UsageWindow>, RemoteError> {
        let secret = decode(secret)?;
        let was_reset = self.reset_windows.lock().unwrap().contains(&secret.identity.account_id);
        let seed = digest(&secret.identity.email).bytes().fold(0u64, |sum, byte| sum.wrapping_mul(31) + byte as u64);
        let minutes = (Timestamp::now().as_second() - self.started.as_second()) as u64 / 60;
        let percent = |base: u64| if was_reset { 0.0 } else { ((base + minutes) % 101) as f64 };
        let resets = |minutes: u64| Some(Timestamp::now() + SignedDuration::from_mins(minutes as i64));
        let mut windows = vec![
            UsageWindow {
                id: "session".to_owned(),
                label: "Session".to_owned(),
                used_percent: Some(percent(seed % 100)),
                resets_at: resets(if was_reset { 300 } else { seed % 300 }),
                primary: true,
                period_seconds: Some(5 * 3600),
            },
            UsageWindow {
                id: "weekly".to_owned(),
                label: "Weekly".to_owned(),
                used_percent: Some(percent(seed / 100 % 60)),
                resets_at: resets(if was_reset { 10_080 } else { seed % 10_080 }),
                primary: true,
                period_seconds: Some(7 * 24 * 3600),
            },
        ];
        if secret.plan.starts_with("Max") {
            windows.push(UsageWindow {
                id: "model".to_owned(),
                label: "Fable".to_owned(),
                used_percent: if was_reset { Some(0.0) } else { None },
                resets_at: None,
                primary: false,
                period_seconds: None,
            });
        }
        Ok(windows)
    }

    fn supports_resets(&self) -> bool {
        self.switchable
    }

    fn resets(&self, secret: &Secret) -> Result<ResetGrants, RemoteError> {
        let secret = decode(secret)?;
        Ok(self.resets.lock().unwrap().get(&secret.identity.account_id).cloned().unwrap_or_default())
    }

    fn redeem_reset(
        &self,
        secret: &Secret,
        _identity: &Identity,
        grant_id: &str,
        _request_id: &str,
    ) -> Result<RedeemOutcome, RedeemError> {
        if !self.switchable {
            return Err(RedeemError::Refused(format!("{} does not support limit resets.", self.name)));
        }
        let secret = decode(secret)?;
        let mut all = self.resets.lock().unwrap();
        let Some(grants) = all.get_mut(&secret.identity.account_id) else {
            return Err(RedeemError::Exhausted("That limit reset is no longer available.".to_owned()));
        };
        let Some(index) = grants.resets.iter().position(|grant| grant.id == grant_id && grant.remaining > 0) else {
            return Err(RedeemError::Exhausted("That limit reset is no longer available.".to_owned()));
        };
        if !grants.resets[index].usable {
            return Err(RedeemError::Refused("Resets cannot be used right now.".to_owned()));
        }
        let remaining = grants.resets[index].remaining.saturating_sub(1);
        if remaining == 0 {
            grants.resets.remove(index);
        } else {
            grants.resets[index].remaining = remaining;
        }
        self.reset_windows.lock().unwrap().insert(secret.identity.account_id);
        Ok(RedeemOutcome { remaining: Some(remaining) })
    }

    fn renew(&self, secret: &Secret) -> Result<Secret, RemoteError> {
        let mut secret = decode(secret)?;
        secret.token += 1;
        Ok(encode(&secret))
    }

    fn expired(&self, _: &Secret) -> bool {
        false
    }

    fn lineage(&self, secret: &Secret) -> Option<String> {
        let secret = decode(secret).ok()?;
        Some(digest(&format!("{}:{}", secret.identity.account_id, secret.token)))
    }

    fn switchable(&self) -> bool {
        self.switchable
    }

    fn sign_in(&self) -> Option<&dyn OAuthSignIn> {
        if self.switchable { None } else { Some(self) }
    }
}

impl OAuthSignIn for Demo {
    fn authorize_url(&self, redirect_uri: &str, state: &str, _: &str) -> String {
        format!("{redirect_uri}?code=sam&state={state}")
    }

    fn exchange(&self, code: &str, _: &str, _: &str) -> Result<Secret, RemoteError> {
        if code.is_empty() || !code.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(RemoteError::Rejected);
        }
        Ok(self.login(&format!("{code}@example.com"), "Pro").secret)
    }
}

fn encode(secret: &DemoSecret) -> Secret {
    Secret::new(serde_json::to_string(secret).expect("demo secrets serialize"))
}

fn decode(secret: &Secret) -> Result<DemoSecret, RemoteError> {
    serde_json::from_str(secret.expose()).map_err(|_| RemoteError::Rejected)
}
