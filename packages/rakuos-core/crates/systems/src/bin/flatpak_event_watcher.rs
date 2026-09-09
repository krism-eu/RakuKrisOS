/// rakuos-flatpak-event-watcher — watches the Flatpak exports/bin dir for
/// installs/uninstalls and triggers system-setup per-app fixups plus a
/// CLI wrapper regeneration.
use anyhow::Result;
use inotify::{EventMask, Inotify, WatchMask};
use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const SETUP_SCRIPT: &str = "/usr/libexec/rakuos/system-setup";
const WRAPPER_GEN: &str = "/usr/libexec/rakuos/flatpak-wrapper-gen";
const BIN_DIR: &str = "/var/lib/flatpak/exports/bin";

fn main() -> Result<()> {
    println!("RakuOS Flatpak event watcher started, watching {BIN_DIR}");

    run_wrapper_gen();

    let mut known_apps: HashSet<String> = std::fs::read_dir(BIN_DIR)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|name| !name.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();

    println!("Setting up watches.");

    let mut inotify = Inotify::init()?;
    let mask = WatchMask::CREATE | WatchMask::DELETE | WatchMask::MOVED_TO | WatchMask::MOVED_FROM;
    inotify.watches().add(BIN_DIR, mask)?;

    let mut buffer = [0u8; 4096];
    loop {
        let events = inotify.read_events_blocking(&mut buffer)?;
        for event in events {
            let Some(name) = event.name else { continue };
            let file = name.to_string_lossy().to_string();

            if file.starts_with('.') || file.starts_with(".export-symlink-") {
                continue;
            }

            let appid = file;

            if event.mask.contains(EventMask::CREATE) || event.mask.contains(EventMask::MOVED_TO) {
                if !known_apps.contains(&appid) {
                    println!("Detected Flatpak install for {appid}");
                    run_setup(&appid, "install");
                    known_apps.insert(appid);
                    run_wrapper_gen();
                }
            } else if event.mask.contains(EventMask::DELETE) || event.mask.contains(EventMask::MOVED_FROM) {
                if known_apps.remove(&appid) {
                    println!("Detected Flatpak uninstall for {appid}");
                    run_setup(&appid, "uninstall");
                    run_wrapper_gen();
                }
            }
        }
    }
}

fn run_setup(appid: &str, action: &str) {
    let _ = Command::new(SETUP_SCRIPT).args(["watcher", appid, action]).status();
}

fn run_wrapper_gen() {
    if is_executable(Path::new(WRAPPER_GEN)) {
        let _ = Command::new(WRAPPER_GEN).status();
    }
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
