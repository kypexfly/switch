use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use switch_client::{Client, Service};
use switch_protocol::Snapshot;

const FIRST_RETRY: Duration = Duration::from_secs(2);
const LAST_RETRY: Duration = Duration::from_secs(30);

pub enum Update {
    Snapshot(Snapshot),
    Lost(String),
}

#[derive(Clone, Default)]
pub struct SharedClient(Arc<Mutex<Option<Client>>>);

impl SharedClient {
    fn slot(&self) -> MutexGuard<'_, Option<Client>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn get(&self) -> Result<Client> {
        self.slot().clone().ok_or_else(|| anyhow!("Not connected to the server"))
    }
}

pub fn sw_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SWITCH_CLI").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("swx")))
        .filter(|sibling| sibling.is_file())
        .unwrap_or_else(|| PathBuf::from("swx"))
}

pub fn spawn_events(service: Service, shared: SharedClient, on_update: impl Fn(Update) + Send + 'static) {
    thread::spawn(move || {
        let mut retry = FIRST_RETRY;
        loop {
            let lost = match service.connect() {
                Ok(client) => {
                    *shared.slot() = Some(client.clone());
                    let lost = follow(&client, &on_update, &mut retry);
                    *shared.slot() = None;
                    lost
                }
                Err(error) => format!("Can't reach the server: {error:#}"),
            };
            on_update(Update::Lost(lost));
            thread::sleep(retry);
            retry = (retry * 2).min(LAST_RETRY);
        }
    });
}

fn follow(client: &Client, on_update: &impl Fn(Update), retry: &mut Duration) -> String {
    let events = match client.events() {
        Ok(events) => events,
        Err(error) => return format!("Can't follow the server: {error:#}"),
    };
    for event in events {
        match event {
            Ok(event) => {
                *retry = FIRST_RETRY;
                on_update(Update::Snapshot(event.into_snapshot()));
            }
            Err(error) => return format!("Lost the connection to the server: {error:#}"),
        }
    }
    "The server closed the connection".to_owned()
}
