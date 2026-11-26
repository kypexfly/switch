mod login;
mod render;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use jiff::Timestamp;
use switch_client::{Client, Service, home_dir};
use switch_protocol::{Account, AccountUpdate, LimitReset, Settings, SettingsUpdate, Snapshot, Strategy};

use crate::render::{Clock, Style};

#[derive(Parser)]
#[command(name = "swx", version, about = "Switch between accounts of AI coding tools")]
struct Cli {
    /// Print JSON instead of formatted text
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the terminal dashboard (the default)
    Tui,
    /// Run the server in the foreground
    Serve {
        /// Started by a client, which may replace it with a newer version
        #[arg(long, hide = true)]
        service: bool,
        /// Serve made-up accounts instead of real logins
        #[arg(long)]
        demo: bool,
        /// Port on 127.0.0.1; 0 picks a free one
        #[arg(long)]
        port: Option<u16>,
        /// Accept requests without the password, for a port reachable only from a private network such as a tailnet
        #[arg(long)]
        no_auth: bool,
    },
    /// Stop the running server
    Stop,
    /// Show accounts and usage
    Status,
    /// Save the current login of a provider as an account
    Add {
        #[arg(default_value = "claude")]
        provider: String,
    },
    /// Add an account by signing in through a browser, for providers that support it
    Login {
        /// Defaults to the only provider that supports signing in
        provider: Option<String>,
    },
    /// Switch to the next or least used account
    Switch {
        #[arg(value_enum, default_value_t = Pick::Next)]
        pick: Pick,
        #[arg(long, default_value = "claude")]
        provider: String,
    },
    /// Switch to an account by number, alias, email or id
    Use { account: String },
    /// Fetch usage again
    Refresh,
    /// Use a banked limit reset on an account
    Reset {
        account: String,
        /// Reset number, id, or name (defaults to the only usable reset)
        grant: Option<String>,
        /// Use without asking for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Show or update auto-switch settings
    Config {
        #[arg(long)]
        auto_switch: Option<bool>,
        #[arg(long)]
        threshold: Option<f64>,
        #[arg(long)]
        cooldown: Option<u64>,
        #[arg(long, value_enum)]
        strategy: Option<Pick>,
    },
    /// Set or clear an account alias
    Alias {
        account: String,
        #[arg(default_value = "")]
        name: String,
    },
    /// Exclude an account from automatic and strategy switching
    Pause { account: String },
    /// Include an account in automatic and strategy switching again
    Resume { account: String },
    /// Remove a saved account and its stored credentials
    Remove {
        account: String,
        /// Remove without asking for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Pick {
    Next,
    Best,
}

impl From<Pick> for Strategy {
    fn from(pick: Pick) -> Self {
        match pick {
            Pick::Next => Self::Next,
            Pick::Best => Self::Best,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let json = cli.json;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => switch_tui::run(connect),
        Command::Serve { service, demo, port, no_auth } => {
            let home = home_dir();
            let config = switch_server::load_config(&home)?;
            switch_server::run(switch_server::Options {
                home,
                demo,
                port: port.or(config.port).unwrap_or(0),
                managed: !service,
                open: no_auth || config.no_auth.unwrap_or(false),
                allowed_hosts: config.allowed_hosts,
            })
        }
        Command::Stop => {
            let stopped = service()?.stop()?;
            if json {
                println!("{}", serde_json::json!({ "stopped": stopped }));
            } else {
                println!("{}", if stopped { "Stopped the server." } else { "The server was not running." });
            }
            Ok(())
        }
        Command::Status => print(&connect()?.snapshot()?, json),
        Command::Add { provider } => print(&connect()?.add_account(&provider)?, json),
        Command::Login { provider } => {
            let client = connect()?;
            let provider = match provider {
                Some(provider) => provider,
                None => sign_in_provider(&client.snapshot()?)?,
            };
            print(&login::run(&client, &provider)?, json)
        }
        Command::Switch { pick, provider } => {
            print_switched(&connect()?.switch(&provider, pick.into())?, json, |provider_id, _| provider_id == provider)
        }
        Command::Use { account } => {
            let client = connect()?;
            let id = find(&client.snapshot()?, &account)?.id.clone();
            print_switched(&client.activate(&id)?, json, |_, account| account.id == id)
        }
        Command::Refresh => print(&connect()?.refresh()?, json),
        Command::Reset { account, grant, yes } => {
            let client = connect()?;
            let snapshot = client.snapshot()?;
            let target = find(&snapshot, &account)?;
            let chosen = resolve_reset(target, grant.as_deref(), Timestamp::now())?;
            let (target_id, reset_id, reset_label) = (target.id.clone(), chosen.id.clone(), chosen.label.clone());
            if !yes {
                confirm_reset(target.label(), chosen)?;
            }
            let updated = client.redeem_reset(&target_id, &reset_id)?;
            if json {
                print(&updated, true)
            } else {
                let style = Style::detect();
                let updated_account = updated
                    .providers
                    .iter()
                    .flat_map(|provider| &provider.accounts)
                    .find(|account| account.id == target_id);
                if let Some(account) = updated_account {
                    println!("{}\n", render::reset_used(account, &reset_label, &style));
                }
                print(&updated, false)
            }
        }
        Command::Config { auto_switch, threshold, cooldown, strategy } => {
            let client = connect()?;
            let settings = if auto_switch.is_none() && threshold.is_none() && cooldown.is_none() && strategy.is_none() {
                client.snapshot()?.settings
            } else {
                client.update_settings(&SettingsUpdate {
                    auto_switch,
                    threshold,
                    cooldown_seconds: cooldown,
                    strategy: strategy.map(Into::into),
                })?
            };
            print_settings(&settings, json)
        }
        Command::Alias { account, name } => {
            let client = connect()?;
            let id = find(&client.snapshot()?, &account)?.id.clone();
            print(&client.update_account(&id, &AccountUpdate { alias: Some(name), enabled: None })?, json)
        }
        Command::Pause { account } => {
            let client = connect()?;
            let id = find(&client.snapshot()?, &account)?.id.clone();
            print(&client.update_account(&id, &AccountUpdate { alias: None, enabled: Some(false) })?, json)
        }
        Command::Resume { account } => {
            let client = connect()?;
            let id = find(&client.snapshot()?, &account)?.id.clone();
            print(&client.update_account(&id, &AccountUpdate { alias: None, enabled: Some(true) })?, json)
        }
        Command::Remove { account, yes } => {
            let client = connect()?;
            let snapshot = client.snapshot()?;
            let target = find(&snapshot, &account)?;
            let label = match &target.alias {
                Some(alias) => format!("{alias} ({})", target.identity.email),
                None => target.identity.email.clone(),
            };
            if !yes {
                confirm_remove(&label)?;
            }
            print(&client.remove_account(&target.id)?, json)
        }
    }
}

fn service() -> Result<Service> {
    let executable: PathBuf = std::env::current_exe().context("could not find the swx executable")?;
    Ok(Service::new(executable))
}

fn connect() -> Result<Client> {
    service()?.connect()
}

fn sign_in_provider(snapshot: &Snapshot) -> Result<String> {
    let ids: Vec<&str> =
        snapshot.providers.iter().filter(|provider| provider.sign_in).map(|provider| provider.id.as_str()).collect();
    match ids.as_slice() {
        [id] => Ok((*id).to_owned()),
        [] => bail!("No provider supports signing in through Switch."),
        _ => bail!("Name the provider to sign in to: {}.", ids.join(", ")),
    }
}

fn find<'a>(snapshot: &'a Snapshot, wanted: &str) -> Result<&'a Account> {
    let accounts: Vec<&Account> = snapshot.providers.iter().flat_map(|provider| &provider.accounts).collect();
    if let Ok(number) = wanted.parse::<usize>() {
        return number
            .checked_sub(1)
            .and_then(|index| accounts.get(index).copied())
            .with_context(|| format!("There is no account {number}."));
    }
    let matches = |account: &&&Account| {
        account.id == wanted
            || account.identity.email.eq_ignore_ascii_case(wanted)
            || account.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(wanted))
    };
    match accounts.iter().filter(matches).copied().collect::<Vec<_>>().as_slice() {
        [account] => Ok(*account),
        [] => bail!("No account matches {wanted}."),
        _ => bail!("More than one account matches {wanted}; use its number from `swx status`."),
    }
}

fn confirm_remove(label: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("Refusing to remove {label} without confirmation; pass --yes to confirm.");
    }
    eprint!("Remove {label} and its stored credentials? [y/N] ");
    read_yes()
}

fn confirm_reset(label: &str, reset: &LimitReset) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("Refusing to use {} on {label} without confirmation; pass --yes to confirm.", reset.label);
    }
    let detail = reset.detail.as_deref().unwrap_or("Refills your usage limit now. This cannot be undone.");
    eprintln!("{detail}");
    eprint!("Use {} on {label}? [y/N] ", reset.label);
    read_yes()
}

fn read_yes() -> Result<()> {
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("Cancelled.");
    }
}

fn resolve_reset<'a>(account: &'a Account, wanted: Option<&str>, now: Timestamp) -> Result<&'a LimitReset> {
    let available: Vec<&LimitReset> = account.resets.iter().filter(|reset| reset.available(now)).collect();
    if available.is_empty() {
        bail!("No limit resets are available for {}.", account.label());
    }
    let chosen = match wanted {
        None => {
            let usable: Vec<&LimitReset> = available.iter().copied().filter(|reset| reset.usable).collect();
            match usable.as_slice() {
                [only] => *only,
                [] => bail!("None of the limit resets on {} can be used right now.", account.label()),
                _ => {
                    let choices: Vec<String> = available
                        .iter()
                        .enumerate()
                        .map(|(index, reset)| format!("{} ({})", index + 1, reset.label))
                        .collect();
                    bail!("More than one reset is available on {}: {}.", account.label(), choices.join(", "));
                }
            }
        }
        Some(wanted) => {
            if let Ok(number) = wanted.parse::<usize>() {
                number
                    .checked_sub(1)
                    .and_then(|index| available.get(index).copied())
                    .with_context(|| format!("There is no reset {number} on {}.", account.label()))?
            } else {
                let matches: Vec<&LimitReset> = available
                    .iter()
                    .copied()
                    .filter(|reset| reset.id == wanted || reset.label.eq_ignore_ascii_case(wanted))
                    .collect();
                match matches.as_slice() {
                    [only] => *only,
                    [] => bail!("No limit reset matches {wanted} on {}.", account.label()),
                    _ => bail!("More than one reset matches {wanted} on {}; pass its number.", account.label()),
                }
            }
        }
    };
    if !chosen.usable {
        bail!("{} on {} cannot be used right now.", chosen.label, account.label());
    }
    Ok(chosen)
}

fn print_settings(settings: &Settings, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(settings)?);
    } else {
        let state = if settings.auto_switch { "on" } else { "off" };
        let strategy = match settings.strategy {
            Strategy::Best => "best",
            Strategy::Next => "next",
        };
        println!("auto_switch: {state}");
        println!("threshold: {}%", settings.threshold);
        println!("cooldown: {}s", settings.cooldown_seconds);
        println!("strategy: {strategy}");
    }
    Ok(())
}

fn print(snapshot: &Snapshot, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(snapshot)?);
    } else {
        print!("{}", render::snapshot(snapshot, &Style::detect(), &Clock::system()));
    }
    Ok(())
}

fn print_switched(snapshot: &Snapshot, json: bool, switched: impl Fn(&str, &Account) -> bool) -> Result<()> {
    if json {
        return print(snapshot, true);
    }
    let style = Style::detect();
    let active = snapshot.providers.iter().find_map(|provider| {
        provider.active().filter(|account| switched(&provider.id, account)).map(|account| (provider, account))
    });
    let clock = Clock::system();
    match active {
        Some((provider, account)) => {
            println!("{}\n", render::switched(account, &style));
            println!("{}", render::snapshot(snapshot, &style, &clock));
            println!("{}", render::restart_hint(&provider.name, &style));
        }
        None => print!("{}", render::snapshot(snapshot, &style, &clock)),
    }
    Ok(())
}
