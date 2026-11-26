#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod connection;
#[cfg(all(test, target_os = "macos"))]
mod fixtures;
#[cfg(target_os = "macos")]
mod format;
#[cfg(target_os = "macos")]
mod icons;
#[cfg(target_os = "macos")]
mod launch_at_login;
#[cfg(target_os = "macos")]
mod notify;
#[cfg(target_os = "macos")]
mod panel;
#[cfg(target_os = "macos")]
mod placement;
#[cfg(target_os = "macos")]
mod preferences;
#[cfg(target_os = "macos")]
mod reorder;
#[cfg(target_os = "macos")]
mod settings;
#[cfg(all(test, target_os = "macos"))]
mod snapshots;
#[cfg(target_os = "macos")]
mod status;
#[cfg(target_os = "macos")]
mod tray_image;

#[cfg(target_os = "macos")]
fn main() {
    app::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("switch-desktop only runs on macOS");
    std::process::exit(1);
}
