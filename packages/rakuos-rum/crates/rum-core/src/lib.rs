//! Shared types for rum: NEVRA identity, dependency expressions, and RPM's
//! version-comparison algorithm. Every other crate builds on these — kept
//! dependency-free (beyond serde) so it can be pulled into resolver, repo
//! metadata, and rpmdb-query code without pulling in tokio/reqwest/etc.

pub mod nevra;
pub mod version;
pub mod versionlock;

pub use nevra::Nevra;
pub use version::EvrCompare;
pub use versionlock::VersionLock;

use serde::{Deserialize, Serialize};

/// Where a package's currently-installed copy actually lives, from rum's
/// perspective. This is the whole point of the overlay-aware resolver: a
/// `Base` package is already present via the read-only image lowerdir and
/// its files/deps must never be re-materialized into the overlay upperdir.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    /// Present in the base image's own rpmdb — read directly from the live,
    /// overlayfs-merged `/usr/share/rpm` — read-only, never written by rum.
    Base,
    /// Installed via rum's own overlay rpmdb (`/var/lib/rakuos/rum-rpmdb`),
    /// entirely separate from the base image's db — no merge/diff needed.
    Overlay,
    /// No RakuOS overlay is present at all (distrobox/podman, an image
    /// build environment): rum is operating against a single plain rpmdb,
    /// same as dnf would, with no base-vs-overlay distinction to make.
    Installed,
    /// Not installed anywhere yet.
    NotInstalled,
}

/// A `Requires`/`Conflicts`/`Provides` comparison operator, as found in RPM
/// dependency strings (`foo >= 1.2-3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Comparator {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

/// One entry from a package's `Requires`/`Provides`/`Conflicts`/`Obsoletes`
/// list: a capability name, optionally version-constrained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub name: String,
    pub constraint: Option<(Comparator, String)>,
}

impl Dependency {
    pub fn unversioned(name: impl Into<String>) -> Self {
        Self { name: name.into(), constraint: None }
    }

    /// Whether an EVR string (`epoch:version-release`) satisfies this
    /// dependency's version constraint. Unversioned deps are satisfied by
    /// any EVR of a package with a matching provided name.
    pub fn satisfied_by_evr(&self, evr: &str) -> bool {
        let Some((cmp, want)) = &self.constraint else { return true };
        let ord = version::compare_evr_for_dep(evr, want);
        match cmp {
            Comparator::Lt => ord.is_lt(),
            Comparator::Le => ord.is_le(),
            Comparator::Eq => ord.is_eq(),
            Comparator::Ge => ord.is_ge(),
            Comparator::Gt => ord.is_gt(),
        }
    }
}

/// A package as known from either repo metadata or an installed rpmdb
/// query — enough identity + dependency info for resolution, not a full
/// RPM header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub nevra: Nevra,
    pub summary: String,
    pub provides: Vec<Dependency>,
    pub requires: Vec<Dependency>,
    pub conflicts: Vec<Dependency>,
    pub obsoletes: Vec<Dependency>,
    /// Weak dependencies (`Recommends`): dnf installs these best-effort
    /// alongside a hard `Requires` closure — unlike `requires`, a missing
    /// provider is never a resolution failure, just silently skipped.
    #[serde(default)]
    pub recommends: Vec<Dependency>,
    /// Weak dependencies (`Suggests`): purely informational, never
    /// auto-installed by rum (same as dnf's default `install_weak_deps`
    /// behavior only pulls in Recommends, not Suggests).
    #[serde(default)]
    pub suggests: Vec<Dependency>,
    /// Weak dependencies (`Enhances`): the reverse of `recommends` — this
    /// package makes another, already-installed one better, rather than the
    /// other way around. Purely informational, never auto-installed.
    #[serde(default)]
    pub enhances: Vec<Dependency>,
    /// Weak dependencies (`Supplements`): the reverse of `suggests` —
    /// installing this alongside its target is desirable, driven from this
    /// package's own metadata rather than the target's. Purely
    /// informational, never auto-installed.
    #[serde(default)]
    pub supplements: Vec<Dependency>,
    /// Repo-relative or absolute location of the `.rpm` payload; empty for
    /// packages sourced from a live rpmdb query (nothing to fetch).
    pub location: String,
    /// Repo id this package came from, or "installed" for rpmdb-sourced
    /// entries — kept for diagnostics/`rum list --repo`.
    pub repo_id: String,
    /// This package's repo's `priority=` (dnf convention: lower number is
    /// preferred). Defaults to dnf's own default of 99 for repos/sources
    /// that don't set one (rpmdb-sourced/"installed" packages included —
    /// priority is only ever a tie-breaker among *candidate* packages from
    /// different repos, never consulted for anything already installed).
    #[serde(default = "default_repo_priority")]
    pub repo_priority: u32,
    /// This package's repo's `cost=` (dnf convention: lower number is
    /// preferred). Only ever consulted as a tiebreaker among candidates
    /// already tied on `repo_priority` — e.g. Fedora's own
    /// `fedora-updates-archive.repo` sets `cost=10000` (vs. the default
    /// 1000) so it's only reached for when nothing else has the package.
    #[serde(default = "default_repo_cost")]
    pub repo_cost: u32,
    /// On-disk size once installed (rpm's `%{SIZE}` / primary.xml's
    /// `<size installed="...">`) — the "Size" column and "After this
    /// operation, N will be used" line in a dnf-style transaction summary.
    #[serde(default)]
    pub install_size: u64,
    /// Compressed `.rpm` payload size (primary.xml's `<size package="...">`)
    /// — 0 for rpmdb-sourced installed packages, which have nothing left to
    /// download.
    #[serde(default)]
    pub download_size: u64,
    /// `<rpm:vendor>` from primary.xml (rpm's `Vendor:` header tag, e.g.
    /// "Fedora Project") — dnf's `--from-vendor` filter matches against
    /// this. Empty for rpmdb-sourced/"installed" packages (rum's rpmdb
    /// reader doesn't currently carry this field back out of an installed
    /// header) and for any repo whose primary.xml omits it.
    #[serde(default)]
    pub vendor: String,
    /// `<checksum type="...">` from primary.xml (e.g. `"sha256"`) — the
    /// algorithm the accompanying [`Package::checksum`] hex digest was
    /// computed with. Empty for rpmdb-sourced/"installed" packages, which
    /// have nothing left to download or verify.
    #[serde(default)]
    pub checksum_type: String,
    /// Hex-encoded digest of the downloadable `.rpm` payload, straight from
    /// primary.xml's `<checksum>` element text — matched against the
    /// downloaded bytes before a package is treated as successfully
    /// fetched, the same integrity check dnf5/librepo performs internally.
    /// Empty for rpmdb-sourced/"installed" packages.
    #[serde(default)]
    pub checksum: String,
}

/// Formats a byte count the way dnf's own transaction summary does:
/// 1024-based units, one decimal place, right down to `B` with none.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// dnf's default repo `priority=` (lower is preferred) for a repo/source
/// that doesn't set one explicitly.
pub fn default_repo_priority() -> u32 {
    99
}

/// dnf's default repo `cost=` (lower is preferred) for a repo/source that
/// doesn't set one explicitly.
pub fn default_repo_cost() -> u32 {
    1000
}

/// dnf5-style `"X.X MiB/s"` throughput for a completed transfer — `bytes`
/// moved over `elapsed` wall-clock time, floored at a nonzero duration so a
/// sub-millisecond (or cached-instant) transfer doesn't divide by zero into
/// an infinite/NaN rate.
pub fn format_speed(bytes: u64, elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs_f64().max(0.001);
    format!("{}/s", format_size((bytes as f64 / secs) as u64))
}

/// dnf5's `"00m00s"` elapsed-time format.
pub fn format_duration(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    format!("{:02}m{:02}s", secs / 60, secs % 60)
}

impl Package {
    /// A package always implicitly provides itself at its own EVR — repo
    /// metadata's `<provides>` list doesn't always include this self-entry,
    /// so resolution must check both.
    pub fn provides_self(&self) -> Dependency {
        Dependency { name: self.nevra.name.clone(), constraint: Some((Comparator::Eq, self.nevra.evr())) }
    }
}

/// A small `fnmatch`-style glob matcher (`*` and `?` only — no character
/// classes), shared by every crate that needs dnf-style pattern lists
/// (`exclude=`, `--repo`, `protected_packages=`, `installonlypkgs=`, …). Not
/// worth pulling in the `glob` crate for two wildcard chars.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => inner(&p[1..], t) || (!t.is_empty() && inner(p, &t[1..])),
            (Some(b'?'), Some(_)) => inner(&p[1..], &t[1..]),
            (Some(pc), Some(tc)) if pc == tc => inner(&p[1..], &t[1..]),
            _ => false,
        }
    }
    if !pattern.contains(['*', '?']) {
        return pattern == text;
    }
    inner(pattern.as_bytes(), text.as_bytes())
}

/// True if `name` matches any pattern in `patterns` (plain names or
/// `glob_match` globs) — the common shape of dnf's `protected_packages=`/
/// `installonlypkgs=`/`exclude=` lists.
pub fn name_matches_any(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

/// True if `pattern` contains a [`glob_match`] wildcard (`*`/`?`) — used to
/// decide whether a package-name CLI argument needs expanding against a
/// live package list (`rum remove 'firefox*'`) or can be used as a literal
/// name/capability lookup directly, unchanged.
pub fn is_glob_pattern(pattern: &str) -> bool {
    pattern.contains(['*', '?'])
}

#[cfg(test)]
mod glob_tests {
    use super::*;

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("kernel*", "kernel-core"));
        assert!(glob_match("kernel", "kernel"));
        assert!(!glob_match("kernel", "kernel-core"));
        assert!(glob_match("fire?ox", "firefox"));
        assert!(name_matches_any("kernel-core", &["glibc".to_string(), "kernel*".to_string()]));
        assert!(!name_matches_any("firefox", &["glibc".to_string(), "kernel*".to_string()]));
    }
}
