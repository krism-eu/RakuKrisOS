//! Queries an on-disk rpmdb (Berkeley or sqlite backend — `rpm` itself
//! handles that distinction transparently) for installed packages.
//!
//! Deliberately shells out to the `rpm` binary rather than reimplementing
//! rpmdb parsing: rpmdb's on-disk format is an implementation detail rpm
//! itself doesn't guarantee stability on, and re-parsing it natively is a
//! correctness/security liability for very little benefit over asking rpm.
//! rum owns resolution, repo metadata, and overlay-awareness; rpm stays the
//! authority on "what does this database actually say."

use anyhow::{Context, Result};
use rum_core::{Comparator, Dependency, Nevra, Package};
use std::path::Path;
use std::process::Command;

/// Field/array delimiters chosen to be extremely unlikely to appear inside
/// an rpm tag value, so a single `--qf` pass can be split unambiguously.
const FIELD_SEP: &str = "\u{1}";
const ARRAY_SEP: &str = "\u{2}";

/// Queries every installed package in the rpmdb at `dbpath`, returning
/// enough identity + dependency data to feed the resolver. `repo_id` is
/// stamped onto each result (callers use `"base"` / `"overlay"` /
/// `"installed"` per [`rum_overlay`]'s split-vs-standalone modes).
///
/// `dbpath: None` omits `--dbpath` entirely, so rpm falls back to its own
/// compiled-in default database location — used in standalone (no-overlay)
/// mode, where rum should see exactly what a plain `rpm`/`dnf` on that
/// system would see, nothing RakuOS-specific.
pub fn query_installed(dbpath: Option<&Path>, repo_id: &str) -> Result<Vec<Package>> {
    // A `Split`-mode overlay rpmdb that hasn't been initialized yet (no
    // install has ever run) simply doesn't exist on disk — that's "nothing
    // installed there", not an error, and querying it shouldn't require
    // creating it (which would need root under `/var/lib/rakuos`) just to
    // find that out. `rpm -qa --dbpath <missing dir>` would otherwise print
    // a real error to stderr and trip the bail below.
    if let Some(path) = dbpath {
        if !path.exists() {
            return Ok(Vec::new());
        }
    }
    // %{FILENAMES} is included alongside the explicit PROVIDENEVRS: rpm
    // resolves a file-path Requires (e.g. a scriptlet's `Requires:
    // /usr/bin/bash`) against whichever installed package's file list
    // *owns* that path, not against an explicit `Provides:` tag — most
    // packages never declare one for paths they simply ship. Without this,
    // any file-based Requires against something already on the base image
    // (bash, /bin/sh, etc.) would always spuriously report "nothing
    // provides", even though the file is right there.
    let qf = format!(
        "%{{NAME}}{FIELD_SEP}%{{EPOCH}}{FIELD_SEP}%{{VERSION}}{FIELD_SEP}%{{RELEASE}}{FIELD_SEP}%{{ARCH}}{FIELD_SEP}%{{SIZE}}{FIELD_SEP}%{{SUMMARY}}{FIELD_SEP}\
         [%{{PROVIDENEVRS}}{ARRAY_SEP}]{FIELD_SEP}[%{{REQUIRENEVRS}}{ARRAY_SEP}]{FIELD_SEP}[%{{RECOMMENDNEVRS}}{ARRAY_SEP}]{FIELD_SEP}[%{{FILENAMES}}{ARRAY_SEP}]\\n"
    );
    let mut cmd = Command::new("rpm");
    if let Some(dbpath) = dbpath {
        cmd.arg("--dbpath").arg(dbpath);
    }
    let output = cmd
        .arg("-qa")
        .arg("--qf")
        .arg(&qf)
        .output()
        .with_context(|| format!("running rpm -qa against {}", dbpath.map(Path::display).map(|d| d.to_string()).unwrap_or_else(|| "rpm's default dbpath".into())))?;

    if !output.status.success() {
        // An empty/never-initialized rpmdb is a normal state (e.g. a
        // freshly-created overlay rpmdb before the first install), not an
        // error — rpm exits non-zero with no packages in that case too,
        // so only treat genuine stderr output as fatal.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            anyhow::bail!("rpm -qa failed: {}", stderr.trim());
        }
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut pkg = parse_line(line)?;
            pkg.repo_id = repo_id.to_string();
            Ok(pkg)
        })
        .collect()
}

/// Reads a package's identity/dependency data straight from a local `.rpm`
/// file's own header — no rpmdb/`--dbpath` involved, same `rpm -qp` pattern
/// `repomanage` uses. Lets a caller (`rum install /path/to/foo.rpm`) build a
/// full [`Package`] for a file the resolver never saw, so it can go through
/// the same `apply_install_ex` transaction path as a repo-resolved one.
pub fn query_local_file(path: &Path) -> Result<Package> {
    let qf = format!(
        "%{{NAME}}{FIELD_SEP}%{{EPOCH}}{FIELD_SEP}%{{VERSION}}{FIELD_SEP}%{{RELEASE}}{FIELD_SEP}%{{ARCH}}{FIELD_SEP}%{{SIZE}}{FIELD_SEP}%{{SUMMARY}}{FIELD_SEP}\
         [%{{PROVIDENEVRS}}{ARRAY_SEP}]{FIELD_SEP}[%{{REQUIRENEVRS}}{ARRAY_SEP}]{FIELD_SEP}[%{{RECOMMENDNEVRS}}{ARRAY_SEP}]{FIELD_SEP}[%{{FILENAMES}}{ARRAY_SEP}]\\n"
    );
    let output = Command::new("rpm")
        .arg("-qp")
        .arg("--qf")
        .arg(&qf)
        .arg("--nosignature")
        .arg(path)
        .output()
        .with_context(|| format!("running rpm -qp on {}", path.display()))?;
    anyhow::ensure!(output.status.success(), "rpm -qp failed on {}: {}", path.display(), String::from_utf8_lossy(&output.stderr).trim());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().with_context(|| format!("no output from rpm -qp on {}", path.display()))?;
    parse_line(line)
}

fn parse_line(line: &str) -> Result<Package> {
    let mut fields = line.split(FIELD_SEP);
    let name = fields.next().context("missing NAME field")?.to_string();
    let epoch_raw = fields.next().context("missing EPOCH field")?;
    let epoch = if epoch_raw == "(none)" { 0 } else { epoch_raw.parse().unwrap_or(0) };
    let version = fields.next().context("missing VERSION field")?.to_string();
    let release = fields.next().context("missing RELEASE field")?.to_string();
    let arch = fields.next().context("missing ARCH field")?.to_string();
    let install_size: u64 = fields.next().unwrap_or_default().parse().unwrap_or(0);
    let summary = fields.next().unwrap_or_default().to_string();
    let provides_raw = fields.next().unwrap_or_default();
    let requires_raw = fields.next().unwrap_or_default();
    let recommends_raw = fields.next().unwrap_or_default();
    let filenames_raw = fields.next().unwrap_or_default();

    let mut provides = parse_nevrs(provides_raw);
    provides.extend(
        filenames_raw
            .split(ARRAY_SEP)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|path| Dependency { name: path.to_string(), constraint: None }),
    );

    Ok(Package {
        nevra: Nevra { name, epoch, version, release, arch },
        summary,
        provides,
        requires: parse_nevrs(requires_raw),
        conflicts: Vec::new(),
        obsoletes: Vec::new(),
        recommends: parse_nevrs(recommends_raw),
        suggests: Vec::new(),
        enhances: Vec::new(),
        supplements: Vec::new(),
        location: String::new(),
        repo_id: String::new(), // filled in by caller with "base"/"overlay"
        repo_priority: rum_core::default_repo_priority(),
            repo_cost: rum_core::default_repo_cost(),
        install_size,
        download_size: 0,
            vendor: String::new(),
        checksum_type: String::new(),
        checksum: String::new(),
    })
}

/// Parses an rpm `PROVIDENEVRS`/`REQUIRENEVRS`-style array entry: each item
/// is either a bare capability name, or `name OP version` (e.g.
/// `libfoo.so.1()(64bit)` or `glibc >= 2.34`).
fn parse_nevrs(raw: &str) -> Vec<Dependency> {
    raw.split(ARRAY_SEP)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            // Rich/boolean dependencies (`(rpm-plugin-selinux if rpm-libs)`)
            // are stored as one space-containing token — not a NAME [OP
            // VER] triple, so leave them intact rather than mis-parsing
            // "if"/"unless" as a comparator and truncating the name.
            if entry.starts_with('(') {
                return Dependency::unversioned(entry.to_string());
            }
            let mut parts = entry.splitn(3, ' ');
            let name = parts.next().unwrap_or_default().to_string();
            let op = parts.next();
            let ver = parts.next();
            match (op, ver) {
                (Some(op), Some(ver)) => {
                    let cmp = match op {
                        "<" => Some(Comparator::Lt),
                        "<=" => Some(Comparator::Le),
                        "=" => Some(Comparator::Eq),
                        ">=" => Some(Comparator::Ge),
                        ">" => Some(Comparator::Gt),
                        _ => None,
                    };
                    match cmp {
                        Some(cmp) => Dependency { name, constraint: Some((cmp, ver.to_string())) },
                        None => Dependency::unversioned(name),
                    }
                }
                _ => Dependency::unversioned(name),
            }
        })
        .collect()
}

/// Makes sure a writable rpmdb exists at `dbpath`, creating and
/// initializing it first if this is the very first time rum has written to
/// it (e.g. a freshly-imaged machine's first `rum install`). `rpm --initdb`
/// is a no-op against an already-initialized db, so this is safe to call
/// unconditionally before every write rather than tracking "have I done
/// this already" state separately.
pub fn ensure_initialized(dbpath: &Path) -> Result<()> {
    std::fs::create_dir_all(dbpath).with_context(|| format!("creating rpmdb directory {}", dbpath.display()))?;
    let status = Command::new("rpm").arg("--dbpath").arg(dbpath).arg("--initdb").status().context("spawning rpm --initdb")?;
    anyhow::ensure!(status.success(), "rpm --initdb --dbpath {} failed with {status}", dbpath.display());
    Ok(())
}

/// Convenience: is `name` present (by capability, not just package name) in
/// this rpmdb at all? Used by the overlay layer's satisfied-by-base check.
///
/// `requiring_arch` is the arch of the package whose `Requires` is being
/// checked, when known. Real repo metadata frequently leaves soname/symbol-
/// version capabilities (`libc.so.6(GLIBC_2.38)`, `libfoo.so.1`) untagged
/// with an explicit `(x86-32)`/`(64bit)` ISA suffix even though they are
/// genuinely arch-specific — an installed x86_64 `glibc`'s untagged
/// `libc.so.6(...)` provide must not be treated as satisfying an i686
/// package's identically-named Requires. Filter candidate providers to
/// `requiring_arch` (or `noarch`) for this class of dependency; plain
/// package-name/file-path capabilities remain arch-unfiltered since those
/// really are shared across arches (or already carry their own ISA tag
/// handled upstream by callers).
/// Strips a concrete rpm ISA suffix (`(x86-64)`/`(x86-32)`) off a capability
/// name, returning the bare name and the arch it names. Real rpm builds
/// auto-generate a `name(x86-64) = EVR`-style self-provide for every 64-bit-
/// capable package — repo metadata (primary.xml) carries it explicitly, but
/// [`Package::provides_self`] only ever synthesizes the bare-name form, so
/// without this, a `Requires: foo(x86-64) = <exact EVR>` (e.g. one auto-
/// generated package pinning a same-build sibling) never matches an
/// installed `foo` no matter how exactly it already satisfies the
/// requirement — the caller falls through to treating it as "needs a fresh
/// install," discarding the perfectly good installed copy for a repo
/// candidate that may be a different (even older, if repo priority ranks it
/// first) build entirely.
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

/// True for a package that ships no shared-library (`.so`) capability of its
/// own — i.e. it's a single-arch-only helper (a spawned executable, a data
/// package, a D-Bus service) rather than a real multilib library that other
/// binaries link against per-arch. Packages like `glycin-loaders` fall in
/// this bucket: their loader binaries live at non-arch-namespaced paths
/// (`/usr/libexec/<name>/...`, not `/usr/lib64` vs `/usr/lib`), so an i686
/// build installed alongside an already-installed x86_64 build doesn't add
/// any real 32-bit capability — it just silently loses the file-write race
/// against the already-present 64-bit build (rpm's own file-color multilib
/// handling keeps the higher-color file and drops the other) while still
/// dragging in its own transitive Requires chain for no benefit. Since
/// nothing actually links against this package per-arch, an ISA-tagged
/// Requires on it (e.g. `glycin-loaders(x86-32)`) is just as well satisfied
/// by an already-installed build of *any* arch.
fn is_single_arch_helper(pkg: &Package) -> bool {
    !pkg.provides.iter().any(|p| p.name.contains(".so"))
}

pub fn provides_capability(installed: &[Package], dep: &Dependency, requiring_arch: Option<&str>) -> bool {
    fn check_one(pkg: &Package, dep: &Dependency, requiring_arch: Option<&str>, is_soname_dep: bool, isa: Option<(&str, &str)>) -> bool {
        if is_soname_dep {
            if let Some(want_arch) = requiring_arch {
                if pkg.nevra.arch != want_arch && pkg.nevra.arch != "noarch" {
                    return false;
                }
            }
        }
        if let Some((base, want_arch)) = isa {
            let arch_ok = pkg.nevra.arch == want_arch || (pkg.nevra.arch != want_arch && is_single_arch_helper(pkg));
            if pkg.nevra.name == base && arch_ok {
                let satisfied = match &dep.constraint {
                    None => true,
                    Some(_) => dep.satisfied_by_evr(&pkg.nevra.evr()),
                };
                if satisfied {
                    return true;
                }
            }
        }
        pkg.provides.iter().chain(std::iter::once(&pkg.provides_self())).any(|prov| {
            if prov.name != dep.name {
                return false;
            }
            // An unversioned dep is satisfied by any provide of the same
            // name. Real rpm also treats an *unversioned* provide as
            // satisfying *any* versioned Requires on that name — e.g.
            // lvm2's `(system-release >= 23 if system-release)` is met by
            // fedora-release-container's bare `Provides: system-release`
            // (no EVR at all) on real dnf/rpm; only a *versioned* provide
            // actually needs its EVR compared against the constraint.
            match (&dep.constraint, &prov.constraint) {
                (None, _) | (Some(_), None) => true,
                (Some(_), Some((_, have))) => dep.satisfied_by_evr(have),
            }
        })
    }

    let is_soname_dep = dep.name.contains(".so");
    let isa = strip_isa_suffix(&dep.name);
    installed.iter().any(|pkg| check_one(pkg, dep, requiring_arch, is_soname_dep, isa))
}

/// A one-time-per-resolve capability index over an installed-package list,
/// letting [`CapabilityIndex::provides`] replace `provides_capability`'s
/// O(installed) linear scan with an O(1)-average `HashMap` lookup. Built
/// once by [`rum_overlay::OverlayContext`] and reused across every
/// dependency edge the resolver's BFS walks — on a base image of thousands
/// of packages and a transaction with thousands of edges, the unindexed scan
/// (each element of which *also* linearly scans that package's own
/// `provides` list) was the dominant cost of dependency resolution.
///
/// Stores indices into the caller-supplied slice rather than borrowed
/// `&Package`s so it isn't self-referential — callers must pass the *same*
/// slice to [`CapabilityIndex::provides`] that was passed to
/// [`CapabilityIndex::build`].
pub struct CapabilityIndex {
    by_capability: std::collections::HashMap<String, Vec<usize>>,
}

impl CapabilityIndex {
    pub fn build(installed: &[Package]) -> Self {
        let mut by_capability: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
        for (i, pkg) in installed.iter().enumerate() {
            by_capability.entry(pkg.nevra.name.clone()).or_default().push(i);
            for p in &pkg.provides {
                by_capability.entry(p.name.clone()).or_default().push(i);
            }
        }
        Self { by_capability }
    }

    /// Equivalent to `provides_capability(installed, dep, requiring_arch)`,
    /// given the same `installed` slice this index was [`build`](Self::build)
    /// from — an indexed candidate lookup replaces the outer linear scan,
    /// with identical per-candidate matching logic (including the ISA-suffix
    /// and soname-arch-filter handling).
    pub fn provides(&self, installed: &[Package], dep: &Dependency, requiring_arch: Option<&str>) -> bool {
        let is_soname_dep = dep.name.contains(".so");
        let isa = strip_isa_suffix(&dep.name);

        // The ISA-suffixed self-provide (`name(x86-64)`) is auto-generated
        // by rpm rather than always present as a literal `provides` entry —
        // provides_capability checks it via `pkg.nevra.name == base`
        // directly rather than through the general provides scan, so the
        // index needs the same separate lookup keyed on the bare `base`
        // name.
        if let Some((base, want_arch)) = isa {
            if let Some(idxs) = self.by_capability.get(base) {
                for &i in idxs {
                    let pkg = &installed[i];
                    if is_soname_dep {
                        if let Some(want) = requiring_arch {
                            if pkg.nevra.arch != want && pkg.nevra.arch != "noarch" {
                                continue;
                            }
                        }
                    }
                    let arch_ok = pkg.nevra.arch == want_arch || (pkg.nevra.arch != want_arch && is_single_arch_helper(pkg));
                    if pkg.nevra.name == base && arch_ok {
                        let satisfied = match &dep.constraint {
                            None => true,
                            Some(_) => dep.satisfied_by_evr(&pkg.nevra.evr()),
                        };
                        if satisfied {
                            return true;
                        }
                    }
                }
            }
        }

        let Some(idxs) = self.by_capability.get(dep.name.as_str()) else {
            return false;
        };
        idxs.iter().any(|&i| {
            let pkg = &installed[i];
            if is_soname_dep {
                if let Some(want_arch) = requiring_arch {
                    if pkg.nevra.arch != want_arch && pkg.nevra.arch != "noarch" {
                        return false;
                    }
                }
            }
            let self_prov = pkg.provides_self();
            // See the matching comment in `provides_capability` above: an
            // unversioned provide satisfies any versioned Requires too,
            // matching real rpm's dependency comparison.
            pkg.provides.iter().chain(std::iter::once(&self_prov)).filter(|prov| prov.name == dep.name).any(|prov| match (&dep.constraint, &prov.constraint) {
                (None, _) | (Some(_), None) => true,
                (Some(_), Some((_, have))) => dep.satisfied_by_evr(have),
            })
        })
    }
}
