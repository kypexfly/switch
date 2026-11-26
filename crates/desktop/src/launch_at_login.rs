use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const LABEL: &str = "dev.switch.desktop";
const INSTALLED_EXE: &str = "/Applications/Switch.app/Contents/MacOS/switch-desktop";

pub fn enabled() -> bool {
    plist_path().is_some_and(|path| enabled_at(&path))
}

pub fn set_enabled(on: bool) -> Result<()> {
    let path = plist_path().context("could not locate the home directory")?;
    let exe = executable_path()?;
    set_enabled_at(&path, &exe, on)
}

pub fn enabled_at(path: &Path) -> bool {
    path.is_file()
}

pub fn set_enabled_at(path: &Path, exe: &Path, on: bool) -> Result<()> {
    if !on {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("could not remove {}", path.display())),
        };
    }
    let parent = path.parent().context("launch agent path has no parent")?;
    fs::create_dir_all(parent)?;
    let contents = plist(exe);
    let temporary = path.with_extension("plist.tmp");
    fs::write(&temporary, contents)?;
    fs::rename(&temporary, path).with_context(|| format!("could not write {}", path.display()))
}

fn plist_path() -> Option<PathBuf> {
    let home = std::env::home_dir()?;
    Some(home.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn executable_path() -> Result<PathBuf> {
    let current = std::env::current_exe().context("could not find the current executable")?;
    let installed = Path::new(INSTALLED_EXE);
    if current == installed || (current.to_string_lossy().contains(".app/Contents/MacOS/") && installed.is_file()) {
        return Ok(installed.to_path_buf());
    }
    Ok(current)
}

fn plist(exe: &Path) -> String {
    let escaped = escape_xml(&exe.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
    <key>Label</key>\n\
    <string>{LABEL}</string>\n\
    <key>ProgramArguments</key>\n\
    <array>\n\
        <string>{escaped}</string>\n\
    </array>\n\
    <key>RunAtLoad</key>\n\
    <true/>\n\
</dict>\n\
</plist>\n"
    )
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_removes_launch_agent_plist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("LaunchAgents/dev.switch.desktop.plist");
        let exe = Path::new("/Applications/Switch & Co.app/Contents/MacOS/switch-desktop");

        assert!(!enabled_at(&path));
        set_enabled_at(&path, exe, true).unwrap();
        assert!(enabled_at(&path));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("<string>dev.switch.desktop</string>"));
        assert!(text.contains("<string>/Applications/Switch &amp; Co.app/Contents/MacOS/switch-desktop</string>"));
        assert!(text.contains("<key>RunAtLoad</key>"));

        set_enabled_at(&path, exe, false).unwrap();
        assert!(!enabled_at(&path));
        set_enabled_at(&path, exe, false).unwrap();
    }
}
