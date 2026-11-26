mod connection;
mod format;
mod model;
mod rows;
mod view;

#[cfg(test)]
mod fixtures;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use jiff::Timestamp;
use opentui::prelude::*;
use switch_client::Client;

use connection::{Update, deliver, follow_server, run_action};
use view::Screen;

pub fn run(connect: impl Fn() -> Result<Client> + Send + 'static) -> Result<()> {
    let mut options = AppOptions::default().with_theme(Theme::terminal());
    options.title = Some("Switch".to_owned());
    options.focus_on_click = false;
    let app = App::new(options)?;

    let client = Arc::new(Mutex::new(None));
    let screen = Screen::build(&app, Timestamp::now, {
        let (client, proxy) = (Arc::clone(&client), app.proxy());
        move |action| run_action(action, Arc::clone(&client), proxy.clone())
    });
    app.on_message(move |_, update: &Update| screen.apply(update));

    let proxy = app.proxy();
    std::thread::spawn(move || follow_server(connect, &client, |update| deliver(&proxy, update)));
    app.run()?;
    Ok(())
}
