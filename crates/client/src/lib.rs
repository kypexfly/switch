pub mod oauth;
mod service;

use std::io::{BufRead, BufReader};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde::de::DeserializeOwned;
use switch_protocol::{
    AccountUpdate, ErrorBody, Event, OrderUpdate, RedeemReset, ServerInfo, Settings, SettingsUpdate, SignIn,
    SignInFinish, SignInStart, Snapshot, Strategy, StrategySwitch,
};
use ureq::{Agent, Body, http::Response};

pub use service::{Service, home_dir, registration_path};

/// Long enough for a refresh that renews logins of several accounts, short enough that a stuck
/// server does not freeze an interface.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(180);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct Client {
    base: String,
    password: String,
    agent: Agent,
    refresh_agent: Agent,
    stream_agent: Agent,
}

impl Client {
    pub fn new(base: impl Into<String>, password: impl Into<String>) -> Self {
        let agent = |timeout| {
            Agent::config_builder()
                .http_status_as_error(false)
                .timeout_connect(Some(CONNECT_TIMEOUT))
                .timeout_global(timeout)
                .build()
                .into()
        };
        Self {
            base: base.into().trim_end_matches('/').to_owned(),
            password: password.into(),
            agent: agent(Some(CALL_TIMEOUT)),
            refresh_agent: agent(Some(REFRESH_TIMEOUT)),
            stream_agent: agent(None),
        }
    }

    pub fn url(&self) -> &str {
        &self.base
    }

    pub fn info(&self) -> Result<ServerInfo> {
        self.call("GET", "/api/info", None::<&()>)
    }

    pub fn stop(&self) -> Result<()> {
        self.request("POST", "/api/stop", None::<&()>).map(drop)
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        self.call("GET", "/api/snapshot", None::<&()>)
    }

    /// Fetches usage again for every account that is due and returns the result.
    pub fn refresh(&self) -> Result<Snapshot> {
        Ok(self.send(&self.refresh_agent, "POST", "/api/refresh", None::<&()>)?.into_body().read_json()?)
    }

    /// Saves the provider's current login as an account, or updates it when already saved.
    pub fn add_account(&self, provider: &str) -> Result<Snapshot> {
        self.call("POST", &format!("/api/provider/{provider}/account"), None::<&()>)
    }

    pub fn switch(&self, provider: &str, strategy: Strategy) -> Result<Snapshot> {
        self.call("POST", &format!("/api/provider/{provider}/switch"), Some(&StrategySwitch { strategy }))
    }

    /// Starts a browser sign-in that comes back to `redirect_uri`, for providers that add accounts that way.
    pub fn begin_sign_in(&self, provider: &str, redirect_uri: &str) -> Result<SignIn> {
        let body = SignInStart { redirect_uri: redirect_uri.to_owned() };
        self.call("POST", &format!("/api/provider/{provider}/sign-in"), Some(&body))
    }

    /// Saves the account from the code the browser came back with.
    pub fn finish_sign_in(&self, provider: &str, state: &str, code: &str) -> Result<Snapshot> {
        let body = SignInFinish { state: state.to_owned(), code: code.to_owned() };
        self.call("POST", &format!("/api/provider/{provider}/sign-in/finish"), Some(&body))
    }

    pub fn activate(&self, account: &str) -> Result<Snapshot> {
        self.call("POST", &format!("/api/account/{account}/activate"), None::<&()>)
    }

    pub fn redeem_reset(&self, account: &str, grant_id: &str) -> Result<Snapshot> {
        self.call(
            "POST",
            &format!("/api/account/{account}/reset"),
            Some(&RedeemReset { grant_id: grant_id.to_owned() }),
        )
    }

    pub fn update_account(&self, account: &str, update: &AccountUpdate) -> Result<Snapshot> {
        self.call("PATCH", &format!("/api/account/{account}"), Some(update))
    }

    pub fn remove_account(&self, account: &str) -> Result<Snapshot> {
        self.call("DELETE", &format!("/api/account/{account}"), None::<&()>)
    }

    pub fn update_order(&self, update: &OrderUpdate) -> Result<Snapshot> {
        self.call("PATCH", "/api/order", Some(update))
    }

    pub fn update_settings(&self, update: &SettingsUpdate) -> Result<Settings> {
        self.call("PATCH", "/api/settings", Some(update))
    }

    /// Live events, starting with the current snapshot. The iterator ends when the connection does.
    pub fn events(&self) -> Result<impl Iterator<Item = Result<Event>> + Send + use<>> {
        let response = self.send(&self.stream_agent, "GET", "/api/event", None::<&()>)?;
        let lines = BufReader::new(response.into_body().into_reader()).lines();
        Ok(lines.filter_map(|line| match line {
            Ok(line) => {
                let data = line.strip_prefix("data:")?;
                Some(serde_json::from_str(data.trim()).context("invalid event"))
            }
            Err(error) => Some(Err(error.into())),
        }))
    }

    pub fn raw(&self, method: &str, path: &str, body: Option<&serde_json::Value>) -> Result<String> {
        Ok(self.request(method, path, body)?.into_body().read_to_string()?)
    }

    fn call<T: DeserializeOwned>(&self, method: &str, path: &str, body: Option<&impl Serialize>) -> Result<T> {
        Ok(self.request(method, path, body)?.into_body().read_json()?)
    }

    fn request(&self, method: &str, path: &str, body: Option<&impl Serialize>) -> Result<Response<Body>> {
        self.send(&self.agent, method, path, body)
    }

    fn send(&self, agent: &Agent, method: &str, path: &str, body: Option<&impl Serialize>) -> Result<Response<Body>> {
        let builder = ureq::http::Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", self.password));
        let response = match body {
            Some(body) => {
                let body = serde_json::to_string(body)?;
                agent.run(builder.header("content-type", "application/json").body(body)?)
            }
            None => agent.run(builder.body(())?),
        }
        .with_context(|| format!("could not reach the server at {}", self.base))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let text = response.into_body().read_to_string().unwrap_or_default();
        Err(match serde_json::from_str::<ErrorBody>(&text) {
            Ok(body) => anyhow!(body.error),
            Err(_) => anyhow!("{method} {path} failed with {status}"),
        })
    }
}
