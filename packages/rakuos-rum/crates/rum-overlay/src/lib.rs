//! Overlay-awareness: the reason rum exists instead of just being another
//! dnf wrapper.
//!
//! rum maintains its **own** rpmdb for everything it installs
//! (`/var/lib/rakuos/rum-rpmdb`), entirely separate from the base image's
//! own rpmdb, which rum reads directly and read-only from the live,
//! overlayfs-merged `/usr/share/rpm` — never a separate snapshot copy. The
//! two are never merged or diffed against each other — rum simply unions
//! two independent, always-accurate databases at query time. Reading
//! `/usr/share/rpm` straight off the live mount means there's no snapshot to
//! go stale relative to what's actually mounted (see the
//! `rakuos_overlay_rpmdb_staleness_fix` project note, which this
//! supersedes). Package *files* still land through the real overlayfs-merged
//! `/usr` exactly as before — only the rpmdb *metadata* rum writes is
//! redirected to its own path; `--dbpath` only controls where rpm records
//! what's installed, not where a package's payload extracts to.
//!
//! Not every environment rum runs in has an overlay at all — a distrobox/
//! podman container or an image-build chroot is a plain, single-rpmdb
//! system, the same as any other dnf/rpm target. [`OverlayPaths::detect`]
//! tells these apart (by checking for `/var/lib/rakuos/current-deploy`, the
//! marker rakuos-core's `overlay_mount.rs` writes only once the overlay is
//! actually mounted for the current boot) and picks
//! [`OverlayMode::Standalone`], where rum behaves exactly like plain
//! rpm/dnf: one rpmdb (rpm's own compiled-in default location, not a
//! RakuOS-specific path), no base-vs-overlay distinction, nothing ever
//! off-limits to install/remove.

use anyhow::{Context, Result};
use rum_core::{Dependency, Origin, Package};
use std::path::{Path, PathBuf};

/// Whether this run is against a real RakuOS overlay system or a plain,
/// unoverlaid one (distrobox/podman, an image-build environment).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum OverlayMode {
    /// A real RakuOS overlay system. `base_snapshot_rpmdb` is read-only and
    /// never written by rum — it's the live, overlayfs-merged rpmdb at
    /// `/usr/share/rpm`, not a separate snapshot copy, so it's always
    /// current with whatever's actually mounted; `overlay_rpmdb` is rum's
    /// own db and the only thing rum ever installs/removes against.
    Split { base_snapshot_rpmdb: PathBuf, overlay_rpmdb: PathBuf },
    /// No overlay present. rum reads/writes rpm's own default dbpath
    /// (`--dbpath` omitted entirely), same as a plain `rpm`/`dnf` call.
    Standalone,
}

/// Well-known paths from `overlay_mount.rs` / `crates/overlay/src/lib.rs`.
/// Kept as `const`s (not hardcoded inline) so a single place needs updating
/// if rakuos-core's layout ever moves.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct OverlayPaths {
    pub mode: OverlayMode,
    /// The overlay upperdir itself — where a fresh install's *files* (not
    /// its rpmdb entry) actually land, for reference/diagnostics. Only
    /// meaningful under [`OverlayMode::Split`].
    pub overlay_upper: PathBuf,
    /// Where rum keeps its own metadata that isn't an rpmdb at all —
    /// currently the transaction history log (`history.jsonl`) and the
    /// versionlock list (`versionlock.list`). Kept separate from the rpmdb
    /// paths above since neither concept exists in rpm/dnf itself; dnf5
    /// keeps the equivalents in its own sqlite state dir under
    /// `/var/lib/dnf5` for the same reason.
    pub state_dir: PathBuf,
    /// `--installroot`: when set, package *files* land under this root
    /// (via `rpm --root`) instead of the real `/`, and rum's own rpmdb is
    /// created fresh at `<installroot>/var/lib/rakuos/rum-rpmdb` rather
    /// than wherever `mode` would otherwise point — regardless of `mode`,
    /// since the whole point is a disposable, isolated write target (image
    /// builds pre-baking the factory overlay; test reproduction setups).
    /// The *base* view is left alone: it still reads from wherever `mode`
    /// says (the live system's own rpmdb in the `Standalone` case that
    /// applies inside an image-build container — exactly what should count
    /// as "already provided" while prebaking).
    pub installroot: Option<PathBuf>,
}

impl Default for OverlayPaths {
    /// Defaults to [`OverlayMode::Split`] — existing callers/tests that
    /// construct `OverlayPaths::default()` expect the historical RakuOS-
    /// overlay behavior. Runtime code (`rum-cli`) should use
    /// [`OverlayPaths::detect`] instead so it picks standalone mode
    /// automatically outside a real overlay system.
    fn default() -> Self {
        Self {
            mode: OverlayMode::Split {
                base_snapshot_rpmdb: PathBuf::from("/usr/share/rpm"),
                overlay_rpmdb: PathBuf::from("/var/lib/rakuos/rum-rpmdb"),
            },
            overlay_upper: PathBuf::from("/var/lib/rakuos/overlay/upper"),
            state_dir: PathBuf::from("/var/lib/rakuos/rum-state"),
            installroot: None,
        }
    }
}

impl OverlayPaths {
    /// Picks [`OverlayMode::Split`] if this looks like a real RakuOS
    /// overlay system (`/var/lib/rakuos/current-deploy` exists — written by
    /// `overlay_mount.rs` only once the overlay is actually mounted for the
    /// current boot), [`OverlayMode::Standalone`] otherwise — covering
    /// distrobox/podman and image-build environments, neither of which ever
    /// has `/var/lib/rakuos/current-deploy`.
    ///
    /// The base view no longer reads a separate snapshot copy
    /// (`/var/lib/rakuos/base-rpmdb`) — it queries the live,
    /// overlayfs-merged rpmdb directly at `/usr/share/rpm`, so it can never
    /// go stale relative to what's actually mounted (see the
    /// `rakuos_overlay_rpmdb_staleness_fix` project note this replaces).
    ///
    /// An earlier version of this also cross-checked `/etc/os-release`'s
    /// `ID=rakuos` and hard-errored when that was set but the marker was
    /// missing, on the theory that a real RakuOS boot without it meant a
    /// broken overlay mount rather than "no overlay to speak of". That check
    /// backfired on image builds: a build chroot's `os-release` already says
    /// `ID=rakuos` (it's building the final OS in place) long before any
    /// overlay is ever mounted, and if a *stale* marker happened to be lying
    /// around from a previous build stage, the ID check forced `Split` mode
    /// onto a build that should be plain `Standalone` — which is exactly the
    /// "pulls unrelated already-installed packages" class of bug, since the
    /// pool then split what should be one rpmdb view into a stale base + a
    /// separate overlay. The marker's existence is the only signal that
    /// matters; `os-release` says nothing about mount state.
    pub fn detect() -> Result<Self> {
        let defaults = Self::default();
        match Path::new("/var/lib/rakuos/current-deploy").exists() {
            true => Ok(defaults),
            false => Ok(Self { mode: OverlayMode::Standalone, overlay_upper: defaults.overlay_upper, state_dir: PathBuf::from("/var/lib/rum"), installroot: None }),
        }
    }

    /// The path under `installroot` where a fresh, disposable overlay
    /// rpmdb should live — same relative layout as the real
    /// `/var/lib/rakuos/rum-rpmdb` regardless of the detected `mode`, so
    /// copying it out of the installroot afterward (image-build prebake) is
    /// a straight path-for-path drop into the real factory tree.
    fn installroot_dbpath(root: &Path) -> PathBuf {
        root.join("var/lib/rakuos/rum-rpmdb")
    }

    /// The dbpath rum should read/write against for its own
    /// (writable/overlay) view. `installroot`, when set, always wins
    /// regardless of `mode` — see its doc comment. Otherwise `None` under
    /// [`OverlayMode::Standalone`] means "let rpm use its own default" —
    /// the whole point of standalone mode is behaving exactly like a plain
    /// rpm/dnf invocation.
    pub fn write_dbpath(&self) -> Option<PathBuf> {
        if let Some(root) = &self.installroot {
            return Some(Self::installroot_dbpath(root));
        }
        match &self.mode {
            OverlayMode::Split { overlay_rpmdb, .. } => Some(overlay_rpmdb.clone()),
            OverlayMode::Standalone => None,
        }
    }

    /// Whether the `rpm` transaction writing to [`Self::write_dbpath`] needs
    /// `--nodeps`: true whenever that dbpath is an isolated db that can't
    /// see every package satisfying a dependency by itself — either the
    /// bare installroot tree (no base packages inside it at all) or, under
    /// [`OverlayMode::Split`], rum's own overlay-only rpmdb, which never
    /// contains the base image's packages (see the module doc comment: the
    /// two are deliberately never merged/diffed, rum's resolver unions them
    /// itself at query time instead). `rpm`'s own dependency checker only
    /// ever consults the db at `--dbpath`, so without `--nodeps` here it
    /// would reject every Requires satisfied only by a base-image package
    /// — glibc, libgcc, etc. — even though rum's SAT resolver already
    /// verified cross-db satisfaction before building this transaction.
    /// [`OverlayMode::Standalone`] doesn't need this: there, `--dbpath` is
    /// omitted entirely and rpm reads/writes its own single, complete db.
    pub fn needs_nodeps(&self) -> bool {
        self.installroot.is_some() || matches!(self.mode, OverlayMode::Split { .. })
    }
}

/// The result of loading both of rum's rpmdb views: every installed
/// package, tagged with where it actually came from.
pub struct OverlayContext {
    /// Base-image packages (`Split` mode only) — read-only, rum never
    /// installs/removes against this list.
    pub base: Vec<Package>,
    /// Everything rum considers writable/mutable: packages in its own
    /// overlay rpmdb (`Split` mode), or every installed package at all
    /// (`Standalone` mode, where there's no base-vs-overlay distinction to
    /// make).
    pub overlay: Vec<Package>,
    pub mode: OverlayMode,
    /// Capability indices over `base`/`overlay`, built once at construction
    /// time and reused by [`Self::satisfied_by_base`]/[`Self::satisfied`]
    /// across every dependency edge the resolver checks — replaces what used
    /// to be a fresh O(installed) linear scan per call. See
    /// [`rum_rpmdb::CapabilityIndex`].
    base_index: rum_rpmdb::CapabilityIndex,
    overlay_index: rum_rpmdb::CapabilityIndex,
}

impl OverlayContext {
    /// Builds a context directly from already-known package lists, no
    /// rpmdb query involved — for tests (`rum-resolver`'s test suite, most
    /// notably) that want to exercise resolver logic against a fixed
    /// base/overlay split without shelling out to `rpm`.
    pub fn for_test(base: Vec<Package>, overlay: Vec<Package>) -> Self {
        Self::new(base, overlay, OverlayMode::Split { base_snapshot_rpmdb: PathBuf::new(), overlay_rpmdb: PathBuf::new() })
    }

    /// Builds a context from already-known package lists and an explicit
    /// `mode` — for callers (e.g. the resolver's own what-if sub-contexts
    /// used while sizing `Recommends`) that construct a derived
    /// `OverlayContext` from an existing one's data rather than querying an
    /// rpmdb or wanting `for_test`'s fixed `Split` mode.
    pub fn new(base: Vec<Package>, overlay: Vec<Package>, mode: OverlayMode) -> Self {
        let base_index = rum_rpmdb::CapabilityIndex::build(&base);
        let overlay_index = rum_rpmdb::CapabilityIndex::build(&overlay);
        Self { base, overlay, mode, base_index, overlay_index }
    }

    /// Builds the context by querying whichever rpmdb(s) apply to
    /// `paths.mode`. In `Split` mode this is two independent dbs (base
    /// snapshot + rum's own overlay db) with no diffing needed — each db
    /// already only contains what it should. In `Standalone` mode it's a
    /// single query against rpm's default dbpath.
    pub fn load(paths: &OverlayPaths) -> Result<Self> {
        // `--installroot`: base is read from wherever `mode` says
        // (unaffected — see `OverlayPaths::installroot`'s doc comment), but
        // the writable/overlay view always comes from a fresh db under the
        // installroot, regardless of `mode`.
        if let Some(root) = &paths.installroot {
            let overlay_rpmdb = OverlayPaths::installroot_dbpath(root);
            let base = match &paths.mode {
                OverlayMode::Split { base_snapshot_rpmdb, .. } => rum_rpmdb::query_installed(Some(base_snapshot_rpmdb), "base")
                    .with_context(|| format!("querying base rpmdb snapshot at {}", base_snapshot_rpmdb.display()))?,
                OverlayMode::Standalone => rum_rpmdb::query_installed(None, "base").context("querying default rpmdb as installroot base")?,
            };
            let overlay = rum_rpmdb::query_installed(Some(&overlay_rpmdb), "overlay")
                .with_context(|| format!("querying installroot overlay rpmdb at {}", overlay_rpmdb.display()))?;
            tracing::debug!(base = base.len(), overlay = overlay.len(), installroot = %root.display(), "loaded installroot overlay context");
            let base_index = rum_rpmdb::CapabilityIndex::build(&base);
            let overlay_index = rum_rpmdb::CapabilityIndex::build(&overlay);
            return Ok(Self { base, overlay, mode: OverlayMode::Split { base_snapshot_rpmdb: PathBuf::new(), overlay_rpmdb }, base_index, overlay_index });
        }
        match &paths.mode {
            OverlayMode::Split { base_snapshot_rpmdb, overlay_rpmdb } => {
                // No `ensure_initialized` here deliberately: this is a
                // read-only query, and `ensure_initialized` needs to
                // `create_dir_all`/`rpm --initdb` under root-owned
                // `/var/lib/rakuos`, which would force every read-only
                // command (`list`, `origin`) to require root too. Callers
                // that actually write to the overlay rpmdb (install,
                // remove, upgrade) initialize it themselves right before
                // writing — see `rum_transaction`. An overlay rpmdb that
                // hasn't been initialized yet simply has nothing installed
                // in it, which `query_installed` already reports as an
                // empty list rather than an error.
                let base = rum_rpmdb::query_installed(Some(base_snapshot_rpmdb), "base")
                    .with_context(|| format!("querying base rpmdb snapshot at {}", base_snapshot_rpmdb.display()))?;
                let overlay = rum_rpmdb::query_installed(Some(overlay_rpmdb), "overlay")
                    .with_context(|| format!("querying overlay rpmdb at {}", overlay_rpmdb.display()))?;
                tracing::debug!(base = base.len(), overlay = overlay.len(), "loaded split overlay context");
                let base_index = rum_rpmdb::CapabilityIndex::build(&base);
                let overlay_index = rum_rpmdb::CapabilityIndex::build(&overlay);
                Ok(Self { base, overlay, mode: paths.mode.clone(), base_index, overlay_index })
            }
            OverlayMode::Standalone => {
                let installed = rum_rpmdb::query_installed(None, "installed").context("querying default rpmdb")?;
                tracing::debug!(installed = installed.len(), "loaded standalone (no-overlay) context");
                let base_index = rum_rpmdb::CapabilityIndex::build(&[]);
                let overlay_index = rum_rpmdb::CapabilityIndex::build(&installed);
                Ok(Self { base: Vec::new(), overlay: installed, mode: OverlayMode::Standalone, base_index, overlay_index })
            }
        }
    }

    /// Where a package (by name) currently lives, if installed at all.
    pub fn origin_of(&self, name: &str) -> Origin {
        if self.base.iter().any(|p| p.nevra.name == name) {
            Origin::Base
        } else if self.overlay.iter().any(|p| p.nevra.name == name) {
            match self.mode {
                OverlayMode::Split { .. } => Origin::Overlay,
                OverlayMode::Standalone => Origin::Installed,
            }
        } else {
            Origin::NotInstalled
        }
    }

    /// The core resolver hook: is `dep` already satisfied by the base
    /// image, such that installing it would be pure waste (extra
    /// overlay-rpmdb-tracked bytes for something the base image already
    /// has)? Always `false` in `Standalone` mode — there's no base image to
    /// check against, only "is it installed at all," which [`Self::satisfied`]
    /// already covers.
    pub fn satisfied_by_base(&self, dep: &Dependency, requiring_arch: Option<&str>) -> bool {
        self.base_index.provides(&self.base, dep, requiring_arch)
    }

    /// Is `dep` satisfied by the overlay list alone (ignoring `base`
    /// entirely)? Used by callers that already checked `satisfied_by_base`
    /// separately and want to short-circuit that repeat work — see
    /// `rum-resolver`'s `rum_rpmdb_provides_overlay`.
    pub fn overlay_satisfied(&self, dep: &Dependency, requiring_arch: Option<&str>) -> bool {
        self.overlay_index.provides(&self.overlay, dep, requiring_arch)
    }

    /// Is `dep` already satisfied by *anything* currently installed
    /// (base or overlay)? Used to short-circuit resolution entirely for a
    /// dependency that's already met, regardless of which layer met it.
    pub fn satisfied(&self, dep: &Dependency, requiring_arch: Option<&str>) -> bool {
        self.satisfied_by_base(dep, requiring_arch) || self.overlay_index.provides(&self.overlay, dep, requiring_arch)
    }
}

/// Small helper for CLI diagnostics (`rum paths`) — not used by the
/// resolver itself.
pub fn describe(paths: &OverlayPaths) -> String {
    match &paths.mode {
        OverlayMode::Split { base_snapshot_rpmdb, overlay_rpmdb } => {
            format!(
                "mode: split (RakuOS overlay)\nbase snapshot rpmdb: {}\noverlay rpmdb: {}\noverlay upperdir: {}",
                base_snapshot_rpmdb.display(),
                overlay_rpmdb.display(),
                paths.overlay_upper.display()
            )
        }
        OverlayMode::Standalone => "mode: standalone (no overlay — distrobox/podman or image-build environment)\ndbpath: rpm's default".to_string(),
    }
}

// ── packages.list / packages-rpm.list ──────────────────────────────────────
//
// The top-level, user-facing lists of overlay-installed package names —
// shared with rakuos-core's `rakuos-overlay-sync`, which reinstalls
// everything in these files after a reset/first boot. Deliberately separate
// from rum's own `reasons.json`/`history.jsonl` (which track every package,
// including dependency-only ones, and live under `OverlayPaths::state_dir`):
// these files are consumed by a different, non-rum binary and their format/
// location predates rum. Only meaningful under `OverlayMode::Split` —
// `Standalone` environments have no overlay-sync step to feed them to.
//
// Two separate lists, exactly as rakuos-core's `rakupkg` kept them: repo-
// resolved packages go in `PACKAGES_LIST` (reinstalled from repos on
// replay); packages installed from a local `.rpm` file go in
// `LOCAL_RPM_LIST` instead, since replaying those needs the actual file
// back, not just a name a repo can resolve — which is what `LOCAL_RPM_CACHE`
// is for.

/// Hardcoded (not derived from `OverlayPaths`) because rakuos-overlay-sync
/// reads this exact path regardless of anything rum's own config says.
pub const PACKAGES_LIST: &str = "/var/lib/rakuos/packages.list";

/// Local-`.rpm`-file counterpart to `PACKAGES_LIST`.
pub const LOCAL_RPM_LIST: &str = "/var/lib/rakuos/packages-rpm.list";

/// Where the actual `.rpm` file for each `LOCAL_RPM_LIST` entry is cached
/// (`<name>.rpm`) so a replay (reset/first-boot) has something to reinstall
/// from — a local file has no repo to re-download it from. Re-installing
/// the same name overwrites the cached copy, so this always holds the
/// latest version installed, matching how `PACKAGES_LIST`/repo installs
/// always pull the current repo candidate rather than a pinned old one.
pub const LOCAL_RPM_CACHE: &str = "/var/lib/rakuos/local-rpms";

fn list_names(path: &str) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn list_contains(path: &str, name: &str) -> bool {
    list_names(path).iter().any(|p| p == name)
}

/// No-ops if `name` is already listed — matches rakuos-core's `rakupkg`
/// behavior (`add_to_list`), which these files' format/consumer predates.
fn list_add(path: &str, name: &str) -> Result<()> {
    if list_contains(path, name) {
        return Ok(());
    }
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).with_context(|| format!("opening {path}"))?;
    writeln!(f, "{name}").with_context(|| format!("writing {path}"))?;
    Ok(())
}

fn list_remove(path: &str, name: &str) -> Result<()> {
    let remaining = list_names(path);
    if !remaining.iter().any(|p| p == name) {
        return Ok(());
    }
    let remaining: Vec<String> = remaining.into_iter().filter(|p| p != name).collect();
    let mut content = remaining.join("\n");
    if !remaining.is_empty() {
        content.push('\n');
    }
    std::fs::write(path, content).with_context(|| format!("writing {path}"))?;
    Ok(())
}

pub fn packages_list_names() -> Vec<String> {
    list_names(PACKAGES_LIST)
}

pub fn packages_list_contains(name: &str) -> bool {
    list_contains(PACKAGES_LIST, name)
}

pub fn packages_list_add(name: &str) -> Result<()> {
    list_add(PACKAGES_LIST, name)
}

pub fn packages_list_remove(name: &str) -> Result<()> {
    list_remove(PACKAGES_LIST, name)
}

pub fn local_rpm_list_names() -> Vec<String> {
    list_names(LOCAL_RPM_LIST)
}

pub fn local_rpm_list_contains(name: &str) -> bool {
    list_contains(LOCAL_RPM_LIST, name)
}

pub fn local_rpm_list_add(name: &str) -> Result<()> {
    list_add(LOCAL_RPM_LIST, name)
}

pub fn local_rpm_list_remove(name: &str) -> Result<()> {
    list_remove(LOCAL_RPM_LIST, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_mode_needs_nodeps_even_without_installroot() {
        // Regression test: a real, on-disk RakuOS overlay install (no
        // --installroot involved) writes to an isolated overlay-only
        // rpmdb that never contains the base image's packages, so rpm's
        // own dependency checker needs --nodeps just as much as the
        // installroot case does — omitting it here caused `rpm -Uvh` to
        // reject every base-satisfied Requires (glibc, libgcc, etc.) on a
        // real VM even though rum's resolver had already verified them.
        let paths = OverlayPaths::default();
        assert!(matches!(paths.mode, OverlayMode::Split { .. }));
        assert!(paths.installroot.is_none());
        assert!(paths.needs_nodeps());
    }

    #[test]
    fn standalone_mode_does_not_need_nodeps() {
        let paths = OverlayPaths { mode: OverlayMode::Standalone, overlay_upper: PathBuf::new(), state_dir: PathBuf::new(), installroot: None };
        assert!(!paths.needs_nodeps());
    }

    #[test]
    fn installroot_needs_nodeps_regardless_of_mode() {
        let paths = OverlayPaths { mode: OverlayMode::Standalone, overlay_upper: PathBuf::new(), state_dir: PathBuf::new(), installroot: Some(PathBuf::from("/tmp/factory")) };
        assert!(paths.needs_nodeps());
    }
}
