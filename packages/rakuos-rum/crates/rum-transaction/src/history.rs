//! A lightweight append-only transaction log, plus the `install`-reason
//! tracking `autoremove`/`mark`/`history` need. dnf keeps the equivalent in
//! a full sqlite "swdb"; rum doesn't have that infrastructure (or the need
//! for its query flexibility), so this is deliberately just a JSON-lines
//! file under [`rum_overlay::OverlayPaths::state_dir`] — one line per
//! transaction, append-only, human-readable, trivially `grep`-able.
//!
//! Every write here is best-effort: a failure to record history must never
//! fail the underlying rpm transaction, which has already actually
//! happened by the time recording runs.

use anyhow::{Context, Result};
use rum_overlay::OverlayPaths;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Backs `history_record=` (dnf default: `true`). `false` only stops new
/// entries being *appended* to the history log itself — install-reason
/// tracking (`reasons.json`, which `autoremove`/`mark` depend on) is a
/// separate dnf concept from history recording and keeps working
/// regardless, matching dnf's own split between its history/swdb table and
/// its yumdb reason tracking.
static HISTORY_RECORD: AtomicBool = AtomicBool::new(true);

pub fn set_history_record(enabled: bool) {
    HISTORY_RECORD.store(enabled, Ordering::Relaxed);
}

/// Why a package is installed — mirrors dnf's install-reason concept
/// closely enough for `autoremove`/`mark` to work the same way: a
/// `Dependency` install with nothing left depending on it is a
/// autoremove candidate, a `User` (explicit) one never is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    User,
    Dependency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Install,
    Remove,
    Upgrade,
    Reinstall,
    Downgrade,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: u64,
    pub unix_time: u64,
    pub action: Action,
    /// `(nevra, reason)` for every package this transaction touched.
    pub packages: Vec<(String, Reason)>,
}

fn log_path(paths: &OverlayPaths) -> PathBuf {
    paths.state_dir.join("history.jsonl")
}

fn reasons_path(paths: &OverlayPaths) -> PathBuf {
    paths.state_dir.join("reasons.json")
}

/// Appends one entry to the history log and updates the persisted
/// name→[`Reason`] map (`Remove` entries erase the name from that map
/// entirely — an uninstalled package has no reason to track). Never
/// returns an error to a caller that doesn't check it: history is
/// diagnostic, not load-bearing, so callers should log-and-continue on
/// failure rather than fail an already-completed rpm transaction over it.
pub fn record(paths: &OverlayPaths, action: Action, packages: &[(String, Reason)]) -> Result<()> {
    std::fs::create_dir_all(&paths.state_dir).with_context(|| format!("creating {}", paths.state_dir.display()))?;

    let mut reasons = read_reasons(paths).unwrap_or_default();
    for (nevra, reason) in packages {
        // Naively splitting `nevra` on '-' truncates any hyphenated package
        // name (e.g. `vulkan-loader`, `mesa-dri-drivers`) to its first
        // segment, silently orphaning it from `reasons.json` under the
        // wrong key — `reason_of` would then see `None` (assume `User`) and
        // `autoremove` would never touch it. `Nevra::parse_nevra`/
        // `parse_nvra` split from the right instead, so they get the name
        // right regardless of hyphens in it.
        let name = rum_core::Nevra::parse_nevra(nevra)
            .or_else(|| rum_core::Nevra::parse_nvra(nevra))
            .map(|n| n.name)
            .unwrap_or_else(|| nevra.split('-').next().unwrap_or(nevra).to_string());
        if action == Action::Remove {
            reasons.remove(&name);
        } else if *reason == Reason::User || reasons.get(&name) != Some(&Reason::User) {
            // A package explicitly installed in a prior step must never be
            // silently downgraded to `Dependency` just because a *later*,
            // unrelated transaction also happens to touch it (e.g. it gets
            // pulled into another install/upgrade's download set without
            // being one of the names the user typed this time) — otherwise
            // `autoremove` could sweep it up despite the user having asked
            // for it explicitly. `Reason::User` only ever regresses via an
            // explicit `rum mark remove`, which goes through `set_reason`
            // (an intentional overwrite), not through this automatic path.
            reasons.insert(name, *reason);
        }
    }
    std::fs::write(reasons_path(paths), serde_json::to_string_pretty(&reasons)?).context("writing reasons.json")?;

    if !HISTORY_RECORD.load(Ordering::Relaxed) {
        return Ok(());
    }

    let entry = HistoryEntry {
        id: next_id(paths),
        unix_time: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        action,
        packages: packages.to_vec(),
    };
    let mut f = OpenOptions::new().create(true).append(true).open(log_path(paths)).with_context(|| format!("opening {}", log_path(paths).display()))?;
    writeln!(f, "{}", serde_json::to_string(&entry)?)?;
    Ok(())
}

fn next_id(paths: &OverlayPaths) -> u64 {
    list(paths).map(|entries| entries.last().map(|e| e.id + 1).unwrap_or(1)).unwrap_or(1)
}

/// Every recorded transaction, oldest first.
pub fn list(paths: &OverlayPaths) -> Result<Vec<HistoryEntry>> {
    let path = log_path(paths);
    let Ok(f) = std::fs::File::open(&path) else { return Ok(Vec::new()) };
    std::io::BufReader::new(f)
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(&l).with_context(|| format!("parsing {}", path.display())))
        .collect()
}

fn read_reasons(paths: &OverlayPaths) -> Result<HashMap<String, Reason>> {
    let path = reasons_path(paths);
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Why `name` is currently installed, if rum has ever recorded it —
/// `None` for anything installed before history tracking existed (e.g.
/// base-image packages, or overlay packages from before this feature
/// shipped), which `autoremove` correctly treats as "assume User" rather
/// than eligible for automatic removal.
pub fn reason_of(paths: &OverlayPaths, name: &str) -> Option<Reason> {
    read_reasons(paths).ok().and_then(|r| r.get(name).copied())
}

/// Explicitly (re)sets `name`'s reason — `rum mark`.
pub fn set_reason(paths: &OverlayPaths, name: &str, reason: Reason) -> Result<()> {
    std::fs::create_dir_all(&paths.state_dir).with_context(|| format!("creating {}", paths.state_dir.display()))?;
    let mut reasons = read_reasons(paths).unwrap_or_default();
    reasons.insert(name.to_string(), reason);
    std::fs::write(reasons_path(paths), serde_json::to_string_pretty(&reasons)?).context("writing reasons.json")?;
    Ok(())
}

/// What `history undo`/`rollback` needs to apply: package names to remove,
/// and package names to (re)install. Deliberately name-only, not pinned to
/// the exact recorded NEVRA — `Remove` entries only ever recorded names to
/// begin with (see [`record`]'s `Action::Remove` branch), so there is no
/// exact old EVR to restore even for the `Install` side; pinning would just
/// make the two sides inconsistent. The installed/removed names are
/// resolved against whatever's in the repos *now*, same as a plain
/// `install`/`remove` invocation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Inverse {
    pub to_remove: Vec<String>,
    pub to_install: Vec<String>,
}

/// `Remove` entries record bare package names (no version/release ever
/// existed to capture at removal time), while `Install`-family entries
/// record full NEVRA strings — both flow through here. A bare name that
/// itself contains a hyphen (`perl-PathTools`, `perl-Text-Tabs+Wrap`) fails
/// both NEVRA and NVRA parsing, so it must be returned as-is rather than
/// truncated at the first hyphen: `split('-').next()` would collapse it to
/// the wrong, unrelated name `perl` (see the hyphenated-name tests above —
/// this is the bare-name counterpart of that same bug class).
fn name_of(nevra: &str) -> String {
    rum_core::Nevra::parse_nevra(nevra).or_else(|| rum_core::Nevra::parse_nvra(nevra)).map(|n| n.name).unwrap_or_else(|| nevra.to_string())
}

/// The inverse of one transaction: an `Install`-family entry (install,
/// upgrade, reinstall, downgrade all currently get recorded as `Install` —
/// see [`record`]'s call sites) undoes by removing what it installed; a
/// `Remove` entry undoes by reinstalling what it removed.
pub fn inverse_of(entry: &HistoryEntry) -> Inverse {
    let names: Vec<String> = entry.packages.iter().map(|(nevra, _)| name_of(nevra)).collect();
    match entry.action {
        Action::Remove => Inverse { to_remove: Vec::new(), to_install: names },
        Action::Install | Action::Upgrade | Action::Reinstall | Action::Downgrade => Inverse { to_remove: names, to_install: Vec::new() },
    }
}

/// The inverse of a whole range of transactions, applied most-recent-first
/// (`entries` need not be pre-sorted). Used by `rollback <id>`: the range is
/// every entry after `<id>`. If a package name is touched by more than one
/// transaction in the range, only the most recent transaction's verdict for
/// that name wins — e.g. installed then later removed within the range nets
/// out to "reinstall" (undoing the more recent `Remove`), not both.
pub fn composite_inverse(entries: &[HistoryEntry]) -> Inverse {
    let mut ordered: Vec<&HistoryEntry> = entries.iter().collect();
    ordered.sort_by(|a, b| b.id.cmp(&a.id));

    let mut decided: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result = Inverse::default();
    for entry in ordered {
        let inv = inverse_of(entry);
        for name in inv.to_remove {
            if decided.insert(name.clone()) {
                result.to_remove.push(name);
            }
        }
        for name in inv.to_install {
            if decided.insert(name.clone()) {
                result.to_install.push(name);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(tag: &str) -> OverlayPaths {
        let dir = std::env::temp_dir().join(format!("rum-history-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        OverlayPaths { state_dir: dir, ..OverlayPaths::default() }
    }

    #[test]
    fn hyphenated_dependency_name_is_tracked_correctly() {
        let paths = test_paths("vulkan");
        record(&paths, Action::Install, &[("vulkan-loader-1.3.250.0-1.fc44.x86_64".to_string(), Reason::Dependency)]).unwrap();
        assert_eq!(reason_of(&paths, "vulkan-loader"), Some(Reason::Dependency));
        // The old naive `split('-').next()` logic would have wrongly
        // recorded this dependency's reason under "vulkan" instead.
        assert_eq!(reason_of(&paths, "vulkan"), None);
        std::fs::remove_dir_all(&paths.state_dir).ok();
    }

    #[test]
    fn hyphenated_name_with_epoch_is_tracked_correctly() {
        let paths = test_paths("mesa");
        record(&paths, Action::Install, &[("mesa-dri-drivers-1:23.1.4-1.fc44.x86_64".to_string(), Reason::Dependency)]).unwrap();
        assert_eq!(reason_of(&paths, "mesa-dri-drivers"), Some(Reason::Dependency));
        std::fs::remove_dir_all(&paths.state_dir).ok();
    }

    #[test]
    fn explicit_install_reason_survives_a_later_unrelated_transaction() {
        let paths = test_paths("pskc");
        // User explicitly installs python3-pskc directly.
        record(&paths, Action::Install, &[("python3-pskc-1.4-1.fc44.noarch".to_string(), Reason::User)]).unwrap();
        assert_eq!(reason_of(&paths, "python3-pskc"), Some(Reason::User));

        // A later, unrelated transaction also happens to (re)download
        // python3-pskc as part of its download set (e.g. it's already
        // installed and gets swept along), but the user didn't name it
        // explicitly this time, so the caller passes Reason::Dependency —
        // this must NOT downgrade the earlier User reason.
        record(&paths, Action::Install, &[("python3-pyscard-2.2.2-6.fc44.x86_64".to_string(), Reason::User), ("python3-pskc-1.4-1.fc44.noarch".to_string(), Reason::Dependency)]).unwrap();
        assert_eq!(reason_of(&paths, "python3-pskc"), Some(Reason::User), "explicit install reason must not be silently downgraded");
        assert_eq!(reason_of(&paths, "python3-pyscard"), Some(Reason::User));
        std::fs::remove_dir_all(&paths.state_dir).ok();
    }

    #[test]
    fn inverse_of_install_removes_and_remove_installs() {
        let install = HistoryEntry { id: 1, unix_time: 0, action: Action::Install, packages: vec![("foo-1.0-1.fc44.x86_64".to_string(), Reason::User)] };
        assert_eq!(inverse_of(&install), Inverse { to_remove: vec!["foo".to_string()], to_install: vec![] });

        let remove = HistoryEntry { id: 2, unix_time: 0, action: Action::Remove, packages: vec![("bar".to_string(), Reason::User)] };
        assert_eq!(inverse_of(&remove), Inverse { to_remove: vec![], to_install: vec!["bar".to_string()] });
    }

    #[test]
    fn composite_inverse_lets_most_recent_transaction_win_per_name() {
        // tx1 installs foo, tx2 removes foo again — rolling back past tx1
        // should net out to "reinstall foo" (undoing tx2's remove), not
        // also try to remove it for tx1.
        let tx1 = HistoryEntry { id: 1, unix_time: 0, action: Action::Install, packages: vec![("foo-1.0-1.fc44.x86_64".to_string(), Reason::User)] };
        let tx2 = HistoryEntry { id: 2, unix_time: 0, action: Action::Remove, packages: vec![("foo".to_string(), Reason::User)] };
        let inv = composite_inverse(&[tx1, tx2]);
        assert_eq!(inv, Inverse { to_remove: vec![], to_install: vec!["foo".to_string()] });
    }
}
