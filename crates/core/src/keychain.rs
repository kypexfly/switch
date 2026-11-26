use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const SECURITY: &str = "/usr/bin/security";
const NOT_FOUND: i32 = 44;
const TIMEOUT: Duration = Duration::from_secs(15);

/// `security -i` silently truncates longer lines.
const STDIN_LINE_LIMIT: usize = 4000;

struct Output {
    code: Option<i32>,
    stdout: String,
}

pub fn get(service: &str, account: &str) -> Result<Option<String>> {
    let output = run(&["find-generic-password", "-a", account, "-s", service, "-w"], None)?;
    match output.code {
        Some(0) => Ok(Some(output.stdout.strip_suffix('\n').unwrap_or(&output.stdout).to_owned())),
        Some(NOT_FOUND) => Ok(None),
        code => bail!("could not read the Keychain (code {code:?})"),
    }
}

pub fn set(service: &str, account: &str, value: &str) -> Result<()> {
    let hex: String = value.bytes().map(|byte| format!("{byte:02x}")).collect();
    let line = format!("add-generic-password -U -a {} -s {} -X {hex}\n", quote(account), quote(service));
    if line.len() <= STDIN_LINE_LIMIT {
        run(&["-i"], Some(&line))?;
    } else {
        run(&["add-generic-password", "-U", "-a", account, "-s", service, "-X", &hex], None)?;
    }
    if get(service, account)?.as_deref() != Some(value) {
        bail!("the Keychain did not save the login");
    }
    Ok(())
}

pub fn delete(service: &str, account: &str) -> Result<()> {
    match run(&["delete-generic-password", "-a", account, "-s", service], None)?.code {
        Some(0 | NOT_FOUND) => Ok(()),
        code => bail!("could not delete from the Keychain (code {code:?})"),
    }
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn run(args: &[&str], input: Option<&str>) -> Result<Output> {
    let mut child = Command::new(SECURITY)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not run the security tool")?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    if let Some(input) = input {
        stdin.write_all(input.as_bytes())?;
    }
    drop(stdin);
    let mut pipe = child.stdout.take().context("no stdout")?;
    let reader = thread::spawn(move || {
        let mut stdout = String::new();
        pipe.read_to_string(&mut stdout).map(|_| stdout)
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().ok();
            child.wait().ok();
            bail!("the Keychain did not answer; it may be locked");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = reader.join().map_err(|_| anyhow::anyhow!("reader failed"))??;
    Ok(Output { code: status.code(), stdout })
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quotes_names() {
        assert_eq!(quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
