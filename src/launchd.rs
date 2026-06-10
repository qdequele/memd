//! launchd integration (macOS first): generate a LaunchAgent plist that runs
//! `memd serve`, and load/unload it (PRD §11).

use crate::paths;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "com.meilisearch.memd";

/// Path to the per-user LaunchAgent plist.
fn plist_path() -> Result<PathBuf> {
    let home = directories::BaseDirs::new()
        .context("home directory")?
        .home_dir()
        .to_path_buf();
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

/// Install and load the launchd service so memd survives reboot. `exe` is the
/// `memd` binary the service should run — pass the stable installed path so the
/// plist does not reference a build-tree location.
pub fn install(exe: &std::path::Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("`memd service install` currently supports macOS launchd only");
    }
    let log = paths::log_file()?;
    let plist = plist_path()?;
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let workdir = paths::data_dir()?;
    let contents = render_plist(
        &exe.to_string_lossy(),
        &log.to_string_lossy(),
        &workdir.to_string_lossy(),
    );
    std::fs::write(&plist, contents).with_context(|| format!("writing {}", plist.display()))?;

    // Unload first in case an old definition is loaded, then load.
    let _ = Command::new("launchctl").arg("unload").arg(&plist).status();
    let status = Command::new("launchctl")
        .arg("load")
        .arg(&plist)
        .status()
        .context("running launchctl load")?;
    if !status.success() {
        bail!("launchctl load failed");
    }
    println!("Installed launchd service: {}", plist.display());
    Ok(())
}

/// Unload and remove the launchd service.
pub fn uninstall() -> Result<()> {
    let plist = plist_path()?;
    if plist.exists() {
        let _ = Command::new("launchctl").arg("unload").arg(&plist).status();
        std::fs::remove_file(&plist).with_context(|| format!("removing {}", plist.display()))?;
        println!("Uninstalled launchd service: {}", plist.display());
    } else {
        println!("No launchd service installed.");
    }
    Ok(())
}

/// Best-effort stop of the running service (without removing the plist).
pub fn unload() -> Result<()> {
    let plist = plist_path()?;
    let _ = Command::new("launchctl").arg("unload").arg(&plist).status();
    Ok(())
}

/// Best-effort start of the installed service.
pub fn load() -> Result<()> {
    let plist = plist_path()?;
    let _ = Command::new("launchctl").arg("load").arg(&plist).status();
    Ok(())
}

/// True if the LaunchAgent plist exists on disk. Note: this does not check
/// whether launchd actually has the job loaded — a plist left behind while
/// unloaded yields a false positive (callers treating this as "launchd will
/// relaunch us" should keep that in mind).
pub fn is_installed() -> bool {
    plist_path().map(|p| p.exists()).unwrap_or(false)
}

fn render_plist(exe: &str, log: &str, workdir: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>serve</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{workdir}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}
