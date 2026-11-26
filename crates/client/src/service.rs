use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use switch_protocol::Registration;

use crate::Client;

const START_TIMEOUT: Duration = Duration::from_secs(25);
const STOP_TIMEOUT: Duration = Duration::from_secs(40);

pub fn home_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("SWITCH_HOME").filter(|home| !home.is_empty()) {
        return PathBuf::from(home);
    }
    let home = std::env::home_dir().unwrap_or_default();
    home.join(".local/share/switch")
}

pub fn registration_path(home: &Path) -> PathBuf {
    home.join("service.json")
}

/// Finds, starts and stops the background server shared by every interface.
pub struct Service {
    home: PathBuf,
    executable: PathBuf,
}

impl Service {
    /// `executable` is the `swx` binary used to start the server.
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self { home: home_dir(), executable: executable.into() }
    }

    /// The healthy running server, if there is one.
    pub fn discover(&self) -> Option<(Client, Registration)> {
        let text = fs::read_to_string(registration_path(&self.home)).ok()?;
        let registration: Registration = serde_json::from_str(&text).ok()?;
        let client = Client::new(&registration.url, &registration.password);
        client.info().ok().map(|_| (client, registration))
    }

    /// Connects to the running server, starting it or replacing an older one as needed.
    pub fn connect(&self) -> Result<Client> {
        if let Some((client, registration)) = self.discover() {
            if registration.managed || parse_version(&registration.version) >= parse_version(env!("CARGO_PKG_VERSION"))
            {
                return Ok(client);
            }
            self.stop()?;
        }
        self.start()
    }

    pub fn start(&self) -> Result<Client> {
        if let Some((client, _)) = self.discover() {
            return Ok(client);
        }
        self.spawn()?;
        let deadline = Instant::now() + START_TIMEOUT;
        while Instant::now() < deadline {
            if let Some((client, _)) = self.discover() {
                return Ok(client);
            }
            thread::sleep(Duration::from_millis(50));
        }
        bail!("the server did not start; see {}", self.log_path().display())
    }

    /// Returns whether a server was running.
    pub fn stop(&self) -> Result<bool> {
        let Some((client, _)) = self.discover() else { return Ok(false) };
        client.stop()?;
        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline {
            if !registration_path(&self.home).exists() && !server_locked(&self.home) {
                return Ok(true);
            }
            thread::sleep(Duration::from_millis(50));
        }
        bail!("the server did not stop")
    }

    pub fn log_path(&self) -> PathBuf {
        self.home.join("server.log")
    }

    fn spawn(&self) -> Result<()> {
        fs::create_dir_all(&self.home)?;
        let log = OpenOptions::new().create(true).append(true).open(self.log_path())?;
        let mut command = Command::new(&self.executable);
        command.args(["serve", "--service"]).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        command.spawn().with_context(|| format!("could not start {}", self.executable.display()))?;
        Ok(())
    }
}

fn server_locked(home: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(home.join("server.lock")) else {
        return false;
    };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

fn parse_version(version: &str) -> Vec<u32> {
    version.split('.').map(|part| part.parse().unwrap_or(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn versions_compare_numerically() {
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
    }
}
