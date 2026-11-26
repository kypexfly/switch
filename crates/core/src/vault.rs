use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::files::{read_optional, write_atomic};
use crate::keychain;
use crate::provider::Secret;

const SERVICE: &str = "dev.switch.accounts";

enum Backend {
    Keychain,
    Files,
    Memory,
}

#[derive(Default)]
struct VaultState {
    cache: HashMap<String, Secret>,
    unsaved: HashSet<String>,
}

/// Saved logins by account id.
pub struct Vault {
    backend: Backend,
    dir: PathBuf,
    state: Mutex<VaultState>,
}

impl Vault {
    pub fn keychain(dir: impl Into<PathBuf>) -> Self {
        Self::new(Backend::Keychain, dir.into())
    }

    pub fn files(dir: impl Into<PathBuf>) -> Self {
        Self::new(Backend::Files, dir.into())
    }

    pub fn memory() -> Self {
        Self::new(Backend::Memory, PathBuf::new())
    }

    fn new(backend: Backend, dir: PathBuf) -> Self {
        Self { backend, dir, state: Mutex::default() }
    }

    pub fn get(&self, id: &str) -> Result<Option<Secret>> {
        if let Some(secret) = self.state.lock().unwrap().cache.get(id) {
            return Ok(Some(secret.clone()));
        }
        let value = match self.backend {
            Backend::Memory => None,
            Backend::Files => read_optional(&self.file(id))?,
            Backend::Keychain => match read_optional(&self.file(id))? {
                Some(recovered) => Some(recovered),
                None => keychain::get(SERVICE, id)?,
            },
        };
        let secret = value.map(Secret::new);
        if let Some(secret) = &secret {
            self.state.lock().unwrap().cache.insert(id.to_owned(), secret.clone());
        }
        Ok(secret)
    }

    /// Saves `secret`, keeping the one it replaces as `<id>.previous`.
    pub fn put(&self, id: &str, secret: &Secret) -> Result<()> {
        let previous = self.get(id).ok().flatten();
        let previous_key = format!("{id}.previous");
        if previous.as_ref() == Some(secret) {
            let (id_unsaved, prev_to_save) = {
                let state = self.state.lock().unwrap();
                let id_unsaved = state.unsaved.contains(id);
                let prev =
                    state.unsaved.contains(&previous_key).then(|| state.cache.get(&previous_key).cloned()).flatten();
                (id_unsaved, prev)
            };
            if !id_unsaved && prev_to_save.is_none() {
                return Ok(());
            }
            if let Some(prev) = prev_to_save
                && self.persist(&previous_key, &prev).is_ok()
            {
                self.state.lock().unwrap().unsaved.remove(&previous_key);
            }
            if id_unsaved {
                self.persist(id, secret)?;
                self.state.lock().unwrap().unsaved.remove(id);
            }
            return Ok(());
        }
        // Cached first, so a secret that cannot be persisted still works until the server exits.
        {
            let mut state = self.state.lock().unwrap();
            state.cache.insert(id.to_owned(), secret.clone());
            state.unsaved.insert(id.to_owned());
            if let Some(prev) = &previous {
                state.cache.insert(previous_key.clone(), prev.clone());
                state.unsaved.insert(previous_key.clone());
            }
        }
        if let Some(prev) = &previous
            && self.persist(&previous_key, prev).is_ok()
        {
            self.state.lock().unwrap().unsaved.remove(&previous_key);
        }
        self.persist(id, secret)?;
        self.state.lock().unwrap().unsaved.remove(id);
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        for key in [id.to_owned(), format!("{id}.previous")] {
            if let Backend::Keychain = self.backend {
                keychain::delete(SERVICE, &key)?;
            }
            match std::fs::remove_file(self.file(&key)) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
                _ => {}
            }
            let mut state = self.state.lock().unwrap();
            state.cache.remove(&key);
            state.unsaved.remove(&key);
        }
        Ok(())
    }

    fn persist(&self, key: &str, secret: &Secret) -> Result<()> {
        match self.backend {
            Backend::Memory => Ok(()),
            Backend::Files => write_atomic(&self.file(key), secret.expose()),
            Backend::Keychain => match keychain::set(SERVICE, key, secret.expose()) {
                Ok(()) => {
                    std::fs::remove_file(self.file(key)).ok();
                    Ok(())
                }
                // Losing a rotated refresh token signs the account out, so fall back to a private file.
                Err(_) => write_atomic(&self.file(key), secret.expose())
                    .context("could not save the login to the Keychain or to disk"),
            },
        }
    }

    fn file(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_replaced_secret() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::files(dir.path());
        vault.put("a", &Secret::new("one")).unwrap();
        vault.put("a", &Secret::new("two")).unwrap();

        let reopened = Vault::files(dir.path());
        assert_eq!(reopened.get("a").unwrap().unwrap().expose(), "two");
        assert_eq!(reopened.get("a.previous").unwrap().unwrap().expose(), "one");

        reopened.remove("a").unwrap();
        assert!(Vault::files(dir.path()).get("a").unwrap().is_none());
        assert!(Vault::files(dir.path()).get("a.previous").unwrap().is_none());
    }

    #[test]
    fn saving_the_same_secret_keeps_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::files(dir.path());
        vault.put("a", &Secret::new("one")).unwrap();
        vault.put("a", &Secret::new("two")).unwrap();
        vault.put("a", &Secret::new("two")).unwrap();
        assert_eq!(vault.get("a.previous").unwrap().unwrap().expose(), "one");
    }

    #[test]
    fn saves_new_secret_even_when_backup_fails() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::files(dir.path());
        vault.put("a", &Secret::new("one")).unwrap();
        std::fs::create_dir(dir.path().join("a.previous.json")).unwrap();

        vault.put("a", &Secret::new("two")).unwrap();

        let reopened = Vault::files(dir.path());
        assert_eq!(reopened.get("a").unwrap().unwrap().expose(), "two");
    }

    #[test]
    fn retries_unsaved_secret_without_clobbering_previous() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::files(dir.path());
        vault.put("a", &Secret::new("one")).unwrap();
        std::fs::remove_file(dir.path().join("a.json")).unwrap();
        std::fs::create_dir(dir.path().join("a.json")).unwrap();

        assert!(vault.put("a", &Secret::new("two")).is_err());
        assert_eq!(vault.get("a").unwrap().unwrap().expose(), "two");

        std::fs::remove_dir(dir.path().join("a.json")).unwrap();
        vault.put("a", &Secret::new("two")).unwrap();

        let reopened = Vault::files(dir.path());
        assert_eq!(reopened.get("a").unwrap().unwrap().expose(), "two");
        assert_eq!(reopened.get("a.previous").unwrap().unwrap().expose(), "one");
    }
}
