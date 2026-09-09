//! Dependency resolution, overlay-aware by construction: every candidate
//! dependency is checked against the live [`OverlayContext`] *before* the
//! resolver goes looking for a repo candidate to satisfy it. If the base
//! image already provides it, it's never added to the install set — dnf (or
//! any resolver working from a single merged view of "installed") has no
//! way to make that distinction, since to it the base-provided package and
//! an overlay-provided one look identical. Base packages are never
//! reconsidered, re-layered, or modified by anything below — the SAT
//! problem this module builds only ever contains variables for repo
//! candidates that might be *added* to the overlay.
//!
//! Unlike the earlier backtracking-worklist resolver this replaced, this is
//! a real (if minimal) SAT solver, matching the approach dnf5/libdnf5 use
//! via libsolv: build the whole relevant "pool" of candidate packages up
//! front, translate every `Requires`/`Conflicts`/`Obsoletes`/rich-dependency
//! relationship into CNF clauses over one boolean variable per candidate
//! package ("is this exact NEVRA installed?"), then solve with unit
//! propagation (two-watched-literal, the standard efficient scheme) and
//! chronological backtracking on the rare occasions a decision is actually
//! needed. The old approach re-explored the same competing-provider choice
//! independently at every occurrence in the dependency tree, which multiplies
//! out combinatorially on a big enough package graph (verified against
//! Steam's real Fedora + RPM Fusion dependency tree: 15+ minutes pinned at
//! 100% CPU with no result). A real solver's unit propagation collapses the
//! overwhelming majority of the graph — anything with only one possible
//! provider — into forced assignments without ever "deciding" anything,
//! and genuinely ambiguous choices get made once and shared across every
//! occurrence instead of being re-litigated per occurrence.
//!
//! It's also properly multilib/arch-aware: a versioned symbol capability
//! like `libjpeg.so.62(LIBJPEG_6.2)` is provided *identically* (same literal
//! string, no arch marker) by both the x86_64 and i686 builds of a library —
//! rpm itself doesn't disambiguate these by capability text, it relies on
//! the resolver staying inside one arch's dependency chain once it's
//! committed to it. So every discovered dependency carries an `arch` hint
//! (the arch of the package whose `Requires` produced it — `None` for a
//! top-level requested name, meaning "prefer the host's own arch"), which
//! feeds into which candidates get offered as literals in each clause. An
//! explicit ISA suffix on the capability itself (`(x86-32)`, `(x86-64)`,
//! `(64bit)`) overrides the inherited hint, since that's rpm's own way of
//! saying "I need the *other* arch's copy of this specifically" (the
//! mechanism that lets a plain x86_64 package pull in an i686 multilib
//! dependency in the first place). Two versions of the *same* (name, arch)
//! pair are mutually exclusive (an at-most-one clause per group); two
//! different arches of the same name are not — that's exactly what lets
//! both the x86_64 and i686 builds of a library end up installed side by
//! side.

use anyhow::{bail, Result};
use rum_core::{Dependency, EvrCompare, Package};
use rum_overlay::OverlayContext;
use std::collections::{HashMap, HashSet};

/// The result of resolving a set of requested package names: what actually
/// needs installing (in dependency-first order, satisfiable by
/// [`rum_transaction`] as-is), and, for visibility, what was skipped
/// because it's already covered by the base image or an existing overlay
/// install.
#[derive(Debug, Default)]
pub struct Plan {
    /// Packages to install. Order is not a dependency-first guarantee —
    /// rpm's own transaction ordering handles that; this is just "the full
    /// install set".
    pub to_install: Vec<Package>,
    pub already_satisfied: Vec<SatisfiedBy>,
    /// Already-installed (base or overlay) packages that an Obsoletes entry
    /// on something in `to_install` will cause `rpm -U` to erase as part of
    /// the same transaction. rum doesn't issue a separate removal for
    /// these — rpm itself auto-erases obsoleted packages during `-U`, the
    /// same as it would under dnf — this list exists purely so callers can
    /// show the user what's about to disappear before running the
    /// transaction, matching dnf's "Obsoleting" line in its plan summary.
    pub to_obsolete: Vec<Package>,
    /// Already-installed *overlay* packages that must be explicitly erased
    /// (a separate `rpm -e` transaction, unlike `to_obsolete`) before/around
    /// installing `to_install`, because something being installed
    /// `Conflicts` with them and [`ResolveOptions::allow_erasing`] was set —
    /// rum's answer to dnf's `--allowerasing`. Never contains a *base*-image
    /// package: rum can't erase those (see `resolve_ex`'s conflicts-block
    /// comment), so a conflict against a base package is always a hard
    /// failure regardless of `allow_erasing`.
    pub to_erase: Vec<Package>,
    /// Requested names dropped by [`ResolveOptions::skip_broken`] because no
    /// combination including them could be resolved — dnf5's `skip_broken=`
    /// parity: the rest of the transaction still goes through instead of
    /// the whole `install` failing over one bad/renamed/unavailable name.
    /// Always empty unless `skip_broken` was set.
    pub skipped: Vec<String>,
    /// Subset of `to_install` that are upgrades of an already-installed
    /// (overlay) package to a newer EVR, as opposed to a brand new package
    /// pulled in as a dependency — only populated by [`resolve_upgrade`] and
    /// [`resolve_distro_sync`]. Exists so callers like `rum check-upgrade`
    /// can report "what would `rum upgrade` actually change" without
    /// re-deriving it via an independent version comparison that wouldn't
    /// respect the resolver's own locking (e.g. Split mode's base-owned
    /// name lock across all arches).
    pub to_upgrade: Vec<Package>,
}

#[derive(Debug, Clone)]
pub struct SatisfiedBy {
    pub dependency: Dependency,
    pub origin: rum_core::Origin,
}

/// Tunable resolution behavior beyond the plain hard-`Requires` closure
/// [`resolve`] always does — see [`resolve_ex`].
#[derive(Debug, Clone)]
pub struct ResolveOptions {
    /// Whether to also pull in `Recommends` (weak deps) best-effort
    /// alongside the hard `Requires` closure — dnf's `install_weak_deps=`
    /// (default `true`). Unlike `Requires`, a `Recommends` target that
    /// can't be resolved (missing provider, or a provider that itself fails
    /// to resolve) is silently skipped rather than failing the whole
    /// transaction. `Suggests` is never auto-installed, matching dnf (it
    /// has no equivalent toggle for that).
    pub install_weak_deps: bool,
    /// Package names (or globs) allowed to have multiple versions
    /// installed side by side — dnf's `installonlypkgs=`. A candidate whose
    /// name matches is exempt from the resolver's usual "only one version
    /// of a (name, arch) at a time" / "never replace an already-installed
    /// version" clauses, since e.g. installing a new kernel alongside the
    /// running one is exactly the point, not a conflict.
    pub installonly_pkgs: Vec<String>,
    /// dnf's `--allowerasing`: when a candidate being installed `Conflicts`
    /// with an already-installed *overlay* package, erase that package as
    /// part of the same logical operation instead of treating the conflict
    /// as a hard failure. Never lets a base-image package be erased this
    /// way — see [`Plan::to_erase`]. `swap` always behaves as if this were
    /// set (see `rum-cli`'s `Command::Swap`), since a swap that can't clear
    /// a conflicting sibling package out of the way isn't a swap at all.
    pub allow_erasing: bool,
    /// Top-level requested names that must be force-expanded into a real
    /// install even if some *differently-named* already-installed package
    /// (base or overlay) happens to `Provides` the same capability — e.g.
    /// `swap`'s install half, where the point is to guarantee the literal
    /// named package ends up installed. Without this, `discover()`'s usual
    /// "already satisfied by base" short-circuit can no-op the install
    /// entirely: `rum swap libswscale-free libswscale` when
    /// `libswscale-free` lives in the *base* image (so the remove half is
    /// also a no-op — `remove` can't touch base packages) and Provides
    /// `libswscale` as a rename-Provides leaves the free build in place
    /// instead of actually swapping it out.
    pub force_names: std::collections::HashSet<String>,
    /// dnf's protected-packages list (kernel, glibc, dnf itself, `rum.conf`'s
    /// `protected_packages=` plus rum's hardcoded always-protected names) —
    /// passed straight through to [`rum_solv::SolveRequest::protected_names`]
    /// so libsolv itself (not just an app-level guard on explicit `rum
    /// remove`) never proposes erasing one of these as a side effect of an
    /// install/upgrade's own conflict or obsolete resolution.
    pub protected_names: Vec<String>,
    /// dnf5's versionlock plugin: names an already-installed package may
    /// never move away from its current EVR, even as a *transitive* pull by
    /// some unrelated install/upgrade — passed straight through to
    /// [`rum_solv::build_pool_locked`] so a locked-but-mismatched-EVR
    /// candidate never enters the pool at all, rather than only being
    /// checked at `rum-cli`'s direct `install`/`upgrade`/`distro-sync` entry
    /// points.
    pub locked_names: Vec<rum_core::VersionLock>,
    /// dnf5's `skip_broken=`/`--skip-broken`: if the full requested set
    /// can't be resolved together, drop whichever requested names are
    /// actually responsible one at a time and retry, instead of failing the
    /// whole `install` over one bad name — see `resolve_ex`'s retry loop.
    /// Default `false` (dnf5's own default too — `skip_broken=0`).
    pub skip_broken: bool,
    /// dnf5's `multilib_policy=`: `false` (`best`, the default) lets a bare
    /// `install foo` job span every arch build of `foo` and leaves libsolv's
    /// own policy to pick one; `true` (`all`) instead installs every
    /// distinct arch build side by side. Passed straight through to
    /// [`rum_solv::SolveRequest::multilib_all`].
    pub multilib_all: bool,
    /// dnf5's `best=`/`--best`: require the newest available EVR to
    /// resolve cleanly rather than silently backtracking to an older,
    /// satisfiable one — passed straight through to
    /// [`rum_solv::SolveRequest::best`]. Default `false` (dnf5's own
    /// default too — `best=0`).
    pub best: bool,
    /// dnf5's `obsoletes=`: when an `install` name matches nothing
    /// literal/provided, widen the search to packages that `Obsoletes` it
    /// — passed straight through to [`rum_solv::SolveRequest::obsoletes`].
    /// Default `true` (dnf5's own default too — `obsoletes=1`).
    pub obsoletes: bool,
    /// dnf5's `allow_downgrade=`/`--allow-downgrade`/`--no-allow-downgrade`
    /// — passed straight through to [`rum_solv::SolveRequest::allow_downgrade`].
    /// Default `true` (dnf5's own default too).
    pub allow_downgrade: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            install_weak_deps: true,
            installonly_pkgs: Vec::new(),
            allow_erasing: false,
            force_names: std::collections::HashSet::new(),
            protected_names: Vec::new(),
            locked_names: Vec::new(),
            skip_broken: false,
            multilib_all: false,
            best: false,
            obsoletes: true,
            allow_downgrade: true,
        }
    }
}

/// A capability -> providers lookup built once per [`resolve`] call, so
/// candidate lookup doesn't have to linearly scan every candidate package
/// for every dependency processed. A capability is indexed under both a
/// package's own name (an unversioned `Requires` on the package itself) and
/// every name in its `Provides` list (a `Requires` on a capability, not a
/// package name directly) — exactly the two ways [`dep_matches_pkg`] can
/// match.
struct CandidateIndex<'a> {
    by_capability: HashMap<&'a str, Vec<&'a Package>>,
}

impl<'a> CandidateIndex<'a> {
    fn build(candidates: &'a [Package]) -> Self {
        let mut by_capability: HashMap<&'a str, Vec<&'a Package>> = HashMap::new();
        for pkg in candidates {
            by_capability.entry(pkg.nevra.name.as_str()).or_default().push(pkg);
            for p in &pkg.provides {
                by_capability.entry(p.name.as_str()).or_default().push(pkg);
            }
            // Real rpm unconditionally auto-generates an ISA-tagged
            // self-provide (`name(x86-64)`/`name(x86-32)`) for every
            // 64-/32-bit-capable package, but repo primary.xml metadata
            // doesn't always spell that out explicitly in `provides` — a
            // package missing the explicit entry would otherwise never be
            // indexed under that capability at all, making a plain
            // `Requires: foo(x86-64)` unsatisfiable even though `foo` is a
            // perfectly valid, present candidate (see `dep_matches_pkg`).
            if let Some(isa) = match pkg.nevra.arch.as_str() {
                "x86_64" => Some("x86-64"),
                "i686" => Some("x86-32"),
                _ => None,
            } {
                let key: &'a str = format!("{}({isa})", pkg.nevra.name).leak();
                by_capability.entry(key).or_default().push(pkg);
            }
        }
        Self { by_capability }
    }

    fn lookup(&self, name: &str) -> &[&'a Package] {
        self.by_capability.get(name).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

/// dnf's repo `cost=` is meant to mean "prefer this repo unless nothing
/// else has the package" — Fedora's own `fedora-updates-archive.repo` sets
/// `cost=10000` specifically, per its own comment, "in order to prefer the
/// normal Fedora yum repositories, which means only packages that can't be
/// found elsewhere will be downloaded from here." But libsolv's SAT pool
/// (`rum_solv::build_pool_locked`) treats every solvable as an equally
/// selectable candidate regardless of which repo it came from — nothing
/// upstream of it encoded repo preference, so a same-name+arch build
/// sitting in a high-cost/low-priority repo could still get picked over an
/// otherwise-equal (or even better) copy from a lower-cost repo. Live bug
/// this fixes: `rum install gcc-c++` resolving to Fedora's
/// `updates-archive` (cost=10000) and failing on that repo's own CDN
/// propagation lag, when `updates`/`fedora` (cost=1000) had the very same
/// build.
///
/// Applied once, before any pool is built: for each `(name, arch)`, only
/// the candidate(s) carrying the lowest `repo_cost` seen for that pair
/// survive into the pool — a higher-cost repo's build of that name+arch
/// only ever reaches libsolv (and thus only ever gets picked, exact-NEVRA
/// pins included) when it's the *only* source for it, exactly matching
/// what `cost=` is supposed to mean. Deliberately not applied to the raw
/// `candidates` slice callers also use for request-string parsing
/// (`match_exact_nevra_request`/`split_name_arch_request` below) — a user
/// naming an exact NEVRA that only exists in a high-cost repo should still
/// resolve to it; this only changes what libsolv reaches for on its own.
fn prefer_low_cost_repos(candidates: &[Package]) -> Vec<Package> {
    let mut min_cost: HashMap<(&str, &str), u32> = HashMap::new();
    for pkg in candidates {
        let key = (pkg.nevra.name.as_str(), pkg.nevra.arch.as_str());
        min_cost.entry(key).and_modify(|c| *c = (*c).min(pkg.repo_cost)).or_insert(pkg.repo_cost);
    }
    candidates.iter().filter(|pkg| pkg.repo_cost == min_cost[&(pkg.nevra.name.as_str(), pkg.nevra.arch.as_str())]).cloned().collect()
}

/// Resolves `requested` package names against `candidates` (typically the
/// concatenation of every enabled repo's package list), skipping anything
/// [`OverlayContext`] already reports as satisfied. `host_arch` (e.g.
/// `"x86_64"`) is preferred for top-level requested names and any
/// dependency with no inherited arch hint — see the module doc comment for
/// why a dependency needs an arch hint at all.
/// Splits a top-level request like `"xz-devel.i686"` into
/// `("xz-devel", Some("i686"))` — rpm/dnf's own `name.arch` disambiguation
/// syntax — but only when a package actually named `xz-devel` at arch
/// `i686` exists among the candidates or the installed set. Falls back to
/// treating the whole string as a literal (unqualified) name otherwise,
/// since an rpm package name can itself legitimately contain a dot.
fn split_name_arch_request<'a>(name: &'a str, candidates: &[Package], overlay: &OverlayContext) -> (&'a str, Option<&'a str>) {
    if let Some((base, arch)) = name.rsplit_once('.') {
        let known = candidates.iter().any(|p| p.nevra.name == base && p.nevra.arch == arch)
            || overlay.base.iter().chain(&overlay.overlay).any(|p| p.nevra.name == base && p.nevra.arch == arch);
        if known {
            return (base, Some(arch));
        }
    }
    (name, None)
}

/// Matches `request` against a full NEVRA spec (`rum install
/// kernel-6.19.14-300.fc44.x86_64`, dnf's own exact-build install syntax)
/// rather than a plain name — tried by literal string equality against
/// every candidate's own `name-version-release.arch` / `name-version-
/// release` formatting, since we have the whole candidate pool in hand
/// rather than needing to guess where the name ends and the EVR begins
/// (rpm names can themselves contain hyphens, so blind splitting is
/// ambiguous — a full pool scan for an exact match isn't). Ambiguity
/// across multilib archs (a version-release with no `.arch` suffix
/// matching both an x86_64 and i686 build) is broken by preferring
/// `host_arch`.
fn match_exact_nevra_request(request: &str, candidates: &[Package], host_arch: &str) -> Option<(String, String, String, String)> {
    let mut matches: Vec<&Package> = candidates
        .iter()
        .filter(|p| {
            let with_arch = format!("{}-{}-{}.{}", p.nevra.name, p.nevra.version, p.nevra.release, p.nevra.arch);
            let no_arch = format!("{}-{}-{}", p.nevra.name, p.nevra.version, p.nevra.release);
            request == with_arch || request == no_arch
        })
        .collect();
    matches.sort_by_key(|p| p.nevra.arch != host_arch);
    matches.first().map(|p| (p.nevra.name.clone(), p.nevra.version.clone(), p.nevra.release.clone(), p.nevra.arch.clone()))
}

pub fn resolve(requested: &[String], candidates: &[Package], overlay: &OverlayContext, host_arch: &str) -> Result<Plan> {
    resolve_ex(requested, candidates, overlay, host_arch, &ResolveOptions::default())
}

/// Same as [`resolve`], with [`ResolveOptions`] controlling weak-dependency
/// and installonly-package behavior.
///
/// Backed by libsolv ([`rum_solv`]) rather than a hand-rolled SAT solver.
/// `installonly_pkgs`/`allow_erasing`/`force_names`/`install_weak_deps` are
/// not yet translated into libsolv jobs/flags — only the plain `Requires`
/// closure and RakuOS's Split-mode "never erase a base package" guarantee
/// (enforced by locking every base solvable before every solve) are wired
/// up so far.
pub fn resolve_ex(requested: &[String], candidates: &[Package], overlay: &OverlayContext, host_arch: &str, options: &ResolveOptions) -> Result<Plan> {
    // RakuOS carries no SELinux userspace at all (see the module doc
    // comment's Split-mode framing), and a handful of upstream repos ship
    // metadata with dependencies that only make sense for one specific,
    // unrelated requiring package (`is_broken_repo_dep_edge_ignored`, e.g.
    // `ffmpeg-libs`'s bogus nvidia kmod pull-in). None of these are real
    // needs, so they're stripped out of `Requires` before the package ever
    // reaches libsolv, rather than ever being treated as an unsatisfiable
    // dependency.
    let selinux_ignored_in_scope = matches!(overlay.mode, rum_overlay::OverlayMode::Split { .. }) && !selinux_allowed();
    let filter_pkg = |pkg: &Package| -> Package {
        let mut p = pkg.clone();
        p.requires.retain(|req| {
            !((selinux_ignored_in_scope && is_selinux_ignored(&req.name))
                || is_broken_repo_dep_edge_ignored(&pkg.nevra.name, &req.name))
        });
        p
    };

    let base: Vec<Package> = overlay.base.iter().map(filter_pkg).collect();
    let overlay_pkgs: Vec<Package> = overlay.overlay.iter().map(filter_pkg).collect();
    let available: Vec<Package> = prefer_low_cost_repos(candidates).iter().map(filter_pkg).collect();

    let mut build = rum_solv::build_pool_locked(host_arch, &base, &overlay_pkgs, &available, &options.locked_names)?;

    let try_solve = |build: &mut rum_solv::PoolBuild, names: &[String]| -> Result<rum_solv::SolveResult> {
        let mut install_names: Vec<(String, Option<String>)> = Vec::new();
        let mut pinned_installs: Vec<(String, String, String, String)> = Vec::new();
        for name in names {
            if let Some(nevra) = match_exact_nevra_request(name, candidates, host_arch) {
                pinned_installs.push(nevra);
            } else {
                let (base_name, arch) = split_name_arch_request(name, candidates, overlay);
                install_names.push((base_name.to_string(), arch.map(str::to_string)));
            }
        }
        rum_solv::solve(
            build,
            &rum_solv::SolveRequest {
                install_names,
                pinned_installs,
                installonly_names: options.installonly_pkgs.clone(),
                protected_names: options.protected_names.clone(),
                allow_erasing: options.allow_erasing,
                multilib_all: options.multilib_all,
                best: options.best,
                obsoletes: options.obsoletes,
                allow_downgrade: options.allow_downgrade,
                ..Default::default()
            },
        )
    };

    // dnf5's `skip_broken=`/`--skip-broken`: rather than failing the whole
    // `install` over one bad/renamed/unavailable requested name, drop
    // whichever single name (tested one at a time) turns an unsolvable
    // request into a solvable one, and keep retrying until either
    // everything solves or no single remaining name's removal helps (a
    // genuine multi-package interaction problem, which skip_broken can't
    // rescue either — dnf5 itself falls back to reporting the failure at
    // that point too). Best-effort, not a full minimal-unsat-core search:
    // covers the common real-world case (one typo'd/unavailable package
    // among several good ones in a scripted `rum install a b c`), not
    // every pathological multi-name conflict.
    let mut remaining: Vec<String> = requested.to_vec();
    let mut skipped: Vec<String> = Vec::new();
    let result = loop {
        match try_solve(&mut build, &remaining) {
            Ok(r) => break r,
            Err(e) => {
                if !options.skip_broken || remaining.is_empty() {
                    return Err(e);
                }
                let culprit = (0..remaining.len()).find(|&i| {
                    let candidate_set: Vec<String> = remaining.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, n)| n.clone()).collect();
                    try_solve(&mut build, &candidate_set).is_ok()
                });
                match culprit {
                    Some(i) => skipped.push(remaining.remove(i)),
                    None => return Err(e),
                }
            }
        }
    };

    // `result.to_upgrade` covers packages libsolv moved to a newer EVR as a
    // side effect of this install (e.g. syncing an already-installed
    // x86_64 build to a newly-requested i686 sibling's EVR, avoiding a
    // multilib file conflict) — these are real transaction members, not
    // optional, so they must ride along in `to_install` (rpm -U handles
    // both new installs and upgrades identically; `apply_install_ex`
    // itself classifies Installing vs Upgrading by comparing against what's
    // already on disk, not by which list a package came from here).
    let mut to_install: Vec<Package> = result.to_install.into_iter().chain(result.to_upgrade).collect();

    // `force_names` (used by `download`/`swap`): libsolv's by-name INSTALL
    // job no-ops when an identical NEVRA is already installed, so a name
    // whose only purpose here is "fetch this package's RPM regardless of
    // install state" (e.g. `rakuos reset-overlay --soft` re-caching every
    // currently installed overlay package before wiping the overlay) would
    // otherwise silently vanish from `to_install` — this was previously
    // dead: `options.force_names` was populated by callers but never read
    // anywhere in the resolver. Backfill any such name directly from
    // whatever's already installed, matched against a same-NEVRA candidate
    // so the fetched file is byte-identical to what's on disk (falling
    // back to the newest available candidate for that name/arch if the
    // exact EVR is no longer offered by any repo).
    if !options.force_names.is_empty() {
        let present: HashSet<String> = to_install.iter().map(|p| p.nevra.name.clone()).collect();
        for name in &options.force_names {
            if present.contains(name) {
                continue;
            }
            let Some(installed) = overlay.base.iter().chain(&overlay.overlay).find(|p| p.nevra.name == *name) else {
                continue;
            };
            let exact = candidates
                .iter()
                .find(|c| c.nevra.name == installed.nevra.name && c.nevra.evr() == installed.nevra.evr() && c.nevra.arch == installed.nevra.arch);
            let pick = exact.or_else(|| {
                candidates
                    .iter()
                    .filter(|c| c.nevra.name == installed.nevra.name && c.nevra.arch == installed.nevra.arch)
                    .max_by(|a, b| a.nevra.evr().as_str().compare_evr(b.nevra.evr().as_str()))
            });
            if let Some(pkg) = pick {
                to_install.push(pkg.clone());
            }
        }
    }

    reject_downgrades_if_disallowed(&to_install, overlay, options)?;

    Ok(Plan { to_install, already_satisfied: Vec::new(), to_obsolete: result.to_obsolete, to_erase: result.to_erase, skipped, to_upgrade: Vec::new() })
}

/// `SOLVER_FLAG_ALLOW_DOWNGRADE` only governs implicit dependency
/// resolution — it doesn't stop libsolv from honoring an *explicit*
/// pinned-NEVRA install job naming an older build than what's currently
/// installed. dnf5 enforces `--no-allow-downgrade` on that case itself, so
/// mirror it here: reject up front rather than let the transaction reach
/// `rpm -U`, which would fail anyway without `--oldpackage`.
fn reject_downgrades_if_disallowed(to_install: &[Package], overlay: &OverlayContext, options: &ResolveOptions) -> Result<()> {
    if options.allow_downgrade {
        return Ok(());
    }
    let installed: HashMap<(&str, &str), String> =
        overlay.base.iter().chain(&overlay.overlay).map(|p| ((p.nevra.name.as_str(), p.nevra.arch.as_str()), p.nevra.evr())).collect();
    for pkg in to_install {
        if let Some(old_evr) = installed.get(&(pkg.nevra.name.as_str(), pkg.nevra.arch.as_str())) {
            if pkg.nevra.evr().as_str().compare_evr(old_evr).is_lt() {
                anyhow::bail!(
                    "package {}-{} (which is newer than {}) is already installed — pass --allow-downgrade to downgrade",
                    pkg.nevra.name,
                    old_evr,
                    pkg.nevra
                );
            }
        }
    }
    Ok(())
}

/// The SELinux/broken-repo-dep filtering `resolve_ex` does before ever
/// building the pool — factored out so the `upgrade`/`distro-sync`/
/// pinned-downgrade-or-reinstall entry points below share it exactly
/// rather than re-deciding it.
fn build_filtered_pool(candidates: &[Package], overlay: &OverlayContext, host_arch: &str, locked_names: &[rum_core::VersionLock]) -> Result<rum_solv::PoolBuild> {
    let selinux_ignored_in_scope = matches!(overlay.mode, rum_overlay::OverlayMode::Split { .. }) && !selinux_allowed();
    let filter_pkg = |pkg: &Package| -> Package {
        let mut p = pkg.clone();
        p.requires.retain(|req| {
            !((selinux_ignored_in_scope && is_selinux_ignored(&req.name)) || is_broken_repo_dep_edge_ignored(&pkg.nevra.name, &req.name))
        });
        p
    };
    let base: Vec<Package> = overlay.base.iter().map(filter_pkg).collect();
    let overlay_pkgs: Vec<Package> = overlay.overlay.iter().map(filter_pkg).collect();
    let available: Vec<Package> = prefer_low_cost_repos(candidates).iter().map(filter_pkg).collect();
    rum_solv::build_pool_locked(host_arch, &base, &overlay_pkgs, &available, locked_names)
}

/// `rum upgrade [names...]` / `rum system-upgrade`: mirrors dnf5's
/// `add_upgrade` (`SOLVER_UPDATE`) — named packages (or every installed
/// package, if `names` is empty) move to their newest available EVR.
/// Unlike [`resolve_ex`]'s install jobs, a name with nothing newer
/// available is silently left alone rather than erroring.
pub fn resolve_upgrade(names: &[String], candidates: &[Package], overlay: &OverlayContext, host_arch: &str, options: &ResolveOptions) -> Result<Plan> {
    let mut build = build_filtered_pool(candidates, overlay, host_arch, &options.locked_names)?;
    let result = rum_solv::solve(
        &mut build,
        &rum_solv::SolveRequest {
            upgrade_names: names.to_vec(),
            upgrade_all: names.is_empty(),
            installonly_names: options.installonly_pkgs.clone(),
            protected_names: options.protected_names.clone(),
            allow_erasing: options.allow_erasing,
            best: options.best,
            allow_downgrade: options.allow_downgrade,
            ..Default::default()
        },
    )?;
    let to_upgrade = result.to_upgrade;
    let to_install: Vec<Package> = result.to_install.into_iter().chain(to_upgrade.clone()).collect();
    Ok(Plan { to_install, already_satisfied: Vec::new(), to_obsolete: result.to_obsolete, to_erase: result.to_erase, skipped: Vec::new(), to_upgrade })
}

/// `rum distro-sync [names...]`: mirrors dnf5's `add_distro_sync`
/// (`SOLVER_DISTUPGRADE`) — named packages (or every installed package)
/// are forced to match exactly whatever the enabled repos currently offer,
/// downgrading if that's what's available, unlike `resolve_upgrade`.
pub fn resolve_distro_sync(names: &[String], candidates: &[Package], overlay: &OverlayContext, host_arch: &str, options: &ResolveOptions) -> Result<Plan> {
    let mut build = build_filtered_pool(candidates, overlay, host_arch, &options.locked_names)?;
    let result = rum_solv::solve(
        &mut build,
        &rum_solv::SolveRequest {
            distro_sync_names: names.to_vec(),
            distro_sync_all: names.is_empty(),
            installonly_names: options.installonly_pkgs.clone(),
            protected_names: options.protected_names.clone(),
            allow_erasing: options.allow_erasing,
            best: options.best,
            allow_downgrade: options.allow_downgrade,
            ..Default::default()
        },
    )?;
    let to_upgrade = result.to_upgrade;
    let to_install: Vec<Package> = result.to_install.into_iter().chain(to_upgrade.clone()).collect();
    Ok(Plan { to_install, already_satisfied: Vec::new(), to_obsolete: result.to_obsolete, to_erase: result.to_erase, skipped: Vec::new(), to_upgrade })
}

/// `rum downgrade`/`rum reinstall`: `targets` is a set of exact NEVRAs the
/// caller already picked (rum-cli's `sync_packages` does its own "highest
/// older EVR" / "exact same EVR" selection — that decision is rum's own
/// policy, not something libsolv's job model expresses directly). This
/// just gets that already-decided target past libsolv's "already
/// satisfied" no-op via a pinned `SOLVER_SOLVABLE | SOLVER_INSTALL` job
/// (mirrors dnf5's `add_downgrade`/`add_reinstall`), so the resulting
/// transaction still goes through a real dependency-satisfiability solve
/// (and Split mode's base-package lock) rather than being trusted as-is.
pub fn resolve_pinned(targets: &[Package], candidates: &[Package], overlay: &OverlayContext, host_arch: &str, options: &ResolveOptions) -> Result<Plan> {
    let mut build = build_filtered_pool(candidates, overlay, host_arch, &options.locked_names)?;
    let pinned_installs: Vec<(String, String, String, String)> =
        targets.iter().map(|p| (p.nevra.name.clone(), p.nevra.version.clone(), p.nevra.release.clone(), p.nevra.arch.clone())).collect();
    let result = rum_solv::solve(
        &mut build,
        &rum_solv::SolveRequest {
            pinned_installs,
            installonly_names: options.installonly_pkgs.clone(),
            protected_names: options.protected_names.clone(),
            allow_erasing: options.allow_erasing,
            allow_downgrade: options.allow_downgrade,
            ..Default::default()
        },
    )?;
    let to_install: Vec<Package> = result.to_install.into_iter().chain(result.to_upgrade).collect();
    // Note: `rum downgrade` itself always calls this with `allow_downgrade:
    // true` implied by its own semantics (rum-cli's `sync_packages` picks
    // the pinned target *because* it's older) — this guard only bites when
    // `--no-allow-downgrade` is combined with a pinned NEVRA install spec
    // naming an older build than what's installed (`rum install foo-1.0`).
    reject_downgrades_if_disallowed(&to_install, overlay, options)?;

    Ok(Plan { to_install, already_satisfied: Vec::new(), to_obsolete: result.to_obsolete, to_erase: result.to_erase, skipped: Vec::new(), to_upgrade: Vec::new() })
}

pub fn compute_erasures(to_install: &[Package], to_obsolete: &[Package], overlay: &OverlayContext, options: &ResolveOptions) -> Result<Vec<Package>> {
    // Overlay packages that `to_install` genuinely conflicts with — the
    // conflicts block in `resolve_ex` only *permitted* selecting such
    // candidates under `allow_erasing`, it didn't decide anything about
    // erasure itself.
    let mut to_erase: Vec<Package> = if options.allow_erasing {
        overlay
            .overlay
            .iter()
            .filter(|installed| to_install.iter().any(|c| c.conflicts.iter().any(|conf| dep_matches_pkg(conf, installed))))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };

    // (name, arch) pairs that will actually *disappear* from "installed" as
    // a direct effect of this transaction: genuinely obsoleted, or a
    // same-name-and-arch in-place upgrade/downgrade/distro-sync (`rpm -U`
    // always erases the prior build of a name+arch it's installing over,
    // before anything else in this function runs). Keyed on arch too, not
    // just name — multilib means an already-installed `glibc.x86_64` and a
    // newly-added `glibc.i686` share a name but are entirely independent
    // packages; matching by name alone wrongly treated installing the
    // i686 side as erasing the untouched x86_64 build, breaking every
    // Requires (e.g. bash's `libc.so.6()(64bit)`) that the real x86_64
    // package still satisfies.
    let install_names: HashSet<(&str, &str)> = to_install.iter().map(|p| (p.nevra.name.as_str(), p.nevra.arch.as_str())).collect();
    let is_disappearing = |pkg: &Package| -> bool {
        to_obsolete.iter().any(|o| o.nevra == pkg.nevra) || install_names.contains(&(pkg.nevra.name.as_str(), pkg.nevra.arch.as_str()))
    };

    // Same-srpm subpackages frequently pin each other's exact EVR (e.g.
    // `rpm-plugin-selinux` requiring `rpm-libs = <exact old EVR>`) — `rpm
    // -U` erases that old build as part of installing the new one
    // regardless of anything above, which breaks any such consumer that
    // has no matching newer build available to move to. dnf/libdnf's own
    // combined-transaction solve either erases a now-broken consumer too
    // (under `--allowerasing`) or refuses the whole transaction up front
    // with a clear message; without this, rum would instead let the
    // transaction reach `rpm -U` and fail there with an opaque
    // "Failed dependencies" error.
    if options.allow_erasing {
        // Only ever re-checks a `Requires` that this transaction is
        // actually touching (i.e. was satisfied by a package that's about
        // to disappear) — never validates an installed package's *entire*
        // Requires list against the provider pool. rpmdb-sourced `Package`
        // entries don't always carry the full implicit file-based Provides
        // real rpm generates (`/bin/sh`, library sonames, …), so a blanket
        // "is every Requires still satisfied" check produces false
        // positives across huge swaths of the system and cascades into
        // erasing far more than the transaction touches.
        let mut removed: Vec<Package> = to_obsolete.iter().cloned().chain(
            overlay.overlay.iter().filter(|p| install_names.contains(&(p.nevra.name.as_str(), p.nevra.arch.as_str()))).cloned(),
        ).collect();
        removed.extend(to_erase.iter().cloned());

        let all_installed: Vec<&Package> = overlay.base.iter().chain(&overlay.overlay).collect();

        // Iterate to a fixed point: erasing one broken consumer can itself
        // remove the last provider of some *other* installed package's
        // Requires. Bounded rather than a `loop` purely as a belt-and-
        // suspenders guard against surprise cycles in real-world repo data.
        for _ in 0..8 {
            let providers: Vec<&Package> = overlay
                .base
                .iter()
                .chain(&overlay.overlay)
                .filter(|p| !is_disappearing(p) && !to_erase.iter().any(|e| e.nevra == p.nevra))
                .chain(to_install)
                .collect();
            let newly_broken: Vec<Package> = overlay
                .overlay
                .iter()
                .filter(|installed| !is_disappearing(installed) && !to_erase.iter().any(|e| e.nevra == installed.nevra))
                .filter(|installed| {
                    expand_rich_requires(installed.requires.iter(), &all_installed).iter().any(|req| {
                        removed.iter().any(|r| dep_matches_pkg(req, r)) && !providers.iter().any(|p| dep_matches_pkg(req, p))
                    })
                })
                .cloned()
                .collect();
            if newly_broken.is_empty() {
                break;
            }
            removed.extend(newly_broken.iter().cloned());
            to_erase.extend(newly_broken);
        }
    } else {
        let removed: Vec<Package> =
            to_obsolete.iter().cloned().chain(overlay.overlay.iter().filter(|p| install_names.contains(&(p.nevra.name.as_str(), p.nevra.arch.as_str()))).cloned()).collect();
        let providers: Vec<&Package> = overlay.base.iter().chain(&overlay.overlay).filter(|p| !is_disappearing(p)).chain(to_install).collect();
        let all_installed: Vec<&Package> = overlay.base.iter().chain(&overlay.overlay).collect();
        for installed in overlay.overlay.iter().filter(|p| !is_disappearing(p)) {
            for req in expand_rich_requires(installed.requires.iter(), &all_installed) {
                if removed.iter().any(|r| dep_matches_pkg(&req, r)) && !providers.iter().any(|p| dep_matches_pkg(&req, p)) {
                    bail!(
                        "'{}' requires '{}', which this transaction would remove (rerun with --allowerasing to erase '{}' instead)",
                        installed.nevra,
                        req.name,
                        installed.nevra.name
                    );
                }
            }
        }
    }

    Ok(to_erase)
}

/// Works out every other installed overlay package that must also be
/// `rpm -e`d for `rum remove <names>` to apply cleanly — dnf's own
/// `remove` computes and includes this same dependent closure (reported as
/// "The following ... will also be removed") rather than handing the bare
/// requested set to the transaction and letting it fail with rpm's opaque
/// "Failed dependencies" error the way a plain `rpm -e` would.
///
/// Iterates to a fixed point the same way [`compute_erasures`] does:
/// erasing one now-broken dependent can itself break another package's
/// Requires (e.g. removing `selinux-policy` breaks `container-selinux`,
/// `passt-selinux`, `flatpak-selinux`, ... all in one pass, but a deeper
/// dependency chain could take more than one). Only ever re-checks a
/// `Requires` that was actually satisfied by something in the removal set —
/// not an installed package's entire Requires list — for the same reason
/// `compute_erasures` doesn't: rpmdb-sourced `Package` entries don't carry
/// the full implicit file-based Provides real rpm generates, so a blanket
/// check produces false positives across the whole system.
pub fn compute_removal_closure(requested: &[Package], overlay: &OverlayContext) -> Vec<Package> {
    let mut closure: Vec<Package> = Vec::new();
    let mut removed: Vec<Package> = requested.to_vec();
    let all_installed: Vec<&Package> = overlay.base.iter().chain(&overlay.overlay).collect();

    for _ in 0..8 {
        let providers: Vec<&Package> = overlay
            .overlay
            .iter()
            .filter(|p| !removed.iter().any(|r| r.nevra == p.nevra) && !closure.iter().any(|c| c.nevra == p.nevra))
            .chain(&overlay.base)
            .collect();
        let newly_broken: Vec<Package> = overlay
            .overlay
            .iter()
            .filter(|installed| !removed.iter().any(|r| r.nevra == installed.nevra) && !closure.iter().any(|c| c.nevra == installed.nevra))
            .filter(|installed| {
                expand_rich_requires(installed.requires.iter(), &all_installed)
                    .iter()
                    .any(|req| removed.iter().any(|r| dep_matches_pkg(req, r)) && !providers.iter().any(|p| dep_matches_pkg(req, p)))
            })
            .cloned()
            .collect();
        if newly_broken.is_empty() {
            break;
        }
        removed.extend(newly_broken.iter().cloned());
        closure.extend(newly_broken);
    }

    closure
}

/// RakuOS ships no SELinux userspace at all (no `selinux-policy`, no
/// `policycoreutils`, kernel/init built without it) — only `libselinux`
/// itself stays, since plenty of unrelated packages link against
/// `libselinux.so.1` at build time even on systems that never enforce a
/// policy. A repo package's hard `Requires: selinux-policy` (or
/// `policycoreutils`, `rpm-plugin-selinux`, etc. — real Fedora packages
/// commonly carry these even though nothing on a SELinux-less system ever
/// needs them) is therefore never actually satisfiable here and must never
/// block an otherwise-installable package or get pulled in as a real
/// candidate — see the `steamos-manager-powerstation` investigation: its
/// `Requires: selinux-policy` chain bottomed out in `rpm-plugin-selinux`'s
/// exact-EVR pin on `rpm-libs`, which the real Fedora repos and RakuOS's own
/// (ahead-of-Fedora) `rpm` build could never agree on — a problem that
/// simply doesn't exist for a distro that never installs any of this in the
/// first place.
/// Escape hatch for the SELinux-Requires stripping below: set
/// `RUM_ALLOW_SELINUX=1` (or any non-empty value) to let SELinux packages
/// flow into dependency resolution normally, same as any other package.
fn selinux_allowed() -> bool {
    std::env::var_os("RUM_ALLOW_SELINUX").is_some_and(|v| !v.is_empty())
}

fn is_selinux_ignored(name: &str) -> bool {
    if name == "libselinux" || name.starts_with("libselinux-") || name.starts_with("libselinux(") {
        return false;
    }
    // The common Fedora packaging convention: any package's own SELinux
    // policy module ships as a `<name>-selinux` subpackage (e.g.
    // `container-selinux`, `docker-selinux`, `abrt-addon-selinux`). RakuOS
    // never wants a main package to drag in its own `-selinux` sibling
    // either, so this is a suffix match, not a fixed list.
    let base = name.split('(').next().unwrap_or(name);
    if base == "selinux" || base.ends_with("-selinux") {
        return true;
    }
    const IGNORED: &[&str] = &["selinux-policy", "policycoreutils", "checkpolicy", "mcstrans", "setroubleshoot", "secilc", "rpm-plugin-selinux", "selinux-policy-any", "selinux-policy-base"];
    IGNORED.iter().any(|p| name == *p || name.starts_with(&format!("{p}-")) || name.starts_with(&format!("{p}(")))
}

/// A broken-repo-metadata ignore, scoped to a specific
/// *requiring* package rather than any occurrence of the capability name —
/// nvidia's kmod/dkms packages must stay normally installable (an explicit
/// `rum install akmod-nvidia`, or the driver meta-package's own real
/// internal `Requires` on `dkms-nvidia-*`, must keep working), so a blanket
/// ignore on the capability name itself is wrong: it would silently drop
/// nvidia's own driver stack from resolving correctly whenever anything
/// legitimately needs it.
///
/// Live-reported bug instead: `ffmpeg-libs.i686` (pulled in purely as a
/// transitive 32-bit codec dependency of `steam.i686` -> `gtk2.i686` ->
/// `gdk-pixbuf2.i686` -> `glycin-libs.i686` -> `glycin-loaders.i686` ->
/// `libheif.i686`) carries a `Requires: dkms-nvidia-580`, whose own
/// `nvidia-580-kmod-common` dependency this repo pool doesn't provide.
/// Nothing about installing 32-bit codec/compat libraries for Steam should
/// ever need a kernel module, proprietary or not — this is this one
/// specific package's broken metadata, not a real need — so only *this*
/// requiring package's edge to the nvidia dkms/akmod family is ignored,
/// leaving every other path to those packages (including a direct request)
/// fully intact.
fn is_broken_repo_dep_edge_ignored(requiring_pkg: &str, dep_name: &str) -> bool {
    requiring_pkg == "ffmpeg-libs" && (dep_name.starts_with("dkms-nvidia") || dep_name.starts_with("akmod-nvidia") || dep_name.starts_with("kmod-nvidia") || (dep_name.starts_with("nvidia-") && dep_name.contains("-kmod")))
}

/// A kernel-module packaging convention (`dkms-*`/`akmod-*`/`kmod-*`) —
/// covers nvidia, virtualbox, zfs, and anything else built this way. These
/// packages sometimes also carry unrelated `Provides:` (bundled userspace
/// libs), which lets them tie for "provides capability X" against an
/// ordinary package that provides the same thing without needing a kernel
/// module at all. See [`is_broken_repo_dep_edge_ignored`]'s doc for the real
/// case this addresses: `dkms-nvidia-580` outranking `mesa-libGL`/`libglvnd`
/// for `libglvnd-glx(x86-32)`. Used only as a provider-ranking tiebreaker,
/// never to exclude these packages outright — a direct or otherwise-sole
/// request for one still resolves normally.
fn is_driver_package(name: &str) -> bool {
    name.starts_with("dkms-") || name.starts_with("akmod-") || name.starts_with("kmod-")
}

/// Maps an explicit ISA suffix on a capability name (rpm's own way of
/// saying "the *other* arch's copy of this, specifically") to the arch it
/// names. Returns `None` for a capability with no such suffix, in which
/// case the dependency keeps whatever arch hint it already inherited.
/// `host_arch` resolves the generic `(64bit)`/`(32bit)` markers, which
/// don't name a specific arch by themselves — only concrete ones like
/// `(x86-32)`/`(x86-64)` do.
fn isa_arch(name: &str, host_arch: &str) -> Option<String> {
    let suffix = name.rsplit_once('(').filter(|(_, rest)| rest.ends_with(')')).map(|(_, rest)| &rest[..rest.len() - 1])?;
    match suffix {
        "x86-32" => Some("i686".to_string()),
        "x86-64" => Some("x86_64".to_string()),
        "64bit" if host_arch == "x86_64" => Some("x86_64".to_string()),
        "32bit" if host_arch == "x86_64" => Some("i686".to_string()),
        _ => None,
    }
}

/// Repo-wide dependency-closure check for `rum repoclosure`: for every
/// candidate in `candidates` whose name matches at least one of
/// `name_patterns` (glob-capable; an empty list matches everything), checks
/// that each of its `Requires` is satisfiable by *some* candidate in the
/// full pool — not just the subset [`resolve`] would actually pull in for
/// one install, since the point here is finding repo-wide gaps rather than
/// simulating a single transaction. Boolean/rich deps (`(a and b)`, `(a if
/// b)`, ...) and `rpmlib(...)` markers are skipped, same as [`resolve_ex`]
/// treats the latter, matching dnf's own repoclosure which doesn't attempt
/// to evaluate rich dep expressions either. Returns one entry per package
/// with at least one unresolved `Requires`, each paired with the list of
/// unresolved capability names.
pub fn check_closure(candidates: &[Package], name_patterns: &[String], host_arch: &str) -> Vec<(String, Vec<String>)> {
    let index = CandidateIndex::build(candidates);
    let mut results = Vec::new();
    for pkg in candidates {
        if !name_patterns.is_empty() && !rum_core::name_matches_any(&pkg.nevra.name, name_patterns) {
            continue;
        }
        let mut missing = Vec::new();
        for req in &pkg.requires {
            if req.name.starts_with("rpmlib(") || parse_boolean_dep(&req.name).is_some() {
                continue;
            }
            let dep_arch = isa_arch(&req.name, host_arch);
            let hard_filter = dep_arch.is_some() || req.name.contains(".so");
            let providers = matching_providers_raw(&index, req, dep_arch.as_deref().or(Some(pkg.nevra.arch.as_str())), host_arch, hard_filter);
            if providers.is_empty() {
                missing.push(req.name.clone());
            }
        }
        if !missing.is_empty() {
            results.push((pkg.nevra.to_string(), missing));
        }
    }
    results
}

/// Walks the transitive `Requires` closure of `requested` (following every
/// alternative provider of every capability encountered, including both
/// operands of rich/boolean deps), returning every candidate package that
/// could conceivably matter to the SAT problem, plus the plain-capability
/// "already satisfied outside the SAT problem" list gathered along the way.
/// This is the "build the pool" phase real depsolvers (libsolv included) do
/// before solving anything — the SAT variables and clauses built in
/// [`resolve`] only ever reference packages this function found.
/// Recursively expands a boolean-dep operand text down to its leaf
/// capability deps, pushing each onto `discover`'s BFS queue — mirrors
/// [`boolean_dep_leaf_names`] but keeps each leaf's version constraint
/// (via [`parse_dep_str`]) instead of discarding it, since discovery still
/// needs the constraint to find version-appropriate reachable providers.
/// Flattens a `Recommends` target down to plain package/capability names,
/// expanding rich/boolean expressions like `(libvirt-daemon-kvm or
/// libvirt-daemon-qemu)` (virt-manager's real Recommends on Fedora) into
/// their leaf operands. Without this, the raw parenthesised text was passed
/// straight into a name lookup, which never matches any real package, so the
/// whole Recommends target silently vanished — installing `virt-manager`
/// via `rum` never pulled in a libvirt/qemu backend at all, unlike `dnf`.
fn arch_compatible(pkg_arch: &str, host_arch: &str) -> bool {
    if pkg_arch == host_arch || pkg_arch == "noarch" {
        return true;
    }
    const X86_32: &[&str] = &["i386", "i486", "i586", "i686"];
    const ARM_32: &[&str] = &["armv7hl", "armv6hl", "armhfp"];
    if host_arch == "x86_64" {
        return X86_32.contains(&pkg_arch);
    }
    if X86_32.contains(&host_arch) {
        return X86_32.contains(&pkg_arch);
    }
    if host_arch == "aarch64" {
        return ARM_32.contains(&pkg_arch);
    }
    if ARM_32.contains(&host_arch) {
        return ARM_32.contains(&pkg_arch);
    }
    false
}

fn matching_providers_raw<'a>(index: &CandidateIndex<'a>, dep: &Dependency, preferred_arch: Option<&str>, host_arch: &str, hard_arch_filter: bool) -> Vec<&'a Package> {
    let target_arch = preferred_arch.unwrap_or(host_arch);
    let mut seen: HashSet<usize> = HashSet::new();
    let mut v: Vec<&Package> = index
        .lookup(&dep.name)
        .iter()
        .copied()
        .filter(|p| dep_matches_pkg(dep, p))
        .filter(|p| arch_compatible(&p.nevra.arch, host_arch))
        .filter(|p| seen.insert(*p as *const Package as usize))
        .collect();
    // A plain (non-ISA-tagged) dependency stays within the requesting
    // package's own arch family wherever possible — rpm only crosses arch
    // when the capability text itself is ISA-tagged (handled separately,
    // via `isa_arch` overriding the arch hint before this is called) or
    // when nothing in the preferred arch/noarch provides it at all (e.g. a
    // noarch-only capability, or a deliberately foreign-arch-only
    // provider). Without this, a same-name capability that happens to be
    // provided by both arches (unusual, but not impossible) could get
    // "satisfied" by whichever arch the solver reaches first, defeating
    // the point of tracking an arch hint at all. Only applies to a real
    // `Requires` relationship, though (`hard_arch_filter`) — a top-level
    // *user* request (or a Conflicts/Obsoletes match) has no "requesting
    // package's arch" to stay within, and must still be allowed to fall
    // back to a foreign-arch build if that's genuinely the only
    // satisfiable option (dnf does exactly this for `steam`, whose x86_64
    // build has an unsatisfiable transitive dependency on this test repo's
    // snapshot and correctly falls back to the i686 build instead).
    if hard_arch_filter {
        let same_family: Vec<&Package> = v.iter().copied().filter(|p| p.nevra.arch == target_arch || p.nevra.arch == "noarch").collect();
        if !same_family.is_empty() {
            v = same_family;
        }
    }
    v.sort_by(|a, b| {
        // A package whose own real `Name:` equals the requested capability
        // outranks one that merely `Provides` it, regardless of version —
        // matches dnf/rpm: `Provides` exists to let other capabilities
        // resolve to a package under a different real name, not to let an
        // unrelated (or channel-variant) package outbid the thing actually
        // named what was asked for. Without this, e.g. `brave-browser-
        // nightly`, which carries `Provides: brave-browser` purely so other
        // packages' `Requires: brave-browser` still resolve regardless of
        // which channel is installed, out-ranked the real `brave-browser`
        // package on EVR alone (nightly builds version higher than
        // stable) — `rum install brave-browser` silently installed nightly.
        let a_exact = a.nevra.name == dep.name;
        let b_exact = b.nevra.name == dep.name;
        b_exact
            .cmp(&a_exact)
            // A `dkms-`/`akmod-`/`kmod-` kernel-module package (nvidia,
            // virtualbox, zfs, ...) only ever belongs in a transaction
            // because something asked for it *by name* — never as an
            // incidental alternative provider of some unrelated virtual
            // capability. Real case: `dkms-nvidia-580` happens to carry
            // `Provides: libglvnd-glx(x86-32)` (bundled GL dispatch libs),
            // which used to let it outrank `mesa-libGL`/`libglvnd` for that
            // capability and drag the whole nvidia driver stack into a
            // `qt6-qtbase-devel` install. Only applies when neither side is
            // the exact-name match above, so `rum install dkms-nvidia-580`
            // itself is unaffected.
            .then_with(|| is_driver_package(&a.nevra.name).cmp(&is_driver_package(&b.nevra.name)))
            .then_with(|| arch_rank(&a.nevra.arch, target_arch).cmp(&arch_rank(&b.nevra.arch, target_arch)))
            .then_with(|| a.repo_priority.cmp(&b.repo_priority))
                .then_with(|| a.repo_cost.cmp(&b.repo_cost))
            // Group by real name *before* comparing EVR, so EVR magnitude
            // only ever breaks a tie between two builds of the *same*
            // package (pick the newer one) — never across unrelated names.
            // For an ambiguous virtual capability with no exact-name
            // provider (e.g. `desktop-notification-daemon`, satisfied by
            // dozens of unrelated packages), comparing raw EVR strings
            // across different names is nonsense: `gnome-shell`'s "50.4"
            // outranks `notification-daemon`'s "3.20.0" purely because it's
            // a bigger number, so this used to always resolve such a
            // Requires to whichever unrelated provider had the largest
            // version counter, silently pulling an entire desktop shell
            // (gdm/mutter/gnome-session and everything under it) in behind
            // a printer-config tool's notification-daemon dependency.
            // (Comparing name before EVR — rather than a pairwise `if
            // a.name == b.name` branch — keeps this a valid total order:
            // `sort_by` requires transitivity across the whole candidate
            // set, and a same-name-conditional field can cycle once three
            // or more different names/repos are in play.)
            .then_with(|| a.nevra.name.cmp(&b.nevra.name))
            .then_with(|| b.nevra.evr().as_str().compare_evr(a.nevra.evr().as_str()))
            .then_with(|| a.repo_id.cmp(&b.repo_id))
            .then_with(|| a.nevra.arch.cmp(&b.nevra.arch))
    });
    v
}

/// Same as [`matching_providers_raw`], but restricted to (and looked up
/// against) the already-discovered `reachable`/`var_of` set — used during
/// clause-building, where every provider must already have a SAT variable.
fn arch_rank(arch: &str, target_arch: &str) -> u8 {
    if arch == target_arch {
        0
    } else if arch == "noarch" {
        1
    } else {
        2
    }
}

/// True if `dep` (a Requires, Conflicts, or Obsoletes entry) matches `pkg`,
/// either by name+EVR directly or via one of `pkg`'s Provides.
fn dep_matches_pkg(dep: &Dependency, pkg: &Package) -> bool {
    if pkg.nevra.name == dep.name {
        return match &dep.constraint {
            None => true,
            Some(_) => dep.satisfied_by_evr(&pkg.nevra.evr()),
        };
    }
    if let Some((base, arch)) = strip_isa_suffix(&dep.name) {
        if pkg.nevra.name == base && pkg.nevra.arch == arch {
            let satisfied = match &dep.constraint {
                None => true,
                Some(_) => dep.satisfied_by_evr(&pkg.nevra.evr()),
            };
            if satisfied {
                return true;
            }
        }
    }
    pkg.provides.iter().any(|p| {
        p.name == dep.name
            && match (&dep.constraint, &p.constraint) {
                (None, _) => true,
                // rpm only compares versions when *both* sides carry one —
                // a bare, unversioned `Provides: foo` (no EQ/GE/etc.) always
                // satisfies a versioned `Requires: foo = X` by name alone,
                // same as real rpm/dnf. Without this, a package whose
                // Provides entry omits a version (common for a package
                // self-providing a virtual capability, e.g. a dkms package
                // providing its own `<name>-kmod`) would never satisfy any
                // versioned Requires for that capability at all.
                (Some(_), None) => true,
                (Some(_), Some((_, v))) => dep.satisfied_by_evr(v),
            }
    })
}

/// Strips a concrete rpm ISA suffix (`(x86-64)`/`(x86-32)`) off a capability
/// name, returning the bare name and the arch it names.
fn strip_isa_suffix(name: &str) -> Option<(&str, &str)> {
    let (base, rest) = name.rsplit_once('(')?;
    let suffix = rest.strip_suffix(')')?;
    let arch = match suffix {
        "x86-64" => "x86_64",
        "x86-32" => "i686",
        _ => return None,
    };
    Some((base, arch))
}

/// Expands rpm's rich/boolean conditional Requires form (`(A if B)`, e.g.
/// `selinux-policy`'s `(rpm-plugin-selinux if rpm-libs)`) into a plain
/// `Requires: A` when `B` is currently satisfied — evaluated against the
/// pre-transaction installed set, since that's what decided whether the
/// condition was ever "live" for this package in the first place. Only the
/// two-clause `(A if B)` form is handled; `unless`/`else` rich deps pass
/// through unexpanded (rare enough in practice not to be worth it yet).
/// Without this, `compute_erasures` never sees these as real Requires at
/// all (the raw string doesn't match any package name), so it can't catch
/// the same class of broken-consumer-after-upgrade cases rpm itself would
/// reject at `rpm -U` time with an opaque "Failed dependencies" error.
fn expand_rich_requires<'a>(reqs: impl Iterator<Item = &'a Dependency>, all_installed: &[&Package]) -> Vec<Dependency> {
    reqs.filter_map(|req| {
        let Some(inner) = req.name.strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
            return Some(req.clone());
        };
        let mut parts = inner.splitn(2, " if ");
        let a = parts.next()?.trim();
        let b = parts.next()?.trim();
        // rich deps can chain " else "/" unless " onto the condition —
        // strip that off since only the simple `if` clause is evaluated.
        let b = b.split(" else ").next().unwrap_or(b).trim();
        let cond = Dependency::unversioned(b);
        if all_installed.iter().any(|p| dep_matches_pkg(&cond, p)) {
            Some(Dependency::unversioned(a))
        } else {
            None
        }
    })
    .collect()
}

/// True if `chosen`'s Obsoletes list covers `candidate`, by *name* — never
/// within the same package name, since a legacy-compat package commonly
/// both Provides an old capability name *and* Obsoletes an old version of
/// it (e.g. `libjpeg-turbo` Provides `libjpeg = 6b-47.fc42` for backward
/// compatibility while Obsoleting `libjpeg < 6b-47`), and the i686/x86_64
/// builds of that same package must be able to coexist (multilib) rather
/// than one arch's Obsoletes list wrongly excluding the other arch's own
/// compat Provides.
#[derive(Debug)]
enum BoolExpr {
    If(String, String),
    /// `(A if B else C)` — rpm's ternary form: require `A` when `B` holds,
    /// otherwise require `C` instead (unlike plain `If`, which requires
    /// nothing when its condition doesn't hold).
    IfElse(String, String, String),
    Unless(String, String),
    Or(String, String),
    And(String, String),
    With(String, String),
    Without(String, String),
}

impl BoolExpr {
    fn operands(&self) -> Vec<&str> {
        match self {
            BoolExpr::If(a, b) | BoolExpr::Unless(a, b) | BoolExpr::Or(a, b) | BoolExpr::And(a, b) | BoolExpr::With(a, b) | BoolExpr::Without(a, b) => {
                vec![a.as_str(), b.as_str()]
            }
            BoolExpr::IfElse(a, b, c) => vec![a.as_str(), b.as_str(), c.as_str()],
        }
    }
}

/// A rich/boolean dependency's operands (e.g. the `A` in `(A if B)`) are
/// plain text, not separate XML attributes — so an operand like
/// `mesa-dri-drivers(x86-32) = 25.1.9-1.fc42` still has its version
/// constraint embedded as text and needs the same `name <op> version`
/// splitting `rum-rpmdb::parse_nevrs` does for `rpm -qa` output.
fn parse_dep_str(s: &str) -> Dependency {
    let mut parts = s.splitn(3, ' ');
    let name = parts.next().unwrap_or_default().to_string();
    let op = parts.next();
    let ver = parts.next();
    match (op, ver) {
        (Some(op), Some(ver)) => {
            let cmp = match op {
                "<" => Some(rum_core::Comparator::Lt),
                "<=" => Some(rum_core::Comparator::Le),
                "=" => Some(rum_core::Comparator::Eq),
                ">=" => Some(rum_core::Comparator::Ge),
                ">" => Some(rum_core::Comparator::Gt),
                _ => None,
            };
            match cmp {
                Some(cmp) => Dependency { name, constraint: Some((cmp, ver.to_string())) },
                None => Dependency::unversioned(s.to_string()),
            }
        }
        _ => Dependency::unversioned(s.to_string()),
    }
}

/// Parses a Requires entry name of the form `(A <op> B)` into a
/// [`BoolExpr`], where `<op>` is one of the RPM boolean dependency
/// keywords. `A`/`B` may themselves contain balanced parens (e.g. an
/// ISA-suffixed capability like `pipewire-alsa(x86-32)`), so the operator
/// search only matches at paren-depth 0 within the outer expression.
/// Returns `None` for any ordinary (non-boolean) dependency name.
/// Strips one layer of redundant surrounding parens from a boolean-dep
/// operand, e.g. rpm emits `((linux-firmware >= X) if linux-firmware)`
/// where the `if` operand is itself wrapped — without this, the wrapped
/// operand's leading `(` ends up glued onto its capability name (parsed as
/// `"(linux-firmware"` instead of `"linux-firmware"`), which then matches
/// no provider and forces the whole boolean dependency unsatisfiable.
/// Only strips when the parens are actually balanced around the whole
/// string (not just first/last char coincidentally matching).
fn strip_balanced_parens(s: &str) -> String {
    let mut s = s;
    while let Some(inner) = s.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        // Confirm the opening '(' is actually matched by the closing ')' at
        // the very end (not by some earlier ')' inside `inner`), i.e. depth
        // never returns to 0 before the last character.
        let mut depth = 0i32;
        let mut closes_early = false;
        let last = inner.char_indices().last().map(|(i, _)| i);
        for (i, c) in inner.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 && Some(i) != last {
                        closes_early = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        if closes_early || depth != 0 {
            break;
        }
        s = inner.trim();
    }
    s.to_string()
}

/// Scans `inner` for a top-level `" if "` followed (still at paren-depth 0)
/// by a later `" else "` — rpm's ternary `(A if B else C)` form. Must be
/// tried before the plain binary-op scan below: that scan finds the first
/// `" if "` token and, without this check first, would treat everything
/// after it (including the literal text `" else C"`) as the condition
/// operand of a plain `(A if B)`, producing a nonsense capability name that
/// silently evaluates to "no provider" and makes the whole rich dependency
/// vacuously true — live-reproduced via `zed`'s real
/// `(zed-cli-compat-zfs if zfs else zed-cli)` Requires, which meant
/// `zed-cli` never got pulled in even though `zed`'s own `/usr/bin/zed`
/// launcher lives in that subpackage.
fn split_if_else(inner: &str) -> Option<(String, String, String)> {
    let mut depth = 0i32;
    let mut if_pos = None;
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth == 0 && if_pos.is_none() && inner[i..].starts_with(" if ") {
            if_pos = Some(i);
        }
    }
    let if_pos = if_pos?;
    let after_if = if_pos + " if ".len();
    let mut depth = 0i32;
    let mut else_pos = None;
    for (i, c) in inner[after_if..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth == 0 && inner[after_if + i..].starts_with(" else ") {
            else_pos = Some(after_if + i);
            break;
        }
    }
    let else_pos = else_pos?;
    let a = strip_balanced_parens(inner[..if_pos].trim());
    let b = strip_balanced_parens(inner[after_if..else_pos].trim());
    let c = strip_balanced_parens(inner[else_pos + " else ".len()..].trim());
    if a.is_empty() || b.is_empty() || c.is_empty() {
        return None;
    }
    Some((a, b, c))
}

fn parse_boolean_dep(name: &str) -> Option<BoolExpr> {
    // Operands recursed into from a chained combinator (e.g. the `B with C
    // with D` remainder of `(A with B with C with D)`, or an operand
    // already unwrapped by `strip_balanced_parens`) are compound boolean
    // expressions in their own right but are no longer themselves wrapped
    // in an outer `(...)` — so a wrapping layer is stripped unconditionally
    // when present (unlike `strip_balanced_parens`, no closes-early check:
    // the top-level call site's `name` is always genuinely `(...)`-wrapped,
    // and requiring full balance here would leave that wrapping in place
    // whenever the first inner sub-group happens to close before the end,
    // e.g. `((A) if B)`), otherwise the recursive re-parse below silently
    // fails and the whole sub-expression gets mis-treated as a single
    // literal capability name.
    let inner = match name.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        Some(inner) => inner,
        None => name,
    };
    if let Some((a, b, c)) = split_if_else(inner) {
        return Some(BoolExpr::IfElse(a, b, c));
    }
    const OPS: [(&str, fn(String, String) -> BoolExpr); 6] = [
        (" unless ", BoolExpr::Unless as fn(String, String) -> BoolExpr),
        (" without ", BoolExpr::Without as fn(String, String) -> BoolExpr),
        (" if ", BoolExpr::If as fn(String, String) -> BoolExpr),
        (" or ", BoolExpr::Or as fn(String, String) -> BoolExpr),
        (" and ", BoolExpr::And as fn(String, String) -> BoolExpr),
        (" with ", BoolExpr::With as fn(String, String) -> BoolExpr),
    ];

    let mut depth = 0i32;
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            for (tok, make) in &OPS {
                if inner[i..].starts_with(tok) {
                    let a = strip_balanced_parens(inner[..i].trim());
                    let b = strip_balanced_parens(inner[i + tok.len()..].trim());
                    if !a.is_empty() && !b.is_empty() {
                        return Some(make(a, b));
                    }
                }
            }
        }
    }
    None
}

/// Recursively extracts the leaf capability names referenced by a `Requires`
/// entry's raw name — for an ordinary dependency this is just the name
/// itself, but for a boolean/rich dependency (e.g.
/// `(python3.14dist(pyscard) < 3~~ with python3.14dist(pyscard) >= 2)`) the
/// literal string is a compound expression, not a capability name, and
/// simplistic consumers that only care about installed-vs-installed
/// "does anything still require capability X" checks (like `autoremove`'s
/// orphan sweep) need the actual capability names inside it, not the
/// parenthesized expression itself — otherwise a package required only via
/// a rich dependency looks unreferenced and gets wrongly swept up, even
/// though `rpm` itself will refuse to remove it.
pub fn boolean_dep_leaf_names(name: &str) -> Vec<String> {
    match parse_boolean_dep(name) {
        Some(expr) => {
            let mut out = Vec::new();
            for operand in expr.operands() {
                out.extend(boolean_dep_leaf_names(operand));
            }
            out
        }
        None => vec![parse_dep_str(name).name],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rum_core::Nevra;

    #[test]
    fn boolean_dep_leaf_names_flattens_chained_with_clauses() {
        // Real dnf5 rich dep from python3-httplib2's Requires on
        // python3.14dist(pyparsing): a 6-way `with`-chain whose tail isn't
        // separately parenthesized. Regression test for `discover()` never
        // reaching a versioned `dist()` capability's provider because the
        // chain's un-parenthesized remainder got mangled into a single
        // garbage leaf instead of being recursed into.
        let s = "((python3.14dist(pyparsing) < 3 or python3.14dist(pyparsing) > 3) with (python3.14dist(pyparsing) < 3.0.1 or python3.14dist(pyparsing) > 3.0.1) with (python3.14dist(pyparsing) < 3.0.2 or python3.14dist(pyparsing) > 3.0.2) with (python3.14dist(pyparsing) < 3.0.3 or python3.14dist(pyparsing) > 3.0.3) with python3.14dist(pyparsing) < 4~~ with python3.14dist(pyparsing) >= 2.4.2)";
        let leaves = boolean_dep_leaf_names(s);
        assert_eq!(leaves.len(), 10);
        assert!(leaves.iter().all(|n| n == "python3.14dist(pyparsing)"));
    }

    #[test]
    fn debug_split_if_else() {
        let parsed = parse_boolean_dep("(zed-cli-compat-zfs if zfs else zed-cli)");
        eprintln!("{:?}", parsed.as_ref().map(|e| e.operands()));
        match parsed {
            Some(BoolExpr::IfElse(a, b, c)) => {
                assert_eq!(a, "zed-cli-compat-zfs");
                assert_eq!(b, "zfs");
                assert_eq!(c, "zed-cli");
            }
            other => panic!("expected IfElse, got {other:?}"),
        }
    }

    fn pkg(name: &str, ver: &str, requires: Vec<Dependency>) -> Package {
        Package {
            nevra: Nevra { name: name.into(), epoch: 0, version: ver.into(), release: "1".into(), arch: "x86_64".into() },
            summary: String::new(),
            provides: vec![],
            requires,
            conflicts: vec![],
            obsoletes: vec![],
            recommends: vec![],
            suggests: vec![],
            enhances: vec![],
            supplements: vec![],
            location: format!("{name}-{ver}-1.x86_64.rpm"),
            repo_id: "test".into(),
            repo_priority: 99,
            repo_cost: 1000,
            install_size: 0,
            download_size: 0,
            vendor: String::new(),
            checksum_type: String::new(),
            checksum: String::new(),
        }
    }

    fn pkg_arch(name: &str, ver: &str, arch: &str, requires: Vec<Dependency>) -> Package {
        let mut p = pkg(name, ver, requires);
        p.nevra.arch = arch.into();
        p.location = format!("{name}-{ver}-1.{arch}.rpm");
        p
    }

    fn resolve_x86_64(requested: &[String], candidates: &[Package], overlay: &OverlayContext) -> Result<Plan> {
        resolve(requested, candidates, overlay, "x86_64")
    }

    #[test]
    fn picks_highest_version() {
        let candidates = vec![pkg("foo", "1.0", vec![]), pkg("foo", "2.0", vec![])];
        let overlay = OverlayContext::for_test(vec![], vec![]);
        let plan = resolve_x86_64(&["foo".to_string()], &candidates, &overlay).unwrap();
        assert_eq!(plan.to_install.len(), 1);
        assert_eq!(plan.to_install[0].nevra.version, "2.0");
    }

    #[test]
    fn exact_nevra_request_installs_a_specific_build_alongside_installonly() {
        // `rum install kernel-2.0-1.x86_64` against an already-installed
        // `kernel-1.0-1` under `installonlypkgs=kernel`: a plain `install
        // kernel` is a same-name no-op (see rum-solv's own test of the
        // same shape), but naming the exact NEVRA must still land the
        // second build side by side — this is the whole reason a version-
        // pinned install spec exists.
        let candidates = vec![pkg("kernel", "1.0", vec![]), pkg("kernel", "2.0", vec![])];
        let overlay = OverlayContext::for_test(vec![], vec![pkg("kernel", "1.0", vec![])]);
        let options = ResolveOptions { installonly_pkgs: vec!["kernel".to_string()], ..Default::default() };
        let plan = resolve_ex(&["kernel-2.0-1.x86_64".to_string()], &candidates, &overlay, "x86_64", &options).unwrap();
        assert_eq!(plan.to_install.len(), 1);
        assert_eq!(plan.to_install[0].nevra.version, "2.0");
    }

    #[test]
    fn exact_nevra_request_without_arch_suffix_also_matches() {
        let candidates = vec![pkg("foo", "1.0", vec![])];
        let overlay = OverlayContext::for_test(vec![], vec![]);
        let plan = resolve_x86_64(&["foo-1.0-1".to_string()], &candidates, &overlay).unwrap();
        assert_eq!(plan.to_install.len(), 1);
        assert_eq!(plan.to_install[0].nevra.version, "1.0");
    }

    #[test]
    fn resolve_upgrade_reports_a_real_upgrade_in_to_upgrade() {
        let candidates = vec![pkg("foo", "1.0", vec![]), pkg("foo", "2.0", vec![])];
        let overlay = OverlayContext::for_test(vec![], vec![pkg("foo", "1.0", vec![])]);
        let options = ResolveOptions::default();
        let plan = resolve_upgrade(&["foo".to_string()], &candidates, &overlay, "x86_64", &options).unwrap();
        assert_eq!(plan.to_upgrade.len(), 1);
        assert_eq!(plan.to_upgrade[0].nevra.version, "2.0");
    }

    #[test]
    fn resolve_upgrade_never_reports_a_foreign_arch_build_of_a_base_owned_name() {
        // Regression test for `rum check-upgrade` listing libcap-ng.i686 as
        // an available update on staging even though `rum upgrade` itself
        // correctly skips it: libcap-ng.x86_64 is base-owned, so Split
        // mode's base-name lock (see rum-solv's `build_pool_locked`) must
        // also lock the i686 build living in the overlay — `to_upgrade`
        // must stay consistent with what `rum upgrade` would actually do.
        let candidates = vec![pkg_arch("foo", "1.0", "x86_64", vec![]), pkg_arch("foo", "1.0", "i686", vec![]), pkg_arch("foo", "2.0", "i686", vec![])];
        let overlay = OverlayContext::for_test(vec![pkg_arch("foo", "1.0", "x86_64", vec![])], vec![pkg_arch("foo", "1.0", "i686", vec![])]);
        let options = ResolveOptions::default();
        let plan = resolve_upgrade(&["foo".to_string()], &candidates, &overlay, "x86_64", &options).unwrap();
        assert!(plan.to_upgrade.is_empty(), "expected no upgrade for a base-owned name's foreign-arch overlay build, got {:?}", plan.to_upgrade);
    }
}
