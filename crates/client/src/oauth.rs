use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

/// The loopback address the Google sign-in client accepts.
pub const PORT: u16 = 51121;
pub const CALLBACK_PATH: &str = "/oauth-callback";
const SIGNED_IN: &str = "Signed in to Switch. You can close this tab.";

pub fn redirect_uri() -> String {
    format!("http://127.0.0.1:{PORT}{CALLBACK_PATH}")
}

pub fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    Command::new(opener).arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
}

pub fn wait_for_callback(listener: &TcpListener, state: &str, timeout: Option<Duration>) -> Result<String> {
    if timeout.is_some() {
        listener.set_nonblocking(true)?;
    }
    let deadline = timeout.map(|limit| Instant::now() + limit);
    loop {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if deadline.is_some_and(|limit| Instant::now() >= limit) {
                    bail!("Sign-in timed out.");
                }
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(_) => continue,
        };
        if timeout.is_some() {
            stream.set_nonblocking(false).ok();
        }
        if let Some(code) = handle_stream(stream, state)? {
            return Ok(code);
        }
    }
}

fn handle_stream(mut stream: TcpStream, state: &str) -> Result<Option<String>> {
    // Browsers open spare connections that never send a request.
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let Some(request_line) = read_request_line(&stream) else { return Ok(None) };
    match callback(&request_line, state) {
        Callback::Code(code) => {
            respond(&mut stream, "200 OK", SIGNED_IN);
            Ok(Some(code))
        }
        Callback::Refused(error) => {
            respond(&mut stream, "200 OK", "Sign-in did not finish. You can close this tab.");
            bail!("Sign-in did not finish: {error}");
        }
        Callback::Ignored(status, message) => {
            respond(&mut stream, status, message);
            Ok(None)
        }
    }
}

fn read_request_line(stream: &TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut header = String::new();
    while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
        header.clear();
    }
    Some(request_line)
}

fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!("<!doctype html><meta charset=\"utf-8\"><title>Switch</title><p>{message}</p>\n");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).ok();
}

#[derive(Debug, PartialEq)]
enum Callback {
    Code(String),
    Refused(String),
    Ignored(&'static str, &'static str),
}

fn callback(request_line: &str, state: &str) -> Callback {
    let target = request_line.split_whitespace().nth(1).unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK_PATH {
        return Callback::Ignored("404 Not Found", "Not found.");
    }
    let parameter = |name: &str| {
        form_urlencoded::parse(query.as_bytes()).find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
    };
    if parameter("state").as_deref() != Some(state) {
        return Callback::Ignored("400 Bad Request", "This page is from another sign-in. Start again.");
    }
    if let Some(error) = parameter("error") {
        return Callback::Refused(error);
    }
    match parameter("code").filter(|code| !code.is_empty()) {
        Some(code) => Callback::Code(code),
        None => Callback::Ignored("400 Bad Request", "The sign-in sent no code. Start again."),
    }
}

/// Accepts the whole address the browser ended up on, `code#state`, or just the code.
pub fn pasted_code(pasted: &str, state: &str) -> Result<String> {
    let pasted = pasted.trim();
    let (code, pasted_state) = if pasted.starts_with("http://") || pasted.starts_with("https://") {
        let query = pasted.split_once('?').map(|(_, query)| query).unwrap_or_default();
        let query = query.split('#').next().unwrap_or_default();
        let parameter = |name: &str| {
            form_urlencoded::parse(query.as_bytes()).find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
        };
        if let Some(error) = parameter("error") {
            bail!("Sign-in did not finish: {error}");
        }
        (parameter("code"), parameter("state"))
    } else if let Some((code, pasted_state)) = pasted.split_once('#') {
        (Some(code.to_owned()), Some(pasted_state.to_owned()))
    } else {
        (Some(pasted.to_owned()), None)
    };
    let code = code.filter(|code| !code.is_empty()).ok_or_else(|| anyhow!("That has no sign-in code."))?;
    if pasted_state.is_some_and(|pasted_state| pasted_state != state) {
        bail!("That address is from another sign-in. Start again.");
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_callback_needs_its_state_and_a_code() {
        let request = |target: &str| format!("GET {target} HTTP/1.1\r\n");
        assert_eq!(
            callback(&request("/oauth-callback?state=s1&code=4%2F0Ab-x&scope=email"), "s1"),
            Callback::Code("4/0Ab-x".into())
        );
        assert_eq!(
            callback(&request("/oauth-callback?state=s1&error=access_denied"), "s1"),
            Callback::Refused("access_denied".into())
        );
        assert!(matches!(
            callback(&request("/oauth-callback?state=s2&code=x"), "s1"),
            Callback::Ignored("400 Bad Request", _)
        ));
        assert!(matches!(
            callback(&request("/oauth-callback?state=s1"), "s1"),
            Callback::Ignored("400 Bad Request", _)
        ));
        assert!(matches!(callback(&request("/favicon.ico"), "s1"), Callback::Ignored("404 Not Found", _)));
        assert!(matches!(callback("", "s1"), Callback::Ignored(..)));
    }

    #[test]
    fn a_pasted_address_or_code_is_read() {
        let address = "http://127.0.0.1:51121/oauth-callback?state=s1&code=4%2F0Ab&scope=x";
        assert_eq!(pasted_code(&format!("  {address}\n"), "s1").unwrap(), "4/0Ab");
        assert_eq!(pasted_code("abc#s1", "s1").unwrap(), "abc");
        assert_eq!(pasted_code("abc\n", "s1").unwrap(), "abc");
        assert!(pasted_code("http://127.0.0.1:51121/oauth-callback?state=s2&code=x", "s1").is_err());
        assert!(pasted_code("http://127.0.0.1:51121/oauth-callback?state=s1&error=access_denied", "s1").is_err());
        assert!(pasted_code("abc#s2", "s1").is_err());
        assert!(pasted_code("  ", "s1").is_err());
    }
}
