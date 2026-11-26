use std::convert::Infallible;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use switch_core::{Antigravity, Claude, Demo, Engine, Error, Provider, Store, Vault};
use switch_protocol::{
    AccountUpdate, ErrorBody, Event, OrderUpdate, RedeemReset, Registration, ServerInfo, Settings, SettingsUpdate,
    SignIn, SignInFinish, SignInStart, Snapshot, StrategySwitch,
};
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};
use tokio_stream::wrappers::WatchStream;
use tokio_stream::{Stream, StreamExt};

const LIVE_CHECK_INTERVAL: Duration = Duration::from_secs(10);
/// How often the engine checks which accounts are due; the engine decides how often each is fetched.
const POLL_INTERVAL: Duration = Duration::from_secs(15);
const SERVER_LOCK_WAIT: Duration = Duration::from_secs(20);
const REGISTERED_GRACE: Duration = Duration::from_millis(300);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub port: Option<u16>,
    pub no_auth: Option<bool>,
    pub allowed_hosts: Vec<String>,
}

pub fn load_config(home: &FsPath) -> Result<ServerConfig> {
    let path = home.join("server.json");
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("could not parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ServerConfig::default()),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

pub struct Options {
    pub home: PathBuf,
    pub demo: bool,
    pub port: u16,
    /// Started on purpose rather than by a client.
    pub managed: bool,
    /// Accept requests without the password. Only for a server that just a private network can reach.
    pub open: bool,
    pub allowed_hosts: Vec<String>,
}

#[derive(Clone)]
struct Server {
    engine: Arc<Engine>,
    snapshots: watch::Sender<Snapshot>,
    fatal: watch::Sender<Option<String>>,
    password: Arc<str>,
    open: bool,
    allowed_hosts: Arc<[String]>,
    stop: Arc<Notify>,
}

pub fn run(options: Options) -> Result<()> {
    let (home, demo) = (options.home.clone(), options.demo);
    run_with(options, SERVER_LOCK_WAIT, move || open_engine(&home, demo))
}

fn run_with(
    options: Options,
    lock_wait: Duration,
    open: impl FnOnce() -> Result<Engine> + Send + 'static,
) -> Result<()> {
    let lock = acquire_server_lock(&options.home, lock_wait)?;
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(serve(options, open));
    drop(runtime);
    drop(lock);
    result
}

async fn serve(options: Options, open: impl FnOnce() -> Result<Engine> + Send + 'static) -> Result<()> {
    let engine = tokio::task::spawn_blocking(open).await??;
    let listener = TcpListener::bind(("127.0.0.1", options.port)).await?;
    let url = format!("http://{}", listener.local_addr()?);
    let password = uuid::Uuid::new_v4().simple().to_string();
    let server = Server::start(engine, password.clone(), options.open, options.allowed_hosts);

    let registration = Registration {
        url: url.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        password,
        managed: options.managed,
    };
    register(&options.home, &registration)?;
    println!("Switch is listening on {url}{}", if options.demo { " with demo accounts" } else { "" });
    if options.open {
        println!("Requests without the password are accepted. Keep this port reachable only from a private network.");
    }

    let stop = server.stop.clone();
    let mut fatal = server.fatal.subscribe();
    let result = tokio::select! {
        result = axum::serve(listener, router(server)) => result.map_err(Into::into),
        () = stop.notified() => Ok(()),
        () = terminated() => Ok(()),
        fatal_ref = fatal.wait_for(|error| error.is_some()) => {
            let message = fatal_ref
                .ok()
                .and_then(|error| error.clone())
                .unwrap_or_else(|| "the engine panicked".to_owned());
            Err(anyhow::anyhow!("{message}"))
        }
    };
    unregister(&options.home, &registration.password);
    result
}

fn open_engine(home: &FsPath, demo: bool) -> Result<Engine> {
    if demo {
        return Demo::engine();
    }
    let logins = home.join("logins");
    let vault = if cfg!(target_os = "macos") { Vault::keychain(logins) } else { Vault::files(logins) };
    let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(Claude::for_user()?), Arc::new(Antigravity::default())];
    Engine::new(providers, Store::file(home.join("accounts.json")), vault)
}

impl Server {
    fn start(engine: Engine, password: String, open: bool, allowed_hosts: Vec<String>) -> Self {
        let (snapshots, _) = watch::channel(engine.snapshot());
        let (fatal, _) = watch::channel(None);
        let server = Self {
            engine: Arc::new(engine),
            snapshots,
            fatal,
            password: password.into(),
            open,
            allowed_hosts: allowed_hosts.into(),
            stop: Arc::new(Notify::new()),
        };
        tokio::spawn(server.clone().every(LIVE_CHECK_INTERVAL, |engine, _| engine.sync_live()));
        tokio::spawn(server.clone().every(POLL_INTERVAL, |engine, snapshots| {
            engine.poll_with(|| publish_snapshot(engine, snapshots));
        }));
        server
    }

    async fn every(self, period: Duration, work: impl Fn(&Engine, &watch::Sender<Snapshot>) + Copy + Send + 'static) {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let snapshots = self.snapshots.clone();
            if self.blocking(move |engine| work(engine, &snapshots)).await.is_err() {
                break;
            }
        }
    }

    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Engine) -> T + Send + 'static,
    ) -> Result<T, ApiError> {
        let engine = self.engine.clone();
        let result = match tokio::task::spawn_blocking(move || work(&engine)).await {
            Ok(result) => result,
            Err(error) => {
                let reason = panic_reason(&error);
                self.fatal.send_if_modified(|current| {
                    if current.is_none() {
                        *current = Some(reason);
                        true
                    } else {
                        false
                    }
                });
                return Err(ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "The server stopped after an internal error.".to_owned(),
                ));
            }
        };
        publish_snapshot(&self.engine, &self.snapshots);
        Ok(result)
    }

    async fn act(&self, work: impl FnOnce(&Engine) -> Result<(), Error> + Send + 'static) -> Reply {
        self.blocking(work).await??;
        Ok(Json(self.current()))
    }

    fn current(&self) -> Snapshot {
        self.snapshots.borrow().clone()
    }
}

fn publish_snapshot(engine: &Engine, snapshots: &watch::Sender<Snapshot>) {
    snapshots.send_if_modified(|current| {
        let snapshot = engine.snapshot();
        if *current == snapshot {
            return false;
        }
        *current = snapshot;
        true
    });
}

fn panic_reason(error: &tokio::task::JoinError) -> String {
    if error.is_panic() { "the engine panicked".to_owned() } else { format!("engine task failed: {error}") }
}

fn router(server: Server) -> Router {
    Router::new()
        .route("/api/info", get(info))
        .route("/api/stop", post(stop))
        .route("/api/snapshot", get(snapshot))
        .route("/api/refresh", post(refresh))
        .route("/api/provider/{provider}/account", post(add_account))
        .route("/api/provider/{provider}/switch", post(switch))
        .route("/api/provider/{provider}/sign-in", post(begin_sign_in))
        .route("/api/provider/{provider}/sign-in/finish", post(finish_sign_in))
        .route("/api/account/{id}/activate", post(activate))
        .route("/api/account/{id}/reset", post(redeem_reset))
        .route("/api/account/{id}", patch(update_account))
        .route("/api/account/{id}", delete(remove_account))
        .route("/api/order", patch(update_order))
        .route("/api/settings", patch(update_settings))
        .route("/api/event", get(events))
        .layer(middleware::from_fn_with_state(server.clone(), authorize))
        .layer(middleware::from_fn(same_site_only))
        .with_state(server)
}

type Reply = Result<Json<Snapshot>, ApiError>;

async fn info() -> Json<ServerInfo> {
    Json(ServerInfo { version: env!("CARGO_PKG_VERSION").to_owned(), pid: std::process::id() })
}

async fn stop(State(server): State<Server>) -> StatusCode {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        server.stop.notify_one();
    });
    StatusCode::NO_CONTENT
}

async fn snapshot(State(server): State<Server>) -> Json<Snapshot> {
    Json(server.current())
}

async fn refresh(State(server): State<Server>) -> Reply {
    let snapshots = server.snapshots.clone();
    server.blocking(move |engine| engine.refresh_with(|| publish_snapshot(engine, &snapshots))).await?;
    Ok(Json(server.current()))
}

async fn add_account(State(server): State<Server>, Path(provider): Path<String>) -> Reply {
    server.act(move |engine| engine.add_account(&provider)).await
}

async fn switch(State(server): State<Server>, Path(provider): Path<String>, Json(body): Json<StrategySwitch>) -> Reply {
    server.act(move |engine| engine.switch(&provider, body.strategy)).await
}

async fn begin_sign_in(
    State(server): State<Server>,
    Path(provider): Path<String>,
    Json(body): Json<SignInStart>,
) -> Result<Json<SignIn>, ApiError> {
    Ok(Json(server.blocking(move |engine| engine.begin_sign_in(&provider, &body.redirect_uri)).await??))
}

async fn finish_sign_in(
    State(server): State<Server>,
    Path(provider): Path<String>,
    Json(body): Json<SignInFinish>,
) -> Reply {
    server.act(move |engine| engine.finish_sign_in(&provider, &body.state, &body.code)).await
}

async fn activate(State(server): State<Server>, Path(id): Path<String>) -> Reply {
    server.act(move |engine| engine.activate(&id)).await
}

async fn redeem_reset(State(server): State<Server>, Path(id): Path<String>, Json(body): Json<RedeemReset>) -> Reply {
    server.act(move |engine| engine.redeem_reset(&id, &body.grant_id)).await
}

async fn update_account(
    State(server): State<Server>,
    Path(id): Path<String>,
    Json(update): Json<AccountUpdate>,
) -> Reply {
    server.act(move |engine| engine.update_account(&id, update)).await
}

async fn remove_account(State(server): State<Server>, Path(id): Path<String>) -> Reply {
    server.act(move |engine| engine.remove_account(&id)).await
}

async fn update_order(State(server): State<Server>, Json(update): Json<OrderUpdate>) -> Reply {
    server.act(move |engine| engine.update_order(update)).await
}

async fn update_settings(
    State(server): State<Server>,
    Json(update): Json<SettingsUpdate>,
) -> Result<Json<Settings>, ApiError> {
    Ok(Json(server.blocking(move |engine| engine.update_settings(update)).await??))
}

async fn events(State(server): State<Server>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let mut changes = server.snapshots.subscribe();
    let first = Event::Connected { snapshot: changes.borrow_and_update().clone() };
    let updates = WatchStream::from_changes(changes).map(|snapshot| Event::Updated { snapshot });
    let stream = tokio_stream::once(first)
        .chain(updates)
        .map(|event| Ok(SseEvent::default().data(serde_json::to_string(&event).expect("events serialize"))));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn authorize(State(server): State<Server>, request: Request, next: Next) -> Response {
    if server.open {
        if !allowed_host(request.headers(), &server.allowed_hosts) {
            return ApiError(StatusCode::FORBIDDEN, "Host is not allowed.".to_owned()).into_response();
        }
        return next.run(request).await;
    }
    let expected = format!("Bearer {}", server.password);
    let given = request.headers().get(header::AUTHORIZATION).map(|value| value.as_bytes()).unwrap_or_default();
    if equal_in_constant_time(given, expected.as_bytes()) {
        next.run(request).await
    } else {
        ApiError(StatusCode::UNAUTHORIZED, "Not authorized.".to_owned()).into_response()
    }
}

/// Browsers attach these headers, so a page on another site cannot act on the server through a visitor's browser.
async fn same_site_only(request: Request, next: Next) -> Response {
    if from_another_site(request.headers()) {
        return ApiError(StatusCode::FORBIDDEN, "Requests from other sites are not allowed.".to_owned())
            .into_response();
    }
    next.run(request).await
}

fn from_another_site(headers: &HeaderMap) -> bool {
    let text = |name| headers.get(name).and_then(|value: &HeaderValue| value.to_str().ok());
    if let Some(site) = text(HeaderName::from_static("sec-fetch-site")) {
        return !matches!(site, "same-origin" | "none");
    }
    let Some(origin) = text(header::ORIGIN) else {
        return false;
    };
    let origin_host = origin.split_once("://").map(|(_, host)| host);
    let hosts = [text(header::HOST), text(HeaderName::from_static("x-forwarded-host"))];
    !hosts.into_iter().flatten().any(|host| origin_host == Some(host))
}

fn allowed_host(headers: &HeaderMap, extra: &[String]) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    if !is_allowed_host(host, extra) {
        return false;
    }
    if let Some(forwarded) =
        headers.get(HeaderName::from_static("x-forwarded-host")).and_then(|value| value.to_str().ok())
    {
        return is_allowed_host(forwarded, extra);
    }
    true
}

fn is_allowed_host(raw: &str, extra: &[String]) -> bool {
    let host = raw.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split_once(']').map_or(host, |(ip, _)| ip)
    } else {
        host.split_once(':').map_or(host, |(name, _)| name)
    };
    let lower = name.to_ascii_lowercase();
    matches!(lower.as_str(), "127.0.0.1" | "localhost" | "::1")
        || lower.ends_with(".ts.net")
        || extra.iter().any(|allowed| allowed.eq_ignore_ascii_case(host) || allowed.eq_ignore_ascii_case(&lower))
}

fn equal_in_constant_time(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |difference, (x, y)| difference | (x ^ y)) == 0
}

struct ApiError(StatusCode, String);

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let status = match error {
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Invalid(_) => StatusCode::CONFLICT,
            Error::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

fn acquire_server_lock(home: &FsPath, wait: Duration) -> Result<File> {
    fs::create_dir_all(home)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(home.join("server.lock"))?;
    let deadline = Instant::now() + wait;
    let mut registered_since: Option<Instant> = None;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {
                if registration_path(home).exists() {
                    let since = *registered_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= REGISTERED_GRACE {
                        bail!("{}", already_running_message(home));
                    }
                } else {
                    registered_since = None;
                }
                if Instant::now() >= deadline {
                    bail!("{}", already_running_message(home));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).context("could not lock the server home");
            }
        }
    }
}

fn registered_url(home: &FsPath) -> Option<String> {
    let text = fs::read_to_string(registration_path(home)).ok()?;
    let registration: Registration = serde_json::from_str(&text).ok()?;
    Some(registration.url)
}

fn already_running_message(home: &FsPath) -> String {
    match registered_url(home) {
        Some(url) => format!("Switch is already running at {url}. Stop it with `swx stop` first."),
        None => "Switch is already running. Stop it with `swx stop` first.".to_owned(),
    }
}

fn registration_path(home: &FsPath) -> PathBuf {
    home.join("service.json")
}

/// The registration holds the password, so only the owner may read it.
fn register(home: &FsPath, registration: &Registration) -> Result<()> {
    fs::create_dir_all(home)?;
    let temporary = home.join("service.json.tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(&temporary)?;
    serde_json::to_writer_pretty(&file, registration)?;
    file.sync_all()?;
    fs::rename(&temporary, registration_path(home)).context("could not write the server registration")
}

fn unregister(home: &FsPath, password: &str) {
    let path = registration_path(home);
    let ours = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Registration>(&text).ok())
        .is_some_and(|registration| registration.password == password);
    if ours {
        fs::remove_file(path).ok();
    }
}

async fn terminated() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = terminate.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use switch_client::Client;

    fn start_demo() -> (String, String) {
        start_demo_server(false)
    }

    fn start_demo_server(open: bool) -> (String, String) {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}", listener.local_addr().unwrap());
                let server = Server::start(Demo::engine().unwrap(), "secret".to_owned(), open, Vec::new());
                sender.send(url).unwrap();
                axum::serve(listener, router(server)).await.unwrap();
            });
        });
        (receiver.recv().unwrap(), "secret".to_owned())
    }

    #[test]
    fn clients_switch_accounts_and_follow_events() {
        let (url, password) = start_demo();
        let client = Client::new(&url, &password);
        let mut events = client.events().unwrap();
        let first = events.next().unwrap().unwrap().into_snapshot();
        let target = first.providers[0].accounts.iter().find(|account| !account.active).unwrap().id.clone();
        let is_active = |snapshot: &Snapshot| {
            snapshot.providers[0].accounts.iter().any(|account| account.id == target && account.active)
        };

        assert!(is_active(&client.activate(&target).unwrap()));
        assert!(events.any(|event| is_active(&event.unwrap().into_snapshot())));
    }

    #[test]
    fn clients_can_redeem_limit_resets_and_receive_events() {
        let (url, password) = start_demo();
        let client = Client::new(&url, &password);
        let mut events = client.events().unwrap();
        let _ = events.next().unwrap().unwrap();

        let refreshed = client.refresh().unwrap();
        let claude = &refreshed.providers[0];
        let target = &claude.accounts[0];
        assert!(!target.resets.is_empty());
        let grant_id = target.resets[0].id.clone();

        let updated = client.redeem_reset(&target.id, &grant_id).unwrap();
        let updated_account = updated.providers[0].accounts.iter().find(|a| a.id == target.id).unwrap();
        assert!(updated_account.resets.iter().all(|r| r.id != grant_id));
        assert!(events.any(|event| {
            let snap = event.unwrap().into_snapshot();
            snap.providers[0]
                .accounts
                .iter()
                .find(|a| a.id == target.id)
                .is_some_and(|a| a.resets.iter().all(|r| r.id != grant_id))
        }));

        let antigravity = updated.providers.iter().find(|p| p.id == "antigravity").unwrap();
        assert!(client.redeem_reset(&antigravity.accounts[0].id, "any").is_err());
    }

    #[test]
    fn clients_change_the_display_order() {
        let (url, password) = start_demo();
        let client = Client::new(&url, &password);
        let mut providers: Vec<String> =
            client.snapshot().unwrap().providers.into_iter().map(|provider| provider.id).collect();
        providers.reverse();

        let update = OrderUpdate { providers: Some(providers.clone()), accounts: None };
        let snapshot = client.update_order(&update).unwrap();

        assert_eq!(snapshot.providers.into_iter().map(|provider| provider.id).collect::<Vec<_>>(), providers);
    }

    #[test]
    fn errors_reach_the_client() {
        let (url, password) = start_demo();
        let client = Client::new(&url, &password);
        assert_eq!(client.activate("missing").unwrap_err().to_string(), "That account no longer exists.");
        assert_eq!(client.add_account("nope").unwrap_err().to_string(), "Unknown provider nope");
        let invalid = SettingsUpdate { threshold: Some(500.0), ..SettingsUpdate::default() };
        assert!(client.update_settings(&invalid).is_err());
        assert!(Client::new(&url, "wrong").snapshot().is_err());
    }

    #[test]
    fn signing_in_adds_a_usage_only_account() {
        let (url, password) = start_demo();
        let client = Client::new(&url, &password);
        let sign_in = client.begin_sign_in("antigravity", "http://127.0.0.1:1/callback").unwrap();
        assert!(sign_in.url.starts_with("http://127.0.0.1:1/callback?code=sam&state="));

        let snapshot = client.finish_sign_in("antigravity", &sign_in.state, "sam").unwrap();

        let provider = snapshot.providers.iter().find(|provider| provider.id == "antigravity").unwrap();
        assert!(!provider.switchable && provider.sign_in);
        assert!(provider.accounts.iter().any(|account| account.identity.email == "sam@example.com"));
        let again = client.finish_sign_in("antigravity", &sign_in.state, "sam").unwrap_err();
        assert_eq!(again.to_string(), "This sign-in expired or was already used. Start again.");
        assert!(client.activate(&provider.accounts[0].id).is_err());
    }

    #[test]
    fn an_open_server_needs_no_password() {
        let (url, _) = start_demo_server(true);
        assert!(Client::new(&url, "").snapshot().is_ok());
    }

    #[test]
    fn requests_from_other_sites_are_refused() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            pairs.iter().map(|(name, value)| (HeaderName::from_static(name), HeaderValue::from_static(value))).collect()
        };
        assert!(!from_another_site(&headers(&[])));
        assert!(!from_another_site(&headers(&[("sec-fetch-site", "same-origin")])));
        assert!(!from_another_site(&headers(&[("sec-fetch-site", "none")])));
        assert!(from_another_site(&headers(&[("sec-fetch-site", "cross-site")])));
        assert!(from_another_site(&headers(&[("sec-fetch-site", "same-site")])));
        assert!(!from_another_site(&headers(&[
            ("origin", "https://vps.example.ts.net"),
            ("host", "vps.example.ts.net")
        ])));
        assert!(!from_another_site(&headers(&[
            ("origin", "https://vps.example.ts.net"),
            ("host", "127.0.0.1:4590"),
            ("x-forwarded-host", "vps.example.ts.net"),
        ])));
        assert!(from_another_site(&headers(&[("origin", "https://evil.example"), ("host", "vps.example.ts.net")])));
        assert!(from_another_site(&headers(&[("origin", "null"), ("host", "vps.example.ts.net")])));
    }

    #[test]
    fn registration_is_private_and_removed_only_by_its_owner() {
        let home = tempfile::tempdir().unwrap();
        let registration = Registration {
            url: "http://127.0.0.1:1".into(),
            version: "0".into(),
            pid: 1,
            password: "a".into(),
            managed: false,
        };
        register(home.path(), &registration).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(registration_path(home.path())).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        unregister(home.path(), "b");
        assert!(registration_path(home.path()).exists());
        unregister(home.path(), "a");
        assert!(!registration_path(home.path()).exists());
    }

    #[test]
    fn stale_registration_with_reused_port_does_not_block_startup() {
        let home = tempfile::tempdir().unwrap();
        let unrelated = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stale = Registration {
            url: format!("http://{}", unrelated.local_addr().unwrap()),
            version: "0.1.0".into(),
            pid: 999_999,
            password: "old".into(),
            managed: false,
        };
        register(home.path(), &stale).unwrap();

        let home_path = home.path().to_path_buf();
        let handle = std::thread::spawn(move || {
            let options = Options {
                home: home_path,
                demo: true,
                port: 0,
                managed: false,
                open: false,
                allowed_hosts: Vec::new(),
            };
            run(options)
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let client = loop {
            if let Ok(text) = fs::read_to_string(registration_path(home.path()))
                && let Ok(reg) = serde_json::from_str::<Registration>(&text)
                && reg.password != "old"
            {
                break Client::new(&reg.url, &reg.password);
            }
            assert!(std::time::Instant::now() < deadline, "server did not replace stale registration");
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        assert!(client.info().is_ok());
        client.stop().unwrap();
        handle.join().unwrap().unwrap();
    }

    fn wait_for_server(home: &FsPath, ignore_password: Option<&str>) -> (Client, String) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = fs::read_to_string(registration_path(home))
                && let Ok(reg) = serde_json::from_str::<Registration>(&text)
                && ignore_password != Some(reg.password.as_str())
            {
                let client = Client::new(&reg.url, &reg.password);
                if client.info().is_ok() {
                    return (client, reg.password);
                }
            }
            assert!(std::time::Instant::now() < deadline, "server did not become ready");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn second_server_does_not_open_engine() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let home = tempfile::tempdir().unwrap();
        let opens = Arc::new(AtomicUsize::new(0));

        let home1 = home.path().to_path_buf();
        let opens1 = opens.clone();
        let handle = std::thread::spawn(move || {
            let options =
                Options { home: home1, demo: true, port: 0, managed: false, open: false, allowed_hosts: Vec::new() };
            run_with(options, Duration::from_secs(2), move || {
                opens1.fetch_add(1, Ordering::SeqCst);
                Demo::engine()
            })
        });

        let (client, _) = wait_for_server(home.path(), None);
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        let opens2 = opens.clone();
        let options2 = Options {
            home: home.path().to_path_buf(),
            demo: true,
            port: 0,
            managed: false,
            open: false,
            allowed_hosts: Vec::new(),
        };
        let second = run_with(options2, Duration::from_secs(2), move || {
            opens2.fetch_add(1, Ordering::SeqCst);
            Demo::engine()
        });
        assert!(second.is_err());
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        client.stop().unwrap();
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn server_can_start_after_stop() {
        let home = tempfile::tempdir().unwrap();

        let home1 = home.path().to_path_buf();
        let handle1 = std::thread::spawn(move || {
            let options =
                Options { home: home1, demo: true, port: 0, managed: false, open: false, allowed_hosts: Vec::new() };
            run_with(options, Duration::from_secs(2), Demo::engine)
        });
        let (client1, pw1) = wait_for_server(home.path(), None);
        client1.stop().unwrap();

        let home2 = home.path().to_path_buf();
        let handle2 = std::thread::spawn(move || {
            let options =
                Options { home: home2, demo: true, port: 0, managed: false, open: false, allowed_hosts: Vec::new() };
            run_with(options, Duration::from_secs(5), Demo::engine)
        });
        let (client2, _) = wait_for_server(home.path(), Some(&pw1));
        assert!(client2.info().is_ok());

        client2.stop().unwrap();
        handle1.join().unwrap().unwrap();
        handle2.join().unwrap().unwrap();
    }

    struct PanickingProvider(Demo);

    impl Provider for PanickingProvider {
        fn id(&self) -> &'static str {
            self.0.id()
        }

        fn name(&self) -> &'static str {
            self.0.name()
        }

        fn live(&self) -> Result<Option<switch_core::Login>> {
            self.0.live()
        }

        fn lock(&self) -> Result<Box<dyn std::any::Any>> {
            self.0.lock()
        }

        fn replace_live(&self, secret: &switch_core::Secret) -> Result<()> {
            self.0.replace_live(secret)
        }

        fn profile(&self, _: &switch_core::Secret) -> Result<switch_core::Profile, switch_core::RemoteError> {
            panic!("boom");
        }

        fn usage(
            &self,
            secret: &switch_core::Secret,
        ) -> Result<Vec<switch_protocol::UsageWindow>, switch_core::RemoteError> {
            self.0.usage(secret)
        }

        fn renew(&self, secret: &switch_core::Secret) -> Result<switch_core::Secret, switch_core::RemoteError> {
            self.0.renew(secret)
        }

        fn expired(&self, secret: &switch_core::Secret) -> bool {
            self.0.expired(secret)
        }

        fn lineage(&self, secret: &switch_core::Secret) -> Option<String> {
            self.0.lineage(secret)
        }
    }

    #[test]
    fn engine_panic_shuts_down_server_and_unregisters() {
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path().to_path_buf();
        let handle = std::thread::spawn(move || {
            let options = Options {
                home: home_path,
                demo: true,
                port: 0,
                managed: false,
                open: false,
                allowed_hosts: Vec::new(),
            };
            run_with(options, Duration::from_secs(2), || {
                let provider = Arc::new(PanickingProvider(Demo::new("claude", "Claude")));
                provider.0.set_live(Some(provider.0.login("ada@example.com", "Pro")));
                Engine::new(vec![provider], Store::memory(), Vault::memory())
            })
        });

        let (client, _) = wait_for_server(home.path(), None);
        assert!(client.add_account("claude").is_err());
        assert!(handle.join().unwrap().is_err());
        assert!(!registration_path(home.path()).exists());
    }

    #[test]
    fn loads_optional_server_config() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(load_config(home.path()).unwrap(), ServerConfig::default());

        fs::write(
            home.path().join("server.json"),
            r#"{"port": 4590, "no_auth": true, "allowed_hosts": ["switch.internal"]}"#,
        )
        .unwrap();
        assert_eq!(
            load_config(home.path()).unwrap(),
            ServerConfig { port: Some(4590), no_auth: Some(true), allowed_hosts: vec!["switch.internal".into()] }
        );
    }

    #[test]
    fn host_check_blocks_dns_rebinding_and_allows_tailscale() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            pairs.iter().map(|(name, value)| (HeaderName::from_static(name), HeaderValue::from_static(value))).collect()
        };
        assert!(!allowed_host(&headers(&[]), &[]));
        assert!(allowed_host(&headers(&[("host", "127.0.0.1:4590")]), &[]));
        assert!(allowed_host(&headers(&[("host", "localhost:4590")]), &[]));
        assert!(allowed_host(&headers(&[("host", "[::1]:4590")]), &[]));
        assert!(allowed_host(&headers(&[("host", "vps.tailnet.ts.net")]), &[]));
        assert!(allowed_host(&headers(&[("host", "127.0.0.1:4590"), ("x-forwarded-host", "vps.tailnet.ts.net")]), &[]));
        assert!(!allowed_host(&headers(&[("host", "evil.example:4590")]), &[]));
        assert!(!allowed_host(&headers(&[("host", "127.0.0.1:4590"), ("x-forwarded-host", "evil.example")]), &[]));
        assert!(allowed_host(&headers(&[("host", "switch.internal:4590")]), &["switch.internal".to_owned()]));
    }
}
