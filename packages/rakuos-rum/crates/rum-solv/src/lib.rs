pub mod sys;

use anyhow::Result;
use rum_core::{Comparator, Dependency, Package, VersionLock};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;

/// Thin RAII wrapper around a libsolv `Pool`. Owns every `Repo` created
/// against it (libsolv frees repos when the pool is freed), so `Repo`
/// handles borrow from `Pool` rather than owning their own lifetime.
pub struct Pool {
    raw: *mut sys::Pool,
}

impl Pool {
    pub fn new() -> Result<Self> {
        let raw = unsafe { sys::pool_create() };
        anyhow::ensure!(!raw.is_null(), "pool_create returned null");
        Ok(Self { raw })
    }

    /// Sets the RPM-style architecture policy (e.g. `"x86_64"`) used to
    /// rank multiarch candidates and decide compatible-arch fallback
    /// (i686 on x86_64, etc.) — mirrors what `rum-resolver`'s `host_arch`/
    /// `pref_arch` did by hand.
    pub fn set_arch(&mut self, arch: &str) -> Result<()> {
        let c = CString::new(arch)?;
        unsafe { sys::pool_setarch(self.raw, c.as_ptr()) };

        // Mirrors dnf5's `Pool` constructor (libdnf5/solv/pool.hpp) exactly —
        // confirmed against dnf5's actual source, not just general libdnf
        // knowledge:
        //   pool_set_flag(pool, POOL_FLAG_WHATPROVIDESWITHDISABLED, 1);
        //   pool_set_flag(pool, POOL_FLAG_IMPLICITOBSOLETEUSESCOLORS, 1);
        // dnf5 does NOT set POOL_FLAG_OBSOLETEUSESCOLORS (we previously did —
        // removed, it's not part of dnf5's actual flag set).
        //
        // WHATPROVIDESWITHDISABLED: excluded packages are never offered as
        // provides candidates by the solver.
        // IMPLICITOBSOLETEUSESCOLORS: makes libsolv's implicit same-name-
        // newer-EVR obsolete rule arch-color-aware, so packages of the same
        // name but different (multilib) arch install side by side instead of
        // one implicitly obsoleting the other — this is the actual fix for
        // the glycin-loaders.i686 clobbering glycin-loaders.x86_64 bug;
        // dnf5's own comment on this flag says exactly that: "Allow packages
        // of the same name with different architectures to be installed in
        // parallel".
        //
        // ADDFILEPROVIDESFILTERED mirrors dnf5's repo_sack.cpp
        // load_repos_common(), which sets it whenever filelists.xml metadata
        // isn't loaded (a perf/correctness fix restricting file-provides
        // matching to primary.xml's files) — rum-repo never parses
        // filelists.xml, so this is unconditional for us where dnf5's is
        // conditional.
        unsafe {
            sys::pool_set_flag(self.raw, sys::POOL_FLAG_WHATPROVIDESWITHDISABLED as i32, 1);
            sys::pool_set_flag(self.raw, sys::POOL_FLAG_IMPLICITOBSOLETEUSESCOLORS as i32, 1);
            sys::pool_set_flag(self.raw, sys::POOL_FLAG_ADDFILEPROVIDESFILTERED as i32, 1);
        }
        Ok(())
    }

    /// Must be called after all repos/solvables are added and before
    /// creating a `Solver` — libsolv builds its provides index lazily.
    pub fn createwhatprovides(&mut self) {
        unsafe { sys::pool_createwhatprovides(self.raw) };
    }

    /// Marks `repo` as *the* installed/system repo — governs libsolv's
    /// default upgrade/erase target semantics. In Split mode this should
    /// be the combined `@System` repo (base + overlay solvables); which
    /// individual solvables must never be erased is enforced separately
    /// via `SOLVER_LOCK` jobs over `PoolBuild::base_solvables`, not by
    /// repo membership — libsolv only supports one "installed" repo.
    pub fn set_installed(&mut self, repo: &Repo) {
        unsafe { sys::pool_set_installed(self.raw, repo.raw) };
    }

    pub fn create_repo(&mut self, name: &str) -> Result<Repo> {
        let c = CString::new(name)?;
        let raw = unsafe { sys::repo_create(self.raw, c.as_ptr()) };
        anyhow::ensure!(!raw.is_null(), "repo_create returned null");
        Ok(Repo { raw, pool: self.raw })
    }

    fn str2id(&mut self, s: &str, create: bool) -> sys::Id {
        // Interior CString alloc per call is fine here: this only runs
        // during one-time pool population, not per-solve.
        let c = CString::new(s).unwrap_or_else(|_| CString::new("").unwrap());
        unsafe { sys::pool_str2id(self.raw, c.as_ptr(), create as i32) }
    }

    fn rel2id(&mut self, name: sys::Id, evr: sys::Id, op: i32, create: bool) -> sys::Id {
        unsafe { sys::pool_rel2id(self.raw, name, evr, op, create as i32) }
    }

    /// Parses an rpm rich/boolean dependency string (`(A if B)`, `(A with
    /// B)`, `(A if B else C)`, ...) into a single libsolv `Id` representing
    /// the whole expression — libsolv evaluates these natively during
    /// solving. Without this, a rich-dep `Requires` string was being fed to
    /// `str2id` as if it were a literal (and unmatchable) capability name,
    /// making every package with one unresolvable.
    fn parse_rich_dep(&mut self, s: &str) -> sys::Id {
        let c = CString::new(s).unwrap_or_else(|_| CString::new("").unwrap());
        unsafe { sys::pool_parserpmrichdep(self.raw, c.as_ptr()) }
    }

    pub fn as_raw(&mut self) -> *mut sys::Pool {
        self.raw
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        unsafe { sys::pool_free(self.raw) };
    }
}

/// A libsolv `Repo` — a named group of `Solvable`s (one rum "source":
/// the base rpmdb, the overlay rpmdb, or one repo's available packages).
/// Freed automatically when its owning `Pool` is freed; does not
/// implement `Drop` itself to avoid a double-free.
pub struct Repo {
    raw: *mut sys::Repo,
    pool: *mut sys::Pool,
}

const REL_GT: i32 = sys::REL_GT as i32;
const REL_EQ: i32 = sys::REL_EQ as i32;
const REL_LT: i32 = sys::REL_LT as i32;

fn comparator_flags(cmp: Comparator) -> i32 {
    match cmp {
        Comparator::Lt => REL_LT,
        Comparator::Le => REL_LT | REL_EQ,
        Comparator::Eq => REL_EQ,
        Comparator::Ge => REL_GT | REL_EQ,
        Comparator::Gt => REL_GT,
    }
}

impl Repo {
    /// Adds one `rum_core::Package` as a new `Solvable` in this repo,
    /// translating its NEVRA and all four dependency lists into libsolv
    /// string/rel ids. Returns the new solvable's pool-wide `Id`.
    pub fn add_package(&mut self, pool: &mut Pool, pkg: &Package) -> sys::Id {
        let p = unsafe { sys::repo_add_solvable(self.raw) };
        let s = unsafe { sys::pool_id2solvable(self.pool, p) };
        debug_assert_eq!(self.pool, pool.raw);

        unsafe {
            (*s).name = pool.str2id(&pkg.nevra.name, true);
            (*s).arch = pool.str2id(&pkg.nevra.arch, true);
            (*s).evr = pool.str2id(&pkg.nevra.evr(), true);
        }

        self.add_deps(pool, s, &pkg.provides, DepKind::Provides);
        self.add_deps(pool, s, &pkg.requires, DepKind::Requires);
        self.add_deps(pool, s, &pkg.conflicts, DepKind::Conflicts);
        // `openh264`'s real upstream (Cisco's binary blob repo) only ever
        // publishes an x86_64 build — there is no genuine i686 counterpart.
        // Some third-party/compat repos rebuild an i686 `openh264` under a
        // bumped epoch purely so 32-bit apps can `dlopen` it, but that
        // rebuild's `Obsoletes: noopenh264` (inherited from the real spec)
        // then collides with `SOLVER_FLAG_YUM_OBSOLETES` (arch-independent
        // obsoletes matching, on to match dnf5) against the real, official
        // multilib `noopenh264.x86_64`, producing an unsolvable rule:
        // `openh264.i686 obsoletes noopenh264 < 1:0 provided by
        // noopenh264.x86_64`. `openh264` was never meant to obsolete
        // anything outside its own arch, so drop that specific Obsoletes
        // edge rather than let it wedge every `rum upgrade`.
        if pkg.nevra.name == "openh264" {
            let obsoletes: Vec<Dependency> = pkg.obsoletes.iter().filter(|d| d.name != "noopenh264").cloned().collect();
            self.add_deps(pool, s, &obsoletes, DepKind::Obsoletes);
        } else {
            self.add_deps(pool, s, &pkg.obsoletes, DepKind::Obsoletes);
        }
        self.add_deps(pool, s, &pkg.recommends, DepKind::Recommends);
        self.add_deps(pool, s, &pkg.suggests, DepKind::Suggests);

        // A solvable always (implicitly, per libsolv convention) provides
        // its own name = evr — most callers of repo_add_solvable do this
        // explicitly since libsolv does not synthesize it automatically.
        unsafe {
            let self_dep = pool.rel2id((*s).name, (*s).evr, REL_EQ, true);
            (*s).provides = sys::repo_addid_dep(self.raw, (*s).provides, self_dep, 0);
        }

        p
    }

    fn add_deps(&mut self, pool: &mut Pool, s: *mut sys::Solvable, deps: &[Dependency], kind: DepKind) {
        for dep in deps {
            let dep_id = if dep.constraint.is_none() && dep.name.starts_with('(') {
                pool.parse_rich_dep(&dep.name)
            } else {
                let name_id = pool.str2id(&dep.name, true);
                match &dep.constraint {
                    Some((cmp, ver)) => {
                        let evr_id = pool.str2id(ver, true);
                        pool.rel2id(name_id, evr_id, comparator_flags(*cmp), true)
                    }
                    None => name_id,
                }
            };
            unsafe {
                let field = match kind {
                    DepKind::Provides => &mut (*s).provides,
                    DepKind::Requires => &mut (*s).requires,
                    DepKind::Conflicts => &mut (*s).conflicts,
                    DepKind::Obsoletes => &mut (*s).obsoletes,
                    DepKind::Recommends => &mut (*s).recommends,
                    DepKind::Suggests => &mut (*s).suggests,
                };
                *field = sys::repo_addid_dep(self.raw, *field, dep_id, 0);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum DepKind {
    Provides,
    Requires,
    Conflicts,
    Obsoletes,
    Recommends,
    Suggests,
}

/// Result of populating a `Pool` from rum's candidate/overlay data:
/// everything a later solve step (task #29) needs, including which
/// solvable ids came from the base image so it can `SOLVER_LOCK` them
/// and preserve RakuOS's Split-mode non-erasure guarantee.
pub struct PoolBuild {
    pub pool: Pool,
    /// `@System` repo: base + overlay solvables, marked installed via
    /// `Pool::set_installed`.
    pub system_repo: Repo,
    /// Available-candidates repo (union of all enabled repos' packages).
    pub available_repo: Repo,
    /// Solvable ids in `system_repo` that came from the *base* image
    /// (`Origin::Base`) — a future solve step must `SOLVER_LOCK` every
    /// one of these so libsolv never proposes erasing/replacing them.
    /// Overlay-origin installed solvables carry no such restriction.
    pub base_solvables: HashSet<sys::Id>,
    /// Every solvable id added to `system_repo` (base + overlay) — i.e.
    /// everything currently installed, regardless of origin. Used to
    /// restrict erase jobs to installed candidates only, mirroring dnf5's
    /// `add_remove` (`libdnf5/rpm/solv/goal_private.hpp`), which is always
    /// called with an already-resolved *installed*-package id queue, never
    /// a name/provides selector.
    pub installed_solvables: HashSet<sys::Id>,
    /// Every solvable id added (system + available), mapped back to the
    /// original `rum_core::Package` it was built from — lets `solve()`
    /// translate a libsolv transaction back into rum types without
    /// re-deriving NEVRA/deps from raw pool string ids.
    pub packages: HashMap<sys::Id, Package>,
}

/// Builds a libsolv `Pool` from rum's already-loaded package lists.
/// `base`/`overlay` are what's currently installed (Split mode: two
/// separate rpmdb snapshots that never merge on disk, per
/// `rum_core::Origin::Base`/`Overlay`); in Standalone mode callers should
/// pass the single installed set as `overlay` and an empty `base`.
/// `available` is the full candidate set from enabled repos.
/// ISA tag rpm's dependency generator appends to a `Requires` when it wants
/// a same-arch provider specifically (`glycin-libs(x86-32)`, mirrored on
/// `Provides` by rpm itself when it can). Only the two arches rum's multilib
/// handling actually deals with matter here.
fn isa_tag(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("x86-64"),
        "i686" | "i386" | "i586" => Some("x86-32"),
        _ => None,
    }
}

/// Strips a `(x86-64)`/`(x86-32)` ISA suffix off a capability name, e.g.
/// `"librepo(x86-64)"` -> `"librepo"`; returns the name unchanged if it
/// carries no such suffix. Live bug this fixes: the base-name constraint
/// relaxation below (`base_names.contains(dep.name)`) only ever matched a
/// plain `Requires: librepo = <evr>` -- an ISA-tagged `Requires:
/// librepo(x86-64) = <evr>` (rpm's `%{name}%{?_isa} = %{version}-%{release}`
/// convention, used by virtually every -devel/-libs subpackage) never
/// equals the plain base name `"librepo"`, so it never got relaxed. With a
/// same-name+arch shadow candidate excluded from the pool (Split mode's
/// entire point), an exact-EVR ISA-tagged require pinned to a *newer* EVR
/// than what's actually on base became flatly unsatisfiable: `rum install
/// librepo-devel` erroring "nothing provides librepo(x86-64) = 1.21.0-1
/// needed by librepo-devel-1.21.0-1" even though librepo genuinely is on
/// base, just at an older build (1.20.0-5) that only lacked the newest
/// point release.
fn strip_isa_suffix(name: &str) -> &str {
    for tag in ["(x86-64)", "(x86-32)"] {
        if let Some(stripped) = name.strip_suffix(tag) {
            return stripped;
        }
    }
    name
}

/// A fixed name allowlist of packages known to be single-arch-only helpers
/// despite shipping an i686 build — not a general ".so-less" heuristic
/// (tried first, reverted: it also caught real multilib packages like
/// `mesa-vulkan-drivers`, which ship no `.so` `Provides` of their own but
/// are still genuinely per-arch ICD/driver packages that must stay
/// installable in both arches for Steam's 32-bit stack). `glycin-loaders`
/// loads its plugins from a non-arch-namespaced path
/// (`/usr/libexec/glycin-loaders/...`, not `/usr/lib64` vs `/usr/lib`), so
/// installing the i686 build alongside an already-installed x86_64 build
/// adds no real 32-bit capability — live-reported, it clobbers the x86_64
/// build's files on disk. Since nothing links against this package per-arch,
/// an ISA-tagged `Requires` on it is just as well satisfied by a build of
/// *any* arch, so rum never offers the foreign-arch build as an install
/// candidate at all — instead the matching host-arch build is granted the
/// foreign ISA tag as an extra `Provides`.
///
/// `glycin-libs` is deliberately *not* in this list, despite the same
/// packaging pattern: unlike `glycin-loaders`, it ships a real per-arch
/// `.so` (`libglycin-2.so.0`) that Steam's 32-bit stack links against
/// directly, so its i686 build is a genuine multilib dependency (Steam
/// needs `glycin-libs.i686` actually installed, not just ISA-tag-satisfied
/// by the x86_64 build) and must stay offered as a normal install
/// candidate.
fn is_single_arch_helper(pkg: &Package) -> bool {
    pkg.nevra.name == "glycin-loaders"
}

pub fn build_pool(host_arch: &str, base: &[Package], overlay: &[Package], available: &[Package]) -> Result<PoolBuild> {
    build_pool_locked(host_arch, base, overlay, available, &[])
}

/// Same as [`build_pool`], but `locks` (dnf5's versionlock parity) keeps
/// any candidate whose name is locked from ever entering `available_repo`
/// unless its `epoch:version-release`/`arch` exactly match what the lock
/// pins ([`VersionLock::allows`]) — this is what stops a *transitive*
/// dependency pull (not just a direct `install`/`upgrade`) from moving a
/// locked package, since libsolv can only ever select something that's
/// actually in the pool. A lock with no recorded EVR/arch (the name wasn't
/// installed yet when it was added) blocks nothing.
pub fn build_pool_locked(host_arch: &str, base: &[Package], overlay: &[Package], available: &[Package], locks: &[VersionLock]) -> Result<PoolBuild> {
    let mut pool = Pool::new()?;
    pool.set_arch(host_arch)?;

    // Names that have both a host-arch/noarch build and a foreign-arch
    // build of a single-arch-helper package anywhere in base+overlay+
    // available. For these, the foreign-arch build is dropped from the
    // pool entirely and the host-arch build gains its ISA tag as an extra
    // Provides — see `is_single_arch_helper`.
    let all_iter = || base.iter().chain(overlay.iter()).chain(available.iter());
    let mut host_names: HashSet<&str> = HashSet::new();
    let mut foreign_helper_names: HashSet<&str> = HashSet::new();
    for pkg in all_iter() {
        if pkg.nevra.arch == host_arch || pkg.nevra.arch == "noarch" {
            host_names.insert(&pkg.nevra.name);
        } else if is_single_arch_helper(pkg) {
            foreign_helper_names.insert(&pkg.nevra.name);
        }
    }
    let suppressed_names: HashSet<&str> = foreign_helper_names.intersection(&host_names).copied().collect();

    // Every unversioned capability (soname `Provides` included, not just the
    // ISA tag) any suppressed foreign-arch build offered — grafted onto the
    // matching host-arch build below so unversioned `Requires` on those
    // capabilities (e.g. `libglycin-2.so.0`, not just `glycin-libs(x86-32)`)
    // still resolve once the foreign-arch build is gone from the pool.
    let mut foreign_provides: HashMap<&str, Vec<String>> = HashMap::new();
    for pkg in all_iter() {
        if suppressed_names.contains(pkg.nevra.name.as_str()) && pkg.nevra.arch != host_arch && pkg.nevra.arch != "noarch" {
            let entry = foreign_provides.entry(pkg.nevra.name.as_str()).or_default();
            for p in &pkg.provides {
                if p.constraint.is_none() && !entry.contains(&p.name) {
                    entry.push(p.name.clone());
                }
            }
            if let Some(tag) = isa_tag(&pkg.nevra.arch) {
                let isa_name = format!("{}({tag})", pkg.nevra.name);
                if !entry.contains(&isa_name) {
                    entry.push(isa_name);
                }
            }
        }
    }

    // Base packages are always `SOLVER_LOCK`ed (see `solve()`) and must
    // never be shadowed by a same-name overlay/available copy either — so
    // a `Requires` pinned to an exact newer EVR of a base-only package
    // (e.g. `rpm-devel = 6.1.0` requiring `rpm = 6.1.0` when base only has
    // `rpm-6.0.2`) can never be satisfied as written. Rather than fail the
    // whole solve or drop the upgrade that carries it, strip the version
    // constraint on any `Requires` targeting a base package's name: the
    // already-installed, locked base copy then trivially satisfies it
    // (unversioned requires are satisfied by any installed provider),
    // exactly like dnf/rpm treat an already-installed dependency as
    // satisfied regardless of version drift from what a newer build might
    // have preferred. Live bug this fixes: `rum upgrade`/`system-upgrade`
    // erroring "cannot install both rpm-0:6.1.0... and rpm-0:6.0.2..."
    // whenever an overlay package's upgrade needed a newer EVR of a
    // package that only exists in the base rpmdb.
    let base_names: HashSet<&str> = base.iter().map(|p| p.nevra.name.as_str()).collect();

    let add_filtered = |repo: &mut Repo, pool: &mut Pool, packages: &mut HashMap<sys::Id, Package>, pkgs: &[Package], want_ids: Option<&mut HashSet<sys::Id>>| {
        let mut want_ids = want_ids;
        for pkg in pkgs {
            if suppressed_names.contains(pkg.nevra.name.as_str()) && pkg.nevra.arch != host_arch && pkg.nevra.arch != "noarch" {
                continue;
            }
            let mut pkg = pkg.clone();
            if let Some(extra) = foreign_provides.get(pkg.nevra.name.as_str()) {
                for name in extra {
                    if !pkg.provides.iter().any(|p| &p.name == name) {
                        pkg.provides.push(Dependency::unversioned(name.clone()));
                    }
                }
            }
            // Every real rpm implicitly provides itself at its own EVR, plus
            // (for arch-sensitive builds) an ISA-tagged self-provide
            // (`name(x86-64) = evr`) -- what `%{name}%{?_isa} = %{version}-
            // %{release}`-style Requires on -devel/-libs subpackages resolve
            // against. Repo metadata/rpmdb parsing doesn't always carry these
            // explicitly (see `Package::provides_self`'s doc comment), and
            // nothing else synthesizes them for the libsolv pool. This was
            // silently masked before Split mode excluded same-name+arch
            // duplicates from `available`: a fresh candidate at the same EVR
            // happened to satisfy the exact match anyway. Once that shadow
            // candidate is gone, the base/overlay/available copy itself must
            // carry its own self-provide or an ISA-qualified exact-EVR
            // Requires becomes unsatisfiable outright -- live bug: `rum
            // upgrade` erroring "nothing provides librepo(x86-64) = ...
            // needed by librepo-devel" even though librepo is on base.
            let evr = pkg.nevra.evr();
            if !pkg.provides.iter().any(|p| p.name == pkg.nevra.name) {
                pkg.provides.push(Dependency { name: pkg.nevra.name.clone(), constraint: Some((Comparator::Eq, evr.clone())) });
            }
            if let Some(tag) = isa_tag(&pkg.nevra.arch) {
                let isa_name = format!("{}({tag})", pkg.nevra.name);
                if !pkg.provides.iter().any(|p| p.name == isa_name) {
                    pkg.provides.push(Dependency { name: isa_name, constraint: Some((Comparator::Eq, evr)) });
                }
            }
            for dep in &mut pkg.requires {
                if dep.constraint.is_some() && base_names.contains(strip_isa_suffix(&dep.name)) {
                    dep.constraint = None;
                }
            }
            let id = repo.add_package(pool, &pkg);
            if let Some(ref mut ids) = want_ids {
                ids.insert(id);
            }
            packages.insert(id, pkg);
        }
    };

    let mut system_repo = pool.create_repo("@System")?;
    let mut base_solvables = HashSet::new();
    let mut installed_solvables = HashSet::new();
    let mut packages = HashMap::new();
    add_filtered(&mut system_repo, &mut pool, &mut packages, base, Some(&mut base_solvables));
    add_filtered(&mut system_repo, &mut pool, &mut packages, overlay, Some(&mut installed_solvables));

    // A base-owned name's identity spans every arch it's installed under,
    // not just the arch(es) actually present in the base rpmdb. Split mode
    // frequently has one arch of a name land in base (e.g. libcap-ng.x86_64,
    // shipped on the image) and a *different* arch of the very same name
    // land in overlay (e.g. libcap-ng.i686, pulled in later for Steam's
    // 32-bit stack) — those are the same logical package, so the overlay
    // arch must be locked in step with the base arch too, not left free to
    // update independently. Without this, `upgrade_all` moved the overlay
    // i686 build forward on its own, and libsolv's same-name multi-install
    // check then collided it against the still-locked base x86_64 build
    // ("cannot install both libcap-ng-0:0.9.5... and libcap-ng-0:0.9.3...")
    // — a live bug, not a hypothetical.
    for &id in &installed_solvables {
        if packages.get(&id).is_some_and(|p| base_names.contains(p.nevra.name.as_str())) {
            base_solvables.insert(id);
        }
    }
    installed_solvables.extend(&base_solvables);
    pool.set_installed(&system_repo);

    // Split mode's entire point: a name+arch the base image already ships is
    // satisfied *only* by the locked base copy, never by a same-name+arch
    // candidate offered fresh into the overlay. `base_names`'s version-
    // constraint relaxation above (add_filtered) only handles requirement
    // *satisfaction* -- it doesn't stop libsolv from independently choosing
    // to install a newer same-name+arch candidate anyway when something
    // else in a large transitive closure needs a NEVRA the relaxed,
    // unversioned require alone can't steer it away from. Live bug this
    // fixes: `rum install stellarium` pulling fresh overlay copies of
    // qt6-qtbase/-gui/-svg/-declarative/qt6ct (all already on base, all
    // SOLVER_LOCKed at their base EVR) at a newer version some *other* new
    // package (e.g. qt6-qtcharts) happened to be built against, instead of
    // resolving against the already-installed, already-locked base copies.
    // A different arch of the same name (e.g. libcap-ng.i686 for Steam's
    // 32-bit stack, when only libcap-ng.x86_64 is on base) is unaffected --
    // that exact (name, arch) genuinely isn't on base, so it's still a
    // legitimate overlay candidate.
    let base_name_arch: HashSet<(&str, &str)> = base.iter().map(|p| (p.nevra.name.as_str(), p.nevra.arch.as_str())).collect();

    let available_for_pool: Vec<Package> = available
        .iter()
        .filter(|pkg| !base_name_arch.contains(&(pkg.nevra.name.as_str(), pkg.nevra.arch.as_str())))
        .filter(|pkg| {
            if locks.is_empty() {
                return true;
            }
            match locks.iter().find(|l| l.name == pkg.nevra.name) {
                Some(lock) => lock.allows(&pkg.nevra.evr(), &pkg.nevra.arch),
                None => true,
            }
        })
        .cloned()
        .collect();

    let mut available_repo = pool.create_repo("available")?;
    add_filtered(&mut available_repo, &mut pool, &mut packages, &available_for_pool, None);

    pool.createwhatprovides();

    Ok(PoolBuild { pool, system_repo, available_repo, base_solvables, installed_solvables, packages })
}

/// What to change, expressed at rum's level (package names, same as the
/// CLI accepts) — translated into libsolv `Job`s by [`solve`].
pub struct SolveRequest {
    /// `(name, arch)` — `arch` is `Some` when the caller explicitly
    /// disambiguated with `name.arch` (rpm's own syntax) and that arch is
    /// known to actually exist in the pool. When present, `solve` targets
    /// that exact solvable rather than a bare name/provides match, so e.g.
    /// `mesa-dri-drivers.i686` still installs the i686 build even though
    /// the already-installed x86_64 build would otherwise make a plain
    /// by-name job see the request as already satisfied and no-op — a
    /// live-reported bug in `build.sh`'s explicit multilib pre-bake step.
    pub install_names: Vec<(String, Option<String>)>,
    pub erase_names: Vec<String>,
    /// dnf's `installonlypkgs=` — package names exempt from the implicit
    /// same-name obsoletes rule, so e.g. installing a new kernel lands
    /// side-by-side with the running one instead of upgrading it in place.
    /// Mirrors dnf5's `GoalPrivate::construct_job`
    /// (`SOLVER_MULTIVERSION | SOLVER_SOLVABLE_PROVIDES` per name).
    pub installonly_names: Vec<String>,
    /// dnf's protected-packages list (kernel, glibc, dnf itself, ...).
    /// Mirrors dnf5's `add_protected_packages`
    /// (`SOLVER_USERINSTALLED | SOLVER_SOLVABLE`): marks every installed
    /// solvable of these names as "user installed" so libsolv's own
    /// erasure/obsolete/conflict resolution never proposes removing them.
    pub protected_names: Vec<String>,
    /// dnf5's `add_upgrade`: named packages to move to their newest
    /// available EVR, or every installed package if `upgrade_all` is set
    /// and this is empty (`rum upgrade` with no args / `rum
    /// system-upgrade`). Unlike `install_names`, this only ever moves an
    /// *already-installed* package forward — a name with nothing newer
    /// available is silently left alone rather than erroring, matching
    /// `SOLVER_UPDATE`'s own semantics (a no-op if already at the best
    /// EVR).
    pub upgrade_names: Vec<String>,
    pub upgrade_all: bool,
    /// dnf5's `add_distro_sync`: named packages (or every installed
    /// package if `distro_sync_all` is set and this is empty) forced to
    /// match exactly whatever the enabled repos currently offer, allowing
    /// downgrades — `SOLVER_DISTUPGRADE`, distinct from `SOLVER_UPDATE`
    /// in that it doesn't just prefer newer, it forces sync either way.
    pub distro_sync_names: Vec<String>,
    pub distro_sync_all: bool,
    /// dnf5's `add_downgrade`/`add_reinstall`: an exact, already-selected
    /// target NEVRA (name, version, release, arch) to force-install via a
    /// single-solvable `SOLVER_SOLVABLE | SOLVER_INSTALL` job — unlike
    /// `install_names`'s `SOLVER_SOLVABLE_ONE_OF` (which lets the solver's
    /// own policy pick the best candidate among several), this pins the
    /// job to the one exact solvable the caller already picked (rum-cli's
    /// `sync_packages` already does its own EVR selection for downgrade/
    /// reinstall — this just gets that exact choice past libsolv's own
    /// "already satisfied" no-op instead of re-deciding it here).
    pub pinned_installs: Vec<(String, String, String, String)>,
    /// dnf5's `allow_uninstall_all_but_protected`: when set, permits
    /// libsolv's own dependency resolution to erase installed packages (other
    /// than `protected_names` and base-locked solvables) to satisfy a
    /// conflict or a Requires that would otherwise make the transaction
    /// unsolvable — `SOLVER_ALLOWUNINSTALL` per installed solvable. Without
    /// this, libsolv treats every installed package as implicitly protected
    /// from removal, so a conflicting install/upgrade simply fails as
    /// unsolvable instead of proposing an erasure (matching dnf's plain,
    /// non-`--allowerasing` behavior).
    pub allow_erasing: bool,
    /// dnf5's `multilib_policy=all` (default is `best`, the always-false
    /// case): for a bare `install_names` entry with no explicit `.arch`,
    /// split candidates across every distinct arch present into separate
    /// jobs (`libdnf5/base/goal.cpp`'s `na_map`-grouped `add_install`
    /// calls) so e.g. both `foo.x86_64` and `foo.i686` install side by
    /// side, instead of the default single `SOLVER_SOLVABLE_ONE_OF` job
    /// that lets the solver's own policy pick just one.
    pub multilib_all: bool,
    /// dnf5's `best=`: require the newest available EVR to resolve
    /// cleanly rather than silently backtracking to an older, satisfiable
    /// one — `SOLVER_FORCEBEST`, applied to every install/upgrade/
    /// distro-sync job (dnf5's own `GoalPrivate::add_install`/
    /// `add_upgrade`/`add_distro_sync`, `libdnf5/rpm/solv/goal_private.hpp`
    /// lines 319-370, all OR in `best ? SOLVER_FORCEBEST : 0`).
    pub best: bool,
    /// dnf5's `obsoletes=` (default `true`): when a requested install name
    /// matches nothing literally and nothing `Provides` it either, widen
    /// the search to packages that `Obsoletes` it — see the fallback in
    /// the `install_names` loop below. `Default` (hand-rolled below,
    /// rather than derived) sets this `true` to match dnf5's own default.
    pub obsoletes: bool,
    /// dnf5's `allow_downgrade=`/`--allow-downgrade`/`--no-allow-downgrade`
    /// (default `true`): whether resolving this request may pick an
    /// available EVR older than what's currently installed — for both a
    /// pinned-NEVRA request naming an older build directly and any
    /// dependency the solver would otherwise need to downgrade to satisfy
    /// a conflict. `SOLVER_FLAG_ALLOW_DOWNGRADE`.
    pub allow_downgrade: bool,
}

impl Default for SolveRequest {
    fn default() -> Self {
        Self {
            install_names: Vec::new(),
            erase_names: Vec::new(),
            installonly_names: Vec::new(),
            protected_names: Vec::new(),
            upgrade_names: Vec::new(),
            upgrade_all: false,
            distro_sync_names: Vec::new(),
            distro_sync_all: false,
            pinned_installs: Vec::new(),
            allow_erasing: false,
            multilib_all: false,
            best: false,
            obsoletes: true,
            allow_downgrade: true,
        }
    }
}

/// Outcome of a successful solve: the transaction's steps, classified and
/// mapped back to `rum_core::Package` via `PoolBuild::packages`. Mirrors
/// the shape `rum_resolver::Plan` needs, without depending on that crate
/// from here (keeps `rum-solv` a leaf FFI crate).
#[derive(Default, Debug)]
pub struct SolveResult {
    pub to_install: Vec<Package>,
    pub to_erase: Vec<Package>,
    pub to_upgrade: Vec<Package>,
    pub to_obsolete: Vec<Package>,
}

/// Runs one full install/erase job through libsolv and classifies the
/// resulting transaction. Every `base.base_solvables` id is `SOLVER_LOCK`ed
/// before solving — libsolv's own conflict/erase resolution can then never
/// select one for removal or replacement, which is RakuOS Split mode's one
/// hard requirement (base-image packages are read-only and must survive
/// any overlay transaction unchanged).
///
/// This is intentionally "bare": no recommends/installonly/allow-erasing/
/// force-names policy is applied yet (see rum-resolver's equivalent knobs,
/// not yet ported) — just what libsolv itself decides from Requires/
/// Conflicts/Obsoletes/Provides alone.
pub fn solve(build: &mut PoolBuild, req: &SolveRequest) -> Result<SolveResult> {
    let mut job = unsafe {
        let mut q: sys::Queue = std::mem::zeroed();
        sys::queue_init(&mut q);
        q
    };

    // Mirrors dnf5's actual `GoalPrivate::add_install`
    // (libdnf5/rpm/solv/goal_private.hpp): resolve the spec ourselves into
    // every matching solvable id first (all versions, all repos — filtered
    // to one arch when the caller disambiguated with `name.arch`), then
    // push exactly one SOLVER_SOLVABLE_ONE_OF job over that id set. ONE_OF
    // asks libsolv to install exactly one of the given solvables — the
    // solver's own policy (FOCUS_NEW/BEST_OBEY_POLICY, already set below)
    // picks the best one — with SETARCH/SETEVR keeping dependency
    // resolution pinned to whichever one it picks rather than treating the
    // job as an open name/provides search.
    //
    // This replaces an earlier hand-rolled pair of jobs (an exact-solvable
    // SOLVER_INSTALL plus a companion by-name SOLVER_UPDATE) that was
    // built to fix a real live bug — `mesa-vulkan-drivers.i686` not
    // syncing an already-installed older x86_64 sibling build, causing a
    // real rpm file conflict — but the companion SOLVER_UPDATE job turned
    // out to itself cause a *different* live bug: pulling in an unrelated
    // cluster of qemu/xen/QAT packages that were never installed or
    // requested. dnf5 needs no such companion job for the sibling-arch
    // sync case — ONE_OF+SETARCH+SETEVR alone reproduces its exact
    // behavior (verified: dnf5's own output for the identical request also
    // lists the x86_64 sibling under "Upgrading", using only this job
    // shape), so the extra job is dropped rather than patched further.
    for (name, arch) in &req.install_names {
        let name_id = build.pool.str2id(name, false);
        let mut candidates: Vec<sys::Id> = if name_id == 0 {
            Vec::new()
        } else {
            build
                .packages
                .iter()
                .filter(|(_, p)| p.nevra.name == *name && arch.as_deref().is_none_or(|a| p.nevra.arch == a))
                .map(|(id, _)| *id)
                .collect()
        };
        if candidates.is_empty() {
            // No package is literally named `name` — fall back to treating
            // it as a capability, exactly like dnf/dnf5 do for `dnf install
            // <provides>` (e.g. `webserver`, or a soname like
            // `libfoo.so.1(ABI)()`): ask libsolv's own provides index for
            // every solvable that has a matching `Provides:` entry, rather
            // than the name-only scan above. This is also what makes an
            // ordinary `dnf install`-style request resolve packages that
            // are only reachable by virtual/alias name, which a pure
            // literal-name match (the common case, tried first for
            // performance and to prefer an exact name over a same-named
            // Provides on some other package) can never see.
            if name_id != 0 {
                unsafe {
                    let mut ptr = sys::pool_whatprovides_ptr(build.pool.as_raw(), name_id);
                    while *ptr != 0 {
                        let id = *ptr;
                        if build.packages.get(&id).is_some_and(|p| arch.as_deref().is_none_or(|a| p.nevra.arch == a)) {
                            candidates.push(id);
                        }
                        ptr = ptr.add(1);
                    }
                }
            }
        }
        // dnf5's `obsoletes=` (default `true`): when nothing is literally
        // named or provides `name` (e.g. it was renamed/replaced in the
        // repos), widen the candidate search to any available package
        // whose `Obsoletes:` covers `name` — `add_obsoletes_to_data`
        // (`libdnf5/base/goal.cpp:1527-1594`), so `dnf install <old-name>`
        // still resolves to its replacement instead of failing outright.
        // `false` keeps rum's older, stricter behavior (literal name/
        // provides only).
        if candidates.is_empty() && req.obsoletes {
            for (&id, p) in &build.packages {
                if !arch.as_deref().is_none_or(|a| p.nevra.arch == a) {
                    continue;
                }
                if p.obsoletes.iter().any(|d| d.name == *name) {
                    candidates.push(id);
                }
            }
        }
        if candidates.is_empty() {
            match arch {
                Some(arch) => anyhow::bail!("nothing provides '{name}.{arch}'"),
                None => anyhow::bail!("nothing provides '{name}'"),
            }
        }
        // dnf5's `multilib_policy=` (`libdnf5/base/goal.cpp:1559-1640`):
        // `best` (the default) issues a single `SOLVER_SOLVABLE_ONE_OF`
        // job spanning every arch build of `name`, letting the solver's
        // own policy pick exactly one — the branch below with a single
        // `push_one_of` call. `all` instead issues one *separate* job per
        // distinct arch present (so e.g. a plain `dnf install foo` with
        // both `foo.x86_64` and `foo.i686` available installs both side by
        // side, not just the solver's preferred one) — only when the
        // caller didn't already disambiguate with an explicit `name.arch`
        // spec, matching dnf5's `!utils::is_glob_pattern(arch)` guard.
        let mut by_arch: HashMap<&str, Vec<sys::Id>> = HashMap::new();
        for &id in &candidates {
            if let Some(p) = build.packages.get(&id) {
                by_arch.entry(p.nevra.arch.as_str()).or_default().push(id);
            }
        }
        let mut push_one_of = |job: &mut sys::Queue, ids: &[sys::Id]| {
            let mut queue = unsafe {
                let mut q: sys::Queue = std::mem::zeroed();
                sys::queue_init(&mut q);
                q
            };
            for &id in ids {
                unsafe { sys::queue_push(&mut queue, id) };
            }
            let what = unsafe { sys::pool_queuetowhatprovides(build.pool.as_raw(), &mut queue) };
            unsafe { sys::queue_free(&mut queue) };
            let mut flags = sys::SOLVER_INSTALL | sys::SOLVER_SOLVABLE_ONE_OF | sys::SOLVER_SETARCH | sys::SOLVER_SETEVR;
            if req.best {
                flags |= sys::SOLVER_FORCEBEST;
            }
            unsafe { sys::queue_push2(job, flags as sys::Id, what) };
        };
        if req.multilib_all && arch.is_none() && by_arch.len() > 1 {
            for ids in by_arch.values() {
                push_one_of(&mut job, ids);
            }
        } else {
            push_one_of(&mut job, &candidates);
        }
    }
    // Mirrors dnf5's `Goal::Impl::set_exclude_from_weak` "autodetect" pass
    // (libdnf5/base/goal.cpp): for every currently-installed package, look
    // at its Recommends. If nothing *currently installed* already provides
    // a given recommend, exclude every candidate that could satisfy it from
    // ever being pulled in to satisfy a weak dependency in this solve —
    // libsolv's own weak-dep-of-an-upgraded-package logic has no equivalent
    // guard (SOLVER_FLAG_ADD_ALREADY_RECOMMENDED only suppresses recommends
    // from solvables whose *exact* id is unchanged by this transaction, not
    // ones merely upgraded in place), so without this a package recommended
    // by e.g. systemd-container — never installed, never requested — gets
    // pulled in purely because systemd-container itself was upgraded as a
    // side effect of an unrelated request. Confirmed live: this is the
    // actual root cause of the qemu/xen/QAT cluster bug, not job type.
    // Capability -> providers index (own name + every `Provides` entry, like
    // dnf5's `query.filter_provides(reldep_list)`), not a bare name match:
    // a `Recommends` target is a capability, and matching it by literal
    // package name alone misses virtual-provide recommends (and, in the
    // by-name provider lookup below, could wrongly skip/include a
    // same-named-but-unrelated package). Mirrors rum-resolver's own
    // `CandidateIndex` for the same reason.
    let mut by_capability: HashMap<&str, Vec<sys::Id>> = HashMap::new();
    for (&id, pkg) in &build.packages {
        by_capability.entry(pkg.nevra.name.as_str()).or_default().push(id);
        for p in &pkg.provides {
            by_capability.entry(p.name.as_str()).or_default().push(id);
        }
    }
    let installed_capabilities: HashSet<&str> = build
        .installed_solvables
        .iter()
        .filter_map(|id| build.packages.get(id))
        .flat_map(|p| std::iter::once(p.nevra.name.as_str()).chain(p.provides.iter().map(|d| d.name.as_str())))
        .collect();
    let mut excluded_from_weak: HashSet<String> = HashSet::new();
    for &id in &build.installed_solvables {
        let Some(pkg) = build.packages.get(&id) else { continue };
        for rec in &pkg.recommends {
            if installed_capabilities.contains(rec.name.as_str()) || !excluded_from_weak.insert(rec.name.clone()) {
                continue;
            }
            for &cand_id in by_capability.get(rec.name.as_str()).map(|v| v.as_slice()).unwrap_or(&[]) {
                unsafe { sys::queue_push2(&mut job, (sys::SOLVER_SOLVABLE | sys::SOLVER_EXCLUDEFROMWEAK) as sys::Id, cand_id) };
            }
        }
    }

    // Mirrors dnf5's `GoalPrivate::add_remove`: operates on already-
    // resolved *installed* solvable ids (plain SOLVER_SOLVABLE, no name/
    // provides selector — dnf5 never lets libsolv search for what to
    // erase, it's always something the caller already knows is installed).
    for name in &req.erase_names {
        let ids: Vec<sys::Id> = build
            .installed_solvables
            .iter()
            .filter(|id| build.packages.get(id).is_some_and(|p| p.nevra.name == *name))
            .copied()
            .collect();
        anyhow::ensure!(!ids.is_empty(), "nothing provides '{name}'");
        for id in ids {
            unsafe { sys::queue_push2(&mut job, (sys::SOLVER_ERASE | sys::SOLVER_SOLVABLE) as sys::Id, id) };
        }
    }
    // Mirrors dnf5's `GoalPrivate::construct_job` installonly handling: a
    // by-provides job, not a precomputed candidate list, so it also covers
    // future/available builds of the name, not just what's currently
    // installed.
    for name in &req.installonly_names {
        let name_id = build.pool.str2id(name, false);
        if name_id != 0 {
            unsafe { sys::queue_push2(&mut job, (sys::SOLVER_MULTIVERSION | sys::SOLVER_SOLVABLE_PROVIDES) as sys::Id, name_id) };
        }
    }
    // Mirrors dnf5's `add_protected_packages`: mark every installed
    // solvable of a protected name as user-installed so libsolv's own
    // erasure/obsolete/conflict resolution never proposes removing it —
    // this is enforcement at the same layer dnf5 does it (the solver
    // itself), not just an app-level guard on explicit `rum remove`.
    //
    // `SOLVER_USERINSTALLED` alone turned out not to be a hard guarantee
    // against erasure under an `upgrade`/`distro-sync` job (`SOLVER_UPDATE`/
    // `SOLVER_DISTUPGRADE`) even with the offending solvable excluded from
    // every `SOLVER_ALLOWUNINSTALL` job below — live-testing a protected
    // package under `rum upgrade --allowerasing` still erased it. `SOLVER_LOCK`
    // is the actual hard constraint (forbids *any* state change, not just
    // erasure) — a stronger guarantee than dnf5's erasure-only protection,
    // but the safe direction to be wrong in for a package an admin marked
    // untouchable.
    for name in &req.protected_names {
        for &id in &build.installed_solvables {
            if build.packages.get(&id).is_some_and(|p| p.nevra.name == *name) {
                unsafe {
                    sys::queue_push2(&mut job, (sys::SOLVER_USERINSTALLED | sys::SOLVER_SOLVABLE) as sys::Id, id);
                    sys::queue_push2(&mut job, (sys::SOLVER_LOCK | sys::SOLVER_SOLVABLE) as sys::Id, id);
                }
            }
        }
    }
    // SOLVER_WEAK is a safety net, not the primary fix — the real fix is
    // in `build_pool_locked`, which strips version constraints off any
    // `Requires` targeting a base-only package name so the locked base
    // copy satisfies it directly (no shadow install, no skip). This flag
    // just prevents a total solve failure if some other, unrelated
    // constraint (e.g. a `Conflicts`, not covered by that relaxation)
    // still made a name's upgrade impossible without disturbing a locked
    // base solvable — that one name is dropped instead of failing
    // everything.
    let update_flags = sys::SOLVER_UPDATE | sys::SOLVER_WEAK | if req.best { sys::SOLVER_FORCEBEST } else { 0 };
    let distupgrade_flags = sys::SOLVER_DISTUPGRADE | sys::SOLVER_WEAK | if req.best { sys::SOLVER_FORCEBEST } else { 0 };
    for name in &req.upgrade_names {
        let name_id = build.pool.str2id(name, false);
        if name_id != 0 {
            unsafe { sys::queue_push2(&mut job, (update_flags | sys::SOLVER_SOLVABLE_PROVIDES) as sys::Id, name_id) };
        }
    }
    if req.upgrade_all && req.upgrade_names.is_empty() {
        unsafe { sys::queue_push2(&mut job, (update_flags | sys::SOLVER_SOLVABLE_ALL) as sys::Id, 0) };
    }
    for name in &req.distro_sync_names {
        let name_id = build.pool.str2id(name, false);
        if name_id != 0 {
            unsafe { sys::queue_push2(&mut job, (distupgrade_flags | sys::SOLVER_SOLVABLE_PROVIDES) as sys::Id, name_id) };
        }
    }
    if req.distro_sync_all && req.distro_sync_names.is_empty() {
        unsafe { sys::queue_push2(&mut job, (distupgrade_flags | sys::SOLVER_SOLVABLE_ALL) as sys::Id, 0) };
    }
    for (name, version, release, arch) in &req.pinned_installs {
        let Some(&id) = build
            .packages
            .iter()
            .find(|(_, p)| p.nevra.name == *name && p.nevra.version == *version && p.nevra.release == *release && p.nevra.arch == *arch)
            .map(|(id, _)| id)
        else {
            anyhow::bail!("nothing provides '{name}-{version}-{release}.{arch}'");
        };
        unsafe { sys::queue_push2(&mut job, (sys::SOLVER_INSTALL | sys::SOLVER_SOLVABLE) as sys::Id, id) };
    }
    if req.allow_erasing {
        let protected: HashSet<&str> = req.protected_names.iter().map(|s| s.as_str()).collect();
        let base: HashSet<sys::Id> = build.base_solvables.iter().copied().collect();
        for &id in &build.installed_solvables {
            if base.contains(&id) {
                continue;
            }
            if build.packages.get(&id).is_some_and(|p| protected.contains(p.nevra.name.as_str())) {
                continue;
            }
            unsafe { sys::queue_push2(&mut job, (sys::SOLVER_ALLOWUNINSTALL | sys::SOLVER_SOLVABLE) as sys::Id, id) };
        }
    }
    for &base_id in &build.base_solvables {
        unsafe { sys::queue_push2(&mut job, (sys::SOLVER_LOCK | sys::SOLVER_SOLVABLE) as sys::Id, base_id) };
    }

    let solver = unsafe { sys::solver_create(build.pool.as_raw()) };
    anyhow::ensure!(!solver.is_null(), "solver_create returned null");

    // Mirrors dnf5's actual flag set exactly, verified against its source
    // (libdnf5/rpm/solv/goal_private.cpp) rather than general libdnf
    // knowledge — a prior version of this comment listed several flags
    // (ALLOW_ARCHCHANGE, SPLITPROVIDES, FOCUS_BEST, INSTALL_ALSO_UPDATES)
    // dnf5 does not actually set, and had ALLOW_DOWNGRADE backwards.
    //
    // dnf5's `init_solver()` sets these four unconditionally on every solve:
    unsafe {
        // Don't remove packages no longer in a repo during distro-sync/dup.
        sys::solver_set_flag(solver, sys::SOLVER_FLAG_KEEP_ORPHANS as i32, 1);
        // No arch change when FORCEBEST is set on a job.
        sys::solver_set_flag(solver, sys::SOLVER_FLAG_BEST_OBEY_POLICY as i32, 1);
        // Support package splits via Obsoletes (dnf's classic obsoletes
        // semantics, distinct from yum's stricter default).
        sys::solver_set_flag(solver, sys::SOLVER_FLAG_YUM_OBSOLETES as i32, 1);
        // Solution reordering closer to what users expect from urpm/dnf;
        // guarded in dnf5 by a libsolv-version feature check, but the
        // constant itself has been stable since libsolv 0.6.6 — every
        // libsolv version rum targets (EL10's 0.7.33, Fedora's newer) has
        // it.
        sys::solver_set_flag(solver, sys::SOLVER_FLAG_URPM_REORDER as i32, 1);
    }

    // dnf5's `GoalPrivate::resolve()` sets these four from user-facing
    // config (`allow_downgrade`, `allow_vendor_change`, `install_weak_deps`
    // in dnf.conf) on every solve, each defaulting to enabled — rum has no
    // equivalent config surface yet, so default-on matches dnf5's own
    // out-of-the-box behavior exactly:
    unsafe {
        // install_weak_deps=true by default → IGNORE_RECOMMENDED=0 (i.e.
        // recommends/suggests are NOT ignored by the solver itself here;
        // rum-resolver already applies its own best-effort recommends
        // handling separately, so this only affects libsolv's own internal
        // weak-dep pulls, matching dnf5's default).
        sys::solver_set_flag(solver, sys::SOLVER_FLAG_IGNORE_RECOMMENDED as i32, 0);
        // allow_downgrade=true by default — dnf5's `--no-allow-downgrade`
        // forces this off for the duration of one solve.
        sys::solver_set_flag(solver, sys::SOLVER_FLAG_ALLOW_DOWNGRADE as i32, if req.allow_downgrade { 1 } else { 0 });
        // allow_vendor_change=true by default (both plain and dup variants).
        sys::solver_set_flag(solver, sys::SOLVER_FLAG_ALLOW_VENDORCHANGE as i32, 1);
        sys::solver_set_flag(solver, sys::SOLVER_FLAG_DUP_ALLOW_VENDORCHANGE as i32, 1);
        // Prefer installing the latest available version of dependencies
        // even if it grows the transaction, matching dnf5's default
        // behavior since libsolv 0.7.30 (again version-gated in dnf5's own
        // source, but the constant is stable across every libsolv we
        // target).
        sys::solver_set_flag(solver, sys::SOLVER_FLAG_FOCUS_NEW as i32, 1);
        // Explicitly force off (should already be libsolv's default): don't
        // re-trigger recommends that were already present-but-unsatisfied on
        // a package before this transaction just because that package is
        // being upgraded in place.
        sys::solver_set_flag(solver, sys::SOLVER_FLAG_ADD_ALREADY_RECOMMENDED as i32, 0);
    }

    let problem_count = unsafe { sys::solver_solve(solver, &mut job) };
    if problem_count > 0 {
        let mut problems = Vec::new();
        unsafe {
            let mut problem = sys::solver_next_problem(solver, 0);
            while problem != 0 {
                let cstr = sys::solver_problem2str(solver, problem);
                if !cstr.is_null() {
                    problems.push(std::ffi::CStr::from_ptr(cstr).to_string_lossy().into_owned());
                }
                problem = sys::solver_next_problem(solver, problem);
            }
            sys::solver_free(solver);
            sys::queue_free(&mut job);
        }
        anyhow::bail!("could not resolve: {}", problems.join("; "));
    }

    let trans = unsafe { sys::solver_create_transaction(solver) };
    anyhow::ensure!(!trans.is_null(), "solver_create_transaction returned null");

    let mut result = SolveResult::default();
    unsafe {
        let steps = &(*trans).steps;
        for i in 0..steps.count as isize {
            let p = *steps.elements.offset(i);
            let ty = sys::transaction_type(trans, p, sys::SOLVER_TRANSACTION_SHOW_ACTIVE as i32);
            let Some(pkg) = build.packages.get(&p) else { continue };
            match ty as u32 {
                sys::SOLVER_TRANSACTION_INSTALL | sys::SOLVER_TRANSACTION_REINSTALL | sys::SOLVER_TRANSACTION_MULTIINSTALL => result.to_install.push(pkg.clone()),
                sys::SOLVER_TRANSACTION_ERASE => result.to_erase.push(pkg.clone()),
                sys::SOLVER_TRANSACTION_UPGRADE | sys::SOLVER_TRANSACTION_DOWNGRADE | sys::SOLVER_TRANSACTION_CHANGE => result.to_upgrade.push(pkg.clone()),
                sys::SOLVER_TRANSACTION_OBSOLETES => result.to_obsolete.push(pkg.clone()),
                _ => {}
            }
        }
        sys::transaction_free(trans);
        sys::solver_free(solver);
        sys::queue_free(&mut job);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rum_core::Nevra;

    fn pkg(name: &str, evr: &str, arch: &str) -> Package {
        Package {
            nevra: Nevra { name: name.to_string(), epoch: 0, version: evr.split('-').next().unwrap().to_string(), release: evr.split('-').nth(1).unwrap_or("1").to_string(), arch: arch.to_string() },
            summary: String::new(),
            provides: vec![],
            requires: vec![Dependency::unversioned("libfoo.so.1")],
            conflicts: vec![],
            obsoletes: vec![],
            recommends: vec![],
            suggests: vec![],
            enhances: vec![],
            supplements: vec![],
            location: String::new(),
            repo_id: "test".to_string(),
            repo_priority: 99,
            repo_cost: 1000,
            install_size: 0,
            download_size: 0,
            vendor: String::new(),
            checksum_type: String::new(),
            checksum: String::new(),
        }
    }

    fn pkg_providing(name: &str, evr: &str, arch: &str, provides: &str) -> Package {
        let mut p = pkg(name, evr, arch);
        p.requires.clear();
        p.provides.push(Dependency::unversioned(provides));
        p
    }

    #[test]
    fn pool_creates_and_frees_without_crashing() {
        let mut pool = Pool::new().unwrap();
        pool.createwhatprovides();
    }

    #[test]
    fn builds_pool_from_packages() {
        let base = vec![pkg_providing("glibc", "2.39-1", "x86_64", "libfoo.so.1")];
        let overlay = vec![pkg("vim", "9.0-1", "x86_64")];
        let available = vec![pkg("vim", "9.1-1", "x86_64")];
        let build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        assert_eq!(build.base_solvables.len(), 1);
    }

    #[test]
    fn solves_simple_install() {
        let base = vec![pkg_providing("glibc", "2.39-1", "x86_64", "libfoo.so.1")];
        let overlay = vec![];
        let available = vec![pkg("htop", "3.3-1", "x86_64")];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();

        let result = solve(&mut build, &SolveRequest { install_names: vec![("htop".to_string(), None)], erase_names: vec![], ..Default::default() }).unwrap();

        assert_eq!(result.to_install.len(), 1);
        assert_eq!(result.to_install[0].nevra.name, "htop");
        assert!(result.to_erase.is_empty());
    }

    #[test]
    fn install_name_falls_back_to_a_provides_match_when_no_literal_name_exists() {
        // dnf-style "install by capability": nothing is literally named
        // `webserver`, but `httpd` Provides it — an install request for the
        // bare capability name should still resolve, the same way `dnf
        // install webserver` does.
        let base = vec![pkg_providing("glibc", "2.39-1", "x86_64", "libfoo.so.1")];
        let overlay = vec![];
        let available = vec![pkg_providing("httpd", "2.4-1", "x86_64", "webserver")];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();

        let result = solve(&mut build, &SolveRequest { install_names: vec![("webserver".to_string(), None)], erase_names: vec![], ..Default::default() }).unwrap();

        assert_eq!(result.to_install.len(), 1);
        assert_eq!(result.to_install[0].nevra.name, "httpd");
    }

    #[test]
    fn install_name_prefers_a_literal_package_name_over_a_same_named_provides() {
        // If a real package is literally named what was requested, that
        // must win over some unrelated package that merely Provides the
        // same string — the literal-name scan runs first and only falls
        // back to the provides index when it finds nothing.
        let base = vec![pkg_providing("glibc", "2.39-1", "x86_64", "libfoo.so.1")];
        let overlay = vec![];
        let available = vec![pkg("vim", "9.1-1", "x86_64"), pkg_providing("vim-enhanced", "9.1-1", "x86_64", "vim")];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();

        let result = solve(&mut build, &SolveRequest { install_names: vec![("vim".to_string(), None)], erase_names: vec![], ..Default::default() }).unwrap();

        assert_eq!(result.to_install.len(), 1);
        assert_eq!(result.to_install[0].nevra.name, "vim");
    }

    #[test]
    fn split_mode_never_erases_base_packages() {
        // A direct erase request against a base-image package must never
        // silently succeed and must never actually erase it — SOLVER_LOCK
        // (applied to every `base_solvables` id in `solve()`) makes an
        // explicit erase job for a locked solvable an outright solve
        // failure ("conflicting requests") rather than a silent no-op or,
        // worse, an actual removal. That's the correct, loud failure mode
        // for RakuOS Split mode: base packages are read-only.
        let base = vec![pkg("glibc", "2.39-1", "x86_64")];
        let overlay = vec![];
        let available = vec![];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();

        let err = solve(&mut build, &SolveRequest { install_names: vec![], erase_names: vec!["glibc".to_string()], ..Default::default() }).unwrap_err();

        assert!(err.to_string().contains("conflicting"), "expected a solve conflict, got: {err}");
    }

    #[test]
    fn installonly_names_do_not_disturb_a_plain_already_satisfied_install() {
        // `install kernel` against an already-installed `kernel` is a
        // ONE_OF job that the installed solvable itself already satisfies
        // — a no-op, matching dnf5's real "install" semantics. Pushing the
        // `SOLVER_MULTIVERSION` job for `installonly_names` shouldn't
        // change that: multiversion only disables the same-name-obsoletes
        // rule, it doesn't force a new install by itself (real dnf5
        // behavior: `dnf install kernel` with kernel already installed
        // says "already installed" even under installonlypkgs — you have
        // to name an exact NEVRA to add a second version alongside it,
        // which rum's install-spec parsing doesn't support yet).
        let base = vec![pkg_providing("glibc", "2.39-1", "x86_64", "libfoo.so.1"), pkg("kernel", "1-1", "x86_64")];
        let overlay = vec![];
        let available = vec![pkg("kernel", "2-1", "x86_64")];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let result = solve(
            &mut build,
            &SolveRequest { install_names: vec![("kernel".to_string(), None)], erase_names: vec![], installonly_names: vec!["kernel".to_string()], ..Default::default() },
        )
        .unwrap();
        assert!(result.to_erase.is_empty());
        assert!(result.to_upgrade.is_empty());
        assert!(result.to_install.is_empty());
    }

    #[test]
    fn upgrade_names_moves_installed_overlay_package_to_newest() {
        let base = vec![pkg_providing("glibc", "2.39-1", "x86_64", "libfoo.so.1")];
        let overlay = vec![pkg("vim", "9.0-1", "x86_64")];
        let available = vec![pkg("vim", "9.1-1", "x86_64")];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let result = solve(&mut build, &SolveRequest { upgrade_names: vec!["vim".to_string()], ..Default::default() }).unwrap();
        assert!(result.to_erase.is_empty());
        assert_eq!(result.to_upgrade.len(), 1);
        assert_eq!(result.to_upgrade[0].nevra.version, "9.1");
    }

    #[test]
    fn upgrade_all_leaves_base_solvables_untouched() {
        // Split mode's one hard rule: SOLVER_LOCK over base_solvables must
        // survive even an "upgrade everything" job — a newer base-named
        // build in `available` must never be selected in place of the
        // locked base copy.
        let base = vec![pkg_providing("glibc", "2.39-1", "x86_64", "libfoo.so.1")];
        let overlay = vec![pkg("vim", "9.0-1", "x86_64")];
        let available = vec![pkg("vim", "9.1-1", "x86_64"), pkg_providing("glibc", "2.40-1", "x86_64", "libfoo.so.1")];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let result = solve(&mut build, &SolveRequest { upgrade_all: true, ..Default::default() }).unwrap();
        assert!(result.to_erase.is_empty());
        assert!(result.to_upgrade.iter().all(|p| p.nevra.name != "glibc"), "base-locked glibc must never be upgraded, got {:?}", result.to_upgrade);
        assert!(result.to_upgrade.iter().any(|p| p.nevra.name == "vim" && p.nevra.version == "9.1"));
    }

    #[test]
    fn upgrade_all_satisfies_a_requires_on_a_newer_base_only_package_from_the_installed_base_copy() {
        // Live bug: an overlay package (rpm-devel) whose only available
        // upgrade Requires an exact newer EVR of a package that lives only
        // in the base rpmdb (rpm) used to make the whole solve fail with
        // "cannot install both rpm-0:6.1.0... and rpm-0:6.0.2...". Base
        // packages must never move (SOLVER_LOCK) AND must never be
        // shadowed by a same-name overlay copy — so the correct outcome is
        // that the version pin on rpm-devel's `Requires: rpm` is relaxed
        // and satisfied directly by the already-installed, untouched base
        // rpm-6.0.2, letting rpm-devel's own upgrade to 6.1.0 proceed
        // normally (nothing gets skipped, nothing gets shadowed).
        let base = vec![{
            let mut p = pkg("rpm", "6.0.2-1", "x86_64");
            p.requires.clear();
            p
        }];
        let overlay = vec![
            {
                let mut p = pkg("rpm-devel", "6.0.2-1", "x86_64");
                p.requires = vec![Dependency { name: "rpm".to_string(), constraint: Some((Comparator::Eq, "6.0.2-1".to_string())) }];
                p
            },
            {
                let mut p = pkg("vim", "9.0-1", "x86_64");
                p.requires.clear();
                p
            },
        ];
        let available = vec![
            {
                let mut p = pkg("rpm-devel", "6.1.0-1", "x86_64");
                p.requires = vec![Dependency { name: "rpm".to_string(), constraint: Some((Comparator::Eq, "6.1.0-1".to_string())) }];
                p
            },
            {
                let mut p = pkg("vim", "9.1-1", "x86_64");
                p.requires.clear();
                p
            },
        ];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let result = solve(&mut build, &SolveRequest { upgrade_all: true, ..Default::default() }).unwrap();
        assert!(result.to_erase.is_empty());
        assert!(result.to_install.iter().all(|p| p.nevra.name != "rpm"), "rpm must never be shadow-installed into the overlay, got {:?}", result.to_install);
        assert!(result.to_upgrade.iter().all(|p| p.nevra.name != "rpm"), "base rpm must never be touched, got {:?}", result.to_upgrade);
        assert!(result.to_upgrade.iter().any(|p| p.nevra.name == "rpm-devel" && p.nevra.version == "6.1.0"), "rpm-devel's upgrade must proceed, satisfied by the base rpm copy, got {:?}", result.to_upgrade);
        assert!(result.to_upgrade.iter().any(|p| p.nevra.name == "vim" && p.nevra.version == "9.1"), "unrelated vim upgrade must still proceed, got {:?}", result.to_upgrade);
    }

    #[test]
    fn install_never_shadows_a_base_owned_dependency_with_a_fresh_overlay_copy() {
        // Live bug (podman repro against rakuos-cosmic:staging): `rum install
        // stellarium` pulled fresh overlay copies of qt6-qtbase (already on
        // base, locked at 6.11.1) up to 6.11.2, because a *new* package in
        // stellarium's dependency closure (here modelled as qt6-qtcharts) was
        // only built against 6.11.2 and Requires that exact EVR by name.
        // Split mode's entire point: a name+arch the base image ships is
        // satisfied *only* by the locked base copy -- installing stellarium
        // must never also drag a same-name+arch qt6-qtbase into the overlay,
        // even transitively.
        let base = vec![pkg("qt6-qtbase", "6.11.1-1", "x86_64")];
        let overlay = vec![];
        let available = vec![
            {
                let mut p = pkg("stellarium", "26.2-2", "x86_64");
                p.requires = vec![Dependency::unversioned("qt6-qtcharts")];
                p
            },
            {
                let mut p = pkg("qt6-qtcharts", "6.11.2-1", "x86_64");
                p.requires = vec![Dependency { name: "qt6-qtbase".to_string(), constraint: Some((Comparator::Eq, "6.11.2-1".to_string())) }];
                p
            },
            // The dangerous candidate: a fresh qt6-qtbase build at the newer
            // EVR that qt6-qtcharts was built against -- must never be
            // selected since (name, arch) already exists on base.
            pkg("qt6-qtbase", "6.11.2-1", "x86_64"),
        ];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let result = solve(&mut build, &SolveRequest { install_names: vec![("stellarium".to_string(), None)], erase_names: vec![], ..Default::default() }).unwrap();

        assert!(result.to_install.iter().all(|p| p.nevra.name != "qt6-qtbase"), "qt6-qtbase must never be shadow-installed into the overlay, got {:?}", result.to_install);
        assert!(result.to_upgrade.iter().all(|p| p.nevra.name != "qt6-qtbase"), "base qt6-qtbase must never be touched, got {:?}", result.to_upgrade);
        assert!(result.to_install.iter().any(|p| p.nevra.name == "stellarium"), "stellarium install must still succeed, got {:?}", result.to_install);
        assert!(result.to_install.iter().any(|p| p.nevra.name == "qt6-qtcharts"), "qt6-qtcharts install must still succeed, satisfied against the locked base qt6-qtbase, got {:?}", result.to_install);
    }

    #[test]
    fn base_package_isa_qualified_self_require_resolves_without_a_shadow_candidate() {
        // Live bug (regression from the fix above): `rum upgrade` erroring
        // "nothing provides librepo(x86-64) = 0:1.21.0-1.fc44 needed by
        // librepo-devel-0:1.21.0-1.fc44.x86_64" even though librepo IS on
        // base. rpm's `%{name}%{?_isa} = %{version}-%{release}` packaging
        // convention (used by essentially every -devel/-libs subpackage)
        // generates an exact-EVR, ISA-tagged Requires like
        // `librepo(x86-64) = 1.21.0-1`. Repo metadata/rpmdb parsing doesn't
        // always carry the matching self-provide explicitly, so before
        // same-name+arch shadow candidates were excluded from `available`,
        // a fresh duplicate librepo candidate at the same EVR happened to
        // satisfy this by coincidence. With shadowing now blocked, the base
        // copy itself must carry (or be given) its own self-provide, or
        // this becomes unsatisfiable outright. Note: no fresh librepo
        // candidate exists in `available` at all here -- only librepo-devel
        // does -- so this only passes if the base copy self-satisfies.
        let mut librepo_base = pkg("librepo", "1.21.0-1", "x86_64");
        librepo_base.provides.clear(); // exercise the case where repo/rpmdb metadata omitted the self-provide entirely
        let base = vec![librepo_base];
        let overlay = vec![];
        let available = vec![{
            let mut p = pkg("librepo-devel", "1.21.0-1", "x86_64");
            p.requires = vec![Dependency { name: "librepo(x86-64)".to_string(), constraint: Some((Comparator::Eq, "0:1.21.0-1".to_string())) }];
            p
        }];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let result = solve(&mut build, &SolveRequest { install_names: vec![("librepo-devel".to_string(), None)], erase_names: vec![], ..Default::default() }).unwrap();

        assert!(result.to_install.iter().any(|p| p.nevra.name == "librepo-devel"), "librepo-devel install must succeed against the base librepo's synthesized self-provide, got {:?}", result.to_install);
        assert!(result.to_install.iter().all(|p| p.nevra.name != "librepo"), "librepo must never be shadow-installed into the overlay, got {:?}", result.to_install);
    }

    #[test]
    fn base_package_older_than_isa_qualified_exact_evr_require_still_resolves() {
        // Live bug, reproduced on a real machine and again in a clean podman
        // Split-mode container: `rum install librepo-devel` (and `rum
        // upgrade`) erroring "nothing provides librepo(x86-64) =
        // 0:1.21.0-1.fc44 needed by librepo-devel-0:1.21.0-1.fc44.x86_64"
        // even though librepo genuinely is on base -- just at an *older*
        // build (1.20.0-5) than the exact EVR librepo-devel's ISA-tagged
        // Requires demands. The base-name constraint-relaxation loop used to
        // only ever match a plain `Requires: librepo = <evr>` against
        // `base_names`; an ISA-tagged `librepo(x86-64) = <evr>` never
        // equalled the plain name "librepo", so the constraint was never
        // relaxed and, with same-name+arch shadow candidates excluded from
        // the pool (the entire point of Split mode), nothing could satisfy
        // it. Base packages must satisfy overlay deps unconditionally --
        // stripping the ISA suffix before the base_names check fixes this.
        let base = vec![pkg("librepo", "1.20.0-5", "x86_64")];
        let overlay = vec![];
        let available = vec![{
            let mut p = pkg("librepo-devel", "1.21.0-1", "x86_64");
            p.requires = vec![Dependency { name: "librepo(x86-64)".to_string(), constraint: Some((Comparator::Eq, "0:1.21.0-1".to_string())) }];
            p
        }];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let result = solve(&mut build, &SolveRequest { install_names: vec![("librepo-devel".to_string(), None)], erase_names: vec![], ..Default::default() }).unwrap();

        assert!(result.to_install.iter().any(|p| p.nevra.name == "librepo-devel"), "librepo-devel install must succeed against base's older librepo, got {:?}", result.to_install);
        assert!(result.to_install.iter().all(|p| p.nevra.name != "librepo"), "librepo must never be shadow-installed into the overlay just to satisfy a newer exact-EVR require, got {:?}", result.to_install);
    }

    #[test]
    fn upgrade_all_locks_a_foreign_arch_overlay_build_of_a_base_owned_name() {
        // Live bug: libcap-ng.x86_64 ships on base (locked at 0.9.3), but
        // libcap-ng.i686 was installed later into the *overlay* (e.g. for
        // Steam's 32-bit stack) — same name, different arch, different
        // origin. `upgrade_all` used to move the overlay i686 build to the
        // newer 0.9.5 on its own, and libsolv's same-name install check then
        // collided it against the still-locked base x86_64 build. The
        // correct outcome is that a base-owned name is locked across every
        // arch it's installed under, so the overlay i686 build stays put
        // right alongside the base x86_64 build.
        let base = vec![{
            let mut p = pkg("libcap-ng", "0.9.3-1", "x86_64");
            p.requires.clear();
            p
        }];
        let overlay = vec![
            {
                let mut p = pkg("libcap-ng", "0.9.3-1", "i686");
                p.requires.clear();
                p
            },
            {
                let mut p = pkg("vim", "9.0-1", "x86_64");
                p.requires.clear();
                p
            },
        ];
        let available = vec![
            {
                let mut p = pkg("libcap-ng", "0.9.5-1", "x86_64");
                p.requires.clear();
                p
            },
            {
                let mut p = pkg("libcap-ng", "0.9.5-1", "i686");
                p.requires.clear();
                p
            },
            {
                let mut p = pkg("vim", "9.1-1", "x86_64");
                p.requires.clear();
                p
            },
        ];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let result = solve(&mut build, &SolveRequest { upgrade_all: true, ..Default::default() }).unwrap();
        assert!(result.to_erase.is_empty());
        assert!(result.to_upgrade.iter().all(|p| p.nevra.name != "libcap-ng"), "libcap-ng must stay locked in every arch, got {:?}", result.to_upgrade);
        assert!(result.to_install.iter().all(|p| p.nevra.name != "libcap-ng"), "libcap-ng must never be shadow-installed, got {:?}", result.to_install);
        assert!(result.to_upgrade.iter().any(|p| p.nevra.name == "vim" && p.nevra.version == "9.1"), "unrelated vim upgrade must still proceed, got {:?}", result.to_upgrade);
    }

    #[test]
    fn distro_sync_names_can_downgrade() {
        let base = vec![pkg_providing("glibc", "2.39-1", "x86_64", "libfoo.so.1")];
        let overlay = vec![pkg("vim", "9.1-1", "x86_64")];
        // Only an older build is available now (e.g. the repo rolled back)
        // — plain `upgrade` would never pick this (never *worse*), but
        // `distro-sync` forces an exact match to what's currently offered.
        let available = vec![pkg("vim", "9.0-1", "x86_64")];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let result = solve(&mut build, &SolveRequest { distro_sync_names: vec!["vim".to_string()], ..Default::default() }).unwrap();
        assert!(result.to_erase.is_empty());
        assert_eq!(result.to_upgrade.len(), 1);
        assert_eq!(result.to_upgrade[0].nevra.version, "9.0");
    }

    #[test]
    fn pinned_install_forces_exact_downgrade_target() {
        let base = vec![pkg_providing("glibc", "2.39-1", "x86_64", "libfoo.so.1")];
        let overlay = vec![pkg("vim", "9.1-1", "x86_64")];
        let available = vec![pkg("vim", "9.0-1", "x86_64"), pkg("vim", "9.2-1", "x86_64")];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let result = solve(
            &mut build,
            &SolveRequest { pinned_installs: vec![("vim".to_string(), "9.0".to_string(), "1".to_string(), "x86_64".to_string())], ..Default::default() },
        )
        .unwrap();
        assert!(result.to_erase.is_empty());
        assert_eq!(result.to_upgrade.len(), 1);
        assert_eq!(result.to_upgrade[0].nevra.version, "9.0", "pinned target must be the exact requested downgrade, not the newest 9.2 available");
    }

    #[test]
    fn pinned_install_missing_target_is_an_error() {
        let mut build = build_pool("x86_64", &[], &[], &[]).unwrap();
        let err = solve(
            &mut build,
            &SolveRequest { pinned_installs: vec![("nonexistent".to_string(), "1".to_string(), "1".to_string(), "x86_64".to_string())], ..Default::default() },
        )
        .unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn unresolvable_request_is_an_error() {
        let mut build = build_pool("x86_64", &[], &[], &[]).unwrap();
        let err = solve(&mut build, &SolveRequest { install_names: vec![("nonexistent-package".to_string(), None)], erase_names: vec![], ..Default::default() }).unwrap_err();
        assert!(err.to_string().contains("nonexistent-package"));
    }

    fn pkg_conflicting(name: &str, evr: &str, arch: &str, conflict: &str) -> Package {
        let mut p = pkg(name, evr, arch);
        p.conflicts.push(Dependency::unversioned(conflict));
        p
    }

    #[test]
    fn conflicting_install_fails_without_allow_erasing() {
        let base = vec![pkg_providing("glibc", "2.39-1", "x86_64", "libfoo.so.1")];
        let overlay = vec![pkg("old-codec", "1.0-1", "x86_64")];
        let available = vec![pkg_conflicting("new-codec", "2.0-1", "x86_64", "old-codec")];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let err = solve(&mut build, &SolveRequest { install_names: vec![("new-codec".to_string(), None)], ..Default::default() }).unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn allow_erasing_lets_libsolv_erase_a_conflicting_installed_package() {
        let base = vec![pkg_providing("glibc", "2.39-1", "x86_64", "libfoo.so.1")];
        let overlay = vec![pkg("old-codec", "1.0-1", "x86_64")];
        let available = vec![pkg_conflicting("new-codec", "2.0-1", "x86_64", "old-codec")];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let result = solve(&mut build, &SolveRequest { install_names: vec![("new-codec".to_string(), None)], allow_erasing: true, ..Default::default() }).unwrap();
        assert_eq!(result.to_install.len(), 1);
        assert_eq!(result.to_install[0].nevra.name, "new-codec");
        assert_eq!(result.to_erase.len(), 1);
        assert_eq!(result.to_erase[0].nevra.name, "old-codec");
    }

    #[test]
    fn allow_erasing_never_erases_a_protected_package() {
        let base = vec![pkg_providing("glibc", "2.39-1", "x86_64", "libfoo.so.1")];
        let overlay = vec![pkg("old-codec", "1.0-1", "x86_64")];
        let available = vec![pkg_conflicting("new-codec", "2.0-1", "x86_64", "old-codec")];
        let mut build = build_pool("x86_64", &base, &overlay, &available).unwrap();
        let err = solve(
            &mut build,
            &SolveRequest {
                install_names: vec![("new-codec".to_string(), None)],
                allow_erasing: true,
                protected_names: vec!["old-codec".to_string()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
