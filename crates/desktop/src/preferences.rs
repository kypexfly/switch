use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use switch_protocol::{Account, ProviderState, Snapshot, UsageWindow};

pub const MAX_TRAY_WINDOWS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place {
    Panel,
    MenuBar,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hidden {
    providers: BTreeSet<String>,
    accounts: BTreeSet<String>,
}

/// What this app shows. The server and the other clients don't know about it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    hidden_in_panel: Hidden,
    hidden_in_menu_bar: Hidden,
    tray_windows: BTreeMap<String, Vec<String>>,
    pub notifications: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            hidden_in_panel: Hidden::default(),
            hidden_in_menu_bar: Hidden::default(),
            tray_windows: BTreeMap::new(),
            notifications: true,
        }
    }
}

impl Preferences {
    fn hidden(&self, place: Place) -> &Hidden {
        match place {
            Place::Panel => &self.hidden_in_panel,
            Place::MenuBar => &self.hidden_in_menu_bar,
        }
    }

    fn hidden_mut(&mut self, place: Place) -> &mut Hidden {
        match place {
            Place::Panel => &mut self.hidden_in_panel,
            Place::MenuBar => &mut self.hidden_in_menu_bar,
        }
    }

    pub fn provider_on(&self, place: Place, provider: &str) -> bool {
        !self.hidden(place).providers.contains(provider)
    }

    pub fn account_on(&self, place: Place, account: &str) -> bool {
        !self.hidden(place).accounts.contains(account)
    }

    pub fn set_provider(&mut self, place: Place, provider: &str, on: bool) {
        set(&mut self.hidden_mut(place).providers, provider, on);
    }

    pub fn set_account(&mut self, place: Place, account: &str, on: bool) {
        set(&mut self.hidden_mut(place).accounts, account, on);
    }

    pub fn shows(&self, place: Place, provider: &ProviderState, account: &Account) -> bool {
        (place == Place::Panel && account.active)
            || (self.provider_on(place, &provider.id) && self.account_on(place, &account.id))
    }

    pub fn tray_accounts<'a>(&'a self, provider: &'a ProviderState) -> impl Iterator<Item = &'a Account> {
        provider.accounts.iter().filter(|account| self.shows(Place::MenuBar, provider, account))
    }

    /// `snapshot` without what is hidden in the panel. The live login always stays.
    pub fn panel_snapshot(&self, snapshot: &Snapshot) -> Snapshot {
        let mut visible = snapshot.clone();
        for (provider, all) in visible.providers.iter_mut().zip(&snapshot.providers) {
            provider.accounts.retain(|account| self.shows(Place::Panel, all, account));
        }
        let mut originals = snapshot.providers.iter();
        visible.providers.retain(|provider| {
            let had_accounts = originals.next().is_some_and(|original| !original.accounts.is_empty());
            let shown_empty = !had_accounts && self.provider_on(Place::Panel, &provider.id);
            !provider.accounts.is_empty() || provider.unsaved_login.is_some() || shown_empty
        });
        visible
    }

    /// The windows drawn in the menu bar: the chosen ones the provider still has, else its first primary ones.
    pub fn tray_windows(&self, provider: &ProviderState) -> Vec<String> {
        let windows = provider_windows(provider);
        let pick = |wanted: &dyn Fn(&UsageWindow) -> bool| -> Vec<String> {
            let wanted = windows.iter().filter(|window| wanted(window));
            wanted.take(MAX_TRAY_WINDOWS).map(|window| window.id.clone()).collect()
        };
        if let Some(chosen) = self.tray_windows.get(&provider.id) {
            let chosen = pick(&|window| chosen.contains(&window.id));
            if !chosen.is_empty() {
                return chosen;
            }
        }
        pick(&|window| window.primary)
    }

    pub fn can_toggle_tray_window(&self, provider: &ProviderState, window: &str) -> bool {
        let chosen = self.tray_windows(provider);
        if chosen.iter().any(|id| id == window) { chosen.len() > 1 } else { chosen.len() < MAX_TRAY_WINDOWS }
    }

    pub fn toggle_tray_window(&mut self, provider: &ProviderState, window: &str) {
        if !self.can_toggle_tray_window(provider, window) {
            return;
        }
        let mut chosen = self.tray_windows(provider);
        match chosen.iter().position(|id| id == window) {
            Some(index) => drop(chosen.remove(index)),
            None => chosen.push(window.to_owned()),
        }
        self.tray_windows.insert(provider.id.clone(), chosen);
    }
}

fn set(hidden: &mut BTreeSet<String>, id: &str, on: bool) {
    if on {
        hidden.remove(id);
    } else {
        hidden.insert(id.to_owned());
    }
}

pub fn provider_windows(provider: &ProviderState) -> Vec<&UsageWindow> {
    let mut windows: Vec<&UsageWindow> = Vec::new();
    for window in provider.accounts.iter().flat_map(|account| &account.usage).flat_map(|usage| &usage.windows) {
        if !windows.iter().any(|other| other.id == window.id) {
            windows.push(window);
        }
    }
    windows
}

pub fn path(home: &Path) -> PathBuf {
    home.join("desktop.json")
}

pub fn load(path: &Path) -> Result<Preferences> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Preferences::default()),
        Err(error) => return Err(error).with_context(|| format!("could not read {}", path.display())),
    };
    serde_json::from_str(&text).with_context(|| format!("could not read {}", path.display()))
}

pub fn save(path: &Path, preferences: &Preferences) -> Result<()> {
    let write = || -> Result<()> {
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_string_pretty(preferences)?)?;
        Ok(fs::rename(&temporary, path)?)
    };
    write().with_context(|| format!("could not write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn ids(snapshot: &Snapshot) -> Vec<&str> {
        let accounts = snapshot.providers.iter().flat_map(|provider| &provider.accounts);
        accounts.map(|account| account.id.as_str()).collect()
    }

    #[test]
    fn the_panel_leaves_out_hidden_accounts_but_never_the_active_one() {
        let snapshot = fixtures::two_providers();
        let mut preferences = Preferences::default();
        assert_eq!(preferences.panel_snapshot(&snapshot), snapshot);
        preferences.set_account(Place::Panel, "a1", false);
        preferences.set_account(Place::Panel, "a2", false);
        assert_eq!(ids(&preferences.panel_snapshot(&snapshot)), ["a1", "c1"]);
        preferences.set_account(Place::Panel, "a2", true);
        preferences.set_provider(Place::Panel, "claude", false);
        assert_eq!(ids(&preferences.panel_snapshot(&snapshot)), ["a1", "c1"]);
    }

    #[test]
    fn a_provider_with_nothing_left_to_show_has_no_section() {
        let mut snapshot = fixtures::usage_only();
        let mut preferences = Preferences::default();
        preferences.set_account(Place::Panel, "g1", false);
        assert_eq!(preferences.panel_snapshot(&snapshot).providers.len(), 1);
        snapshot.providers[1].accounts.clear();
        assert_eq!(preferences.panel_snapshot(&snapshot).providers.len(), 2);
        preferences.set_provider(Place::Panel, "antigravity", false);
        assert_eq!(preferences.panel_snapshot(&snapshot).providers.len(), 1);
        let unsaved = fixtures::unsaved_login();
        preferences.set_provider(Place::Panel, "claude", false);
        assert_eq!(preferences.panel_snapshot(&unsaved).providers.len(), 1);
    }

    #[test]
    fn hiding_a_provider_keeps_its_accounts_choices() {
        let snapshot = fixtures::two_providers();
        let claude = &snapshot.providers[0];
        let mut preferences = Preferences::default();
        let tray = |preferences: &Preferences| {
            preferences.tray_accounts(claude).map(|account| account.id.clone()).collect::<Vec<_>>()
        };
        preferences.set_account(Place::MenuBar, "a2", false);
        preferences.set_provider(Place::MenuBar, "claude", false);
        assert!(tray(&preferences).is_empty());
        preferences.set_provider(Place::MenuBar, "claude", true);
        assert_eq!(tray(&preferences), ["a1"]);
    }

    #[test]
    fn one_or_two_windows_show_in_the_menu_bar() {
        let snapshot = fixtures::usage_only();
        let antigravity = &snapshot.providers[1];
        let mut preferences = Preferences::default();
        assert_eq!(preferences.tray_windows(antigravity), ["gemini-5h", "gemini-weekly"]);
        preferences.toggle_tray_window(antigravity, "3p-5h");
        assert_eq!(preferences, Preferences::default());
        preferences.toggle_tray_window(antigravity, "gemini-5h");
        preferences.toggle_tray_window(antigravity, "gemini-weekly");
        assert_eq!(preferences.tray_windows(antigravity), ["gemini-weekly"]);
        preferences.toggle_tray_window(antigravity, "3p-5h");
        assert_eq!(preferences.tray_windows(antigravity), ["gemini-weekly", "3p-5h"]);
        preferences.tray_windows.insert("antigravity".into(), vec!["gone".into()]);
        assert_eq!(preferences.tray_windows(antigravity), ["gemini-5h", "gemini-weekly"]);
    }

    #[test]
    fn preferences_survive_a_restart_and_an_unreadable_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = path(dir.path());
        assert_eq!(load(&path).unwrap(), Preferences::default());
        let mut preferences = Preferences::default();
        preferences.set_account(Place::Panel, "a2", false);
        preferences.set_provider(Place::MenuBar, "claude", false);
        save(&path, &preferences).unwrap();
        assert_eq!(load(&path).unwrap(), preferences);
        fs::write(&path, "{").unwrap();
        assert!(load(&path).is_err());
    }
}
