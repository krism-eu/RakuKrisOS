//! `rum system-upgrade`'s Standalone-mode offline-transaction machinery —
//! a from-scratch Rust port of dnf5's `libdnf5::offline` + `dnf5 offline`/
//! `dnf5 system-upgrade` plugin (see `dnf5/commands/{offline,system-upgrade}`
//! upstream): download a full transaction while still on the old release,
//! then apply it pre-boot via systemd's `system-update.target` convention
//! (`systemd-system-update-generator(8)`) so rpm/glibc/systemd itself are
//! never upgraded out from under a live process tree.
//!
//! Only meaningful under [`rum_overlay::OverlayMode::Standalone`] — a real
//! RakuOS overlay system never does a dnf-style major-version bump this
//! way; see `system_upgrade` in `main.rs` for the Split-mode half (ordinary
//! overlay package upgrade + `bootc upgrade`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where downloaded packages and the transaction state file live.
/// Equivalent to dnf5's `DEFAULT_DATADIR` (`/var/lib/dnf5/offline`).
pub const DATADIR: &str = "/var/lib/rum/offline";
/// systemd's "Offline Updates" trigger: `systemd-system-update-generator`
/// isolates to `system-update.target` at early boot iff this exists and is
/// a symlink. Fixed path, not configurable — it's part of systemd's own
/// contract, not rum's.
pub const MAGIC_SYMLINK: &str = "/system-update";
const STATE_FILENAME: &str = "state.json";
const PACKAGES_DIRNAME: &str = "packages";
const UNIT_PATH: &str = "/etc/systemd/system/rum-system-upgrade.service";
const UNIT_NAME: &str = "rum-system-upgrade.service";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    DownloadIncomplete,
    DownloadComplete,
    Ready,
    TransactionIncomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub status: Status,
    pub system_releasever: String,
    pub target_releasever: String,
    pub cmd_line: String,
    #[serde(default)]
    pub poweroff_after: bool,
    /// Absolute paths of every downloaded RPM, in the order they should be
    /// handed to `rpm` at apply time.
    pub packages: Vec<PathBuf>,
}

pub fn datadir() -> PathBuf {
    PathBuf::from(DATADIR)
}

pub fn packages_dir() -> PathBuf {
    datadir().join(PACKAGES_DIRNAME)
}

fn state_path() -> PathBuf {
    datadir().join(STATE_FILENAME)
}

pub fn load_state() -> Result<Option<State>> {
    let path = state_path();
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let state: State = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(state))
}

pub fn write_state(state: &State) -> Result<()> {
    std::fs::create_dir_all(datadir())?;
    let path = state_path();
    let text = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

/// Removes downloaded packages and the state file, but leaves the systemd
/// unit installed — matches dnf5's `offline clean` (harmless to leave a
/// `ConditionPathExists=/system-update`-gated unit sitting disabled).
pub fn clean() -> Result<()> {
    let _ = std::fs::remove_file(MAGIC_SYMLINK);
    let dir = datadir();
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

/// Writes and enables the systemd unit that `system-update.target` will
/// start pre-boot to invoke `rum system-upgrade execute`. Static content —
/// idempotent to call on every `download`, unlike dnf5 (which ships this
/// unit as part of its own package instead of writing it at runtime, since
/// rum currently has no separate systemd-unit-carrying subpackage).
pub fn install_unit() -> Result<()> {
    let unit = "[Unit]\n\
Description=rum offline system upgrade\n\
ConditionPathExists=/system-update\n\
\n\
[Service]\n\
Type=oneshot\n\
ExecStart=/usr/bin/rum system-upgrade execute\n\
StandardOutput=journal+console\n\
StandardError=journal+console\n\
\n\
[Install]\n\
WantedBy=system-update.target\n";
    std::fs::write(UNIT_PATH, unit).with_context(|| format!("writing {UNIT_PATH}"))?;
    let status = std::process::Command::new("systemctl").args(["daemon-reload"]).status();
    if !matches!(status, Ok(s) if s.success()) {
        eprintln!("warning: `systemctl daemon-reload` failed; the offline-upgrade unit may not take effect");
    }
    let status = std::process::Command::new("systemctl").args(["enable", UNIT_NAME]).status();
    if !matches!(status, Ok(s) if s.success()) {
        eprintln!("warning: `systemctl enable {UNIT_NAME}` failed; run it manually before `system-upgrade reboot`");
    }
    Ok(())
}

/// `dnf5 offline reboot`'s sanity check — refuses to reboot into the
/// transaction if the unit isn't actually wanted by `system-update.target`,
/// which would otherwise boot straight back to `default.target` having done
/// nothing.
pub fn unit_is_wanted() -> bool {
    Path::new("/etc/systemd/system/system-update.target.wants").join(UNIT_NAME).exists()
}

pub fn create_trigger_symlink() -> Result<()> {
    if !Path::new(MAGIC_SYMLINK).is_symlink() {
        std::os::unix::fs::symlink(datadir(), MAGIC_SYMLINK).with_context(|| format!("creating {MAGIC_SYMLINK} -> {}", datadir().display()))?;
    }
    Ok(())
}

pub fn remove_trigger_symlink() -> Result<()> {
    if Path::new(MAGIC_SYMLINK).is_symlink() {
        std::fs::remove_file(MAGIC_SYMLINK).with_context(|| format!("removing {MAGIC_SYMLINK}"))?;
    }
    Ok(())
}

pub fn systemctl_reboot(poweroff: bool) -> Result<()> {
    if std::env::var_os("RUM_SYSTEM_UPGRADE_NO_REBOOT").is_some() {
        eprintln!("RUM_SYSTEM_UPGRADE_NO_REBOOT is set, not {}.", if poweroff { "powering off" } else { "rebooting" });
        return Ok(());
    }
    let verb = if poweroff { "poweroff" } else { "reboot" };
    let status = std::process::Command::new("systemctl").arg(verb).status().with_context(|| format!("running systemctl {verb}"))?;
    anyhow::ensure!(status.success(), "systemctl {verb} failed");
    Ok(())
}
