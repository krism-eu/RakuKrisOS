/// rakuos-generate-base-manifest — generate the list of all files in /usr owned
/// by protected packages, at image build time. Run at the end of the
/// Containerfile via a RUN command.
///
/// Consumed solely by rakuos-base-protect's inotify daemon, which refuses to
/// let the overlay shadow/delete any path listed here.
///
/// Protected packages come from /usr/share/rakuos/protected-packages.txt,
/// which is the union of:
///   - base image protected-packages.txt  (shipped by rakuos-base)
///   - DE-specific packages appended by each DE's post-build-overlay.sh
///
/// Only explicitly installed packages belong in that file — not all their deps.
/// This keeps the overlay able to upgrade most packages while protecting the
/// DE shell, kernel, and other core components.
use anyhow::{Result, bail};
use std::collections::HashSet;
use std::fs;
use std::process::{Command, exit};

const MANIFEST_DIR: &str = "/usr/share/rakuos";
const RPM_DBPATH: &str = "/usr/lib/sysimage/rpm";

const KNOWN_ARCHES: &[&str] =
    &["x86_64", "i686", "noarch", "aarch64", "armv7hl", "s390x", "ppc64le"];

const PYTHON_EXCLUDE_PREFIXES: &[&str] = &[
    "python3-dbus.",
    "python3-gobject.",
    "python3-gobject-base.",
    "python3-brotli.",
    "python3-ibus.",
    "python3-pip."
];

fn main() {
    if let Err(e) = run() {
        eprintln!("ERROR: {e}");
        exit(1);
    }
}

fn run() -> Result<()> {
    let manifest_file = format!("{MANIFEST_DIR}/base-manifest.txt");
    let protected_file = format!("{MANIFEST_DIR}/protected-packages.txt");
    let extra_packages_file = format!("{MANIFEST_DIR}/extra-manifest-packages.txt");
    let extra_paths_file = format!("{MANIFEST_DIR}/extra-manifest-paths.txt");

    fs::create_dir_all(MANIFEST_DIR)?;

    let protected_content = fs::read_to_string(&protected_file).unwrap_or_default();
    if protected_content.trim().is_empty() {
        bail!("{protected_file} not found or empty — cannot generate manifest.");
    }
    let protected_pkgs = parse_package_list(&protected_content);

    println!("Querying installed packages to resolve protected package names...");
    let base_packages_sorted = query_all_installed()?;

    let base_packages_set: HashSet<&str> =
        base_packages_sorted.iter().map(String::as_str).collect();

    // ── Python packages (arch-qualified, already confirmed installed) ──────────
    let python_pkgs: Vec<String> = base_packages_sorted
        .iter()
        .filter(|p| p.starts_with("python"))
        .filter(|p| !PYTHON_EXCLUDE_PREFIXES.iter().any(|pre| p.starts_with(pre)))
        .cloned()
        .collect();
    println!(
        "Found {} python packages to add to manifest and excludepkgs.",
        python_pkgs.len()
    );

    // ── Resolve + verify protected packages ─────────────────────────────────────
    let qualified_protected: Vec<String> = protected_pkgs
        .iter()
        .filter_map(|p| resolve_pkg(p, &base_packages_set, &base_packages_sorted))
        .collect();

    // ── Extra packages (optional) ────────────────────────────────────────────────
    let mut extra_pkgs: Vec<String> = Vec::new();
    if let Ok(content) = fs::read_to_string(&extra_packages_file) {
        if !content.trim().is_empty() {
            let extra_raw = parse_package_list(&content);
            extra_pkgs = extra_raw
                .iter()
                .filter_map(|p| resolve_pkg(p, &base_packages_set, &base_packages_sorted))
                .collect();
            println!(
                "Extra manifest packages: {} from {extra_packages_file}",
                extra_pkgs.len()
            );
        }
    }

    // ── Extra raw paths (optional) ───────────────────────────────────────────────
    let mut extra_paths: Vec<String> = Vec::new();
    if let Ok(content) = fs::read_to_string(&extra_paths_file) {
        if !content.trim().is_empty() {
            extra_paths = content
                .lines()
                .map(strip_comment)
                .map(str::trim)
                .filter(|l| l.starts_with("/usr/"))
                .map(String::from)
                .collect();
            println!(
                "Extra manifest paths: {} from {extra_paths_file}",
                extra_paths.len()
            );
        }
    }

    println!(
        "Generating RakuOS base file manifest from {} protected + {} python + {} extra packages + {} extra paths...",
        qualified_protected.len(),
        python_pkgs.len(),
        extra_pkgs.len(),
        extra_paths.len()
    );

    // ── Build manifest from files owned by all protected + python + extra
    //    packages, plus any manually listed paths. ────────────────────────────
    let mut manifest_lines: Vec<String> = Vec::new();
    for pkg in qualified_protected
        .iter()
        .chain(python_pkgs.iter())
        .chain(extra_pkgs.iter())
    {
        manifest_lines.extend(query_pkg_files(pkg));
    }
    manifest_lines.extend(extra_paths.iter().cloned());
    manifest_lines.retain(|l| l.starts_with("/usr/"));
    manifest_lines.sort();
    manifest_lines.dedup();

    fs::write(&manifest_file, manifest_lines.join("\n") + "\n")?;
    println!("Manifest generated: {} files → {manifest_file}", manifest_lines.len());

    println!("RakuOS base manifest generation complete.");
    Ok(())
}

// ── Parsing helpers ──────────────────────────────────────────────────────────

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(idx) => &line[..idx],
        None => line,
    }
}

// Parses a package-list file: strips comments/blanks, splits on whitespace,
// drops empty tokens and package-group entries (@foo).
fn parse_package_list(content: &str) -> Vec<String> {
    content
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .map(strip_comment)
        .flat_map(str::split_whitespace)
        .filter(|t| !t.is_empty() && !t.starts_with('@'))
        .map(String::from)
        .collect()
}

fn has_arch(pkg: &str) -> bool {
    match pkg.rsplit_once('.') {
        Some((_, arch)) => KNOWN_ARCHES.contains(&arch),
        None => false,
    }
}

// Resolves NAME or NAME.ARCH → NAME.ARCH confirmed against the installed
// package DB. Bare names prefer x86_64, then noarch, then the first arch
// found in the DB. Returns None (and warns to stderr) if not installed.
fn resolve_pkg(pkg: &str, base_set: &HashSet<&str>, base_sorted: &[String]) -> Option<String> {
    if has_arch(pkg) {
        if base_set.contains(pkg) {
            return Some(pkg.to_string());
        }
        eprintln!("WARN: {pkg} not installed, skipping from excludepkgs");
        return None;
    }

    for arch in ["x86_64", "noarch", "i686", "aarch64"] {
        let candidate = format!("{pkg}.{arch}");
        if base_set.contains(candidate.as_str()) {
            return Some(candidate);
        }
    }

    let prefix = format!("{pkg}.");
    if let Some(found) = base_sorted.iter().find(|p| p.starts_with(&prefix)) {
        return Some(found.clone());
    }

    eprintln!("WARN: {pkg} not installed in any arch, skipping from excludepkgs");
    None
}

// ── RPM queries ──────────────────────────────────────────────────────────────

fn query_all_installed() -> Result<Vec<String>> {
    let output = Command::new("rpm")
        .args([
            "--dbpath",
            RPM_DBPATH,
            "-qa",
            "--qf",
            "%{NAME}.%{ARCH}\\n",
        ])
        .output()?;
    let mut pkgs: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(String::from)
        .collect();
    pkgs.sort();
    pkgs.dedup();
    Ok(pkgs)
}

fn query_pkg_files(pkg: &str) -> Vec<String> {
    Command::new("rpm")
        .args(["--dbpath", RPM_DBPATH, "-ql", pkg])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}
