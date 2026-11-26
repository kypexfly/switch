use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use opentui::app::ProxyError;
use opentui::prelude::AppProxy;
use switch_client::Client;
use switch_protocol::Snapshot;

use crate::model::Action;

const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const FULL_QUEUE_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub enum Update {
    Snapshot(Snapshot),
    Disconnected { reason: String },
    Finished { error: Option<String> },
}

/// Returns whether the app is still running.
pub fn deliver(proxy: &AppProxy, update: Update) -> bool {
    loop {
        match proxy.send(update.clone()) {
            Ok(()) => return true,
            Err(ProxyError::Closed) => return false,
            Err(ProxyError::Full) => thread::sleep(FULL_QUEUE_DELAY),
        }
    }
}

pub fn follow_server(
    connect: impl Fn() -> Result<Client>,
    current: &Mutex<Option<Client>>,
    deliver: impl Fn(Update) -> bool,
) {
    loop {
        let reason = match follow_connection(&connect, current, &deliver) {
            Ok(()) => "The server closed the connection".to_owned(),
            Err(error) => format!("{error:#}"),
        };
        if !deliver(Update::Disconnected { reason }) {
            return;
        }
        thread::sleep(RECONNECT_DELAY);
    }
}

fn follow_connection(
    connect: impl Fn() -> Result<Client>,
    current: &Mutex<Option<Client>>,
    deliver: impl Fn(Update) -> bool,
) -> Result<()> {
    let client = connect()?;
    let events = client.events()?;
    *current.lock().unwrap_or_else(PoisonError::into_inner) = Some(client);
    for event in events {
        if !deliver(Update::Snapshot(event?.into_snapshot())) {
            break;
        }
    }
    Ok(())
}

pub fn run_action(action: Action, current: Arc<Mutex<Option<Client>>>, proxy: AppProxy) {
    thread::spawn(move || {
        let client = current.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let result = client.context("not connected to the server").and_then(|client| perform(action, &client));
        deliver(&proxy, Update::Finished { error: result.err().map(|error| format!("{error:#}")) });
    });
}

// Results arrive as snapshots through the event stream, so responses cannot overwrite a newer state.
fn perform(action: Action, client: &Client) -> Result<()> {
    match action {
        Action::Activate { account, .. } => client.activate(&account).map(drop),
        Action::RedeemReset { account, grant_id, .. } => client.redeem_reset(&account, &grant_id).map(drop),
        Action::Switch { provider, strategy } => client.switch(&provider, strategy).map(drop),
        Action::Add { provider } => client.add_account(&provider).map(drop),
        Action::Refresh => client.refresh().map(drop),
        Action::Update { account, update } => client.update_account(&account, &update).map(drop),
        Action::Remove { account, .. } => client.remove_account(&account).map(drop),
        Action::Order(update) => client.update_order(&update).map(drop),
        Action::Settings(update) => client.update_settings(&update).map(drop),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use switch_protocol::Event;

    use super::*;
    use crate::fixtures;

    #[test]
    fn streams_snapshots_and_reports_the_end_of_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let snapshot = fixtures::snapshot();
        let event = serde_json::to_string(&Event::Connected { snapshot: snapshot.clone() }).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            for line in BufReader::new(stream.try_clone().unwrap()).lines() {
                if line.unwrap().is_empty() {
                    break;
                }
            }
            write!(stream, "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\ndata: {event}\n\n").unwrap();
        });

        let current = Mutex::new(None);
        let updates = RefCell::new(Vec::new());
        follow_server(
            || Ok(Client::new(base.clone(), "secret")),
            &current,
            |update| {
                let running = !matches!(update, Update::Disconnected { .. });
                updates.borrow_mut().push(update);
                running
            },
        );
        server.join().unwrap();

        assert!(current.lock().unwrap().is_some());
        let updates = updates.into_inner();
        assert!(matches!(&updates[0], Update::Snapshot(received) if *received == snapshot));
        assert!(matches!(&updates[1], Update::Disconnected { reason } if reason.contains("closed the connection")));
    }

    #[test]
    fn reports_why_connecting_failed() {
        let reason = RefCell::new(String::new());
        follow_server(
            || anyhow::bail!("no server found"),
            &Mutex::new(None),
            |update| {
                if let Update::Disconnected { reason: failure } = update {
                    *reason.borrow_mut() = failure;
                }
                false
            },
        );
        assert_eq!(reason.into_inner(), "no server found");
    }
}
