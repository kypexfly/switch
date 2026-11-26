use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};
use switch_protocol::Identity;

use super::locks::DirLock;
use crate::files::{read_optional, write_atomic};
use crate::keychain;
use crate::provider::{Login, Secret};

type Object = Map<String, Value>;

/// Fields that belong to this machine instead of the account, so they stay when accounts switch.
const MACHINE_FIELDS: [&str; 5] = ["mcpOAuth", "mcpOAuthClientConfig", "mcpXaaIdp", "mcpXaaIdpConfig", "pluginSecrets"];

/// Where Claude Code keeps the live login.
pub struct LiveStore {
    config_home: PathBuf,
    config_file: PathBuf,
    keychain: Option<(String, String)>,
}

enum Source {
    Keychain,
    File,
    Absent,
}

struct LiveBlob {
    source: Source,
    text: Option<String>,
    object: Object,
}

impl LiveStore {
    /// Always the default profile, whatever `CLAUDE_CONFIG_DIR` the server happened to inherit.
    pub fn for_user() -> Result<Self> {
        let home = std::env::home_dir().context("no home directory")?;
        let keychain = if cfg!(target_os = "macos") {
            let user = std::env::var("USER").context("USER is not set")?;
            Some(("Claude Code-credentials".to_owned(), user))
        } else {
            None
        };
        Ok(Self::at(&home, keychain))
    }

    pub fn at(home: &Path, keychain: Option<(String, String)>) -> Self {
        let config_home = home.join(".claude");
        let legacy = config_home.join(".config.json");
        let has_legacy = fs::metadata(&legacy).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0);
        let config_file = if has_legacy { legacy } else { home.join(".claude.json") };
        Self { config_home, config_file, keychain }
    }

    /// Same locks in the same order as Claude Code.
    pub fn lock(&self) -> Result<[DirLock; 3]> {
        const LOGIN_STALE: Duration = Duration::from_secs(60);
        const CONFIG_STALE: Duration = Duration::from_secs(10);
        Ok([
            DirLock::acquire(&self.config_home.join(".oauth_refresh.lock"), LOGIN_STALE)?,
            DirLock::acquire(&with_suffix(&self.config_home, ".lock"), LOGIN_STALE)?,
            DirLock::acquire(&with_suffix(&self.config_file, ".lock"), CONFIG_STALE)?,
        ])
    }

    pub fn read(&self) -> Result<Option<Login>> {
        let blob = self.read_blob()?;
        if !has_token(&blob.object) {
            return Ok(None);
        }
        let Some(config) = self.read_config()? else { return Ok(None) };
        let Some(account) = config.get("oauthAccount").and_then(Value::as_object) else { return Ok(None) };
        let mut login = blob.object;
        login.retain(|key, _| !MACHINE_FIELDS.contains(&key.as_str()));
        Ok(Some(Login { identity: identity(account)?, secret: pack(login, account.clone()) }))
    }

    pub fn replace(&self, secret: &Secret) -> Result<()> {
        let (mut login, account) = unpack(secret)?;
        if !has_token(&login) {
            bail!("the saved login is empty");
        }
        let before = self.read_blob()?;
        for field in MACHINE_FIELDS {
            login.remove(field);
            if let Some(value) = before.object.get(field) {
                login.insert(field.to_owned(), value.clone());
            }
        }
        let mut config = self.read_config()?.unwrap_or_default();
        config.insert("oauthAccount".to_owned(), Value::Object(account));
        let config = serde_json::to_string_pretty(&config)?;

        let written = self
            .write_blob(&before.source, &serde_json::to_string(&login)?)
            .and_then(|()| write_atomic(&self.config_file, &config));
        let Err(error) = written else { return Ok(()) };
        match self.restore(&before) {
            Ok(()) => Err(error.context("the switch failed and was undone")),
            Err(restore) => Err(error.context(format!("the switch failed and could not be undone: {restore:#}"))),
        }
    }

    fn restore(&self, before: &LiveBlob) -> Result<()> {
        match (&before.text, &before.source, &self.keychain) {
            (Some(text), source, _) => self.write_blob(source, text),
            (None, _, Some((service, user))) => keychain::delete(service, user),
            (None, _, None) => Ok(fs::remove_file(self.login_file())?),
        }
    }

    fn read_blob(&self) -> Result<LiveBlob> {
        if let Some((service, user)) = &self.keychain
            && let Some(text) = keychain::get(service, user)?
        {
            return Ok(LiveBlob { source: Source::Keychain, object: parse_object(&text)?, text: Some(text) });
        }
        match read_optional(&self.login_file())? {
            Some(text) => Ok(LiveBlob { source: Source::File, object: parse_object(&text)?, text: Some(text) }),
            None => Ok(LiveBlob { source: Source::Absent, object: Object::new(), text: None }),
        }
    }

    fn write_blob(&self, source: &Source, text: &str) -> Result<()> {
        let file = self.login_file();
        match (source, &self.keychain) {
            (Source::Keychain | Source::Absent, Some((service, user))) => {
                keychain::set(service, user, text)?;
                if file.exists() { write_atomic(&file, text) } else { Ok(()) }
            }
            _ => write_atomic(&file, text),
        }
    }

    fn read_config(&self) -> Result<Option<Object>> {
        read_optional(&self.config_file)?
            .map(|text| parse_object(&text).with_context(|| format!("could not parse {}", self.config_file.display())))
            .transpose()
    }

    fn login_file(&self) -> PathBuf {
        self.config_home.join(".credentials.json")
    }
}

pub fn pack(login: Object, account: Object) -> Secret {
    let mut object = Object::new();
    object.insert("login".to_owned(), Value::Object(login));
    object.insert("account".to_owned(), Value::Object(account));
    Secret::new(Value::Object(object).to_string())
}

pub fn unpack(secret: &Secret) -> Result<(Object, Object)> {
    let mut object = parse_object(secret.expose())?;
    let mut take = |key: &str| match object.remove(key) {
        Some(Value::Object(value)) => Ok(value),
        _ => Err(anyhow!("the saved login is damaged")),
    };
    Ok((take("login")?, take("account")?))
}

/// The error must not quote `text`, which can hold tokens.
fn parse_object(text: &str) -> Result<Object> {
    serde_json::from_str(text).map_err(|_| anyhow!("expected a JSON object"))
}

fn has_token(login: &Object) -> bool {
    let token = |key: &str| {
        login.get("claudeAiOauth").and_then(|oauth| oauth.get(key)).and_then(Value::as_str).unwrap_or_default()
    };
    !token("accessToken").is_empty() || !token("refreshToken").is_empty()
}

fn identity(account: &Object) -> Result<Identity> {
    let text = |key: &str| account.get(key).and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_owned);
    Ok(Identity {
        account_id: text("accountUuid").context("Claude Code's config has no account id")?,
        email: text("emailAddress").context("Claude Code's config has no email")?,
        organization_id: text("organizationUuid"),
        organization: text("organizationName"),
    })
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> Object {
        value.as_object().unwrap().clone()
    }

    fn seed(home: &Path) -> LiveStore {
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(
            home.join(".claude/.credentials.json"),
            json!({
                "claudeAiOauth": {"accessToken": "at-a", "refreshToken": "rt-a"},
                "trustedDeviceToken": "device-a",
                "mcpOAuth": {"server": "live"}
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            home.join(".claude.json"),
            json!({
                "zeta": 1,
                "oauthAccount": {"accountUuid": "a", "emailAddress": "a@example.com", "organizationUuid": "org"},
                "alpha": {"cost": 0.1}
            })
            .to_string(),
        )
        .unwrap();
        LiveStore::at(home, None)
    }

    fn other() -> Secret {
        pack(
            object(json!({
                "claudeAiOauth": {"accessToken": "at-b", "refreshToken": "rt-b"},
                "mcpOAuth": {"server": "old"}
            })),
            object(json!({"accountUuid": "b", "emailAddress": "b@example.com"})),
        )
    }

    fn read_json(path: PathBuf) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn reading_leaves_machine_fields_out() {
        let home = tempfile::tempdir().unwrap();
        let login = seed(home.path()).read().unwrap().unwrap();
        assert_eq!(login.identity.email, "a@example.com");
        assert!(!login.secret.expose().contains("mcpOAuth"));
        assert!(login.secret.expose().contains("device-a"));
    }

    #[test]
    fn replacing_swaps_only_what_belongs_to_the_account() {
        let home = tempfile::tempdir().unwrap();
        let store = seed(home.path());
        store.replace(&other()).unwrap();

        let login = read_json(home.path().join(".claude/.credentials.json"));
        assert_eq!(login["claudeAiOauth"]["refreshToken"], "rt-b");
        assert_eq!(login["mcpOAuth"]["server"], "live");
        assert!(login.get("trustedDeviceToken").is_none());

        let config = read_json(home.path().join(".claude.json"));
        assert_eq!(config["oauthAccount"]["accountUuid"], "b");
        assert_eq!(config["alpha"]["cost"], 0.1);
        let keys: Vec<_> = config.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["zeta", "oauthAccount", "alpha"]);
    }

    #[test]
    fn a_switch_round_trips() {
        let home = tempfile::tempdir().unwrap();
        let store = seed(home.path());
        let original = store.read().unwrap().unwrap().secret;
        store.replace(&other()).unwrap();
        store.replace(&original).unwrap();
        assert_eq!(store.read().unwrap().unwrap().secret, original);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_config_write_restores_the_login() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let store = seed(home.path());
        let before = fs::read_to_string(home.path().join(".claude/.credentials.json")).unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let result = store.replace(&other());
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();

        assert!(format!("{:#}", result.unwrap_err()).contains("was undone"));
        assert_eq!(fs::read_to_string(home.path().join(".claude/.credentials.json")).unwrap(), before);
    }

    #[test]
    fn a_damaged_config_is_never_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let store = seed(home.path());
        fs::write(home.path().join(".claude.json"), "{\"projects\":").unwrap();
        assert!(store.replace(&other()).is_err());
        assert_eq!(fs::read_to_string(home.path().join(".claude.json")).unwrap(), "{\"projects\":");
        assert!(fs::read_to_string(home.path().join(".claude/.credentials.json")).unwrap().contains("rt-a"));
    }

    #[test]
    fn an_emptied_login_counts_as_signed_out() {
        let home = tempfile::tempdir().unwrap();
        let store = seed(home.path());
        fs::write(
            home.path().join(".claude/.credentials.json"),
            json!({"claudeAiOauth": {"accessToken": "", "refreshToken": ""}}).to_string(),
        )
        .unwrap();
        assert!(store.read().unwrap().is_none());
    }

    #[test]
    fn errors_never_quote_the_login() {
        let error = parse_object("sk-ant-secret").unwrap_err();
        assert!(!format!("{error:?}").contains("sk-ant"));
    }

    #[test]
    fn locks_are_released() {
        let home = tempfile::tempdir().unwrap();
        let store = seed(home.path());
        let locks = store.lock().unwrap();
        assert!(home.path().join(".claude.lock").exists());
        assert!(home.path().join(".claude.json.lock").exists());
        drop(locks);
        assert!(!home.path().join(".claude.lock").exists());
        assert!(!home.path().join(".claude/.oauth_refresh.lock").exists());
    }
}
