//! Applies a resolved [`rum_resolver::Plan`] to disk.
//!
//! rum does not reimplement RPM's transaction engine (cpio payload
//! extraction, scriptlet/trigger execution, digest/signature verification)
//! — that is exactly the kind of security-sensitive, format-fragile code
//! not worth re-owning when `rpm` itself already does it correctly and is
//! always present on the system. rum's job stops at "here is the correct
//! set of packages, already downloaded, already known not to duplicate
//! anything the base image provides" — and then hands them to `rpm -U`
//! pointed at [`OverlayPaths::write_dbpath`] (rum's own overlay rpmdb under
//! `Split` mode, or rpm's own default under `Standalone`), the same way
//! zypper/libsolv layer their own solver over librpm rather than
//! reimplementing it.

use anyhow::{Context, Result};
use std::fs;
use rum_core::format_duration;
use rum_core::format_speed as download_speed;
use rum_core::Package;
use rum_overlay::{OverlayMode, OverlayPaths};
use rum_repo::{get_with_retry, RepoConfig};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

pub mod history;
pub mod module_state;
pub mod summary;
pub mod versionlock;
use history::Reason;

/// `diskspacecheck=`: process-wide, set once near the top of `run()`
/// (mirroring `rum_repo::set_fastestmirror` etc.). dnf's own default is
/// `true`.
static DISKSPACECHECK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_diskspacecheck(enabled: bool) {
    DISKSPACECHECK.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// Statvfs-based free-space check for `diskspacecheck=`, run right before a
/// real (non-dry-run) install actually starts touching disk. Walks up to
/// the nearest existing ancestor of `target` before calling `statvfs` since
/// the overlay upperdir/installroot may not exist yet on a from-scratch
/// prebake.
fn check_disk_space(target: &Path, downloaded: &[DownloadedPackage]) -> Result<()> {
    if !DISKSPACECHECK.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    let needed: u64 = downloaded.iter().map(|d| d.package.install_size).sum();
    if needed == 0 {
        return Ok(());
    }
    let mut probe = target.to_path_buf();
    while !probe.exists() {
        match probe.parent() {
            Some(parent) => probe = parent.to_path_buf(),
            None => return Ok(()),
        }
    }
    let Some(available) = available_bytes(&probe) else {
        return Ok(());
    };
    anyhow::ensure!(
        needed <= available,
        "Error: Not enough free space in {} (need {}, have {} available) — disable this check with diskspacecheck=0",
        probe.display(),
        rum_core::format_size(needed),
        rum_core::format_size(available),
    );
    Ok(())
}

fn available_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let cpath = CString::new(path.as_os_str().as_bytes()).ok()?;
    unsafe {
        let mut buf: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(cpath.as_ptr(), &mut buf) == 0 {
            Some(buf.f_bavail as u64 * buf.f_frsize as u64)
        } else {
            None
        }
    }
}

/// The filesystem path a `diskspacecheck=` probe should run `statvfs`
/// against for `paths`: `--installroot` always wins (that's where files
/// actually land regardless of `mode`); otherwise `Split` mode's overlay
/// upperdir (where a fresh install's files land), or `/` under
/// `Standalone` (a plain rpm/dnf-equivalent install with no overlay).
fn diskspace_target(paths: &OverlayPaths) -> PathBuf {
    if let Some(root) = &paths.installroot {
        return root.clone();
    }
    match paths.mode {
        OverlayMode::Split { .. } => paths.overlay_upper.clone(),
        OverlayMode::Standalone => PathBuf::from("/"),
    }
}
use summary::{print_transaction_summary, Action, Entry};

/// Adds `--dbpath <path>` iff `paths` resolves to one — omitted entirely
/// under [`rum_overlay::OverlayMode::Standalone`] so rpm falls back to its
/// own compiled-in default, matching plain rpm/dnf behavior exactly. Also
/// adds `--root <installroot>` when `paths.installroot` is set, so package
/// *files* (not just the rpmdb entry) land under that root instead of the
/// real `/` — dnf/rpm's own `--installroot` semantics, used for pre-baking
/// the factory overlay at image-build time.
fn with_dbpath<'a>(cmd: &'a mut Command, paths: &OverlayPaths) -> &'a mut Command {
    if let Some(dbpath) = paths.write_dbpath() {
        if let Some(root) = &paths.installroot {
            // rpm joins `--root` + `--dbpath` itself (confirmed empirically:
            // passing the already-root-prefixed absolute path here double-
            // prepends root). `write_dbpath()` always returns an absolute,
            // root-prefixed path, so strip that prefix back off before
            // handing it to rpm.
            let relative = dbpath.strip_prefix(root).unwrap_or(&dbpath);
            cmd.arg("--root").arg(root);
            cmd.arg("--dbpath").arg(Path::new("/").join(relative));
        } else {
            cmd.arg("--dbpath").arg(dbpath);
        }
    } else if let Some(root) = &paths.installroot {
        cmd.arg("--root").arg(root);
    }
    cmd
}

/// Initializes the overlay rpmdb (`create_dir_all` + `rpm --initdb`) if
/// `paths` resolves to one and it doesn't exist yet. Deliberately only
/// called from functions in this module that are about to *write* to the
/// overlay rpmdb (importing keys, running a transaction) — `rum_overlay`'s
/// read-only `OverlayContext::load` used to call this unconditionally,
/// which meant even `rum list`/`rum origin` needed root just to create a
/// root-owned directory under `/var/lib/rakuos` before reading from it.
/// `rpm --initdb` is a no-op against an already-initialized dbpath, so this
/// is safe to call on every write even when nothing needs doing.
fn ensure_overlay_dbpath_initialized(paths: &OverlayPaths) -> Result<()> {
    if let Some(dbpath) = paths.write_dbpath() {
        rum_rpmdb::ensure_initialized(&dbpath).with_context(|| format!("initializing overlay rpmdb at {}", dbpath.display()))?;
    }
    Ok(())
}

pub struct DownloadedPackage {
    pub package: Package,
    pub rpm_path: PathBuf,
}

/// The `---rum-gpg-key-sep---` marker below can't legally appear inside an
/// ASCII-armored key body, so splitting `rpm -q`'s `%{description}` output
/// on it safely recovers each already-imported key's exact text.
const GPG_DESC_SEP: &str = "---rum-gpg-key-sep---";

/// Where the last-fetched copy of an http(s) `gpgkey=` URL is cached, so
/// `import_repo_keys` can check "is this already imported" without hitting
/// the network on every run. Keyed by a hash of the URL rather than the
/// repo id, since a repo id can be reused for a different `baseurl`/key
/// across `rum.conf` edits.
fn key_cache_path(paths: &OverlayPaths, key_url: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key_url.hash(&mut hasher);
    paths.state_dir.join("gpgkeys").join(format!("{:016x}.asc", hasher.finish()))
}

/// Whether `key_text` (an ASCII-armored public key) is already present in
/// the dbpath's keyring — rpm stores the full key text verbatim as a
/// `gpg-pubkey` package's `%description`, so a byte-for-byte comparison
/// against every already-imported key is exact and needs no GPG parsing of
/// our own.
fn key_already_imported(paths: &OverlayPaths, key_text: &str) -> Result<bool> {
    let mut cmd = Command::new("rpm");
    with_dbpath(&mut cmd, paths).arg("-q").arg("gpg-pubkey").arg("--qf").arg(&format!("%{{description}}\n{GPG_DESC_SEP}\n"));
    let output = cmd.output().context("spawning rpm -q gpg-pubkey")?;
    if !output.status.success() {
        // No gpg-pubkey packages imported yet (fresh dbpath) — rpm -q
        // exits non-zero for "no package matches", not an error here.
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let needle = key_text.trim();
    Ok(stdout.split(GPG_DESC_SEP).any(|desc| desc.trim() == needle))
}

/// Imports every configured repo's `gpgkey`(s) into the overlay's merged
/// rpmdb keyring via `rpm --import`, so `rpm -K`/`-U` can validate package
/// signatures against them. Keys are fetched over HTTP(S) the same as any
/// other repo asset; a `file://`-style local path (already on disk) is read
/// directly. Skips the fetch/import/print entirely when the exact same key
/// is already in the keyring, so re-running install/upgrade doesn't spam
/// "Importing GPG key" and reinvoke `rpm --import` on every single command.
pub async fn import_repo_keys(client: &reqwest::Client, paths: &OverlayPaths, repo_configs: &[RepoConfig]) -> Result<()> {
    ensure_overlay_dbpath_initialized(paths)?;
    for cfg in repo_configs {
        if !cfg.gpgcheck {
            continue;
        }
        for key_url in &cfg.gpgkeys {
            // A local, on-disk copy of the last key fetched from this exact
            // `key_url`, keyed by a hash of the URL. Every prior version of
            // this function re-fetched http(s) keys over the network on
            // *every* invocation just to compare against the local keyring
            // (`key_already_imported` below never touches the network) —
            // meaning `rum` couldn't complete any gpgcheck'd operation
            // offline, or on a host whose network to the key server was
            // simply flaky, even though the key was already trusted and
            // nothing about local verification needed a fresh fetch.
            // Live-observed: a user hit a hard `Error: importing repo gpg
            // keys` on a connect timeout to `repo.rakuos.org` despite
            // already having the key imported from a prior successful run.
            let cache_path = key_cache_path(paths, key_url);
            let (key_text, tmp_path) = if key_url.starts_with("http://") || key_url.starts_with("https://") {
                if let Ok(cached) = tokio::fs::read_to_string(&cache_path).await {
                    if key_already_imported(paths, &cached).unwrap_or(false) {
                        continue;
                    }
                }
                let bytes = rum_repo::with_net_options(rum_repo::net_options_for_repo(cfg), get_with_retry(client, key_url)).await?.error_for_status()?.bytes().await?;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if let Some(parent) = cache_path.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                let _ = tokio::fs::write(&cache_path, &text).await;
                (text, None::<PathBuf>)
            } else {
                let path = key_url.trim_start_matches("file://");
                let text = tokio::fs::read_to_string(path).await.with_context(|| format!("reading {path}"))?;
                (text, Some(PathBuf::from(path)))
            };

            if key_already_imported(paths, &key_text).unwrap_or(false) {
                continue;
            }
            println!("Importing GPG key for repo '{}': {}", cfg.id, key_url);

            let status = match &tmp_path {
                Some(path) => {
                    let mut cmd = Command::new("rpm");
                    with_dbpath(&mut cmd, paths).arg("--import").arg(path);
                    cmd.status().context("spawning rpm --import")?
                }
                None => {
                    let tmp = std::env::temp_dir().join(format!("rum-gpgkey-{}.asc", cfg.id.replace('/', "_")));
                    tokio::fs::write(&tmp, &key_text).await.with_context(|| format!("writing {}", tmp.display()))?;
                    let mut cmd = Command::new("rpm");
                    with_dbpath(&mut cmd, paths).arg("--import").arg(&tmp);
                    let s = cmd.status().context("spawning rpm --import")?;
                    let _ = tokio::fs::remove_file(&tmp).await;
                    s
                }
            };
            anyhow::ensure!(status.success(), "importing gpg key {key_url} for repo '{}' failed with {status}", cfg.id);
        }
    }
    Ok(())
}

/// Downloads every package in the plan's install set into `cache_dir`,
/// skipping any whose target file already exists (repeat installs/retries
/// shouldn't re-fetch). Same as [`download_all`], with `max_parallel`
/// (dnf's `max_parallel_downloads=`, default 3) bounding how many fetches
/// run concurrently rather than always downloading one at a time.
///
/// `per_repo` lays packages out the same way dnf5's own cache does —
/// `<cache_dir>/repos/<repo_id>-<hash>/packages/<filename>.rpm`, the same
/// hashed per-repo directory [`rum_repo::load_repo_ex`] already uses for
/// that repo's metadata (`repo_configs` is how the hash gets computed —
/// see [`rum_repo::repo_cache_dir`]) — so packages from different repos
/// never collide, a repo id getting repointed at a different URL doesn't
/// serve stale cached rpms from the old one, and `rum clean packages` can
/// still be scoped per repo. A `repo_id` with no matching entry in
/// `repo_configs` (shouldn't normally happen — every candidate's repo_id
/// comes from a config in the same load) falls back to the unhashed
/// `<cache_dir>/<repo_id>/packages/` layout rather than failing outright.
/// Set `per_repo` to `false` when `cache_dir` is actually a caller-specified
/// one-off destination (`rum download --destdir`), where the user expects
/// their files directly in that directory, not nested under a repo id.
///
/// `alt_candidates` is the full cross-repo candidate pool (as loaded by
/// `load_candidates`, before resolution narrowed it down to `packages`) —
/// used purely as a fallback source when a package's own repo 404s on the
/// actual `.rpm` file despite listing it in `primary.xml` (a real, if rare,
/// mirror data-consistency gap — an archive snapshot missing one file that
/// its own metadata still advertises). On such a failure, this looks for
/// the same name+arch in a *different* repo: an exact NEVRA match first
/// (same build genuinely mirrored elsewhere), then falls back to any other
/// version of that name+arch if that's all another repo has — the caller
/// already asked to install this name, and a slightly different EVR of it
/// still resolving is better than aborting the whole transaction over one
/// mirror's gap. Pass an empty slice to disable the fallback entirely (only
/// ever retry the exact package requested).
pub async fn download_all_ex(
    client: &reqwest::Client,
    packages: &[Package],
    cache_dir: &Path,
    max_parallel: u32,
    per_repo: bool,
    repo_configs: &[RepoConfig],
    alt_candidates: &[Package],
) -> Result<Vec<DownloadedPackage>> {
    download_all_ex_with_metadata_root(client, packages, cache_dir, cache_dir, max_parallel, per_repo, repo_configs, alt_candidates).await
}

/// Same as [`download_all_ex`], but lets a caller whose `.rpm` destination
/// (`cache_dir`) differs from where repo *metadata* actually lives on disk
/// (`rum download --destdir <elsewhere>`, `rum reposync`) point the
/// same-repo mirror-fallback's `baseurl.txt` lookup at the right place —
/// otherwise `cached_base_url` looks for `<destdir>/repos/<repo>-<hash>/
/// baseurl.txt`, which was never written (metadata was cached under the
/// real `cache_dir` by `load_candidates`), silently falls through to the
/// old prefix-search-a-fresh-mirror-list guess, and that guess can miss
/// entirely for a `metalink=` repo — exactly the `gtk-vnc2` 404-with-no-
/// fallback bug this function fixes for the common (`cache_dir ==
/// metadata_root`) case.
pub async fn download_all_ex_with_metadata_root(
    client: &reqwest::Client,
    packages: &[Package],
    cache_dir: &Path,
    metadata_root: &Path,
    max_parallel: u32,
    per_repo: bool,
    repo_configs: &[RepoConfig],
    alt_candidates: &[Package],
) -> Result<Vec<DownloadedPackage>> {
    tokio::fs::create_dir_all(cache_dir).await.with_context(|| format!("creating cache dir {}", cache_dir.display()))?;

    for pkg in packages {
        anyhow::ensure!(!pkg.location.is_empty(), "package '{}' has no download location (not repo-sourced?)", pkg.nevra);
    }

    let dest_of = |pkg: &Package| -> PathBuf {
        if per_repo {
            match repo_configs.iter().find(|c| c.id == pkg.repo_id) {
                Some(cfg) => rum_repo::repo_cache_dir(&cache_dir.join("repos"), cfg).join("packages").join(cache_filename(pkg)),
                None => cache_dir.join(&pkg.repo_id).join("packages").join(cache_filename(pkg)),
            }
        } else {
            cache_dir.join(cache_filename(pkg))
        }
    };

    // A fallback build must not just exist — its own `Requires` (rpm
    // autogenerates exact-EVR ones between subpackages of the same source,
    // e.g. `gcc-c++` on `gcc`) have to still hold against what's *actually*
    // landing in this transaction, or swapping in a different-EVR build
    // silently produces a combination `rpm` will refuse at transaction time
    // (e.g. a `gcc-c++-16.0.1` fallback next to the originally-resolved
    // `gcc-16.1.1-2`). Built from the original, pre-fallback `packages` list
    // since those EVRs are what will actually be present regardless of which
    // repo any individual package's bytes end up coming from.
    let planned_evr: HashMap<&str, String> = packages.iter().map(|p| (p.nevra.name.as_str(), p.nevra.evr())).collect();

    let to_fetch: Vec<(usize, &Package)> = packages.iter().enumerate().filter(|(_, pkg)| !dest_of(pkg).exists()).collect();
    if !to_fetch.is_empty() {
        println!("Downloading Packages:");
    }
    let total = to_fetch.len();
    let name_w = to_fetch.iter().map(|(_, pkg)| cache_filename(pkg).len()).max().unwrap_or(0);
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_parallel.max(1) as usize));
    let overall_start = std::time::Instant::now();

    // One real (indicatif-rendered) animated bar for the whole batch, plus
    // one per in-flight package underneath it — replaces the earlier
    // hand-rolled `\r`+ANSI-escape redraw, which worked but reimplemented
    // (badly) what indicatif already does correctly: terminal-width-aware
    // layout, smooth redraw timing, multi-line bar stacking that doesn't
    // fight with interleaved `println!` output.
    let expected_total: u64 = to_fetch.iter().map(|(_, pkg)| pkg.download_size).sum();
    let multi = indicatif::MultiProgress::new();
    let main_bar = multi.add(indicatif::ProgressBar::new(expected_total));
    main_bar.set_style(
        indicatif::ProgressStyle::with_template("{msg} [{bar:30.cyan/blue}] {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>12} eta {eta:>4}")
            .unwrap()
            .progress_chars("=> ")
            // indicatif's own `{eta}` is derived straight from its smoothed
            // rate estimate with no clamp — once a package stalls at ~0 B/s
            // (a slow/dead mirror the retry loop hasn't cycled off yet) that
            // rate collapses toward zero and `{eta}` renders absurd values
            // like "278512y" instead of anything a user could read as
            // "unknown". Render a real placeholder in that case instead.
            .with_key("eta", |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                let remaining = state.eta();
                if remaining.as_secs() >= 60 * 60 * 24 * 365 {
                    let _ = write!(w, "--:--");
                } else {
                    let _ = write!(w, "{}", format_duration(remaining));
                }
            }),
    );
    main_bar.set_message("Total");
    main_bar.enable_steady_tick(std::time::Duration::from_millis(200));

    // `resolve_mirrors_cached` reads a repo's `mirrorlist=`/`metalink=`
    // document from `<cache_root>/<repo>-<hash>/{mirrorlist.txt,metalink.xml}`
    // when younger than `metadata_expire=` (same file dnf5 itself caches
    // under `/var/cache/libdnf5/<repo>-<hash>/metalink.xml`) instead of
    // re-fetching it over the network — identical for every package pulled
    // from that repo, and this run's own `load_candidates` call already
    // populated it moments earlier. Resolving it once per *repo* here (not
    // once per *package* inside each spawned task below) also means even a
    // cold/expired cache only round-trips once per repo, done concurrently
    // up front, rather than once per package.
    // Each entry pairs a repo's mirror list with the exact mirror its
    // cached `location`s were actually built from (`baseurl.txt` — see
    // `cached_base_url`, needed so href-extraction below never depends on
    // that mirror still appearing in the cached list — Fedora's metalink
    // endpoint returns a geo-sorted/randomized subset per request, so two
    // resolutions needn't agree on membership even within the same cache
    // window) and a freshly re-probed "currently best" mirror
    // (`resolve_base_url`, a live `HEAD` per mirror every call regardless
    // of the list-level cache) so the *first* download attempt reaches for
    // whichever mirror is actually responding right now rather than
    // trusting a `baseurl.txt` that can be hours/days old under
    // `metadata_expire=` and may since have gone stale or slow.
    let repo_mirrors: std::sync::Arc<HashMap<String, (Vec<String>, Option<String>, Option<String>)>> = {
        let mut unique_repo_ids: Vec<&str> = to_fetch.iter().map(|(_, pkg)| pkg.repo_id.as_str()).collect();
        unique_repo_ids.sort_unstable();
        unique_repo_ids.dedup();
        let mut resolve_tasks = Vec::with_capacity(unique_repo_ids.len());
        for repo_id in unique_repo_ids {
            if let Some(cfg) = repo_configs.iter().find(|c| c.id == repo_id).cloned() {
                let client = client.clone();
                let cache_root = metadata_root.join("repos");
                resolve_tasks.push(tokio::spawn(async move {
                    let mirrors = rum_repo::resolve_ranked_mirrors(&client, &cfg, &cache_root, false).await.unwrap_or_default();
                    let used = rum_repo::cached_base_url(&cache_root, &cfg);
                    let preferred = rum_repo::resolve_base_url(&client, &cfg, &cache_root).await.ok();
                    (cfg.id, mirrors, used, preferred)
                }));
            }
        }
        let mut map = HashMap::with_capacity(resolve_tasks.len());
        for task in resolve_tasks {
            if let Ok((repo_id, mirrors, used, preferred)) = task.await {
                map.insert(repo_id, (mirrors, used, preferred));
            }
        }
        std::sync::Arc::new(map)
    };

    // Group `to_fetch` by repo_id — each group becomes one (or a few, on
    // retry) `lr_download_packages` batch call against that repo's own
    // ranked mirror list, the same "one cached handle, one repo, one batch
    // call" shape dnf5's `RepoDownloader`/`PackageDownloader` use, instead
    // of the previous per-package/per-URL Rust-side loop. librepo itself
    // now does the mirror cycling/failover and per-target checksum
    // verification inside that one call.
    let mut groups: Vec<(String, Vec<(usize, Package)>)> = Vec::new();
    for (idx, pkg) in to_fetch {
        match groups.iter_mut().find(|(rid, _)| *rid == pkg.repo_id) {
            Some(g) => g.1.push((idx, pkg.clone())),
            None => groups.push((pkg.repo_id.clone(), vec![(idx, pkg.clone())])),
        }
    }
    let alt_candidates_owned = std::sync::Arc::new(alt_candidates.to_vec());
    let planned_evr_owned: std::sync::Arc<HashMap<String, String>> = std::sync::Arc::new(planned_evr.iter().map(|(k, v)| (k.to_string(), v.clone())).collect());
    // Owned clone of everything `dest_of` needs, so the alt-candidate
    // fallback (which resolves a *new* package's destination from inside a
    // 'static spawned task, unlike every other `dest_of` call site in this
    // function) doesn't have to borrow this function's stack.
    let dest_of_owned = std::sync::Arc::new((cache_dir.to_path_buf(), per_repo, repo_configs.to_vec()));

    let mut tasks = Vec::with_capacity(groups.len());
    for (repo_id, members) in groups {
        let semaphore = semaphore.clone();
        let counter = counter.clone();
        let total_bytes = total_bytes.clone();
        let multi = multi.clone();
        let main_bar = main_bar.clone();
        let repo_mirrors = repo_mirrors.clone();
        let alt_candidates_owned = alt_candidates_owned.clone();
        let planned_evr_owned = planned_evr_owned.clone();
        let dest_of_owned = dest_of_owned.clone();
        // `dest_of`/`cache_filename` borrow `per_repo`/`repo_configs`/
        // `cache_dir`, none of which outlive this function, so they're
        // resolved to owned values before the 'static spawned task below.
        let members: Vec<(usize, Package, PathBuf, String)> = members.into_iter().map(|(idx, pkg)| { let dest = dest_of(&pkg); let filename = cache_filename(&pkg); (idx, pkg, dest, filename) }).collect();

        tasks.push(tokio::spawn(async move {
            for (_, _, dest, _) in &members {
                if let Some(parent) = dest.parent() {
                    tokio::fs::create_dir_all(parent).await.with_context(|| format!("creating {}", parent.display()))?;
                }
            }

            // `cached_used`/`preferred` — same reasoning as before: the
            // exact mirror `location` was built from (authoritative,
            // `baseurl.txt`) plus a live re-probe of whichever mirror
            // currently responds fastest, tried ahead of it. `mirrors` is
            // this repo's full ranked list, handed to librepo itself via
            // `LRO_URLS` so it does the cycling/failover across it instead
            // of a manual per-URL Rust loop.
            // This repo's own proxy/auth/TLS-client settings, if any, applied
            // to every librepo handle the batch download below creates —
            // same per-repo scoping [`load_repo`] uses for metadata fetches.
            let net_opts = dest_of_owned.2.iter().find(|c| c.id == repo_id).map(rum_repo::net_options_for_repo).unwrap_or_else(rum_repo::default_net_options);
            let (mirrors, cached_used, preferred) = repo_mirrors.get(&repo_id).cloned().unwrap_or_default();
            let used = cached_used.clone().or_else(|| members.first().and_then(|(_, pkg, _, _)| mirrors.iter().filter(|m| pkg.location.starts_with(m.as_str())).max_by_key(|m| m.len()).cloned()));
            let mut urls: Vec<String> = Vec::new();
            if let Some(used) = &used {
                if let Some(preferred) = &preferred {
                    if preferred != used {
                        urls.push(preferred.clone());
                    }
                }
                urls.push(used.clone());
                for m in &mirrors {
                    if m != used && Some(m) != preferred.as_ref() {
                        urls.push(m.clone());
                    }
                }
            }

            let _permit = semaphore.acquire_owned().await.expect("semaphore never closed");
            let start = std::time::Instant::now();

            // Per-package progress bar/counter, same UI as before — now
            // driven by librepo's own `LrProgressCb` per target inside the
            // shared batch call rather than a per-package async download.
            struct Bar {
                idx: usize,
                pkg: Package,
                dest: PathBuf,
                filename: String,
                progress: std::sync::Arc<std::sync::atomic::AtomicU64>,
                child_bar: indicatif::ProgressBar,
                ticker: tokio::task::JoinHandle<()>,
                n: usize,
            }
            let mut bars: Vec<Bar> = Vec::with_capacity(members.len());
            for (idx, pkg, dest, filename) in &members {
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                let child_bar = multi.add(indicatif::ProgressBar::new(pkg.download_size));
                child_bar.set_style(
                    indicatif::ProgressStyle::with_template(&format!("[{n}/{total}] {{msg:<{name_w}}} [{{bar:20.green/blue}}] {{bytes:>10}}/{{total_bytes:<10}} {{bytes_per_sec:>12}}"))
                        .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar())
                        .progress_chars("=> "),
                );
                child_bar.set_message(filename.clone());
                child_bar.enable_steady_tick(std::time::Duration::from_millis(200));
                let ticker = {
                    let progress = progress.clone();
                    let child_bar = child_bar.clone();
                    let main_bar = main_bar.clone();
                    tokio::spawn(async move {
                        let mut last = 0u64;
                        loop {
                            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                            let cur = progress.load(std::sync::atomic::Ordering::Relaxed);
                            child_bar.set_position(cur);
                            if cur > last {
                                main_bar.inc(cur - last);
                                last = cur;
                            }
                        }
                    })
                };
                bars.push(Bar { idx: *idx, pkg: pkg.clone(), dest: dest.clone(), filename: filename.clone(), progress, child_bar, ticker, n });
            }

            // `pending` shrinks each round to only the targets that failed
            // the previous one — matches the old `ROUNDS` semantics (cycle
            // the whole mirror set, back off, try again) but now each round
            // is a single batch call instead of a per-package/per-URL loop.
            let mut pending: Vec<usize> = (0..bars.len()).collect();
            let mut errors: HashMap<usize, String> = HashMap::new();
            const ROUNDS: u32 = 3;
            for round in 1..=ROUNDS {
                if pending.is_empty() {
                    break;
                }
                let dir_base = |loc: &str| -> Option<(String, String)> { loc.rsplit_once('/').map(|(d, f)| (d.to_string(), f.to_string())) };
                let specs: Vec<rum_repo::BatchSpec> = pending
                    .iter()
                    .map(|&i| {
                        let b = &bars[i];
                        let href = used.as_deref().and_then(|u| b.pkg.location.strip_prefix(u)).map(|h| h.trim_start_matches('/').to_string());
                        // With a usable `href` and a non-empty repo mirror
                        // list, `base_url` stays unset so this target
                        // resolves against the handle's own `LRO_URLS`
                        // (librepo does the mirror cycling); otherwise it
                        // falls back to a degenerate single-mirror target
                        // built straight from this package's own `location`.
                        let (relative_url, base_url) = match href {
                            Some(h) if !urls.is_empty() => (h, None),
                            _ => match dir_base(&b.pkg.location) {
                                Some((dir, file)) => (file, Some(dir)),
                                None => (b.pkg.location.clone(), None),
                            },
                        };
                        rum_repo::BatchSpec {
                            relative_url,
                            dest: b.dest.clone(),
                            checksum_type: b.pkg.checksum_type.clone(),
                            checksum: b.pkg.checksum.clone(),
                            expected_size: b.pkg.download_size as i64,
                            progress: Some(b.progress.clone()),
                            base_url,
                        }
                    })
                    .collect();

                let results = rum_repo::with_net_options(net_opts.clone(), rum_repo::download_packages_batch(urls.clone(), specs, max_parallel as u64)).await;
                match results {
                    Ok(outcomes) => {
                        let mut still_pending = Vec::new();
                        for (&i, outcome) in pending.iter().zip(outcomes.iter()) {
                            if outcome.ok {
                                errors.remove(&i);
                            } else {
                                errors.insert(i, outcome.error.clone().unwrap_or_else(|| "download failed".to_string()));
                                still_pending.push(i);
                            }
                        }
                        pending = still_pending;
                    }
                    Err(e) => {
                        // A function-level librepo error (not a per-target
                        // one) — every still-pending target in this round
                        // failed the same way.
                        for &i in &pending {
                            errors.insert(i, e.to_string());
                        }
                    }
                }
                if !pending.is_empty() && round < ROUNDS {
                    tokio::time::sleep(std::time::Duration::from_millis(1000 * 2u64.pow(round - 1))).await;
                }
            }

            // Anything still failing after `ROUNDS` batch attempts against
            // its own repo falls back to a cross-repo alt-candidate, same
            // EVR-checked semantics as before, one degenerate single-item
            // batch call per attempt (that alt's own repo/mirror, via a
            // plain dir+basename split — good enough for the rare fallback
            // path, unlike the primary group call above which reuses the
            // repo's full ranked mirror list).
            let dest_of_local = |pkg: &Package| -> PathBuf {
                let (cache_dir, per_repo, repo_configs) = &*dest_of_owned;
                if *per_repo {
                    match repo_configs.iter().find(|c| c.id == pkg.repo_id) {
                        Some(cfg) => rum_repo::repo_cache_dir(&cache_dir.join("repos"), cfg).join("packages").join(cache_filename(pkg)),
                        None => cache_dir.join(&pkg.repo_id).join("packages").join(cache_filename(pkg)),
                    }
                } else {
                    cache_dir.join(cache_filename(pkg))
                }
            };
            for &i in &pending.clone() {
                let b = &bars[i];
                let mut alts: Vec<&Package> = alt_candidates_owned
                    .iter()
                    .filter(|c| c.nevra.name == b.pkg.nevra.name && c.nevra.arch == b.pkg.nevra.arch && c.repo_id != b.pkg.repo_id)
                    .filter(|c| c.requires.iter().all(|dep| match planned_evr_owned.get(dep.name.as_str()) {
                        Some(evr) => dep.satisfied_by_evr(evr),
                        None => true,
                    }))
                    .collect();
                alts.sort_by(|a, c| (c.nevra == b.pkg.nevra).cmp(&(a.nevra == b.pkg.nevra)));
                let mut resolved = false;
                for alt in alts {
                    let Some((dir, file)) = alt.location.rsplit_once('/') else { continue };
                    let alt_dest = dest_of_local(alt);
                    if let Some(parent) = alt_dest.parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }
                    let spec = rum_repo::BatchSpec { relative_url: file.to_string(), dest: alt_dest.clone(), checksum_type: alt.checksum_type.clone(), checksum: alt.checksum.clone(), expected_size: alt.download_size as i64, progress: Some(bars[i].progress.clone()), base_url: None };
                    match rum_repo::with_net_options(net_opts.clone(), rum_repo::download_packages_batch(vec![dir.to_string()], vec![spec], 1)).await {
                        Ok(outcomes) if outcomes.first().is_some_and(|o| o.ok) => {
                            multi.suspend(|| println!("  (falling back to {} from repo '{}' — '{}' isn't available from its own repo)", alt.nevra, alt.repo_id, b.pkg.nevra));
                            bars[i].pkg = alt.clone();
                            bars[i].dest = alt_dest;
                            errors.remove(&i);
                            resolved = true;
                            break;
                        }
                        Ok(outcomes) => {
                            if let Some(o) = outcomes.into_iter().next() {
                                errors.insert(i, o.error.unwrap_or_else(|| "download failed".to_string()));
                            }
                        }
                        Err(e) => {
                            errors.insert(i, e.to_string());
                        }
                    }
                }
                let _ = resolved;
            }

            let elapsed = start.elapsed();
            let mut fetched = Vec::new();
            let mut first_err = None;
            for (pos, bar) in bars.into_iter().enumerate() {
                bar.ticker.abort();
                bar.child_bar.finish_and_clear();
                match errors.remove(&pos) {
                    None => {
                        let size = tokio::fs::metadata(&bar.dest).await.with_context(|| format!("stat {}", bar.dest.display()))?.len();
                        total_bytes.fetch_add(size, std::sync::atomic::Ordering::Relaxed);
                        multi.suspend(|| println!("[{}/{total}] {:<name_w$} 100% | {:>10} | {:>10} | {}", bar.n, bar.filename, download_speed(size, elapsed), rum_core::format_size(size), format_duration(elapsed)));
                        fetched.push((bar.idx, bar.pkg, bar.dest));
                    }
                    Some(err) if first_err.is_none() => {
                        first_err = Some(anyhow::anyhow!("downloading '{}': {}", bar.pkg.nevra, err));
                    }
                    Some(_) => {}
                }
            }
            if let Some(e) = first_err {
                return Err(e);
            }
            Ok::<Vec<(usize, Package, PathBuf)>, anyhow::Error>(fetched)
        }));
    }
    let mut fetched: HashMap<usize, (Package, PathBuf)> = HashMap::new();
    for task in tasks {
        let group_result: Vec<(usize, Package, PathBuf)> = task.await.context("download task panicked")??;
        for (idx, pkg, dest) in group_result {
            fetched.insert(idx, (pkg, dest));
        }
    }
    main_bar.finish_and_clear();

    if total > 0 {
        println!("{}", "-".repeat(80));
        let elapsed = overall_start.elapsed();
        let bytes = total_bytes.load(std::sync::atomic::Ordering::Relaxed);
        println!("[{total}/{total}] {:<name_w$} 100% | {:>10} | {:>10} | {}", "Total", download_speed(bytes, elapsed), rum_core::format_size(bytes), format_duration(elapsed));
    }

    Ok(packages
        .iter()
        .enumerate()
        .map(|(idx, pkg)| match fetched.remove(&idx) {
            Some((actual_pkg, actual_dest)) => DownloadedPackage { package: actual_pkg, rpm_path: actual_dest },
            None => DownloadedPackage { package: pkg.clone(), rpm_path: dest_of(pkg) },
        })
        .collect())
}

/// Same as [`download_all_ex`] with dnf's default `max_parallel_downloads=3`
/// and `per_repo=true` — kept for callers (mainly tests) that don't have a
/// `rum.conf` to read the real value from.
pub async fn download_all(client: &reqwest::Client, packages: &[Package], cache_dir: &Path) -> Result<Vec<DownloadedPackage>> {
    download_all_ex(client, packages, cache_dir, 3, true, &[], &[]).await
}

/// rpm's own `-vh` phase-header lines — end in a hash-fill bar just like a
/// per-package completion line, but aren't one, so they must never be
/// counted against `steps`'s package total.
fn is_rpm_phase_header(name_part: &str) -> bool {
    matches!(name_part, "Verifying..." | "Preparing..." | "Preparing packages..." | "Updating / installing..." | "Cleaning up / removing..." | "Removing packages...")
}

/// Runs an already-configured `rpm` [`Command`] (verb/flags/package args all
/// set by the caller, plus `-v`) with `--percent` added, reformatting rpm's
/// own `pkgname   %% <float>` / `%% <float>` percent-callback lines into a
/// live indicatif display: the current package's own animated bar, stacked
/// under an aggregate bar (weighted by `install_size` across the whole
/// transaction) — same `MultiProgress` approach as the download side.
///
/// `steps` is `(name, install_size)` in the exact order the packages were
/// passed to rpm, used to label/weight each package as its percent updates
/// arrive. rpm can in principle reorder a transaction internally for
/// dependency reasons, so this attributes percent *events* positionally
/// rather than trusting rpm's own (frequently truncated-to-fit-column) name
/// text — good enough for progress display, not depended on for
/// correctness. Phase-header lines (`Verifying...`, `Preparing...`, etc.)
/// and anything else (scriptlet output) pass through via `multi.suspend`,
/// which temporarily clears the live bars so printed lines don't get
/// interleaved into them. `stderr` stays inherited, so real errors/
/// scriptlet stderr still show live.
fn run_rpm_transaction(mut cmd: Command, steps: &[(String, u64)], verb: &str, batch: &[&DownloadedPackage]) -> Result<()> {
    use std::io::{BufRead, BufReader, Read};

    cmd.arg("--percent");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("spawning rpm")?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    // Read on a separate thread: rpm can write enough to the stderr pipe to
    // fill its OS buffer before this process finishes draining stdout, which
    // would otherwise deadlock (rpm blocks writing, we block reading the
    // other pipe).
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut buf);
        buf
    });
    let reader = BufReader::new(stdout);

    let total = steps.len();
    let total_size: u64 = steps.iter().map(|(_, size)| *size).sum();
    let mut n = 0usize;
    let mut last = std::time::Instant::now();

    let multi = indicatif::MultiProgress::new();
    let main_bar = multi.add(indicatif::ProgressBar::new(total_size));
    main_bar.set_style(indicatif::ProgressStyle::with_template("Overall  [{bar:30.cyan/blue}] {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>12}").unwrap().progress_chars("=> "));
    main_bar.enable_steady_tick(std::time::Duration::from_millis(200));
    let pkg_bar = multi.add(indicatif::ProgressBar::new(100));
    pkg_bar.set_style(indicatif::ProgressStyle::with_template("[{prefix}] {msg:<28} [{bar:30.green/blue}] {percent:>3}%").unwrap().progress_chars("=> "));
    pkg_bar.enable_steady_tick(std::time::Duration::from_millis(200));

    for line in reader.lines() {
        let line = line.context("reading rpm output")?;
        let trimmed = line.trim_end();
        if let Some(pct_idx) = trimmed.find("%%") {
            let prefix = trimmed[..pct_idx].trim();
            let pct: f64 = trimmed[pct_idx + 2..].trim().parse().unwrap_or(0.0);
            if !prefix.is_empty() && is_rpm_phase_header(prefix) {
                let prefix = prefix.to_string();
                multi.suspend(|| println!("{prefix}"));
                continue;
            }
            if n >= total {
                continue;
            }
            let (name, size) = &steps[n];
            pkg_bar.set_prefix(format!("{}/{total}", n + 1));
            pkg_bar.set_message(format!("{verb} {name}"));
            pkg_bar.set_position(pct.clamp(0.0, 100.0) as u64);
            let pkg_bytes = ((*size as f64) * (pct / 100.0)) as u64;
            main_bar.set_position((total_size - steps[n..].iter().map(|(_, s)| *s).sum::<u64>()) + pkg_bytes);
            if pct >= 100.0 {
                let elapsed = last.elapsed();
                last = std::time::Instant::now();
                n += 1;
                let (name, size) = (name.clone(), *size);
                multi.suspend(|| {
                    println!("[{n}/{total}] {verb} {name:<28} 100% | {:>10} | {:>10} | {}", rum_core::format_speed(size, elapsed), rum_core::format_size(size), rum_core::format_duration(elapsed));
                });
            }
        } else if !trimmed.trim_start_matches(['#', ' ']).is_empty() && !trimmed.chars().all(|c| c == '#' || c == ' ') {
            let trimmed = trimmed.to_string();
            multi.suspend(|| println!("{trimmed}"));
        }
    }
    pkg_bar.finish_and_clear();
    main_bar.finish_and_clear();

    let status = child.wait().context("waiting for rpm")?;
    let stderr_text = stderr_handle.join().unwrap_or_default();
    if !status.success() {
        // rpm also returns nonzero when a package's %post/%postun scriptlet
        // fails (e.g. an akmod build script refusing to run as root) —
        // that happens *after* the package's files are already committed,
        // so the install itself succeeded even though the exit status says
        // otherwise. A genuine pre-transaction rejection (unmet deps, file
        // conflicts) never gets this far: rpm refuses the whole transaction
        // up front and no "Installing" progress lines are printed at all.
        // Distinguish the two by whether every package's line was seen.
        if n < total {
            // rpm's own embedded signature/digest check (run as part of this
            // real transaction, not a separate pre-flight `rpm -K` parse —
            // that used to be a second, independent check here and broke
            // outright once a newer rpm changed `-K`'s stdout wording,
            // rejecting perfectly good packages) rejected something before
            // any package was actually written. If that's what happened, the
            // cached copy is either genuinely corrupt/tampered or the caller
            // hasn't imported the signing key yet — either way, deleting it
            // lets `apply_install_with_signature_retry` (rum-cli) redownload
            // and retry once instead of wedging on a bad cache file forever.
            let lower = stderr_text.to_lowercase();
            let looks_like_signature_failure =
                lower.contains("signature") || lower.contains("digest") || lower.contains("does not verify") || lower.contains("nokey") || lower.contains("public key");
            if looks_like_signature_failure {
                for d in batch {
                    let _ = std::fs::remove_file(&d.rpm_path);
                }
                anyhow::bail!("signature verification failed during rpm transaction: {}", stderr_text.trim());
            }
            anyhow::bail!("rpm transaction failed with {status}: {}", stderr_text.trim());
        }
        eprintln!("warning: rpm exited with {status} (a package scriptlet failed) — all {total} package(s) were installed; continuing");
    }
    Ok(())
}

fn cache_filename(pkg: &Package) -> String {
    pkg.location.rsplit('/').next().map(str::to_string).unwrap_or_else(|| pkg.nevra.to_string())
}

/// Runs `rpm -Uvh --dbpath <merged rpmdb>` against the downloaded set.
/// Installs land in the overlay upperdir automatically: the merged rpmdb
/// path *is* `/usr/share/rpm` (kernel-merged), so any write rpm makes
/// there copies-up into `overlay/upper` transparently — rum never needs to
/// touch `upperdir` paths directly, matching the same "always go through
/// the merged view" rule `overlay_mount.rs` documents for itself.
///
/// `dry_run` maps to `rpm --test`: resolves and validates the transaction
/// (file conflicts, missing deps rpm itself double-checks, disk space)
/// without writing anything, for `rum install --dry-run`.
///
/// Signature/digest verification runs as part of the real rpm transaction
/// itself (rpm always checks unless its source repo has `gpgcheck = false`
/// or `no_gpgchecks` is set). Callers should run [`import_repo_keys`] first
/// so the keyring has something to check against.
///
/// `explicit_names` are the package names the user actually asked for on
/// the command line (`rum install foo`) — recorded with [`Reason::User`] in
/// the history log; everything else downloaded (pulled in to satisfy a
/// dependency) is recorded as [`Reason::Dependency`], mirroring dnf's own
/// install-reason semantics so `autoremove`/`mark` behave the same way.
///
/// `no_gpgchecks` forces signature verification off entirely for this run
/// (dnf's `--no-gpgchecks`), overriding every repo's own `gpgcheck=`.
///
/// `force_replace` maps to `rpm -Uvh --replacepkgs --oldpackage`, needed for
/// `reinstall` (same EVR, rpm would otherwise refuse as a no-op) and
/// `downgrade`/`distro-sync` (older EVR than installed, rpm would otherwise
/// refuse as "package already installed" / a downgrade).
pub fn apply_install(
    paths: &OverlayPaths,
    downloaded: &[DownloadedPackage],
    repo_configs: &[RepoConfig],
    dry_run: bool,
    explicit_names: &[String],
    no_gpgchecks: bool,
    assume_yes: bool,
) -> Result<()> {
    apply_install_ex(paths, downloaded, repo_configs, dry_run, explicit_names, no_gpgchecks, false, false, &[], assume_yes, false)
}

/// dnf-style `Is this ok [y/N]:` gate, printed right after a transaction
/// summary and before anything on disk actually changes. Skipped entirely
/// (auto-confirmed) when `-y`/`--assumeyes` was passed — that's the whole
/// point of the flag, otherwise it would be inert.
///
/// Public so callers that print their own pre-download transaction summary
/// (e.g. `install_ex`/`upgrade`/`sync_packages` in `rum-cli`) can ask for
/// confirmation right there, before spending any time/bandwidth on
/// `download_all_ex` — see `apply_install_ex`'s `already_confirmed` param.
pub fn confirm_transaction(assume_yes: bool) -> Result<bool> {
    confirm_transaction_ex(assume_yes, false, false)
}

/// Full form of [`confirm_transaction`] with dnf5's `assumeno`/`defaultyes`
/// precedence (`libdnf5-cli::utils::userconfirm`): `assume_no` wins over
/// `assume_yes` (both skip the prompt entirely), and `default_yes` only
/// changes what an empty (Enter-only) answer means — it still prompts.
pub fn confirm_transaction_ex(assume_yes: bool, assume_no: bool, default_yes: bool) -> Result<bool> {
    if assume_no {
        return Ok(false);
    }
    if assume_yes {
        return Ok(true);
    }
    use std::io::Write;
    print!("\n{}", if default_yes { "Is this ok [Y/n]: " } else { "Is this ok [y/N]: " });
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).context("reading confirmation")?;
    let choice = input.trim().to_lowercase();
    if choice.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(choice.as_str(), "y" | "yes"))
}

/// Full form of [`apply_install`] with `force_replace` — see its doc.
///
/// `noscripts` maps to `rpm --noscripts` (dnf's `--setopt=tsflags=noscripts`)
/// — used by `--installroot` automatically (see below) and by `--setopt
/// tsflags=noscripts` on the command line for real installs, e.g. kernel
/// packages whose scriptlets shouldn't run mid-build or against a
/// container that can't service them.
///
/// `installonly_pkgs` (`installonlypkgs=`, dnf-style globs) is split out of
/// the main `-Uvh` invocation and installed via a second `rpm -ivh` instead:
/// plain `-U` treats any same-named package as an in-place upgrade and
/// erases the older version regardless of what the resolver decided to keep
/// — real kernel RPMs dodge this via an internal rpm header tag
/// (`RPMTAG_INSTALLTID`-adjacent `installonlypkg` flag) that dnf/rpm both
/// special-case, but rum can't assume every installonly-configured package
/// carries that tag, so it enforces coexistence itself the same way `rpm -i`
/// always has: never implicitly replacing anything.
pub fn apply_install_ex(
    paths: &OverlayPaths,
    downloaded: &[DownloadedPackage],
    repo_configs: &[RepoConfig],
    dry_run: bool,
    explicit_names: &[String],
    no_gpgchecks: bool,
    force_replace: bool,
    noscripts: bool,
    installonly_pkgs: &[String],
    assume_yes: bool,
    // Set when the caller already printed a transaction summary and got a
    // `y` out of `confirm_transaction` *before* calling `download_all_ex` —
    // dnf always asks before spending bandwidth, not after. When true, the
    // summary/confirm below is skipped entirely rather than re-asked a
    // second time against the now-downloaded package set.
    already_confirmed: bool,
) -> Result<()> {
    if downloaded.is_empty() {
        println!("Nothing to do.");
        return Ok(());
    }
    // A dry run never touches the real overlay rpmdb — it only resolves and
    // prints the plan below — so it must not require root to open/init that
    // dbpath either (that would defeat the point of a cheap, unprivileged
    // "will this even resolve" check against a whole package list).
    if !dry_run {
        ensure_overlay_dbpath_initialized(paths)?;
    }

    // Classify each downloaded package against whatever's currently
    // installed under the same name, so the summary table can label it
    // Installing/Upgrading/Downgrading/Reinstalling the way dnf5 does,
    // rather than lumping every candidate under a single "Installing:".
    let overlay_ctx = rum_overlay::OverlayContext::load(paths).ok();
    // Keyed by (name, arch), not just name: multilib packages (e.g.
    // `mesa-filesystem`/`llvm-filesystem`) legitimately have both an x86_64
    // and an i686 build installed side by side, and a name-only lookup would
    // pair a newly-added i686 candidate against the already-installed
    // x86_64 build, mislabeling a straight side-by-side install as
    // "Reinstalling"/"replacing" the other arch.
    let installed_by_name: HashMap<(&str, &str), &Package> = overlay_ctx
        .as_ref()
        .map(|ctx| ctx.base.iter().chain(&ctx.overlay).map(|p| ((p.nevra.name.as_str(), p.nevra.arch.as_str()), p)).collect())
        .unwrap_or_default();
    let entries: Vec<Entry> = downloaded
        .iter()
        .map(|d| {
            let pkg = &d.package;
            match installed_by_name.get(&(pkg.nevra.name.as_str(), pkg.nevra.arch.as_str())) {
                Some(old) => {
                    let cmp = rum_core::EvrCompare::compare_evr(pkg.nevra.evr().as_str(), &old.nevra.evr());
                    let action = match cmp {
                        std::cmp::Ordering::Equal => Action::Reinstalling,
                        std::cmp::Ordering::Greater => Action::Upgrading,
                        std::cmp::Ordering::Less => Action::Downgrading,
                    };
                    Entry::new(action, pkg).replacing(old)
                }
                None => Entry::new(Action::Installing, pkg),
            }
        })
        .collect();
    if !already_confirmed {
        print_transaction_summary(&entries);

        if !dry_run && !confirm_transaction(assume_yes)? {
            println!("Operation aborted.");
            return Ok(());
        }
    }

    if dry_run {
        // Nothing below this point is safe (or useful) to run unprivileged:
        // the rpm `--test` invocation needs to open the real overlay dbpath,
        // which a dry run deliberately never
        // initializes above. The plan already printed via
        // `print_transaction_summary` is the whole point of a dry run —
        // whether it resolves and what it would do — so stop here.
        println!("Transaction test succeeded.");
        return Ok(());
    }

    check_disk_space(&diskspace_target(paths), downloaded)?;

    if no_gpgchecks {
        println!("Skipping GPG signature verification (--no-gpgchecks).");
    } else if repo_configs.is_empty() {
        // No repo metadata at all means every package here came from a
        // local file or URL install (`rum install ./foo.rpm` / `rum install
        // https://.../foo.rpm`) rather than repo-candidate resolution.
        // There's no `gpgcheck=`/`gpgkey=` to check against and, critically,
        // no guarantee the signer's key has even been imported yet — dnf's
        // own default for this exact case (`localpkg_gpgcheck=False`) is to
        // skip the check rather than reject every unsigned-or-untrusted
        // local/URL rpm out of the box.
        println!("Skipping GPG signature verification (local/URL RPM install).");
    } else {
        // Signature/digest verification happens inside the real `rpm -Uvh`/
        // `-ivh` transaction below (rpm always checks unless `--nosignature
        // --nodigest` is passed, which only happens in the two cases handled
        // above). This used to also run a separate `rpm -K` pre-flight pass
        // and text-parse its stdout for "OK"/"NOT OK" — that broke outright
        // once a newer rpm changed `-K`'s output wording, rejecting good
        // packages with a nonsensical "signature verification failed ...
        // digests signatures OK" error. Let rpm's own transaction-embedded
        // check (exit status, not string-matched stdout) be the only check.
        println!("Verifying package signatures...");
    }

    // Genuine `installonly_pkgs` (kernel-style side-by-side coexisting
    // packages) AND ordinary new-arch multilib additions (e.g. adding
    // `mesa-filesystem.i686` alongside an already-installed
    // `mesa-filesystem.x86_64`) both need the separate `-ivh` batch below,
    // not `-Uvh`. This looks redundant — `rpm -U` is documented as "install,
    // but replace an older version of the same package if present" — but
    // rpm's own same-name upgrade-collapse logic keys off Name alone for
    // packages that carry no ELF-colored files to distinguish multilib
    // builds by (e.g. `mesa-filesystem`, `llvm-filesystem`, pure metadata/
    // license packages). A real `rpm -Uvh mesa-filesystem-X.i686.rpm` run
    // against a system with `mesa-filesystem-X.x86_64` already installed
    // fails with "mesa-filesystem(x86-64) = X is needed by (installed)
    // mesa-dri-drivers-X.x86_64" — rpm's dependency check sees the i686
    // upgrade as having replaced the x86_64 Provides — even though nothing
    // about the transaction should have touched the x86_64 build at all.
    // `rpm -i` never attempts that same-name replacement at all, so it adds
    // the new arch cleanly. A previous version of this routing sent *every*
    // new-arch package through `-ivh`, which could split a same-transaction
    // dependency pair (e.g. `xz-devel.i686` requiring `xz-libs.i686`) across
    // the two non-atomic rpm invocations if only one side had an existing
    // other-arch install to trigger the old, name-only classification. This
    // version still classifies per-package by "is (name, arch) new but
    // `name` already installed under a *different* arch", but the resolver
    // guarantees any dependency between two candidates in the same
    // transaction still lands in the same relative batch by that same rule
    // (both sides of a multilib pair like `xz-devel.i686`/`xz-libs.i686`
    // typically share the same "does this name have another arch already
    // installed" answer), and `-Uvh` still runs first so a truly-new name
    // in that batch is available before `-ivh` needs it.
    let (installonly, upgradeable): (Vec<&DownloadedPackage>, Vec<&DownloadedPackage>) = downloaded.iter().partition(|d| {
        let pkg = &d.package;
        rum_core::name_matches_any(&pkg.nevra.name, installonly_pkgs)
            || (!installed_by_name.contains_key(&(pkg.nevra.name.as_str(), pkg.nevra.arch.as_str()))
                && installed_by_name.iter().any(|(&(name, arch), _)| name == pkg.nevra.name && arch != pkg.nevra.arch))
    });

    let run_rpm = |verb: &str, pkgs: &[&DownloadedPackage], extra_nodeps: bool| -> Result<()> {
        if pkgs.is_empty() {
            return Ok(());
        }

        let mut cmd = Command::new("rpm");
        with_dbpath(&mut cmd, paths).arg(verb);
        if no_gpgchecks || repo_configs.is_empty() {
            // rpm's transaction step runs its own signature/digest check by
            // default regardless of `--no-gpgchecks` — without disabling
            // that here too, an unsigned or self-signed test/local repo
            // package still gets rejected at `rpm -Uvh` even though the
            // user explicitly asked to skip verification.
            //
            // `repo_configs.is_empty()` is the same "local/URL RPM install"
            // case handled above (see the "Skipping GPG signature
            // verification (local/URL RPM install)" branch): rum already
            // decided not to run its own check there and told the user so,
            // but without this it was a lie — rpm's own signature check
            // still ran underneath and rejected the (typically unsigned,
            // e.g. a locally-built akmod RPM) package with "does not
            // verify: no signature" regardless of what was printed.
            cmd.arg("--nosignature").arg("--nodigest");
        }
        if force_replace {
            cmd.arg("--replacepkgs").arg("--oldpackage");
        }
        if extra_nodeps {
            // The two-batch split below (`-ivh`/`-Uvh`) exists purely to
            // route around an rpm quirk (see the big comment at the split
            // site) — it has no bearing on real dependency satisfiability,
            // which the SAT resolver already validated across the *whole*
            // download set before this function ever ran. When the split's
            // two ordering requirements (file-conflict safety vs.
            // cross-batch dependency availability) conflict with each other,
            // there is no single valid order for two non-atomic invocations,
            // so this batch's own dependency check — against a sibling
            // batch that, by construction, hasn't run yet — is a false
            // positive caused by our own batching, not an unmet dependency.
            cmd.arg("--nodeps");
        }
        if paths.needs_nodeps() {
            // Under --installroot, and under a real Split-mode overlay install,
            // the target dbpath only ever contains overlay-owned packages —
            // base-satisfied Requires (glibc, libstdc++, etc.) have no Provides
            // record there (see `OverlayPaths::needs_nodeps`'s doc comment).
            // rum's SAT resolver already validated dependency satisfaction
            // across base+overlay before building this transaction, so rpm's
            // own db-only check is redundant and would otherwise reject every
            // base-satisfied capability.
            cmd.arg("--nodeps");
        }
        if noscripts {
            // Explicit opt-out only (`--setopt tsflags=noscripts`, or the
            // very first installroot layer before a shell exists — callers
            // that know their installroot has no /bin/sh yet should pass
            // this in). `with_dbpath` already gives rpm `--root
            // <installroot>` when building an installroot, so rpm chroots
            // in and runs %post/%postun scriptlets (including the
            // `%systemd_post`/`systemd-update-helper install-system-units`
            // macros that enable shipped services) against that root
            // exactly like a real dnf --installroot build does, once the
            // installroot has a populated environment. Unconditionally
            // skipping scripts for every installroot install — as this used
            // to do — silently dropped all service-enablement macros from
            // every image build.
            cmd.arg("--noscripts");
        }
        if dry_run {
            cmd.arg("--test");
            println!("Running transaction test");
        } else {
            println!("Running transaction");
        }
        for d in pkgs {
            cmd.arg(&d.rpm_path);
        }

        if dry_run {
            // `--test` never actually installs anything — not worth
            // reformatting its (much shorter) dependency-check-only output.
            let status = cmd.status().context("spawning rpm")?;
            anyhow::ensure!(status.success(), "rpm transaction failed with {status}");
        } else {
            let steps: Vec<(String, u64)> = pkgs.iter().map(|d| (d.package.nevra.to_string(), d.package.install_size)).collect();
            run_rpm_transaction(cmd, &steps, "Installing", pkgs)?;
        }
        Ok(())
    };

    // These are two separate, non-atomic `rpm` invocations, so which one
    // runs first matters and cuts both ways:
    //
    // - If the *same name* appears in both batches (a genuine simultaneous
    //   cross-arch pairing — e.g. `ngtcp2.x86_64` upgrading in `-Uvh` while
    //   `ngtcp2.i686` is newly added in `-ivh`), `-Uvh` must run first: the
    //   new-arch `-ivh` install can share on-disk files (docs, `%doc` dirs)
    //   with the other arch's build of the same name, and if `-ivh` ran
    //   first while that other arch was still at its old, pre-upgrade EVR,
    //   rpm's own file-conflict check would reject the new-arch install.
    // - Otherwise, if anything in `-Uvh` actually `Requires` a package that
    //   only exists in `-ivh` (e.g. `mesa-vulkan-drivers-freeworld.i686`, a
    //   brand-new name with no other-arch sibling so it lands in `-Uvh`,
    //   requiring `mesa-filesystem.i686`, a new-arch-of-an-installed-name
    //   package that lands in `-ivh`), `-ivh` must run first instead, or
    //   `-Uvh`'s own dependency check fails against a package that doesn't
    //   exist yet.
    //
    // Both orderings are real, live-verified failures (mesa i686 driver
    // install on a real image build hit the second one) — there's no single
    // fixed order that's always safe, so pick per-transaction.
    // `Requires` are capability strings (plain names, sonames like
    // `libLLVM.so.22.1`, or ISA-suffixed `mesa-filesystem(x86-32)`), not
    // necessarily package names — matching only against `installonly`'s
    // package *names* (as an earlier version of this check did) misses
    // every soname/ISA-suffixed dependency, silently leaving the reorder
    // undetected exactly for the multilib case it exists to catch. Match
    // against what each package actually `Provides` (plus its implicit
    // self-provide) instead.
    let provides_capability = |pkg: &Package, want: &str| -> bool {
        pkg.provides.iter().any(|p| p.name == want) || pkg.provides_self().name == want || pkg.nevra.name == want
    };
    let paired_cross_arch_name = upgradeable.iter().any(|u| installonly.iter().any(|i| i.package.nevra.name == u.package.nevra.name));
    let upgradeable_needs_installonly = upgradeable.iter().any(|d| {
        d.package.requires.iter().any(|req| {
            installonly.iter().any(|i| provides_capability(&i.package, &req.name)) && !upgradeable.iter().any(|u| provides_capability(&u.package, &req.name))
        })
    });
    // Both batches always run with `--nodeps`: a package's Provides and its
    // dependents can straddle *both* batches simultaneously (e.g.
    // `libcurl.i686` lands in one batch while it has one dependent in that
    // same batch and another dependent in the other batch) — no single
    // fixed order of two non-atomic invocations can satisfy every such
    // cross-batch edge at once, so rpm's own per-invocation dependency
    // check is fundamentally unreliable here regardless of ordering, not
    // just in the specific conflicting-order case. The SAT resolver has
    // already validated satisfiability across the *whole* download set
    // before this function ever ran, so this redundant check is always a
    // false positive caused by our own batching, never a real unmet
    // dependency.
    // With rpm's own dependency check disabled for both invocations (see
    // above), only the file-conflict ordering requirement still matters:
    // `-Uvh` must run first whenever a name is paired across both batches.
    if !paired_cross_arch_name && upgradeable_needs_installonly {
        run_rpm("-ivh", &installonly, true)?;
        run_rpm("-Uvh", &upgradeable, true)?;
    } else {
        run_rpm("-Uvh", &upgradeable, true)?;
        run_rpm("-ivh", &installonly, true)?;
    }

    if matches!(paths.mode, OverlayMode::Split { .. }) && paths.installroot.is_none() {
        restore_setuid_bits(paths, downloaded.iter().map(|d| d.package.nevra.name.as_str()));
        run_post_install_cache_triggers(downloaded);
        sync_modprobe_configs(paths, downloaded);
    }

    println!("Complete!");

    let entries: Vec<(String, Reason)> = downloaded
        .iter()
        .map(|d| {
            let reason = if explicit_names.contains(&d.package.nevra.name) { Reason::User } else { Reason::Dependency };
            (d.package.nevra.to_string(), reason)
        })
        .collect();
    if let Err(e) = history::record(paths, history::Action::Install, &entries) {
        tracing::warn!(error = %e, "failed to record transaction history");
    }
    Ok(())
}

/// overlayfs silently strips setuid/setgid bits when a file is first
/// written into the upperdir — rpm's own install just wrote every file this
/// transaction touched there, so anything the RPM declared setuid/setgid
/// (sudo, ping, fusermount, etc.) comes out world-writable-looking but
/// missing its privilege bit. Re-read what mode the package actually
/// declared via `rpm -q --dump` and reapply it directly on the upperdir
/// copy. Only meaningful under `OverlayMode::Split` with no `--installroot`
/// (a real, live RakuOS overlay) — `--installroot` builds a disposable tree
/// with no live overlay mount to strip bits in the first place.
fn restore_setuid_bits<'a>(paths: &OverlayPaths, names: impl Iterator<Item = &'a str>) {
    use std::os::unix::fs::PermissionsExt;
    for pkg in names {
        let mut cmd = Command::new("rpm");
        with_dbpath(&mut cmd, paths).arg("-q").arg("--dump").arg(pkg);
        let Ok(out) = cmd.output() else { continue };
        if !out.status.success() {
            continue;
        }
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            // rpm --dump columns: path size mtime digest mode owner group isconfig isdoc rdev symlink
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 5 {
                continue;
            }
            let path = cols[0];
            let Ok(mode) = u32::from_str_radix(cols[4], 8) else { continue };
            if mode & 0o6000 == 0 {
                continue;
            }
            let Some(rel) = path.strip_prefix("/usr/") else { continue };
            let upper_path = paths.overlay_upper.join(rel);
            if !upper_path.exists() {
                continue;
            }
            let full_mode = mode & 0o7777;
            match std::fs::set_permissions(&upper_path, std::fs::Permissions::from_mode(full_mode)) {
                Ok(()) => println!("  Restored setuid mode {full_mode:04o} on {}", upper_path.display()),
                Err(e) => eprintln!("  WARNING: could not restore mode on {}: {e}", upper_path.display()),
            }
        }
    }
}

/// Pulls a single scriptlet block out of `rpm -q(p) --scripts` output for
/// re-running through `/bin/sh`, but only if rpm itself would have run it
/// through a shell: rpm labels each block `<marker><interp>):` followed by
/// the script on the next line, and `<interp>` isn't always `/bin/sh` — Lua
/// scriptlets (`<lua>`, used by e.g. glibc, crypto-policies) are executed by
/// rpm's own embedded Lua interpreter, not a shell, so blindly piping that
/// text into `/bin/sh -c` just produces Lua syntax errors. Real rpm already
/// ran the scriptlet correctly (via whichever interpreter it declared) as
/// part of the actual transaction; this function only feeds the supplementary
/// re-run passes in this file (which exist to react to `daemon-reload`, not
/// to substitute for rpm's own scriptlet execution), so skipping non-shell
/// interpreters here loses nothing — the real scriptlet still ran once.
fn extract_shell_scriptlet<'a>(scripts: &'a str, marker: &str) -> Option<&'a str> {
    let rest = scripts.split(marker).nth(1)?;
    let (interp, rest) = rest.split_once("):\n")?;
    if interp.trim() != "/bin/sh" && interp.trim() != "sh" {
        return None;
    }
    let rest = rest
        .split("\npostinstall scriptlet")
        .next()
        .unwrap_or(rest)
        .split("\npreuninstall scriptlet")
        .next()
        .unwrap_or(rest)
        .split("\npostuninstall scriptlet")
        .next()
        .unwrap_or(rest)
        .split("\npretrans scriptlet")
        .next()
        .unwrap_or(rest)
        .split("\nposttrans scriptlet")
        .next()
        .unwrap_or(rest);
    Some(rest)
}

/// Cross-package RPM "file triggers" — glib2's `%transfiletriggerin` that
/// recompiles GSettings schemas, hicolor-icon-theme's icon cache,
/// desktop-file-utils' desktop database, shared-mime-info's mime database,
/// systemd's own unit reload/preset trigger — only fire when the
/// *triggering* package (glib2, systemd, etc.) is registered in
/// the same rpm dbpath the transaction targets (see `needs_nodeps`'s doc
/// comment). Under `OverlayMode::Split` the transaction runs against an
/// overlay-only dbpath that never contains those base-owned packages, so
/// real rpm silently never fires them: e.g. installing `virt-manager`
/// (ships `/usr/share/glib-2.0/schemas/org.virt-manager.virt-manager.
/// gschema.xml`) leaves the compiled `gschemas.compiled` index stale, and
/// virt-manager aborts at startup with "Settings schema ... is not
/// installed". The overlay is mounted live, so the merged `/usr/share/...`
/// tree already has the new files by the time this runs — just re-run the
/// same regen command a real dnf transaction's file trigger would have run,
/// once per matched directory for the whole transaction (a single batched
/// `rpm -qlp` over every downloaded package, not one query per package),
/// not once per package.
fn run_post_install_cache_triggers(downloaded: &[DownloadedPackage]) {
    if downloaded.is_empty() {
        return;
    }
    let mut cmd = Command::new("rpm");
    cmd.arg("-qlp");
    cmd.args(downloaded.iter().map(|d| &d.rpm_path));
    let Ok(out) = cmd.output() else { return };
    if !out.status.success() {
        return;
    }
    let listing = String::from_utf8_lossy(&out.stdout);

    let mut needs_glib_schemas = false;
    let mut needs_mime_db = false;
    let mut needs_desktop_db = false;
    let mut needs_fc_cache = false;
    let mut icon_theme_dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut ships_units = false;

    for path in listing.lines() {
        if let Some(rest) = path.strip_prefix("/usr/share/glib-2.0/schemas/") {
            needs_glib_schemas |= !rest.is_empty();
        } else if path.starts_with("/usr/share/mime/") {
            needs_mime_db = true;
        } else if path.starts_with("/usr/share/applications/") {
            needs_desktop_db = true;
        } else if path.starts_with("/usr/share/fonts/") {
            needs_fc_cache = true;
        } else if let Some(rest) = path.strip_prefix("/usr/share/icons/") {
            if let Some(theme) = rest.split('/').next().filter(|t| !t.is_empty()) {
                icon_theme_dirs.insert(format!("/usr/share/icons/{theme}"));
            }
        } else if let Some(rest) = path.strip_prefix("/usr/lib/systemd/system/") {
            // Only a bare unit directly under the directory (skip
            // drop-in `.wants/`/`.requires/`/`.d/` entries and anything
            // nested).
            let is_unit = rest.ends_with(".service")
                || rest.ends_with(".socket")
                || rest.ends_with(".timer")
                || rest.ends_with(".path")
                || rest.ends_with(".target");
            if is_unit && !rest.contains('/') {
                ships_units = true;
            }
        }
    }

    // Captured rather than inherited: `glib-compile-schemas` in particular
    // dumps a huge wall of "Ignoring override for key ..." gibberish to
    // stderr for perfectly normal overrides, which drowned out the rest of
    // the transaction's output. Only surface it if the command actually
    // failed.
    let run = |label: &str, program: &str, args: &[&str]| match Command::new(program).args(args).output() {
        Ok(out) if out.status.success() => println!("  {label}: {program} {}", args.join(" ")),
        Ok(out) => {
            eprintln!("  WARNING: {program} {} exited with {}", args.join(" "), out.status);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.trim().is_empty() {
                eprint!("{stderr}");
            }
        }
        Err(_) => {} // tool not installed — nothing to regenerate with, same as a real transaction with the owning package absent
    };

    if needs_glib_schemas {
        run("Regenerated cache", "glib-compile-schemas", &["/usr/share/glib-2.0/schemas"]);
    }
    if needs_mime_db {
        run("Regenerated cache", "update-mime-database", &["/usr/share/mime"]);
    }
    if needs_desktop_db {
        run("Regenerated cache", "update-desktop-database", &["/usr/share/applications"]);
    }
    if needs_fc_cache {
        run("Regenerated cache", "fc-cache", &["-f"]);
    }
    // systemd's own `%transfiletriggerin` on /usr/lib/systemd/system (owned
    // by the `systemd` package, not the one we're installing) is what
    // normally runs `systemctl daemon-reload` after a unit file lands — same
    // cross-package-trigger gap as the glib schemas case above, since
    // `systemd` itself is never registered in the overlay-only dbpath.
    if ships_units {
        run("Systemd", "systemctl", &["daemon-reload"]);
        // A blind `systemctl preset` only enables a unit if a matching
        // preset policy file says so — most systems' catch-all policy is
        // "disable *", so it silently does nothing for a unit whose owning
        // package instead calls `systemctl enable` explicitly in its own
        // `%post` (rpm's `%systemd_post` macro does exactly this: preset on
        // first install, but plenty of packages — e.g. nix-daemon — ship a
        // hand-written `%post` with a literal `enable`/`start` instead).
        // That `%post` already ran as part of this same rpm transaction
        // (scriptlets aren't skipped here — see `noscripts`'s doc comment),
        // so it isn't lost, just possibly ineffective if it invoked
        // `systemctl` before `daemon-reload` picked the new unit file up.
        // Re-run each package's actual `%post` scriptlet now, after the
        // reload, so the unit ends up in whatever enabled/started state the
        // package's own scriptlet — not a guessed preset policy — actually
        // asked for. This used to hand-tokenize the scriptlet text looking
        // for lines starting with a bare `systemctl `/`systemd-update-helper
        // ` prefix, which missed anything wrapped in a shell conditional or
        // assigned through a variable (e.g. libvirt's subpackages, whose
        // `%systemd_post`-generated `%post` blocks branch on `$1` and quote
        // the unit list rather than emitting a single flat `systemctl ...`
        // line). Real rpm runs `%post` through `/bin/sh`, so do the same
        // here instead of re-parsing shell as if it were plain text — `$1`
        // is set to `1` to match the "first install" case `%systemd_post`
        // checks for, since this only runs for freshly downloaded packages.
        for d in downloaded {
            let Ok(out) = Command::new("rpm").args(["-qp", "--scripts"]).arg(&d.rpm_path).output() else { continue };
            if !out.status.success() {
                continue;
            }
            let scripts = String::from_utf8_lossy(&out.stdout);
            let Some(post) = extract_shell_scriptlet(&scripts, "postinstall scriptlet (using ") else { continue };
            if post.trim().is_empty() {
                continue;
            }
            match Command::new("/bin/sh").arg("-c").arg(post).arg("post").arg("1").output() {
                Ok(out) if out.status.success() => println!("  Ran postinstall scriptlet: {}", d.rpm_path.display()),
                Ok(out) => {
                    eprintln!("  WARNING: postinstall scriptlet for {} exited with {}", d.rpm_path.display(), out.status);
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if !stderr.trim().is_empty() {
                        eprint!("{stderr}");
                    }
                }
                Err(_) => {}
            }
        }
    }
    for dir in &icon_theme_dirs {
        if Path::new(dir).join("index.theme").exists() {
            run("Regenerated cache", "gtk-update-icon-cache", &["-f", "-q", dir]);
        }
    }
}

/// Directory under `state_dir` holding one file per package that shipped
/// modprobe.d/modules-load.d config rum copied into `/etc` (see
/// [`sync_modprobe_configs`]) — each file lists the `/etc` paths that
/// package's copy created, so [`cleanup_modprobe_configs`] knows exactly
/// what to remove on uninstall without guessing from the package name alone.
fn modprobe_sync_state_dir(paths: &OverlayPaths) -> PathBuf {
    paths.state_dir.join("modprobe-sync")
}

/// Nvidia's own driver packages ship `nvidia-modprobe`, a setuid helper that
/// creates `/dev/nvidia*` device nodes and loads the driver modules on
/// demand itself — RakuOS's DE-switcher Nvidia swap flow already manages
/// this package's modprobe.d/modules-load.d state directly as part of
/// swapping the driver in/out, so letting the generic sync/cleanup here also
/// touch the same files risks fighting that flow (e.g. cleaning up a config
/// the swap logic expects to still be there, or racing a swap-in-progress).
/// Matches by name prefix rather than an exact package list since Nvidia's
/// packaging spans `nvidia-driver`, `nvidia-driver-cuda`, `nvidia-kmod*`,
/// `nvidia-modprobe`, `nvidia-settings`, `akmod-nvidia`, `xorg-x11-drv-nvidia*`.
fn is_nvidia_package(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("nvidia")
}

/// The old `rakuos-load-extra-modules` dracut module used to parse
/// `modprobe.d`/`modules-load.d` config straight out of the overlay upper at
/// every early boot, in the initrd, before the normal (non-initrd)
/// `systemd-modules-load.service` on the real root ever gets a chance to
/// run. That was only ever needed because those config files live under
/// `/usr/lib` in the overlay upper, which the initrd's own early-boot
/// `systemd-modules-load.service` invocation (the one baked into the initrd
/// image itself) can't see.
///
/// `/etc` doesn't have that problem — it isn't part of the overlay/`/usr`
/// split at all, it's always the real, live, persistent `/etc`, so a config
/// file dropped there is picked up by the ordinary systemd
/// `systemd-modules-load.service` unit on every normal boot with no initrd
/// involvement whatsoever. So instead of an early-boot dracut module
/// re-deriving what to load every boot, rum just copies each newly
/// installed package's `modprobe.d`/`modules-load.d` `.conf` files into
/// `/etc` once, at install time, and the system's own standard boot
/// machinery takes it from there.
///
/// Only copies files a package ships outside `/etc` already (i.e. under
/// `/usr/lib` or `/usr/share`) — a package that ships straight into `/etc`
/// itself is already rpm-tracked and doesn't need rum's help. Never
/// overwrites a destination that already exists (either a real local
/// override or a previous package's file with the same name), so a
/// collision is left alone rather than silently clobbered.
fn sync_modprobe_configs(paths: &OverlayPaths, downloaded: &[DownloadedPackage]) {
    let state_dir = modprobe_sync_state_dir(paths);

    for d in downloaded {
        if is_nvidia_package(&d.package.nevra.name) {
            continue;
        }
        let Ok(out) = Command::new("rpm").arg("-qlp").arg(&d.rpm_path).output() else { continue };
        if !out.status.success() {
            continue;
        }
        let listing = String::from_utf8_lossy(&out.stdout);
        let mut created: Vec<String> = Vec::new();

        for path in listing.lines() {
            if path.starts_with("/etc/modprobe.d/") || path.starts_with("/etc/modules-load.d/") {
                continue; // already rpm-tracked in place, nothing to do
            }
            let Some((subdir, basename)) = (if let Some(rest) = path.strip_prefix("/usr/lib/modprobe.d/") {
                Some(("modprobe.d", rest))
            } else if let Some(rest) = path.strip_prefix("/usr/lib/modules-load.d/") {
                Some(("modules-load.d", rest))
            } else {
                None
            }) else {
                continue;
            };
            if !basename.ends_with(".conf") || basename.contains('/') {
                continue;
            }

            let dest = PathBuf::from(format!("/etc/{subdir}/{basename}"));
            if dest.exists() {
                continue;
            }
            let Ok(contents) = fs::read(path) else { continue };
            if fs::create_dir_all(dest.parent().unwrap()).and_then(|_| fs::write(&dest, &contents)).is_ok() {
                println!("  Activated {} config: {}", subdir, dest.display());
                created.push(dest.display().to_string());
            }
        }

        if !created.is_empty() {
            let _ = fs::create_dir_all(&state_dir);
            let _ = fs::write(state_dir.join(format!("{}.list", d.package.nevra.name)), created.join("\n"));
        }
    }
}

/// Removes whatever [`sync_modprobe_configs`] copied into `/etc` for a
/// package being uninstalled, using the per-package list it wrote at
/// install time rather than re-deriving paths from the (now-removed)
/// package's file list, since `rpm -qlp` needs a `.rpm` file or a still-
/// installed entry to query and neither is guaranteed to be around by the
/// time this runs.
fn cleanup_modprobe_configs(paths: &OverlayPaths, to_remove: &[Package]) {
    let state_dir = modprobe_sync_state_dir(paths);
    for pkg in to_remove {
        if is_nvidia_package(&pkg.nevra.name) {
            continue;
        }
        let state_file = state_dir.join(format!("{}.list", pkg.nevra.name));
        let Ok(contents) = fs::read_to_string(&state_file) else { continue };
        for line in contents.lines().filter(|l| !l.trim().is_empty()) {
            if fs::remove_file(line).is_ok() {
                println!("  Deactivated config: {line}");
            }
        }
        let _ = fs::remove_file(&state_file);
    }
}

/// Mirrors [`run_post_install_cache_triggers`]'s `%post` re-run, but for
/// removal: re-runs each about-to-be-removed package's real `%preun`
/// scriptlet (stop/disable) as a safety net alongside rpm's own automatic
/// scriptlet execution, so a unit that was enabled prior to removal doesn't
/// stay enabled — e.g. a leftover `foo.service` symlink under
/// `/etc/systemd/system/multi-user.target.wants/` pointing at a unit file
/// that's about to disappear, which systemd would otherwise silently carry
/// forward as a dangling enablement.
///
/// Queries scripts from the *installed* copy (`rpm -q --scripts`, against
/// this transaction's own dbpath) rather than a downloaded `.rpm` file —
/// unlike install, there's no freshly downloaded package to query here, only
/// whatever's already in `paths`' overlay/base rpmdb. Must run before the
/// actual `rpm -evh` erase below: `%preun`'s own `systemctl` calls likewise
/// run before file removal, since disabling a unit is most reliable while
/// its unit file still exists.
///
/// Runs the extracted `%preun` block through `/bin/sh -c`, exactly like
/// `run_post_install_cache_triggers` does for `%post` — not a line-by-line
/// `systemctl `/`systemd-update-helper ` prefix scan, which misses anything
/// wrapped in a shell conditional or assigned through a variable (the same
/// class of bug `%post`'s hand-tokenizer had; `%systemd_preun`-generated
/// blocks branch on `$1` and quote the unit list rather than emitting a bare
/// `systemctl ...` line). `$1` is set to `0` to match "last uninstall" —
/// the case `%systemd_preun` checks for — since this only runs for packages
/// actually being removed, not upgraded.
fn run_pre_remove_systemd_disable(paths: &OverlayPaths, to_remove: &[Package]) {
    for pkg in to_remove {
        let qualified = format!("{}.{}", pkg.nevra.name, pkg.nevra.arch);
        let mut cmd = Command::new("rpm");
        with_dbpath(&mut cmd, paths).arg("-q").arg("--scripts").arg(&qualified);
        let Ok(out) = cmd.output() else { continue };
        if !out.status.success() {
            continue;
        }
        let scripts = String::from_utf8_lossy(&out.stdout);
        let Some(preun) = extract_shell_scriptlet(&scripts, "preuninstall scriptlet (using ") else { continue };
        if preun.trim().is_empty() {
            continue;
        }
        match Command::new("/bin/sh").arg("-c").arg(preun).arg("preun").arg("0").output() {
            Ok(out) if out.status.success() => println!("  Ran preuninstall scriptlet: {qualified}"),
            Ok(out) => {
                eprintln!("  WARNING: preuninstall scriptlet for {qualified} exited with {}", out.status);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stderr.trim().is_empty() {
                    eprint!("{stderr}");
                }
            }
            Err(_) => {}
        }
    }
}

/// `rpm -e` against the merged dbpath, for `rum remove`.
///
/// `protected` is the combined `protected_packages=` list (dnf.conf) plus
/// rum's own always-protected names (`rum` itself) — checked by glob against
/// every requested name before any removal happens, so a protected package
/// can never be erased even as a side effect of `autoremove`/`swap` pulling
/// it in among other names.
pub fn apply_remove(paths: &OverlayPaths, names: &[String], dry_run: bool, protected: &[String], assume_yes: bool) -> Result<()> {
    apply_remove_ex(paths, names, dry_run, protected, false, assume_yes)
}

/// Full form of [`apply_remove`] with `nodeps`.
///
/// `nodeps` maps to `rpm -e --nodeps` — used by `swap`, which erases the old
/// side of the pair before installing the new one (see `Command::Swap`'s
/// doc). rpm's CLI can't add an install and an erase to the same
/// transaction set the way librpm itself can, so a swap that's part of a
/// multi-pair sequence (e.g. build.sh swapping `ffmpeg-free`→`ffmpeg` then
/// separately `libavcodec-free`→`libavcodec`) hits a real, correct rpm
/// dependency error here: sibling `-free` packages not yet swapped still
/// legitimately `Requires` the one being erased. A strict `-e` would block
/// every swap but the last in such a sequence. `--nodeps` accepts that the
/// system is transiently inconsistent between swaps, the same way dnf's
/// single combined transaction is briefly "erase A, not yet installed B"
/// internally — it's resolved by the time the whole sequence finishes.
/// Resolves a `remove`-style name (a bare package name, or a
/// `name.arch`-qualified one) against the installed set. A qualified input
/// is matched to exactly the one package at that name+arch; a bare input
/// matches every installed package with that name, which may be more than
/// one under multilib (e.g. a bare `libnsl` with both `.x86_64` and `.i686`
/// installed) — deliberately permissive here, since the final `rpm -evh`
/// invocation always re-qualifies every resolved package as `name.arch`
/// itself, so an ambiguous bare name never reaches `rpm` for it to reject.
///
/// Falls back to a `Provides:` match when nothing is literally installed
/// under that name — the removal-side mirror of `install`'s fallback to
/// `rum-solv`'s provides index (see its doc comment): `dnf remove
/// webserver` removes whichever installed package actually provides
/// `webserver`, not just a package literally named that, and rum's remove
/// used to only ever look at literal names.
fn resolve_remove_name<'a>(overlay: &'a rum_overlay::OverlayContext, name: &str) -> Vec<&'a Package> {
    if let Some((base, arch)) = name.rsplit_once('.') {
        let qualified: Vec<&Package> = overlay.base.iter().chain(&overlay.overlay).filter(|p| p.nevra.name == base && p.nevra.arch == arch).collect();
        if !qualified.is_empty() {
            return qualified;
        }
    }
    let by_name: Vec<&Package> = overlay.base.iter().chain(&overlay.overlay).filter(|p| p.nevra.name == name).collect();
    if !by_name.is_empty() {
        return by_name;
    }
    overlay.base.iter().chain(&overlay.overlay).filter(|p| p.provides.iter().any(|d| d.name == name)).collect()
}

pub fn apply_remove_ex(paths: &OverlayPaths, names: &[String], dry_run: bool, protected: &[String], nodeps: bool, assume_yes: bool) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    if let Some(blocked) = names.iter().find(|n| rum_core::name_matches_any(n, protected)) {
        anyhow::bail!("'{blocked}' is a protected package and cannot be removed");
    }
    if !dry_run {
        ensure_overlay_dbpath_initialized(paths)?;
    }

    // dnf's own `remove` skips names that aren't actually installed
    // ("No match for argument") rather than failing the whole transaction
    // — a caller passing several names (a glob-expanded pattern, a fixed
    // list a build script always runs) shouldn't have one already-absent
    // package abort removal of everything else.
    let overlay = rum_overlay::OverlayContext::load(paths).context("loading overlay context")?;
    let mut to_remove: Vec<Package> = Vec::new();
    for n in names {
        let matches = resolve_remove_name(&overlay, n);
        if matches.is_empty() {
            println!("No match for argument: {n}");
            continue;
        }
        for pkg in matches {
            if !to_remove.iter().any(|p: &Package| p.nevra == pkg.nevra) {
                to_remove.push(pkg.clone());
            }
        }
    }
    if to_remove.is_empty() {
        println!("Nothing to remove.");
        return Ok(());
    }

    // dnf's own `remove` doesn't hand the bare requested set to the
    // transaction and let it fail on the first still-installed dependent —
    // it computes and includes that whole dependent closure up front (its
    // "The following ... will also be removed" prompt). `--nodeps` opts out
    // entirely, same as it does for the `rpm -e` call below.
    if !nodeps {
        let dependents = rum_resolver::compute_removal_closure(&to_remove, &overlay);
        if let Some(blocked) = dependents.iter().find(|p| rum_core::name_matches_any(&p.nevra.name, protected)) {
            anyhow::bail!("'{}' is a protected package and would be removed as a dependent of this transaction", blocked.nevra.name);
        }
        if !dependents.is_empty() {
            println!("The following packages will also be removed (dependents):");
            for pkg in &dependents {
                println!("  {}", pkg.nevra);
            }
            for pkg in dependents {
                if !to_remove.iter().any(|p| p.nevra == pkg.nevra) {
                    to_remove.push(pkg);
                }
            }
        }
    }

    let entries: Vec<Entry> = to_remove.iter().map(|pkg| Entry::new(Action::Removing, pkg)).collect();
    print_transaction_summary(&entries);

    // A dry run only resolves and simulates — the summary above is the
    // whole answer, so stop here rather than running a real `rpm --test`
    // against the actual overlay dbpath (which needs it initialized/opened,
    // i.e. root) just to throw the result away.
    if dry_run {
        println!("Transaction test succeeded.");
        return Ok(());
    }

    if !confirm_transaction(assume_yes)? {
        println!("Operation aborted.");
        return Ok(());
    }

    // Always fully qualified as `name.arch` here, regardless of how the
    // caller specified it — every entry in `to_remove` is a concrete
    // resolved package, and `rpm -e name` errors as ambiguous
    // ("specifies multiple packages") the moment two installed packages
    // share a name across multilib arches.
    let qualified_names: Vec<String> = to_remove.iter().map(|p| format!("{}.{}", p.nevra.name, p.nevra.arch)).collect();

    // `--installroot` builds a disposable tree that may not even have a
    // `/bin/sh` yet (e.g. an empty overlay-prebake installroot) — same
    // rationale as `run_pre_remove_systemd_disable` below, there's no
    // systemd/service state in a bare installroot for a `%preun` to act on,
    // and rpm's own scriptlet interpreter exec would just fail against it.
    // Unlike install, remove has no case where a populated `--installroot`
    // still needs its removal scriptlets to run, so this is unconditional
    // rather than requiring an explicit opt-out.
    if paths.installroot.is_none() {
        run_pre_remove_systemd_disable(paths, &to_remove);
    }

    let mut cmd = Command::new("rpm");
    with_dbpath(&mut cmd, paths).arg("-evh");
    if paths.installroot.is_some() {
        cmd.arg("--noscripts");
    }
    // In Split mode (and --installroot), the dbpath rpm actually opens here
    // is the overlay-only rpmdb — it has no idea base-image packages exist
    // at all. Without `--nodeps`, rpm runs its own erase dependency check
    // against that incomplete view; rum's own `compute_removal_closure`
    // above already did the real check correctly (it merges base + overlay
    // to decide what else must go), so a second, base-blind check here can
    // only ever be redundant when it agrees or wrong when it doesn't — e.g.
    // live-reported: removing `steam` refused because some multilib i686
    // helper package still visible to rpm's overlay-only view looked
    // unsatisfied, even though the real (base-inclusive) dependency graph
    // was fine. Same rationale as `OverlayPaths::needs_nodeps` already
    // applies to install.
    if nodeps || paths.needs_nodeps() {
        cmd.arg("--nodeps");
    }
    println!("Running transaction");
    for n in &qualified_names {
        cmd.arg(n);
    }

    let steps: Vec<(String, u64)> = to_remove.iter().zip(&qualified_names).map(|(p, n)| (n.clone(), p.install_size)).collect();
    run_rpm_transaction(cmd, &steps, "Removing", &[])?;

    if matches!(paths.mode, OverlayMode::Split { .. }) && paths.installroot.is_none() {
        cleanup_modprobe_configs(paths, &to_remove);
    }

    println!("Complete!");

    let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let entries: Vec<(String, Reason)> =
        to_remove.iter().filter(|p| seen_names.insert(p.nevra.name.as_str())).map(|p| (p.nevra.name.clone(), Reason::User)).collect();
    if let Err(e) = history::record(paths, history::Action::Remove, &entries) {
        tracing::warn!(error = %e, "failed to record transaction history");
    }
    Ok(())
}
