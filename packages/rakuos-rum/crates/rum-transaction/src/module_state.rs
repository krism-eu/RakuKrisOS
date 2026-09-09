//! `rum module enable/disable/reset` state — which module streams the user
//! has explicitly turned on or off, persisted under
//! [`rum_overlay::OverlayPaths::state_dir`] the same way
//! [`crate::versionlock`] does. Two plain lists, not one: an enabled module
//! also needs to remember *which* stream, while a disabled module doesn't
//! (disabling blocks every stream of that module name, matching dnf's own
//! "disabled means the whole module is off-limits" behavior).

use anyhow::{Context, Result};
use rum_overlay::OverlayPaths;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

fn enabled_path(paths: &OverlayPaths) -> PathBuf {
    paths.state_dir.join("module_enable.list")
}

fn disabled_path(paths: &OverlayPaths) -> PathBuf {
    paths.state_dir.join("module_disable.list")
}

/// module name -> enabled stream.
pub fn enabled(paths: &OverlayPaths) -> Result<HashMap<String, String>> {
    match std::fs::read_to_string(enabled_path(paths)) {
        Ok(s) => Ok(s.lines().map(str::trim).filter(|l| !l.is_empty()).filter_map(|l| l.split_once(':')).map(|(n, s)| (n.to_string(), s.to_string())).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", enabled_path(paths).display())),
    }
}

/// Disabled module names.
pub fn disabled(paths: &OverlayPaths) -> Result<HashSet<String>> {
    match std::fs::read_to_string(disabled_path(paths)) {
        Ok(s) => Ok(s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", disabled_path(paths).display())),
    }
}

/// Enables `name` at `stream`, clearing any prior disable/enable state for
/// that module name first (a module can only have one active stream, same
/// as dnf's own "switching streams" rule).
pub fn enable(paths: &OverlayPaths, name: &str, stream: &str) -> Result<()> {
    let mut en = enabled(paths)?;
    let mut dis = disabled(paths)?;
    dis.remove(name);
    en.insert(name.to_string(), stream.to_string());
    write_all(paths, &en, &dis)
}

/// Disables `name` entirely, blocking every stream's packages regardless of
/// any prior `enable`.
pub fn disable(paths: &OverlayPaths, name: &str) -> Result<()> {
    let mut en = enabled(paths)?;
    let mut dis = disabled(paths)?;
    en.remove(name);
    dis.insert(name.to_string());
    write_all(paths, &en, &dis)
}

/// Clears both enable and disable state for `name`, returning it to
/// "whatever the repo's `modulemd-defaults` says" (or unfiltered, if the
/// repo has no default stream for it).
pub fn reset(paths: &OverlayPaths, name: &str) -> Result<()> {
    let mut en = enabled(paths)?;
    let mut dis = disabled(paths)?;
    en.remove(name);
    dis.remove(name);
    write_all(paths, &en, &dis)
}

fn write_all(paths: &OverlayPaths, en: &HashMap<String, String>, dis: &HashSet<String>) -> Result<()> {
    std::fs::create_dir_all(&paths.state_dir).with_context(|| format!("creating {}", paths.state_dir.display()))?;
    let mut en_sorted: Vec<(&String, &String)> = en.iter().collect();
    en_sorted.sort();
    let en_contents = en_sorted.iter().map(|(n, s)| format!("{n}:{s}")).collect::<Vec<_>>().join("\n") + if en_sorted.is_empty() { "" } else { "\n" };
    std::fs::write(enabled_path(paths), en_contents).with_context(|| format!("writing {}", enabled_path(paths).display()))?;

    let mut dis_sorted: Vec<&String> = dis.iter().collect();
    dis_sorted.sort();
    let dis_contents = dis_sorted.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n") + if dis_sorted.is_empty() { "" } else { "\n" };
    std::fs::write(disabled_path(paths), dis_contents).with_context(|| format!("writing {}", disabled_path(paths).display()))
}
