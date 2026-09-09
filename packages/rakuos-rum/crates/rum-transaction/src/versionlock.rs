//! `rum versionlock` — a small on-disk list of [`rum_core::VersionLock`]
//! entries under [`rum_overlay::OverlayPaths::state_dir`], consulted by
//! every resolve path (direct `install`/`upgrade`/`distro-sync` *and*,
//! since each entry pins an exact `epoch:version-release`/`arch`, any
//! transitive dependency pull too — see `rum_solv::build_pool_locked`).
//! `add` snapshots the name's *currently installed* EVR/arch at lock time,
//! matching dnf5's versionlock plugin (which locks to the installed EVR,
//! not to "whatever happens to be installed at solve time" — so the lock
//! stays effective even across a remove+reinstall of the same name).

use anyhow::{Context, Result};
use rum_core::VersionLock;
use rum_overlay::{OverlayContext, OverlayPaths};
use std::path::PathBuf;

fn list_path(paths: &OverlayPaths) -> PathBuf {
    paths.state_dir.join("versionlock.list")
}

pub fn list(paths: &OverlayPaths) -> Result<Vec<VersionLock>> {
    match std::fs::read_to_string(list_path(paths)) {
        Ok(s) => Ok(s.lines().map(str::trim).filter(|l| !l.is_empty()).filter_map(|l| l.parse().ok()).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", list_path(paths).display())),
    }
}

/// Names only — for callers (CLI `versionlock list`, name-based existence
/// checks) that don't care about the pinned EVR/arch.
pub fn list_names(paths: &OverlayPaths) -> Result<Vec<String>> {
    Ok(list(paths)?.into_iter().map(|l| l.name).collect())
}

/// Locks `name` at its currently-installed EVR/arch (looked up in
/// `overlay`), or as an unconstrained name-only entry if it isn't installed
/// yet — mirrors dnf5's own behavior of a versionlock entry for a package
/// that isn't installed doing nothing until it is.
pub fn add(paths: &OverlayPaths, overlay: &OverlayContext, name: &str) -> Result<()> {
    let installed = overlay.base.iter().chain(&overlay.overlay).find(|p| p.nevra.name == name);
    let entry = VersionLock { name: name.to_string(), evr: installed.map(|p| p.nevra.evr()), arch: installed.map(|p| p.nevra.arch.clone()) };
    let mut entries: Vec<VersionLock> = list(paths)?.into_iter().filter(|l| l.name != name).collect();
    entries.push(entry);
    write_all(paths, &entries)
}

pub fn delete(paths: &OverlayPaths, name: &str) -> Result<()> {
    let entries: Vec<VersionLock> = list(paths)?.into_iter().filter(|l| l.name != name).collect();
    write_all(paths, &entries)
}

pub fn clear(paths: &OverlayPaths) -> Result<()> {
    write_all(paths, &[])
}

fn write_all(paths: &OverlayPaths, entries: &[VersionLock]) -> Result<()> {
    std::fs::create_dir_all(&paths.state_dir).with_context(|| format!("creating {}", paths.state_dir.display()))?;
    let mut sorted: Vec<&VersionLock> = entries.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let contents = sorted.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n") + if sorted.is_empty() { "" } else { "\n" };
    std::fs::write(list_path(paths), contents).with_context(|| format!("writing {}", list_path(paths).display()))
}
