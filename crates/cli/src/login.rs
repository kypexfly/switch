use std::io::BufRead;
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use anyhow::{Result, anyhow};
use switch_client::{Client, oauth};
use switch_protocol::Snapshot;

/// Signs in through a browser on this machine. The server may run elsewhere; it only sees the code.
pub fn run(client: &Client, provider: &str) -> Result<Snapshot> {
    let redirect_uri = oauth::redirect_uri();
    let listener = TcpListener::bind(("127.0.0.1", oauth::PORT));
    let sign_in = client.begin_sign_in(provider, &redirect_uri)?;
    eprintln!("Open this page to sign in:\n\n  {}\n", sign_in.url);

    let (sender, receiver) = mpsc::channel();
    match listener {
        Ok(listener) => {
            if !over_ssh() {
                oauth::open_in_browser(&sign_in.url);
            }
            let (sender, state) = (sender.clone(), sign_in.state.clone());
            thread::spawn(move || sender.send(oauth::wait_for_callback(&listener, &state, None)).ok());
            eprintln!("Waiting for the browser. If it can not reach this machine, paste the address it ends up on:");
        }
        Err(error) => eprintln!(
            "Could not listen on {redirect_uri} ({error}). After signing in, paste the address the browser ends up on:"
        ),
    }
    let state = sign_in.state.clone();
    thread::spawn(move || {
        let mut lines = std::io::stdin().lock().lines().map_while(Result::ok);
        if let Some(line) = lines.find(|line| !line.trim().is_empty()) {
            sender.send(oauth::pasted_code(&line, &state)).ok();
        }
    });
    let code = receiver.recv().map_err(|_| anyhow!("Sign-in was cancelled."))??;
    client.finish_sign_in(provider, &sign_in.state, &code)
}

fn over_ssh() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}
