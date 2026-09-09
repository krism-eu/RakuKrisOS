mod offline;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rum_core::{EvrCompare, Package};
use rum_overlay::{OverlayContext, OverlayMode, OverlayPaths};
use rum_repo::RepoConfig;
use rum_transaction::history::{self, Reason};
use std::net::ToSocketAddrs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// rum — RakuOS's overlay-aware package manager. Speaks plain yum/dnf repo
/// metadata, resolves and applies transactions itself.
/// Default repo directory, matching dnf's own built-in default.
const DEFAULT_REPO_DIR: &str = "/etc/yum.repos.d";
/// Default cache directory.
const DEFAULT_CACHE_DIR: &str = "/var/cache/rum";

#[derive(Parser)]
#[command(name = "rum", version)]
struct Cli {
    /// Path to rum's main config file (dnf.conf-compatible `[main]` section).
    #[arg(long, default_value = rum_repo::main_config::DEFAULT_CONF_PATH, global = true)]
    config: PathBuf,

    /// Directory of dnf/yum-style `.repo` files.
    #[arg(long, default_value = DEFAULT_REPO_DIR, global = true)]
    repo_dir: PathBuf,

    /// Where downloaded .rpm files are cached before install.
    #[arg(long, default_value = DEFAULT_CACHE_DIR, global = true)]
    cache_dir: PathBuf,

    /// Assume "yes" to any prompt.
    #[arg(short = 'y', long = "assumeyes", global = true)]
    assume_yes: bool,
    /// Assume "no" to any prompt.
    #[arg(long = "assumeno", global = true)]
    assume_no: bool,
    /// Default answer to confirmation prompts is "yes" (config-only, no CLI flag in dnf5 either).
    #[arg(skip)]
    default_yes: bool,
    /// Suppress non-essential output.
    #[arg(short = 'q', long = "quiet", global = true)]
    quiet: bool,
    /// Exclude a package (by name, glob-capable) from consideration — repeatable.
    #[arg(short = 'x', long = "exclude", global = true, value_delimiter = ',')]
    exclude: Vec<String>,
    /// Run entirely from cached repo metadata; error instead of touching the network.
    #[arg(short = 'C', long = "cacheonly", global = true)]
    cacheonly: bool,
    /// Force a repo metadata refresh, ignoring `metadata_expire=`.
    #[arg(long = "refresh", global = true)]
    refresh: bool,
    /// Restrict operations to only these repo ids (glob-capable, repeatable).
    #[arg(long = "repo", visible_alias = "repoid", global = true, value_delimiter = ',')]
    repo: Vec<String>,
    /// Enable a repo (by id, glob-capable) for this run only, even if disabled — repeatable.
    #[arg(long = "enablerepo", visible_alias = "enable-repo", global = true, value_delimiter = ',')]
    enablerepo: Vec<String>,
    /// Disable a repo (by id, glob-capable) for this run only, even if enabled — repeatable.
    #[arg(long = "disablerepo", visible_alias = "disable-repo", global = true, value_delimiter = ',')]
    disablerepo: Vec<String>,
    /// Skip GPG signature verification entirely for this run.
    #[arg(long = "no-gpgchecks", visible_alias = "nogpgcheck", global = true)]
    no_gpgchecks: bool,
    /// dnf-style `KEY=VALUE` config override, repeatable. rum only acts on
    /// `tsflags=noscripts` (skip rpm scriptlets); other keys are accepted and ignored.
    #[arg(long = "setopt", global = true)]
    setopt: Vec<String>,
    /// Allow removing conflicting/obsoleted packages to complete a transaction,
    /// instead of refusing — same as dnf's `--allowerasing`.
    #[arg(long = "allowerasing", global = true)]
    allowerasing: bool,
    /// Require the newest available version to resolve cleanly, rather than
    /// silently falling back to an older one — dnf's `--best`.
    #[arg(long = "best", global = true)]
    best: bool,
    /// Drop package names that don't match anything in the enabled repos
    /// instead of failing the whole transaction — dnf's `--skip-unavailable`.
    /// Only affects `install`.
    #[arg(long = "skip-unavailable", global = true)]
    skip_unavailable: bool,
    /// Add an ad-hoc repo from a `name,baseurl` pair, for this run only —
    /// dnf's `--repofrompath`. Repeatable.
    #[arg(long = "repofrompath", global = true)]
    repofrompath: Vec<String>,
    /// Don't remove now-unneeded dependency-only packages after this `remove`
    /// — dnf's `--no-autoremove`.
    #[arg(long = "no-autoremove", visible_alias = "noautoremove", global = true)]
    no_autoremove: bool,
    /// Operate against an alternate root instead of the real `/` — same as
    /// dnf/rpm's own `--installroot`. Used to pre-bake an overlay for image
    /// builds and for isolated testing.
    #[arg(long = "installroot", global = true)]
    installroot: Option<PathBuf>,
    /// Don't pull in weak dependencies (`Recommends`) for this run — dnf's
    /// `--setopt install_weak_deps=false` shorthand.
    #[arg(long = "no-recommends", global = true)]
    no_recommends: bool,
    /// If the full requested `install` set can't be resolved together, drop
    /// whichever names are responsible and proceed with the rest — dnf's `--skip-broken`.
    #[arg(long = "skip-broken", global = true)]
    skip_broken: bool,
    /// Install every distinct architecture build of a matched package side
    /// by side, instead of just one — dnf's `multilib_policy=all`.
    #[arg(long = "multilib-all", global = true)]
    multilib_all: bool,
    /// Resolve and download for a different architecture than the host's —
    /// dnf5's `--forcearch`. Useful when pre-baking an overlay/installroot
    /// for a foreign arch.
    #[arg(long = "forcearch", global = true)]
    forcearch: Option<String>,
    /// Restrict candidate resolution to packages from a specific repo id
    /// (glob-capable, repeatable) — dnf5's `--from-repo`. Unlike `--repo`,
    /// every enabled repo's metadata is still loaded for dependency checks.
    #[arg(long = "from-repo", global = true, value_delimiter = ',')]
    from_repo: Vec<String>,
    /// Restrict candidate resolution to packages whose `Vendor:` tag matches
    /// — dnf5's `--from-vendor`, repeatable.
    #[arg(long = "from-vendor", global = true, value_delimiter = ',')]
    from_vendor: Vec<String>,
    /// Resolve and download the transaction, but don't apply it — dnf5's
    /// `--downloadonly`. Applies to `install`/`upgrade`/`downgrade`/
    /// `reinstall`/`distro-sync`/`autoremove`/`swap`.
    #[arg(long = "downloadonly", global = true)]
    downloadonly: bool,
    /// With `--downloadonly`, where to write the downloaded `.rpm`s —
    /// defaults to `--cache-dir`.
    #[arg(long = "destdir", global = true)]
    destdir: Option<PathBuf>,
    /// Allow resolving to an older version than what's currently installed —
    /// dnf5's `--allow-downgrade`. This is the default; the flag exists for
    /// parity/explicitness. See `--no-allow-downgrade`.
    #[arg(long = "allow-downgrade", global = true, conflicts_with = "no_allow_downgrade")]
    allow_downgrade: bool,
    /// Forbid the resolver from downgrading anything to satisfy this request
    /// — dnf5's `--no-allow-downgrade`.
    #[arg(long = "no-allow-downgrade", global = true)]
    no_allow_downgrade: bool,

    /// Not a real flag — populated from `rum.conf` after parsing.
    #[arg(skip)]
    main_conf: rum_repo::main_config::MainConfig,

    #[command(subcommand)]
    command: Command,
}

use rum_core::glob_match;

/// Expands any `@group-id` or `@^environment-id` entry in `names` (dnf's
/// comps install syntax, e.g. `@fonts @hardware-support @^workstation-
/// product-environment`) into its member package names, leaving plain
/// package names untouched. `@id` pulls in a group's mandatory + default
/// packagereqs (matching dnf's own default `group install` set — see
/// [`rum_repo::comps::parse_comps_xml`]); `@^id` pulls in every `<grouplist>`
/// group of an environment (not `<optionlist>`), transitively expanded the
/// same way. Matches by `id` first, falling back to the human-readable
/// `name` case-insensitively (dnf accepts either on the command line).
/// Errors if a `@`/`@^`-prefixed name matches no group/environment in any
/// enabled repo's comps data.
fn expand_groups(names: &[String], groups: &[rum_repo::Group], environments: &[rum_repo::Environment]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let add_group = |group: &rum_repo::Group, out: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
        println!("Adding packages from group '{}':", group.name);
        for pkg in &group.packages {
            if seen.insert(pkg.clone()) {
                println!("  {pkg}");
                out.push(pkg.clone());
            }
        }
    };
    for name in names {
        if let Some(id) = name.strip_prefix("@^") {
            let env = environments.iter().find(|e| e.id == id).or_else(|| environments.iter().find(|e| e.name.eq_ignore_ascii_case(id)));
            let env = env.with_context(|| format!("environment '{id}' not found (no comps data for it in any enabled repo)"))?;
            println!("Adding groups from environment '{}':", env.name);
            for group_id in &env.group_ids {
                let group = groups.iter().find(|g| &g.id == group_id);
                let Some(group) = group else {
                    eprintln!("warning: environment '{}' references group '{group_id}', which has no comps data in any enabled repo", env.name);
                    continue;
                };
                add_group(group, &mut out, &mut seen);
            }
        } else if let Some(id) = name.strip_prefix('@') {
            let group = groups.iter().find(|g| g.id == id).or_else(|| groups.iter().find(|g| g.name.eq_ignore_ascii_case(id)));
            let group = group.with_context(|| format!("group '{id}' not found (no comps data for it in any enabled repo)"))?;
            add_group(group, &mut out, &mut seen);
        } else if seen.insert(name.clone()) {
            out.push(name.clone());
        }
    }
    Ok(out)
}

/// True if `--setopt tsflags=noscripts` (or `...,noscripts,...` in a
/// comma-separated tsflags value) was passed — the only `--setopt` key rum
/// actually acts on.
fn setopt_noscripts(cli: &Cli) -> bool {
    cli.setopt.iter().any(|kv| match kv.split_once('=') {
        Some((k, v)) if k.trim() == "tsflags" => v.split(',').any(|f| f.trim() == "noscripts"),
        _ => false,
    })
}

/// Merges `rum.conf`'s `[main]` section into `cli` wherever the
/// corresponding flag wasn't given a non-default value on the command
/// line — command-line flags always win, matching dnf's own precedence
/// (`cmdline > config file > built-in default`). `repo_dir`/`cache_dir`
/// use "still equal to the hardcoded default" as a proxy for "wasn't
/// explicitly passed" (clap doesn't expose which); a user who explicitly
/// re-passes the exact default path is indistinguishable from one who
/// didn't pass it at all, which is a harmless edge case since both mean
/// the same effective value anyway.
fn apply_main_config(cli: &mut Cli, conf: &rum_repo::main_config::MainConfig) {
    if cli.repo_dir == PathBuf::from(DEFAULT_REPO_DIR) {
        if let Some(dir) = &conf.reposdir {
            cli.repo_dir = dir.clone();
        }
    }
    if cli.cache_dir == PathBuf::from(DEFAULT_CACHE_DIR) {
        if let Some(dir) = &conf.cachedir {
            cli.cache_dir = dir.clone();
        }
    }
    cli.assume_yes = cli.assume_yes || conf.assumeyes;
    cli.no_gpgchecks = cli.no_gpgchecks || !conf.gpgcheck;
    cli.assume_no = cli.assume_no || conf.assumeno;
    cli.default_yes = conf.defaultyes;
    // `disable_excludes=main` (or `*`) turns off `exclude=`/`--exclude`
    // filtering entirely — dnf's own escape hatch for a system-wide
    // exclude that's getting in the way of one particular install.
    let excludes_disabled = conf.disable_excludes.iter().any(|s| s == "main" || s == "*");
    if !excludes_disabled {
        for pat in &conf.exclude {
            if !cli.exclude.iter().any(|e| e == pat) {
                cli.exclude.push(pat.clone());
            }
        }
    }
}

/// Builds the shared `reqwest::Client` every subcommand talks to repos/
/// mirrors with. `timeout=` from `rum.conf` (dnf's own default is 30s,
/// matching [`rum_repo::main_config::MainConfig::default`]) is deliberately
/// *not* applied here as reqwest's whole-request `.timeout()` — that covers
/// connect through the entire body download, so any package that legitimately
/// takes longer than `timeout` seconds to fully download (a large RPM on a
/// slower mirror) would get killed mid-transfer and, since downloads have no
/// resume support, restarted from byte 0 every time — exactly backwards for
/// "big packages should be more robust, not less". dnf's own `timeout=`
/// is actually a *stall* timeout (abort if no data arrives for that long),
/// not a cap on total transfer time, so only `.connect_timeout()` is set
/// here; the stall semantics are applied per-chunk in
/// [`rum_repo::get_bytes_with_retry`] instead. Retry *count* (`retries=`) is
/// handled separately, via the process-wide [`rum_repo::set_max_attempts`]
/// call in `main` — [`rum_repo::get_with_retry`] reads it from there rather
/// than from this client.
/// Backs `ip_resolve=ipv4`/`ipv6` (dnf5's `CURLOPT_IPRESOLVE`): resolves via
/// the same async system resolver reqwest would otherwise use, then drops
/// whichever address family wasn't asked for. `reqwest::ClientBuilder`
/// exposes `local_address()` (binds the *outgoing* socket, doesn't restrict
/// which family gets dialed) and `dns_resolver()` (swaps the whole
/// resolver) — only the latter can actually implement a family restriction,
/// so this plugs in here instead.
struct FamilyFilterResolver {
    want_v4: bool,
    want_v6: bool,
}

impl reqwest::dns::Resolve for FamilyFilterResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let want_v4 = self.want_v4;
        let want_v6 = self.want_v6;
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|a| (want_v4 && a.is_ipv4()) || (want_v6 && a.is_ipv6()))
                .collect();
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn build_client(cli: &Cli) -> Result<reqwest::Client> {
    let user_agent = cli.main_conf.user_agent.clone().unwrap_or_else(|| concat!("rum/", env!("CARGO_PKG_VERSION")).to_string());
    let mut builder = reqwest::Client::builder()
        .user_agent(user_agent)
        .connect_timeout(std::time::Duration::from_secs(cli.main_conf.timeout))
        // `sslverify=0` is dnf's escape hatch for a repo behind a broken/
        // self-signed cert chain — never the default, only ever an
        // explicit opt-out via rum.conf/--setopt.
        .danger_accept_invalid_certs(!cli.main_conf.sslverify);
    builder = match cli.main_conf.ip_resolve.as_str() {
        "ipv4" => builder.dns_resolver(std::sync::Arc::new(FamilyFilterResolver { want_v4: true, want_v6: false })),
        "ipv6" => builder.dns_resolver(std::sync::Arc::new(FamilyFilterResolver { want_v4: false, want_v6: true })),
        _ => builder,
    };
    if let Some(proxy_url) = &cli.main_conf.proxy {
        let mut proxy = reqwest::Proxy::all(proxy_url).with_context(|| format!("invalid proxy= URL '{proxy_url}'"))?;
        if let Some(user) = &cli.main_conf.proxy_username {
            proxy = proxy.basic_auth(user, cli.main_conf.proxy_password.as_deref().unwrap_or(""));
        }
        builder = builder.proxy(proxy);
    }
    // `username=`/`password=`: global default repo basic-auth, applied as a
    // default `Authorization` header on every request this client makes —
    // reqwest's `ClientBuilder` has no per-client `basic_auth()` (that's a
    // per-`RequestBuilder` method only), so the header is built by hand.
    if let Some(user) = &cli.main_conf.username {
        use base64::Engine;
        let creds = format!("{}:{}", user, cli.main_conf.password.as_deref().unwrap_or(""));
        let encoded = base64::engine::general_purpose::STANDARD.encode(creds);
        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(mut value) = reqwest::header::HeaderValue::from_str(&format!("Basic {encoded}")) {
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
            builder = builder.default_headers(headers);
        }
    }
    builder.build().context("building HTTP client")
}

/// Runs `apply` (an `apply_install_ex` call) against `downloaded`, and if it
/// fails specifically because a package's cached copy failed signature
/// verification, redownloads and retries once before giving up.
///
/// `verify_signatures` already deletes the bad file it rejected, so a plain
/// re-invocation of `download_all_ex` for the same `to_install` set only
/// re-fetches whatever was just deleted (`dest_of(pkg).exists()` filtering
/// skips everything still good) — this turns what used to be "abort the
/// whole transaction and make the user notice and rerun the command by
/// hand" into an automatic one-shot recovery from a corrupted/stale mirror
/// copy, the same class of transient failure `get_with_retry`/
/// `download_to_file_with_retry` already retry for connection-level errors.
async fn apply_install_with_signature_retry(
    client: &reqwest::Client,
    mut downloaded: Vec<rum_transaction::DownloadedPackage>,
    to_install: &[Package],
    cache_dir: &Path,
    max_parallel: u32,
    repo_configs: &[RepoConfig],
    fallback_candidates: &[Package],
    apply: impl Fn(&[rum_transaction::DownloadedPackage]) -> Result<()>,
) -> Result<Vec<rum_transaction::DownloadedPackage>> {
    match apply(&downloaded) {
        Ok(()) => Ok(downloaded),
        Err(e) if format!("{e:#}").contains("signature verification failed") => {
            eprintln!("warning: {e:#}");
            eprintln!("Re-downloading the affected package(s) and retrying once...");
            downloaded = rum_transaction::download_all_ex(client, to_install, cache_dir, max_parallel, true, repo_configs, fallback_candidates)
                .await
                .context("downloading packages")?;
            apply(&downloaded).context("applying rpm transaction")?;
            Ok(downloaded)
        }
        Err(e) => Err(e).context("applying rpm transaction"),
    }
}

/// Overrides `rum.conf`'s `metadata_expire=` (if set) onto any repo that
/// didn't specify its own — same fallback dnf uses. Detected by "still
/// equal to [`rum_repo::DEFAULT_METADATA_EXPIRE`]", so a repo that
/// explicitly sets the exact same value as the built-in default is
/// indistinguishable from one that didn't set it at all; harmless, since
/// both cases want the same effective value.
fn apply_metadata_expire_default(configs: &mut [RepoConfig], conf: &rum_repo::main_config::MainConfig) {
    let Some(default) = conf.metadata_expire else { return };
    for cfg in configs {
        if cfg.metadata_expire == rum_repo::DEFAULT_METADATA_EXPIRE {
            cfg.metadata_expire = default;
        }
    }
}

/// Best-effort equivalent of dnf5's `protect_running_kernel` (see
/// `GoalPrivate::limit_installonly_packages`/`PackageSack::get_running_kernel_id`
/// in dnf5's own source): the currently-booted kernel build must never be
/// pruned by `installonly_limit=` or removed outright, since either leaves
/// an unbootable/broken running system until a reboot happens to land on a
/// different kernel. dnf5 finds it by asking the rpmdb which package owns
/// `/boot/vmlinuz-<uname -r>` (falling back to `/lib/modules/<uname -r>`);
/// rum has no cheap merged-rpmdb file-ownership query spanning Split mode's
/// base+overlay split the way dnf5's single rpmdb does, so this instead
/// leans on a stronger guarantee Fedora's kernel packaging already gives
/// for free: the running kernel's `version-release` is always exactly
/// `uname -r` with the trailing `.<arch>` stripped (e.g. `uname -r` of
/// `6.19.14-300.fc44.x86_64` on x86_64 <-> kernel package EVR
/// `6.19.14-300.fc44`), so a plain string match against every installed
/// package's own EVR finds it without touching the filesystem at all.
fn running_kernel_evr(host_arch: &str) -> Option<String> {
    let out = std::process::Command::new("uname").arg("-r").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let release = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(release.strip_suffix(&format!(".{host_arch}")).map(str::to_string).unwrap_or(release))
}

/// Combines `rum.conf`'s `protected_packages=` with rum's own hardcoded
/// always-protected names (currently just `rum` itself), for passing to
/// [`rum_transaction::apply_remove`].
fn protected_packages(cli: &Cli) -> Vec<String> {
    let mut protected = cli.main_conf.protected_packages.clone();
    for p in rum_repo::main_config::ALWAYS_PROTECTED {
        protected.push(p.to_string());
    }
    protected
}

/// Builds the [`rum_resolver::ResolveOptions`] every resolve call site
/// shares, from `rum.conf` plus this run's `--no-recommends` override.
fn resolve_options(cli: &Cli, paths: &OverlayPaths) -> rum_resolver::ResolveOptions {
    rum_resolver::ResolveOptions {
        install_weak_deps: cli.main_conf.install_weak_deps && !cli.no_recommends,
        installonly_pkgs: cli.main_conf.installonlypkgs.clone(),
        allow_erasing: cli.allowerasing,
        force_names: std::collections::HashSet::new(),
        protected_names: protected_packages(cli),
        locked_names: rum_transaction::versionlock::list(paths).unwrap_or_default(),
        skip_broken: cli.main_conf.skip_broken || cli.skip_broken,
        multilib_all: cli.main_conf.multilib_policy == "all" || cli.multilib_all,
        best: cli.main_conf.best || cli.best,
        obsoletes: cli.main_conf.obsoletes,
        allow_downgrade: !cli.no_allow_downgrade,
    }
}

/// `--from-repo`/`--from-vendor`: narrows the *candidate* pool (not the set
/// of loaded repos — see `effective_repo_configs` for that) to packages
/// matching, right before it's handed to the resolver. A no-op (returns
/// `candidates` unchanged) when neither flag is given.
fn apply_from_filters(cli: &Cli, candidates: Vec<Package>) -> Vec<Package> {
    if cli.from_repo.is_empty() && cli.from_vendor.is_empty() {
        return candidates;
    }
    candidates
        .into_iter()
        .filter(|pkg| cli.from_repo.is_empty() || cli.from_repo.iter().any(|pat| glob_match(pat, &pkg.repo_id)))
        .filter(|pkg| cli.from_vendor.is_empty() || cli.from_vendor.iter().any(|pat| glob_match(pat, &pkg.vendor)))
        .collect()
}

/// `--downloadonly`: resolve normally, but fetch `to_install` into
/// `--destdir` (or `--cache-dir`) instead of running the rpm transaction —
/// shared by every install-like command's handler. dnf5's own
/// `--downloadonly` doesn't gpg-import repo keys either (nothing is being
/// installed), so this skips `import_repo_keys` too.
async fn download_only(cli: &Cli, client: &reqwest::Client, to_install: &[Package], repo_configs: &[RepoConfig], fallback_candidates: &[Package]) -> Result<()> {
    let per_repo = cli.destdir.is_none();
    let dest = cli.destdir.as_deref().unwrap_or(&cli.cache_dir);
    let downloaded = rum_transaction::download_all_ex_with_metadata_root(client, to_install, dest, &cli.cache_dir, cli.main_conf.max_parallel_downloads, per_repo, repo_configs, fallback_candidates)
        .await
        .context("downloading packages")?;
    for d in &downloaded {
        println!("{}", d.rpm_path.display());
    }
    Ok(())
}

/// Applies `--repo`/`--enablerepo`/`--disablerepo` on top of each repo's own
/// `enabled=` setting, matching dnf5's precedence: `--repo` (if given at
/// all) is an allow-list overriding everything else, then `--disablerepo`
/// wins over `--enablerepo`, which wins over the file's own `enabled=`.
fn effective_repo_configs(cli: &Cli, all: Vec<RepoConfig>) -> Vec<RepoConfig> {
    all.into_iter()
        .filter(|cfg| {
            if !cli.repo.is_empty() {
                return cli.repo.iter().any(|pat| glob_match(pat, &cfg.id));
            }
            let mut enabled = cfg.enabled;
            if cli.enablerepo.iter().any(|pat| glob_match(pat, &cfg.id)) {
                enabled = true;
            }
            if cli.disablerepo.iter().any(|pat| glob_match(pat, &cfg.id)) {
                enabled = false;
            }
            enabled
        })
        .collect()
}

#[derive(Subcommand)]
enum Command {
    /// Easter egg.
    #[command(hide = true)]
    Rum,
    /// Resolve and install one or more packages.
    #[command(visible_aliases = ["in", "pour"])]
    Install {
        packages: Vec<String>,
        /// Resolve and validate only — don't download or write anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove one or more packages.
    #[command(visible_aliases = ["rm", "erase"])]
    Remove {
        packages: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Upgrade one or more installed packages to the newest available
    /// version. Always overlay-only: a base-image package named explicitly
    /// is reported instead of being layered into the overlay, since the
    /// base image is read-only. `--minimal` (dnf5's `upgrade-minimal`) isn't
    /// implemented — a plain upgrade always picks the newest candidate.
    #[command(visible_aliases = ["up", "update"])]
    Upgrade {
        packages: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        /// Only upgrade to a candidate covered by this exact advisory id
        /// (e.g. `FEDORA-2024-abc123`) — repeatable.
        #[arg(long = "advisory")]
        advisory: Vec<String>,
        /// Only upgrade to candidates covered by a security advisory.
        #[arg(long)]
        security: bool,
        /// Only upgrade to candidates covered by a bugfix advisory.
        #[arg(long)]
        bugfix: bool,
        /// Only upgrade to candidates covered by an enhancement advisory.
        #[arg(long)]
        enhancement: bool,
        /// Only upgrade to candidates covered by an advisory at/above this
        /// severity (`critical`/`important`/`moderate`/`low`).
        #[arg(long = "advisory-severity")]
        advisory_severity: Option<String>,
    },
    /// Mode-aware system upgrade: dnf5's `system-upgrade` on a plain host,
    /// RakuOS's image-update flow on an overlay system.
    ///
    /// On a plain host, a bare invocation is an error — do a major-version
    /// bump (e.g. Fedora 44 -> 45) via `download --releasever=X` to resolve
    /// and download the new release's packages, `reboot` to stage and
    /// reboot into applying it, `clean` to discard a staged transaction,
    /// and `log`/`status` to inspect past/current runs.
    ///
    /// On a RakuOS overlay system, a bare invocation runs an ordinary
    /// overlay package upgrade (same as `rum upgrade`), then checks for and
    /// stages a new base image. A new release arrives as a new image here,
    /// so the `download`/`reboot`/`execute`/`clean`/`log`/`status`
    /// subcommands only work on a plain host.
    SystemUpgrade {
        #[command(subcommand)]
        action: Option<SystemUpgradeAction>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Reinstall one or more installed packages at their exact current
    /// version, e.g. to repair a corrupted/modified install.
    #[command(visible_alias = "rei")]
    Reinstall {
        packages: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Downgrade one or more installed packages to the newest available
    /// version older than what's installed.
    #[command(visible_alias = "dg")]
    Downgrade {
        packages: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Make installed packages exactly match the best available repo
    /// version, upgrading or downgrading as needed (unlike `upgrade`, which
    /// only ever moves forward). With no names given, every overlay-
    /// installed package is synced.
    #[command(visible_aliases = ["dsync", "distrosync"])]
    DistroSync {
        packages: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Removes overlay packages that were pulled in only as a dependency
    /// and are no longer required by anything still installed. Packages
    /// installed explicitly (or `rum mark install`ed) are never touched.
    Autoremove {
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove `remove_spec` and install `install_spec` as two back-to-back
    /// transactions — an approximation of dnf's single-transaction swap.
    Swap { remove_spec: String, install_spec: String },
    /// Change a package's recorded install reason, controlling whether
    /// `autoremove` may remove it later.
    Mark {
        #[command(subcommand)]
        action: MarkAction,
    },
    /// Checks the installed package set for problems: unmet dependencies,
    /// installed-package conflicts, obsoleted packages, and duplicate
    /// name.arch installs. With no flags, all checks run; pass a subset to
    /// run only those.
    Check {
        /// Report unmet `Requires` and installed-vs-installed `Conflicts`.
        #[arg(long)]
        dependencies: bool,
        /// Report multiple installed versions of the same name.arch (other
        /// than packages like the kernel that are expected to coexist).
        #[arg(long)]
        duplicates: bool,
        /// Report installed packages obsoleted by another installed package.
        #[arg(long)]
        obsoleted: bool,
    },
    /// List installed packages, optionally filtered by a name substring.
    /// Lists overlay installs by default; `--base` lists the base image's
    /// package set instead.
    #[command(visible_alias = "ls")]
    List {
        filter: Option<String>,
        /// List base-image packages instead of overlay-installed ones.
        /// Also scopes `--extras`/`--obsoletes`/`--upgrades` to the base
        /// set; has no effect on `--available` or `--autoremove`.
        #[arg(long)]
        base: bool,
        /// List repo candidates not currently installed — dnf's `list --available`.
        #[arg(long)]
        available: bool,
        /// List installed packages not provided by any enabled repo — dnf's
        /// `list --extras` (locally built/sideloaded, or from a removed repo).
        #[arg(long)]
        extras: bool,
        /// List installed packages obsoleted by an available repo candidate
        /// — dnf's `list --obsoletes`.
        #[arg(long)]
        obsoletes: bool,
        /// List installed packages with a newer version available — dnf's
        /// `list --upgrades`.
        #[arg(long)]
        upgrades: bool,
        /// List packages `rum autoremove` would remove, without removing
        /// anything — dnf's `list --autoremove`. Always scoped to the
        /// overlay; `--base` has no effect.
        #[arg(long)]
        autoremove: bool,
        /// With `--available`, list every build (EVR) a repo offers instead
        /// of just the newest — dnf's `list --showduplicates`.
        #[arg(long)]
        showduplicates: bool,
    },
    /// Search enabled repos' package name and summary for a substring.
    #[command(visible_alias = "se")]
    Search {
        query: String,
        /// Show which repo each result came from.
        #[arg(long)]
        showrepo: bool,
    },
    /// Shows what's installed or available that provides a capability
    /// (name, versioned dep, or file path) — mirrors dnf's `provides`.
    #[command(visible_alias = "whatprovides")]
    Provides { capability: String },
    /// Lists installed packages that have a newer version available,
    /// without downloading or installing anything — the same candidate
    /// selection `upgrade` uses, printed instead of applied.
    #[command(visible_alias = "check-update")]
    CheckUpgrade {
        packages: Vec<String>,
        /// Emit `{"updates": [...]}` instead of plain text, for callers
        /// that parse the result (e.g. rakuos-software's update checker).
        #[arg(long)]
        json: bool,
    },
    /// Queries repo package metadata — a subset of dnf's `repoquery`: no
    /// `--list` (file listing), but name/glob matching, `--installed`/
    /// `--available`, `--whatprovides`, `--whatrequires`, `--requires`,
    /// `--provides` all work.
    #[command(visible_alias = "rq")]
    Repoquery {
        pattern: Option<String>,
        /// Only installed packages (base + overlay) — no repo query at all.
        #[arg(long)]
        installed: bool,
        /// Only available (repo) packages — the default when neither this
        /// nor `--installed` is given.
        #[arg(long)]
        available: bool,
        /// List packages (installed by default, or repo candidates with
        /// `--available`) that provide this capability.
        #[arg(long)]
        whatprovides: Option<String>,
        /// List packages that `Requires` this capability.
        #[arg(long)]
        whatrequires: Option<String>,
        /// List packages that `Conflicts` with this capability.
        #[arg(long)]
        whatconflicts: Option<String>,
        /// List packages that `Obsoletes` this capability.
        #[arg(long)]
        whatobsoletes: Option<String>,
        /// List packages that `Recommends` this capability (weak dep).
        #[arg(long)]
        whatrecommends: Option<String>,
        /// List packages that `Enhances` this capability (weak dep).
        #[arg(long)]
        whatenhances: Option<String>,
        /// List packages that `Suggests` this capability (weak dep).
        #[arg(long)]
        whatsuggests: Option<String>,
        /// List packages that `Supplements` this capability (weak dep).
        #[arg(long)]
        whatsupplements: Option<String>,
        /// With `--whatrequires`, follow the reverse-dependency closure
        /// transitively (packages requiring a match's own provides, and so
        /// on) instead of stopping at direct requirers.
        #[arg(long)]
        recursive: bool,
        /// Only match packages of this architecture (glob-capable).
        #[arg(long)]
        arch: Option<String>,
        /// Only show packages that have more than one build available
        /// (same name+arch, different EVR) among the matched pool.
        #[arg(long)]
        duplicates: bool,
        /// Print full detailed info (same shape as `rum info`) for each
        /// match instead of its NEVRA.
        #[arg(short, long)]
        info: bool,
        /// dnf-style custom output format, e.g. `%{name}-%{evr}.%{arch}`.
        /// Recognized tags: name, epoch, version, release, evr, arch,
        /// nevra, summary, repoid, size, installsize, location.
        #[arg(long, visible_alias = "qf")]
        queryformat: Option<String>,
        /// Print each matched package's `Requires` instead of its NEVRA.
        #[arg(long)]
        requires: bool,
        /// Print each matched package's `Provides` instead of its NEVRA.
        #[arg(long)]
        provides: bool,
        /// Print each matched package's `Conflicts` instead of its NEVRA.
        #[arg(long)]
        conflicts: bool,
        /// Print each matched package's `Obsoletes` instead of its NEVRA.
        #[arg(long)]
        obsoletes: bool,
        /// Print each matched package's `Recommends` instead of its NEVRA.
        #[arg(long)]
        recommends: bool,
        /// Print each matched package's `Enhances` instead of its NEVRA.
        #[arg(long)]
        enhances: bool,
        /// Print each matched package's `Suggests` instead of its NEVRA.
        #[arg(long)]
        suggests: bool,
        /// Print each matched package's `Supplements` instead of its NEVRA.
        #[arg(long)]
        supplements: bool,
        /// Only match source packages (arch `src`) — dnf5's `--srpm`.
        #[arg(long, alias = "source")]
        srpm: bool,
        /// Print each match's repo `location` (download path) instead of
        /// its NEVRA.
        #[arg(long)]
        location: bool,
        /// Print `name-version-release` instead of the full NEVRA.
        #[arg(long)]
        nvr: bool,
        /// Print `epoch:name-version-release.arch`.
        #[arg(long)]
        envra: bool,
        /// Installed packages that aren't available from any configured
        /// repo (matched by name) — dnf's own `--extras`. Implies querying
        /// the installed set regardless of `--installed`/`--available`.
        #[arg(long)]
        extras: bool,
        /// Installed packages with a newer version available in a
        /// configured repo — dnf's own `--upgrades`. Implies querying the
        /// installed set regardless of `--installed`/`--available`.
        #[arg(long)]
        upgrades: bool,
        /// Prints the `--queryformat` tags rum recognizes, then exits.
        #[arg(long)]
        querytags: bool,
    },
    /// Shows detailed info (NEVRA, repo, summary) for a package name.
    #[command(visible_aliases = ["if", "more"])]
    Info { package: String },
    /// Lists overlay-installed packages that nothing else installed
    /// depends on — dnf's own "leaf" concept, roughly "what was installed
    /// on purpose and nothing else needs."
    Leaves,
    /// Show whether a package is installed, and if so from the base image
    /// or the overlay.
    Origin { package: String },
    /// Print the overlay paths rum is using.
    Paths,
    /// Unconditionally refresh cached repo metadata for every enabled repo.
    #[command(visible_alias = "mc", alias = "makecache")]
    MakeCache,
    /// Enable or disable a Copr repo.
    Copr {
        #[command(subcommand)]
        action: CoprAction,
    },
    /// Clears cached data under `--cache-dir`.
    Clean {
        #[arg(value_enum, default_value = "all")]
        what: CleanWhat,
    },
    /// Resolves and downloads packages (and their dependencies) without
    /// installing them.
    Download {
        packages: Vec<String>,
        /// Where to write downloaded `.rpm`s — defaults to `--cache-dir`.
        #[arg(long)]
        destdir: Option<PathBuf>,
        /// Download the `.src.rpm` instead of the binary package — dnf5's
        /// `--srpm` (`--source` accepted as dnf4's alias for the same flag).
        /// Needs a source repo enabled (e.g. Fedora's `*-source` repos).
        #[arg(long, alias = "source")]
        srpm: bool,
        /// Load repo metadata against a different `$releasever` than the
        /// running system's own — dnf's `download --releasever=X`.
        #[arg(long)]
        releasever: Option<String>,
    },
    /// Installs `<name>-debuginfo` for one or more already-named packages.
    /// Needs the debuginfo packages to be present in a configured repo
    /// (e.g. Fedora's `*-debuginfo` repos).
    DebuginfoInstall {
        packages: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Prints the RPM changelog for one or more installed packages.
    Changelog { packages: Vec<String> },
    /// Scans running processes for ones using a shared library or binary
    /// that was since deleted/replaced on disk (e.g. after an update) —
    /// a partial equivalent of dnf's `needs-restarting`: reports affected
    /// PIDs, not systemd units.
    NeedsRestarting,
    /// Lists or shows details for configured repos.
    Repo {
        #[command(subcommand)]
        action: RepoAction,
    },
    /// Views recorded install/remove transactions.
    History {
        #[command(subcommand)]
        action: HistoryAction,
    },
    /// Excludes packages from `upgrade`/`distro-sync` candidate selection.
    Versionlock {
        #[command(subcommand)]
        action: VersionlockAction,
    },
    /// Minimal `.repo` file editing: enable/disable existing repos, or add
    /// a new one from a bare `baseurl=`.
    ConfigManager {
        /// Repo id(s) to enable — repeatable.
        #[arg(long = "set-enabled")]
        set_enabled: Vec<String>,
        /// Repo id(s) to disable — repeatable.
        #[arg(long = "set-disabled")]
        set_disabled: Vec<String>,
        /// `id=baseurl` pairs to write as a new `<id>.repo` file — repeatable.
        /// Only a plain baseurl is supported (not a `.repo` file URL).
        #[arg(long = "add-repo")]
        add_repo: Vec<String>,
        /// dnf5-style `config-manager addrepo` subcommand (as opposed to the
        /// dnf4-style `--add-repo`/`--set-enabled` flags above).
        #[command(subcommand)]
        action: Option<ConfigManagerAction>,
    },
    /// Alias for `repo info` (dnf5 top-level compat command).
    Repoinfo { id: String },
    /// Alias for `repo list` (dnf5 top-level compat command).
    Repolist {
        #[arg(long)]
        all: bool,
    },
    /// Comps group management: `@id` install syntax, `group list/info/
    /// install/remove`. `install`/`remove` act on a group's mandatory and
    /// default members, same as dnf's default group transaction.
    Group {
        #[command(subcommand)]
        action: GroupAction,
    },
    /// Comps environment management: `@^id` install syntax, `environment
    /// list/info/install/remove`. `install`/`remove` act on every group in
    /// the environment, same as dnf's default environment transaction.
    Environment {
        #[command(subcommand)]
        action: EnvironmentAction,
    },
    /// Modularity (module streams) — not yet implemented.
    Module {
        #[command(subcommand)]
        action: ModuleAction,
    },
    /// Security/bugfix/enhancement advisories (updateinfo). Not every repo
    /// (especially third-party ones) publishes this data.
    #[command(visible_alias = "updateinfo")]
    Advisory {
        #[command(subcommand)]
        action: AdvisoryAction,
    },
    /// Offline (reboot-time) transactions — not yet implemented.
    Offline,
    /// Alias for `offline upgrade` — not yet implemented, see `offline`.
    #[command(name = "offline-upgrade")]
    OfflineUpgrade,
    /// Alias for `offline distro-sync` — not yet implemented, see `offline`.
    #[command(name = "offline-distrosync")]
    OfflineDistrosync,
    /// Mirrors a repo's packages to local disk. Respects the global
    /// `--repo`/`--repoid` filter for which repo(s) to sync.
    Reposync {
        /// Where to write downloaded `.rpm`s — defaults to `--cache-dir`.
        #[arg(long = "download-path", short = 'p')]
        download_path: Option<PathBuf>,
        /// Only download the newest version of each package (dnf's default
        /// downloads every version).
        #[arg(long = "newest-only")]
        newest_only: bool,
        /// Write files flat into the download path instead of nesting them
        /// under a per-repo-id subdirectory.
        #[arg(long = "norepopath")]
        norepopath: bool,
    },
    /// Prunes old package versions from a local directory of `.rpm` files,
    /// grouped by name+arch.
    Repomanage {
        /// Directory to scan for `.rpm` files (recursively).
        path: PathBuf,
        /// Print the newest version(s) per package instead of the old ones.
        #[arg(long)]
        new: bool,
        /// How many newest versions per package count as "new" (kept)
        /// rather than "old" (prunable) — dnf-utils' `repomanage --keep`.
        #[arg(long, default_value_t = 1)]
        keep: u32,
    },
    /// Reports packages whose dependencies aren't satisfiable by anything
    /// in the enabled repos — a repo-wide sanity check, not a simulated install.
    Repoclosure {
        /// Restrict which packages get checked (glob-capable, repeatable).
        /// Empty means check everything.
        #[arg(long)]
        pkg: Vec<String>,
    },
    /// Installs build dependencies from a `.spec` or `.src.rpm` file.
    #[command(name = "builddep", visible_alias = "build-dep")]
    BuildDep {
        /// Path to the `.spec` or `.src.rpm` file.
        spec: PathBuf,
        /// rpm macro definition `NAME VALUE`, repeatable — dnf's `builddep --define`.
        #[arg(long = "define")]
        define: Vec<String>,
        /// Resolve and validate only — don't download or write anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Replays a recorded transaction file — not yet implemented.
    Replay,
    /// Runs an arbitrary transaction spec file — not yet implemented.
    Do,
    /// dnf-automatic equivalent: a non-interactive upgrade suitable for a
    /// periodic timer or cron job, driven by `automatic.conf`. Resolves an
    /// upgrade, optionally downloads it, optionally applies it.
    Automatic {
        /// Apply `random_sleep=`'s random delay before doing anything —
        /// dnf5's `--timer`, to spread out load when many machines run on
        /// the same schedule.
        #[arg(long)]
        timer: bool,
        /// Override `download_updates=` on for this run.
        #[arg(long, conflicts_with = "no_downloadupdates")]
        downloadupdates: bool,
        /// Override `download_updates=` off for this run.
        #[arg(long)]
        no_downloadupdates: bool,
        /// Override `apply_updates=` on for this run.
        #[arg(long, conflicts_with_all = ["no_installupdates", "no_downloadupdates"])]
        installupdates: bool,
        /// Override `apply_updates=` off for this run.
        #[arg(long)]
        no_installupdates: bool,
    },
    /// Generates a shell completion script for `rum`.
    Completions {
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
enum SystemUpgradeAction {
    /// Resolve a full distro-sync transaction against `--releasever` and
    /// download every package it needs, without touching the running
    /// system yet. Only works on a plain (non-overlay) host.
    Download {
        /// The target major release, e.g. `45`.
        #[arg(long)]
        releasever: String,
        /// Upgrade in place instead of distro-sync: never install an older
        /// package than what's currently installed — dnf5's `--no-downgrade`.
        #[arg(long)]
        no_downgrade: bool,
        packages: Vec<String>,
    },
    /// Stage the downloaded transaction for pre-boot application and
    /// reboot. Only works on a plain (non-overlay) host.
    Reboot {
        /// Power off instead of rebooting once the transaction completes.
        #[arg(long)]
        poweroff: bool,
    },
    /// Internal use only — applies the staged transaction during boot.
    Execute,
    /// Discard a stored offline transaction and any downloaded packages.
    /// Only works on a plain (non-overlay) host.
    Clean,
    /// Show logs from past offline transactions. Only works on a plain
    /// (non-overlay) host.
    Log,
    /// Show the status of the currently stored offline transaction, if
    /// any. Only works on a plain (non-overlay) host.
    Status,
}

#[derive(Subcommand)]
enum GroupAction {
    List {
        /// Only groups that are fully installed (base or overlay) — dnf5's
        /// `group list --installed`.
        #[arg(long)]
        installed: bool,
        /// Only groups that aren't fully installed — dnf5's `group list --available`.
        #[arg(long)]
        available: bool,
    },
    Info { id: String },
    Install { id: String },
    /// Upgrades every currently-installed member of this group to its
    /// newest available version — dnf5's `group upgrade`.
    Upgrade { id: String },
    Remove { id: String },
}

#[derive(Subcommand)]
enum EnvironmentAction {
    List {
        /// Only environments that are fully installed — dnf5's
        /// `environment list --installed`.
        #[arg(long)]
        installed: bool,
        /// Only environments that aren't fully installed — dnf5's
        /// `environment list --available`.
        #[arg(long)]
        available: bool,
    },
    Info { id: String },
    Install { id: String },
    /// Upgrades every currently-installed package belonging to this
    /// environment's groups — dnf5's `environment upgrade`.
    Upgrade { id: String },
    Remove { id: String },
}

#[derive(Subcommand)]
enum ModuleAction {
    List,
    Info { name: String },
    Enable { name: String },
    Disable { name: String },
    Reset { name: String },
    /// Enables the module's stream and installs the named profile's
    /// packages — `name`, `name:stream`, or `name:stream/profile`.
    Install { name: String },
    /// Removes the packages belonging to the given module's installed
    /// profile(s). Doesn't disable or reset the module stream itself — use
    /// `module disable`/`module reset` for that.
    Remove { name: String },
}

/// Shared filters for every `advisory` subcommand.
#[derive(clap::Args)]
struct AdvisoryFilters {
    /// Which bucket of advisories to consider (default: `--available`).
    #[arg(long)]
    available: bool,
    /// Advisories covering a package already installed at that exact EVR.
    #[arg(long)]
    installed: bool,
    /// Advisories covering an installed package with a newer available EVR.
    #[arg(long)]
    updates: bool,
    /// All three buckets combined.
    #[arg(long)]
    all: bool,
    /// Only this advisory id (repeatable).
    #[arg(long = "advisory")]
    advisory: Vec<String>,
    #[arg(long)]
    bugfix: bool,
    #[arg(long)]
    security: bool,
    #[arg(long)]
    enhancement: bool,
    #[arg(long)]
    newpackage: bool,
    /// Only advisories at/above this severity (`critical`/`important`/
    /// `moderate`/`low`).
    #[arg(long)]
    severity: Option<String>,
    /// Package name-spec(s) (glob-capable) to restrict which advisories are
    /// shown — empty means every advisory in the selected bucket.
    packages: Vec<String>,
}

/// `rum upgrade --advisory=`/`--security`/`--bugfix`/`--enhancement`/
/// `--advisory-severity=` — same filter shape as [`AdvisoryFilters`], minus
/// the bucket/package-name arguments `upgrade` doesn't take.
struct AdvisoryUpgradeFilter<'a> {
    ids: &'a [String],
    security: bool,
    bugfix: bool,
    enhancement: bool,
    severity: Option<&'a str>,
}

impl AdvisoryUpgradeFilter<'_> {
    /// No filter — every candidate is eligible, same as `upgrade` with none
    /// of the `--advisory*` flags given. Used by internal callers (`group
    /// upgrade`, `environment upgrade`, the Split-mode overlay bump inside
    /// `system-upgrade`) that call [`upgrade`] directly rather than through
    /// the CLI's own `Command::Upgrade` arm.
    const NONE: AdvisoryUpgradeFilter<'static> = AdvisoryUpgradeFilter { ids: &[], security: false, bugfix: false, enhancement: false, severity: None };

    fn is_active(&self) -> bool {
        !self.ids.is_empty() || self.security || self.bugfix || self.enhancement || self.severity.is_some()
    }

    fn matches(&self, adv: &rum_repo::Advisory) -> bool {
        if !self.ids.is_empty() && !self.ids.contains(&adv.id) {
            return false;
        }
        let want_kind = self.security || self.bugfix || self.enhancement;
        if want_kind {
            let ok = (self.security && adv.kind == rum_repo::updateinfo::AdvisoryKind::Security)
                || (self.bugfix && adv.kind == rum_repo::updateinfo::AdvisoryKind::Bugfix)
                || (self.enhancement && adv.kind == rum_repo::updateinfo::AdvisoryKind::Enhancement);
            if !ok {
                return false;
            }
        }
        if let Some(sev) = self.severity {
            if adv.severity < rum_repo::updateinfo::Severity::parse_cli(sev) {
                return false;
            }
        }
        true
    }
}

#[derive(Subcommand)]
enum AdvisoryAction {
    /// One line per matching advisory: id, type, severity, title.
    List {
        #[command(flatten)]
        filters: AdvisoryFilters,
    },
    /// Full detail (title, severity, issued date, description, covered
    /// packages) per matching advisory.
    Info {
        #[command(flatten)]
        filters: AdvisoryFilters,
    },
    /// Counts of matching advisories, grouped by type.
    Summary {
        #[command(flatten)]
        filters: AdvisoryFilters,
    },
}

#[derive(Subcommand)]
enum ConfigManagerAction {
    /// dnf5-style `config-manager addrepo`: adds a repo either from a
    /// remote/local `.repo` file (kept verbatim, own id(s)/options as
    /// published) or from an explicit `--id`/baseurl pair.
    Addrepo {
        /// URL (or local path) of an existing `.repo` file to download and
        /// save verbatim — repeatable.
        #[arg(long = "from-repofile")]
        from_repofile: Vec<String>,
        /// Repo id for a `--set baseurl=...`-style add (only used when
        /// `--from-repofile` isn't given).
        #[arg(long = "id")]
        id: Option<String>,
        /// `key=value` repo options for a `--id`-based add — only
        /// `baseurl=` is currently supported.
        #[arg(long = "set")]
        set: Vec<String>,
    },
}

#[derive(Subcommand)]
enum RepoAction {
    /// Lists configured repos and whether each is enabled.
    List {
        /// Also list disabled repos (dnf's default `repo list` shows
        /// enabled only).
        #[arg(long)]
        all: bool,
    },
    /// Shows one repo's full configuration.
    Info { id: String },
}

#[derive(Subcommand)]
enum HistoryAction {
    /// Lists every recorded transaction, oldest first.
    List,
    /// Shows one transaction's full package list by id.
    Info { id: u64 },
    /// Reverses one transaction: packages it installed are removed,
    /// packages it removed are reinstalled at whatever version is
    /// currently available.
    Undo {
        id: u64,
        #[arg(long)]
        dry_run: bool,
    },
    /// Re-applies transaction `<id>` forward, as if it were run again now.
    Redo {
        id: u64,
        #[arg(long)]
        dry_run: bool,
    },
    /// Undoes every transaction after `<id>`, most recent first, returning
    /// the system to its state right after `<id>` completed.
    Rollback {
        id: u64,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum VersionlockAction {
    Add { package: String },
    Delete { package: String },
    List,
    Clear,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum CleanWhat {
    All,
    Packages,
    Metadata,
}

#[derive(Subcommand)]
enum CoprAction {
    /// Enable `owner/project`, auto-detecting the right chroot from
    /// `/etc/os-release` (`ID_LIKE`/`ID` + `VERSION_ID`) unless one is
    /// given explicitly.
    Enable {
        owner_project: String,
        /// Override the auto-detected chroot, e.g. `fedora-44-x86_64`.
        chroot: Option<String>,
    },
    /// Disable (remove) a previously-enabled Copr project's repo file.
    Disable { owner_project: String },
}

#[derive(Subcommand)]
enum MarkAction {
    /// Marks one or more packages as explicitly (user-)installed, exempting
    /// them from `autoremove`.
    Install { packages: Vec<String> },
    /// Marks one or more packages as dependency-installed, making them
    /// `autoremove` candidates once nothing else requires them — e.g.
    /// `mark dependency $(rpm -qa --qf '%{NAME} ')` to bulk-seed every
    /// currently-installed package as a dependency at image-build time.
    Dependency { packages: Vec<String> },
}

fn main() -> std::process::ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("Error: failed to start async runtime: {err:#}");
            return std::process::ExitCode::FAILURE;
        }
    };
    rt.block_on(async move {
        if let Err(err) = run().await {
            eprintln!("Error: {err:#}");
            eprintln!("If you think this is a bug, please report it: https://rakuos.org/bugs/p/rakuos-packages-rakuos-rakuos-rum");
            return std::process::ExitCode::FAILURE;
        }
        std::process::ExitCode::SUCCESS
    })
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let mut cli = Cli::parse();
    let mut main_conf = rum_repo::main_config::load(&cli.config).with_context(|| format!("loading {}", cli.config.display()))?;
    rum_repo::main_config::apply_setopt(&mut main_conf, &cli.setopt);
    apply_main_config(&mut cli, &main_conf);
    rum_repo::set_max_attempts(main_conf.retries);
    rum_repo::set_stall_timeout(main_conf.timeout);
    rum_repo::set_minrate(main_conf.minrate);
    rum_transaction::history::set_history_record(main_conf.history_record);
    rum_repo::set_fastestmirror(main_conf.fastestmirror);
    rum_repo::set_default_net_options(rum_repo::NetOptions {
        proxy: main_conf.proxy.clone(),
        proxy_userpwd: main_conf.proxy_username.clone().map(|u| format!("{u}:{}", main_conf.proxy_password.as_deref().unwrap_or(""))),
        username: None,
        password: None,
        sslcacert: None,
        sslclientcert: None,
        sslclientkey: None,
    });
    rum_repo::set_group_package_types(main_conf.group_package_types.clone());
    rum_transaction::set_diskspacecheck(main_conf.diskspacecheck);
    cli.main_conf = main_conf;
    // Auto-detects: a real RakuOS overlay system gets `Split` mode (rum's
    // own overlay rpmdb, kept separate from the base image's), anything
    // else (distrobox/podman, an image-build environment) gets
    // `Standalone` — a single plain rpmdb, exactly like dnf.
    let mut paths = OverlayPaths::detect()?;
    paths.installroot = cli.installroot.clone();

    // Every command that mutates real system state (the overlay rpmdb,
    // /etc/yum.repos.d, rum's own state dir under /var/lib/rakuos) needs
    // root — fail fast with a clear message before doing any resolving or
    // downloading, rather than getting partway through a transaction and
    // hitting a permission error from `rpm`/`open()` deep in the pipeline.
    // `--installroot` is exempt: it writes into a caller-owned, disposable
    // tree (image-build prebake, test setups), not the real system. A
    // `--dry-run` invocation is exempt too — it only resolves/validates and
    // never touches the rpmdb, repo files, or state dir, so there's nothing
    // it would fail to write; forcing root on it just makes "will this even
    // resolve" checks (e.g. sanity-checking a package list before a real
    // install) needlessly require sudo.
    if let Some(name) =
        (paths.installroot.is_none() && !is_root() && !command_is_dry_run(&cli.command)).then(|| command_needs_root(&cli.command)).flatten()
    {
        anyhow::bail!("rum {name} requires root — try: sudo rum {name}");
    }

    match &cli.command {
        Command::Paths => {
            println!("{}", rum_overlay::describe(&paths));
        }
        Command::Origin { package } => {
            let overlay = OverlayContext::load(&paths).context("loading overlay context")?;
            println!("{package}: {:?}", overlay.origin_of(package));
        }
        Command::Rum => println!("Put the bottle down kid you had enough."),
        Command::Install { packages, dry_run } => install(&cli, &paths, packages, *dry_run).await?,
        Command::Remove { packages, dry_run } => {
            let overlay_for_glob = OverlayContext::load(&paths).context("loading overlay context")?;
            let installed_names: Vec<&str> = overlay_for_glob.base.iter().chain(&overlay_for_glob.overlay).map(|p| p.nevra.name.as_str()).collect();
            let packages = &expand_name_patterns(packages, &installed_names);
            let host_arch = detect_host_arch(&cli);
            if let Some(running) = cli.main_conf.protect_running_kernel.then(|| running_kernel_evr(&host_arch)).flatten() {
                if let Some(blocked) = overlay_for_glob
                    .base
                    .iter()
                    .chain(&overlay_for_glob.overlay)
                    .find(|p| format!("{}-{}", p.nevra.version, p.nevra.release) == running && packages.iter().any(|n| *n == p.nevra.name || *n == format!("{}.{}", p.nevra.name, p.nevra.arch)))
                {
                    anyhow::bail!("'{}' is the currently running kernel and cannot be removed", blocked.nevra);
                }
            }
            // Captured before removal so the removed packages' own
            // Requires/Recommends are still walkable — feeds the scoped
            // autoremove sweep below.
            let removed: Vec<&Package> = overlay_for_glob
                .base
                .iter()
                .chain(&overlay_for_glob.overlay)
                .filter(|p| packages.iter().any(|n| *n == p.nevra.name || *n == format!("{}.{}", p.nevra.name, p.nevra.arch)))
                .collect();
            let scope = removed_dependency_closure_names(&removed, &overlay_for_glob);
            rum_transaction::apply_remove(&paths, packages, *dry_run, &protected_packages(&cli), cli.assume_yes)?;
            if matches!(paths.mode, OverlayMode::Split { .. }) && !dry_run {
                for name in packages {
                    rum_overlay::packages_list_remove(name)?;
                    if rum_overlay::local_rpm_list_contains(name) {
                        rum_overlay::local_rpm_list_remove(name)?;
                        let cached = Path::new(rum_overlay::LOCAL_RPM_CACHE).join(format!("{name}.rpm"));
                        std::fs::remove_file(cached).ok();
                    }
                }
            }
            // `clean_requirements_on_remove=` (dnf default: true) — sweep up
            // now-unneeded dependency-only packages right after a remove,
            // not just when the user runs `autoremove` explicitly. Scoped to
            // just the removed packages' own dependency closure (see
            // `removed_dependency_closure_names`), matching dnf5's
            // `SOLVER_CLEANDEPS`-in-the-same-solve behavior rather than
            // sweeping unrelated pre-existing orphans too.
            if cli.main_conf.clean_requirements_on_remove && !cli.no_autoremove && !dry_run {
                autoremove_ex(&cli, &paths, false, Some(&scope))?;
            }
        }
        Command::Upgrade { packages, dry_run, advisory, security, bugfix, enhancement, advisory_severity } => {
            let filter = AdvisoryUpgradeFilter { ids: advisory, security: *security, bugfix: *bugfix, enhancement: *enhancement, severity: advisory_severity.as_deref() };
            upgrade(&cli, &paths, packages, *dry_run, &filter).await?
        }
        Command::SystemUpgrade { action, dry_run } => system_upgrade(&cli, &paths, action.as_ref(), *dry_run).await?,
        Command::Reinstall { packages, dry_run } => {
            // Local .rpm file arguments (e.g. akmods' `dnf reinstall
            // --disablerepo='*' <freshly-built-kmod.rpm>`) never go through
            // repo-candidate resolution or the installed-package check below
            // — same as `install`, they carry their own header. This also
            // covers the case where the package was never installed before:
            // dnf's `reinstall` of a local file behaves like a plain install.
            let (rpm_files, names): (Vec<String>, Vec<String>) = packages.iter().cloned().partition(|p| p.ends_with(".rpm") && Path::new(p).is_file());
            if !rpm_files.is_empty() {
                install_local_rpm_files(&cli, &paths, &rpm_files, *dry_run, true).await?;
            }
            if !names.is_empty() {
                sync_packages(&cli, &paths, &names, *dry_run, SyncMode::Reinstall).await?;
            }
        }
        Command::Downgrade { packages, dry_run } => sync_packages(&cli, &paths, packages, *dry_run, SyncMode::Downgrade).await?,
        Command::DistroSync { packages, dry_run } => sync_packages(&cli, &paths, packages, *dry_run, SyncMode::DistroSync).await?,
        Command::Autoremove { dry_run } => autoremove(&cli, &paths, *dry_run)?,
        Command::Swap { remove_spec, install_spec } => {
            rum_transaction::apply_remove_ex(&paths, std::slice::from_ref(remove_spec), false, &protected_packages(&cli), true, cli.assume_yes)?;
            install_ex(&cli, &paths, std::slice::from_ref(install_spec), false, true).await?;
        }
        Command::Mark { action } => {
            let overlay_for_glob = OverlayContext::load(&paths).context("loading overlay context")?;
            let installed_names: Vec<&str> = overlay_for_glob.base.iter().chain(&overlay_for_glob.overlay).map(|p| p.nevra.name.as_str()).collect();
            match action {
                MarkAction::Install { packages } => {
                    for package in &expand_name_patterns(packages, &installed_names) {
                        history::set_reason(&paths, package, Reason::User)?;
                        println!("Marked {package} as user-installed.");
                    }
                }
                MarkAction::Dependency { packages } => {
                    for package in &expand_name_patterns(packages, &installed_names) {
                        history::set_reason(&paths, package, Reason::Dependency)?;
                        println!("Marked {package} as dependency-installed.");
                    }
                }
            }
        }
        Command::Check { dependencies, duplicates, obsoleted } => {
            let (dependencies, duplicates, obsoleted) = if !dependencies && !duplicates && !obsoleted { (true, true, true) } else { (*dependencies, *duplicates, *obsoleted) };
            check(&paths, &cli.main_conf.installonlypkgs, dependencies, duplicates, obsoleted)?
        }
        Command::List { filter, base, available, extras, obsoletes, upgrades, autoremove, showduplicates } => {
            let overlay = OverlayContext::load(&paths).context("loading overlay context")?;
            // `--base` selects the read-only base image's packages instead
            // of the overlay's own installs, for every filter below that's
            // installed-set-scoped. `Standalone` mode has no base/overlay
            // split at all (everything installed lands in `overlay.overlay`),
            // so `--base` there is a no-op that lists the same single
            // installed set rather than always coming back empty.
            let installed: &[Package] = match (&overlay.mode, base) {
                (OverlayMode::Split { .. }, true) => &overlay.base,
                (OverlayMode::Split { .. }, false) | (OverlayMode::Standalone, _) => &overlay.overlay,
            };
            let matches_filter = |name: &str| filter.as_deref().is_none_or(|f| name.contains(f));

            if *autoremove {
                // Always overlay-scoped (see the flag's own doc comment) —
                // `--base` is deliberately ignored here, same as `rum
                // autoremove` itself only ever sweeps `overlay.overlay`.
                let protected_names = protected_packages(&cli);
                for pkg in compute_autoremove_candidates(&paths, &overlay, &protected_names, None) {
                    if matches_filter(&pkg.nevra.name) {
                        println!("{}", pkg.nevra);
                    }
                }
            } else if *available || *extras || *obsoletes || *upgrades {
                let (_, candidates, _groups, _environments, ..) = load_candidates(&cli).await?;
                if *available {
                    let installed_nevras: std::collections::HashSet<&rum_core::Nevra> = overlay.base.iter().chain(&overlay.overlay).map(|p| &p.nevra).collect();
                    let mut shown: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
                    let mut sorted: Vec<&Package> = candidates.iter().filter(|c| !installed_nevras.contains(&c.nevra)).collect();
                    sorted.sort_by(|a, b| a.nevra.name.cmp(&b.nevra.name).then_with(|| b.nevra.evr().as_str().compare_evr(a.nevra.evr().as_str())));
                    for pkg in sorted {
                        if !matches_filter(&pkg.nevra.name) {
                            continue;
                        }
                        // Without `--showduplicates`, only the newest build
                        // per name+arch — `sorted`'s ordering (name, then
                        // EVR descending) means the first one seen per
                        // (name, arch) pair is always that newest build.
                        if !showduplicates && !shown.insert((pkg.nevra.name.as_str(), pkg.nevra.arch.as_str())) {
                            continue;
                        }
                        println!("{} : {}", pkg.nevra, pkg.repo_id);
                    }
                }
                if *extras {
                    let repo_names: std::collections::HashSet<&str> = candidates.iter().map(|c| c.nevra.name.as_str()).collect();
                    for pkg in installed {
                        if matches_filter(&pkg.nevra.name) && !repo_names.contains(pkg.nevra.name.as_str()) {
                            println!("{}", pkg.nevra);
                        }
                    }
                }
                if *obsoletes {
                    for pkg in installed {
                        if matches_filter(&pkg.nevra.name) && candidates.iter().any(|c| pkg_obsoletes(c, &pkg.nevra.name)) {
                            println!("{}", pkg.nevra);
                        }
                    }
                }
                if *upgrades {
                    for pkg in installed {
                        if !matches_filter(&pkg.nevra.name) {
                            continue;
                        }
                        if let Some(newer) = candidates
                            .iter()
                            .filter(|c| c.nevra.name == pkg.nevra.name && c.nevra.arch == pkg.nevra.arch && c.nevra.evr().as_str().compare_evr(&pkg.nevra.evr()) == std::cmp::Ordering::Greater)
                            .max_by(|a, b| a.nevra.evr().as_str().compare_evr(b.nevra.evr().as_str()))
                        {
                            println!("{} -> {} ({})", pkg.nevra, newer.nevra, newer.repo_id);
                        }
                    }
                }
            } else {
                for pkg in installed {
                    if matches_filter(&pkg.nevra.name) {
                        println!("{}", pkg.nevra);
                    }
                }
            }
        }
        Command::Search { query, showrepo } => {
            let (_, candidates, _groups, _environments, ..) = load_candidates(&cli).await?;
            let query_lower = query.to_lowercase();
            for pkg in &candidates {
                if pkg.nevra.name.to_lowercase().contains(&query_lower) || pkg.summary.to_lowercase().contains(&query_lower) {
                    if *showrepo {
                        println!("{} : {} [{}]", pkg.nevra, pkg.summary, pkg.repo_id);
                    } else {
                        println!("{} : {}", pkg.nevra, pkg.summary);
                    }
                }
            }
        }
        Command::Provides { capability } => provides(&cli, &paths, capability).await?,
        Command::CheckUpgrade { packages, json } => check_upgrade(&cli, &paths, packages, *json).await?,
        Command::Repoquery {
            pattern,
            installed,
            available,
            whatprovides,
            whatrequires,
            whatconflicts,
            whatobsoletes,
            whatrecommends,
            whatenhances,
            whatsuggests,
            whatsupplements,
            recursive,
            arch,
            duplicates,
            info,
            queryformat,
            requires,
            provides,
            conflicts,
            obsoletes,
            recommends,
            enhances,
            suggests,
            supplements,
            srpm,
            location,
            nvr,
            envra,
            extras,
            upgrades,
            querytags,
        } => {
            repoquery(
                &cli,
                &paths,
                RepoqueryArgs {
                    pattern: pattern.as_deref(),
                    installed: *installed,
                    available: *available,
                    whatprovides: whatprovides.as_deref(),
                    whatrequires: whatrequires.as_deref(),
                    whatconflicts: whatconflicts.as_deref(),
                    whatobsoletes: whatobsoletes.as_deref(),
                    whatrecommends: whatrecommends.as_deref(),
                    whatenhances: whatenhances.as_deref(),
                    whatsuggests: whatsuggests.as_deref(),
                    whatsupplements: whatsupplements.as_deref(),
                    recursive: *recursive,
                    arch: arch.as_deref(),
                    duplicates: *duplicates,
                    info: *info,
                    queryformat: queryformat.as_deref(),
                    requires: *requires,
                    provides: *provides,
                    conflicts: *conflicts,
                    obsoletes: *obsoletes,
                    recommends: *recommends,
                    enhances: *enhances,
                    suggests: *suggests,
                    supplements: *supplements,
                    srpm: *srpm,
                    location: *location,
                    nvr: *nvr,
                    envra: *envra,
                    extras: *extras,
                    upgrades: *upgrades,
                    querytags: *querytags,
                },
            )
            .await?
        }
        Command::Info { package } => info(&cli, &paths, package).await?,
        Command::Leaves => leaves(&paths)?,
        Command::MakeCache => {
            let mut cli = cli;
            cli.refresh = true;
            let (repo_configs, _, _groups, _environments, ..) = load_candidates(&cli).await?;
            println!("Refreshed metadata for {} repo(s).", repo_configs.len());
        }
        Command::Copr { action } => match action {
            CoprAction::Enable { owner_project, chroot } => {
                let client = build_client(&cli)?;
                let path = rum_repo::copr::enable(&client, owner_project, chroot.as_deref(), &cli.repo_dir)
                    .await
                    .with_context(|| format!("enabling copr repo '{owner_project}'"))?;
                println!("Enabled {owner_project} -> {}", path.display());
            }
            CoprAction::Disable { owner_project } => {
                rum_repo::copr::disable(owner_project, &cli.repo_dir).with_context(|| format!("disabling copr repo '{owner_project}'"))?;
                println!("Disabled {owner_project}.");
            }
        },
        Command::Clean { what } => clean(&cli, *what)?,
        Command::Download { packages, destdir, srpm, releasever } => download(&cli, &paths, packages, destdir.as_deref(), *srpm, releasever.as_deref()).await?,
        Command::DebuginfoInstall { packages, dry_run } => {
            let debuginfo_names: Vec<String> = packages.iter().map(|p| format!("{p}-debuginfo")).collect();
            install(&cli, &paths, &debuginfo_names, *dry_run).await?
        }
        Command::Changelog { packages } => changelog(&paths, packages)?,
        Command::NeedsRestarting => needs_restarting()?,
        Command::Repo { action } => match action {
            RepoAction::List { all } => repo_list(&cli, *all)?,
            RepoAction::Info { id } => repo_info(&cli, id)?,
        },
        Command::Repoinfo { id } => repo_info(&cli, id)?,
        Command::Repolist { all } => repo_list(&cli, *all)?,
        Command::History { action } => match action {
            HistoryAction::List => {
                for entry in history::list(&paths)? {
                    println!("{:>4}  {:>10?}  {} package(s)", entry.id, entry.action, entry.packages.len());
                }
            }
            HistoryAction::Info { id } => {
                let entries = history::list(&paths)?;
                let entry = entries.iter().find(|e| e.id == *id).with_context(|| format!("no transaction with id {id}"))?;
                println!("Id     : {}", entry.id);
                println!("Action : {:?}", entry.action);
                for (nevra, reason) in &entry.packages {
                    println!("  {nevra} ({reason:?})");
                }
            }
            HistoryAction::Undo { id, dry_run } => {
                let entries = history::list(&paths)?;
                let entry = entries.iter().find(|e| e.id == *id).with_context(|| format!("no transaction with id {id}"))?;
                apply_history_inverse(&cli, &paths, history::inverse_of(entry), *dry_run).await?;
            }
            HistoryAction::Rollback { id, dry_run } => {
                let entries = history::list(&paths)?;
                anyhow::ensure!(entries.iter().any(|e| e.id == *id), "no transaction with id {id}");
                let after: Vec<_> = entries.into_iter().filter(|e| e.id > *id).collect();
                if after.is_empty() {
                    println!("Nothing to roll back — transaction {id} is already the most recent.");
                } else {
                    apply_history_inverse(&cli, &paths, history::composite_inverse(&after), *dry_run).await?;
                }
            }
            HistoryAction::Redo { id, dry_run } => {
                let entries = history::list(&paths)?;
                let entry = entries.iter().find(|e| e.id == *id).with_context(|| format!("no transaction with id {id}"))?;
                let names: Vec<String> = entry.packages.iter().map(|(nevra, _)| {
                    rum_core::Nevra::parse_nevra(nevra).or_else(|| rum_core::Nevra::parse_nvra(nevra)).map(|n| n.name).unwrap_or_else(|| nevra.clone())
                }).collect();
                match entry.action {
                    history::Action::Remove => {
                        rum_transaction::apply_remove_ex(&paths, &names, *dry_run, &protected_packages(&cli), true, cli.assume_yes)?;
                    }
                    history::Action::Install | history::Action::Upgrade | history::Action::Reinstall | history::Action::Downgrade => {
                        install(&cli, &paths, &names, *dry_run).await?;
                    }
                }
            }
        },
        Command::Versionlock { action } => match action {
            VersionlockAction::Add { package } => {
                let overlay = OverlayContext::load(&paths).context("loading overlay context")?;
                rum_transaction::versionlock::add(&paths, &overlay, package)?;
                println!("Locked {package}.");
            }
            VersionlockAction::Delete { package } => {
                rum_transaction::versionlock::delete(&paths, package)?;
                println!("Unlocked {package}.");
            }
            VersionlockAction::List => {
                for lock in rum_transaction::versionlock::list(&paths)? {
                    match (lock.evr, lock.arch) {
                        (Some(evr), Some(arch)) => println!("{} ({evr}.{arch})", lock.name),
                        _ => println!("{}", lock.name),
                    }
                }
            }
            VersionlockAction::Clear => {
                rum_transaction::versionlock::clear(&paths)?;
                println!("Cleared versionlock.");
            }
        },
        Command::ConfigManager { set_enabled, set_disabled, add_repo, action } => {
            for id in set_enabled {
                rum_repo::config_manager::set_enabled(&cli.repo_dir, id, true).with_context(|| format!("enabling '{id}'"))?;
                println!("Enabled {id}.");
            }
            for id in set_disabled {
                rum_repo::config_manager::set_enabled(&cli.repo_dir, id, false).with_context(|| format!("disabling '{id}'"))?;
                println!("Disabled {id}.");
            }
            for spec in add_repo {
                let (id, url) = spec.split_once('=').with_context(|| format!("--add-repo expects 'id=baseurl', got '{spec}'"))?;
                let vars = detect_vars(&cli);
                let path = rum_repo::config_manager::add_repo(&cli.repo_dir, id, url, &vars)?;
                println!("Added {id} -> {}", path.display());
            }
            match action {
                Some(ConfigManagerAction::Addrepo { from_repofile, id, set }) => {
                    for source in from_repofile {
                        let contents = if source.starts_with("http://") || source.starts_with("https://") {
                            let client = build_client(&cli)?;
                            client.get(source).send().await.with_context(|| format!("downloading '{source}'"))?.error_for_status().with_context(|| format!("downloading '{source}'"))?.text().await.with_context(|| format!("reading body of '{source}'"))?
                        } else {
                            std::fs::read_to_string(source).with_context(|| format!("reading '{source}'"))?
                        };
                        let filename = source.rsplit('/').next().filter(|s| !s.is_empty() && s.ends_with(".repo")).unwrap_or("downloaded.repo");
                        std::fs::create_dir_all(&cli.repo_dir).with_context(|| format!("creating {}", cli.repo_dir.display()))?;
                        let path = cli.repo_dir.join(filename);
                        std::fs::write(&path, &contents).with_context(|| format!("writing {}", path.display()))?;
                        println!("Added from {source} -> {}", path.display());
                    }
                    if let Some(id) = id {
                        let baseurl = set
                            .iter()
                            .find_map(|kv| kv.strip_prefix("baseurl="))
                            .with_context(|| "--id requires --set baseurl=<url>")?;
                        let vars = detect_vars(&cli);
                        let path = rum_repo::config_manager::add_repo(&cli.repo_dir, id, baseurl, &vars)?;
                        println!("Added {id} -> {}", path.display());
                    }
                }
                None => {}
            }
        }
        Command::Group { action } => match action {
            GroupAction::List { installed, available } => {
                let (_, _, groups, _environments, ..) = load_candidates(&cli).await?;
                let installed_names: std::collections::HashSet<String> = if *installed || *available {
                    let overlay = OverlayContext::load(&paths).context("loading overlay context")?;
                    overlay.base.iter().chain(&overlay.overlay).map(|p| p.nevra.name.clone()).collect()
                } else {
                    std::collections::HashSet::new()
                };
                let is_group_installed = |g: &rum_repo::Group| !g.packages.is_empty() && g.packages.iter().all(|p| installed_names.contains(p));
                let shown: Vec<&rum_repo::Group> = groups
                    .iter()
                    .filter(|g| if *installed { is_group_installed(g) } else { true })
                    .filter(|g| if *available { !is_group_installed(g) } else { true })
                    .collect();
                if shown.is_empty() {
                    println!("No groups available (no comps data in enabled repos).");
                } else {
                    println!("Available groups:");
                    for g in shown {
                        println!("  {} ({})", g.name, g.id);
                    }
                }
            }
            GroupAction::Info { id } => {
                let (_, _, groups, _environments, ..) = load_candidates(&cli).await?;
                let g = groups.iter().find(|g| g.id == *id).or_else(|| groups.iter().find(|g| g.name.eq_ignore_ascii_case(id))).with_context(|| format!("group '{id}' not found"))?;
                println!("Group   : {}", g.name);
                println!("Group-Id: {}", g.id);
                println!("Packages:");
                for p in &g.packages {
                    println!("  {p}");
                }
            }
            GroupAction::Install { id } => install(&cli, &paths, &[format!("@{id}")], false).await?,
            GroupAction::Upgrade { id } => {
                let (_, _, groups, _environments, ..) = load_candidates(&cli).await?;
                let g = groups.iter().find(|g| g.id == *id).or_else(|| groups.iter().find(|g| g.name.eq_ignore_ascii_case(id))).with_context(|| format!("group '{id}' not found"))?;
                upgrade(&cli, &paths, &g.packages, false, &AdvisoryUpgradeFilter::NONE).await?;
            }
            GroupAction::Remove { id } => {
                let (_, _, groups, _environments, ..) = load_candidates(&cli).await?;
                let g = groups.iter().find(|g| g.id == *id).or_else(|| groups.iter().find(|g| g.name.eq_ignore_ascii_case(id))).with_context(|| format!("group '{id}' not found"))?;
                let overlay_for_scope = OverlayContext::load(&paths).context("loading overlay context")?;
                let removed: Vec<&Package> = overlay_for_scope.base.iter().chain(&overlay_for_scope.overlay).filter(|p| g.packages.iter().any(|n| *n == p.nevra.name)).collect();
                let scope = removed_dependency_closure_names(&removed, &overlay_for_scope);
                rum_transaction::apply_remove(&paths, &g.packages, false, &protected_packages(&cli), cli.assume_yes)?;
                if cli.main_conf.clean_requirements_on_remove && !cli.no_autoremove {
                    autoremove_ex(&cli, &paths, false, Some(&scope))?;
                }
            }
        },
        Command::Environment { action } => match action {
            EnvironmentAction::List { installed, available } => {
                let (_, _, groups, environments, ..) = load_candidates(&cli).await?;
                let installed_names: std::collections::HashSet<String> = if *installed || *available {
                    let overlay = OverlayContext::load(&paths).context("loading overlay context")?;
                    overlay.base.iter().chain(&overlay.overlay).map(|p| p.nevra.name.clone()).collect()
                } else {
                    std::collections::HashSet::new()
                };
                let is_env_installed = |e: &rum_repo::Environment| {
                    !e.group_ids.is_empty()
                        && e.group_ids.iter().all(|gid| {
                            groups.iter().find(|g| &g.id == gid).is_some_and(|g| !g.packages.is_empty() && g.packages.iter().all(|p| installed_names.contains(p)))
                        })
                };
                let shown: Vec<&rum_repo::Environment> = environments
                    .iter()
                    .filter(|e| if *installed { is_env_installed(e) } else { true })
                    .filter(|e| if *available { !is_env_installed(e) } else { true })
                    .collect();
                if shown.is_empty() {
                    println!("No environments available (no comps data in enabled repos).");
                } else {
                    println!("Available environments:");
                    for e in shown {
                        println!("  {} ({})", e.name, e.id);
                    }
                }
            }
            EnvironmentAction::Info { id } => {
                let (_, _, groups, environments, ..) = load_candidates(&cli).await?;
                let e = environments.iter().find(|e| e.id == *id).or_else(|| environments.iter().find(|e| e.name.eq_ignore_ascii_case(id))).with_context(|| format!("environment '{id}' not found"))?;
                println!("Environment   : {}", e.name);
                println!("Environment-Id: {}", e.id);
                println!("Groups:");
                for group_id in &e.group_ids {
                    let name = groups.iter().find(|g| &g.id == group_id).map(|g| g.name.as_str()).unwrap_or(group_id.as_str());
                    println!("  {name} ({group_id})");
                }
            }
            EnvironmentAction::Install { id } => install(&cli, &paths, &[format!("@^{id}")], false).await?,
            EnvironmentAction::Upgrade { id } => {
                let (_, _, groups, environments, ..) = load_candidates(&cli).await?;
                let e = environments.iter().find(|e| e.id == *id).or_else(|| environments.iter().find(|e| e.name.eq_ignore_ascii_case(id))).with_context(|| format!("environment '{id}' not found"))?;
                let mut pkgs = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for group_id in &e.group_ids {
                    let Some(g) = groups.iter().find(|g| &g.id == group_id) else { continue };
                    for p in &g.packages {
                        if seen.insert(p.clone()) {
                            pkgs.push(p.clone());
                        }
                    }
                }
                anyhow::ensure!(!pkgs.is_empty(), "environment '{id}' has no known packages to upgrade (no comps data for its groups)");
                upgrade(&cli, &paths, &pkgs, false, &AdvisoryUpgradeFilter::NONE).await?;
            }
            EnvironmentAction::Remove { id } => {
                let (_, _, groups, environments, ..) = load_candidates(&cli).await?;
                let e = environments.iter().find(|e| e.id == *id).or_else(|| environments.iter().find(|e| e.name.eq_ignore_ascii_case(id))).with_context(|| format!("environment '{id}' not found"))?;
                let mut pkgs = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for group_id in &e.group_ids {
                    let Some(g) = groups.iter().find(|g| &g.id == group_id) else { continue };
                    for p in &g.packages {
                        if seen.insert(p.clone()) {
                            pkgs.push(p.clone());
                        }
                    }
                }
                anyhow::ensure!(!pkgs.is_empty(), "environment '{id}' has no known packages to remove (no comps data for its groups)");
                let overlay_for_scope = OverlayContext::load(&paths).context("loading overlay context")?;
                let removed: Vec<&Package> = overlay_for_scope.base.iter().chain(&overlay_for_scope.overlay).filter(|p| pkgs.iter().any(|n| *n == p.nevra.name)).collect();
                let scope = removed_dependency_closure_names(&removed, &overlay_for_scope);
                rum_transaction::apply_remove(&paths, &pkgs, false, &protected_packages(&cli), cli.assume_yes)?;
                if cli.main_conf.clean_requirements_on_remove && !cli.no_autoremove {
                    autoremove_ex(&cli, &paths, false, Some(&scope))?;
                }
            }
        },
        Command::Module { action } => match action {
            ModuleAction::List => {
                let (_, _, _, _, modules, module_defaults, ..) = load_candidates(&cli).await?;
                anyhow::ensure!(!modules.is_empty(), "no modular content in enabled repos");
                let module_paths = OverlayPaths::detect()?;
                let enabled_streams = rum_transaction::module_state::enabled(&module_paths)?;
                let disabled_modules = rum_transaction::module_state::disabled(&module_paths)?;
                let mut names: Vec<&str> = modules.iter().map(|m| m.name.as_str()).collect();
                names.sort();
                names.dedup();
                println!("{:<20} {:<10} {:<24} Summary", "Name", "Stream", "Profiles");
                for name in names {
                    let mut streams: Vec<&rum_repo::Module> = modules.iter().filter(|m| m.name == name).collect();
                    streams.sort_by(|a, b| a.stream.cmp(&b.stream));
                    for m in streams {
                        let tag = if disabled_modules.contains(&m.name) {
                            " [d]"
                        } else if enabled_streams.get(&m.name).map(String::as_str) == Some(m.stream.as_str()) {
                            " [e]"
                        } else if module_defaults.get(&m.name).map(String::as_str) == Some(m.stream.as_str()) {
                            " [default]"
                        } else {
                            ""
                        };
                        let profiles: Vec<&str> = m.profiles.iter().map(|(n, _)| n.as_str()).collect();
                        println!("{:<20} {:<10} {:<24} {}{}", m.name, m.stream, profiles.join(", "), m.summary, tag);
                    }
                }
            }
            ModuleAction::Info { name } => {
                let (_, _, _, _, modules, module_defaults, ..) = load_candidates(&cli).await?;
                let (mod_name, stream_filter) = name.split_once(':').map(|(n, s)| (n.to_string(), Some(s.to_string()))).unwrap_or_else(|| (name.clone(), None));
                let mut matches: Vec<&rum_repo::Module> = modules.iter().filter(|m| m.name == mod_name && stream_filter.as_deref().is_none_or(|s| s == m.stream)).collect();
                anyhow::ensure!(!matches.is_empty(), "module '{name}' not found");
                matches.sort_by(|a, b| a.stream.cmp(&b.stream));
                for m in matches {
                    let is_default = module_defaults.get(&m.name).map(String::as_str) == Some(m.stream.as_str());
                    println!("Name         : {}", m.name);
                    println!("Stream       : {}{}", m.stream, if is_default { " [d]" } else { "" });
                    println!("Version      : {}", m.version);
                    println!("Context      : {}", m.context);
                    println!("Architecture : {}", m.arch);
                    println!("Profiles     :");
                    for (pname, rpms) in &m.profiles {
                        println!("  {} : {}", pname, rpms.join(", "));
                    }
                    println!("Summary      : {}", m.summary);
                    println!("Description  : {}", m.description);
                    println!("Artifacts    :");
                    for a in &m.artifacts {
                        println!("  {a}");
                    }
                    println!();
                }
            }
            ModuleAction::Enable { name } => {
                let (_, _, _, _, modules, module_defaults, ..) = load_candidates(&cli).await?;
                let (mod_name, stream) = name
                    .split_once(':')
                    .map(|(n, s)| (n.to_string(), s.to_string()))
                    .or_else(|| module_defaults.get(name).map(|s| (name.clone(), s.clone())))
                    .with_context(|| format!("module '{name}' has no default stream; specify one explicitly as 'name:stream'"))?;
                anyhow::ensure!(modules.iter().any(|m| m.name == mod_name && m.stream == stream), "module '{mod_name}:{stream}' not found in any enabled repo");
                rum_transaction::module_state::enable(&OverlayPaths::detect()?, &mod_name, &stream)?;
                println!("Enabled module stream '{mod_name}:{stream}'.");
            }
            ModuleAction::Disable { name } => {
                rum_transaction::module_state::disable(&OverlayPaths::detect()?, name)?;
                println!("Disabled module '{name}' — every stream's packages are now blocked.");
            }
            ModuleAction::Reset { name } => {
                rum_transaction::module_state::reset(&OverlayPaths::detect()?, name)?;
                println!("Reset module '{name}' to its repo-default state.");
            }
            ModuleAction::Install { name } => {
                let (_, _, _, _, modules, module_defaults, ..) = load_candidates(&cli).await?;
                let (rest, profile) = name.split_once('/').map(|(r, p)| (r.to_string(), Some(p.to_string()))).unwrap_or_else(|| (name.clone(), None));
                let (mod_name, stream) = rest
                    .split_once(':')
                    .map(|(n, s)| (n.to_string(), s.to_string()))
                    .or_else(|| module_defaults.get(&rest).map(|s| (rest.clone(), s.clone())))
                    .with_context(|| format!("module '{rest}' has no default stream; specify one explicitly as 'name:stream'"))?;
                let m = modules.iter().find(|m| m.name == mod_name && m.stream == stream).with_context(|| format!("module '{mod_name}:{stream}' not found in any enabled repo"))?;
                let rpms: Vec<String> = match &profile {
                    Some(p) => m.profiles.iter().find(|(n, _)| n == p).map(|(_, rpms)| rpms.clone()).with_context(|| format!("module '{mod_name}:{stream}' has no profile '{p}'"))?,
                    None => m
                        .profiles
                        .iter()
                        .find(|(n, _)| n == "default")
                        .map(|(_, rpms)| rpms.clone())
                        .unwrap_or_else(|| m.profiles.iter().flat_map(|(_, rpms)| rpms.iter().cloned()).collect::<std::collections::HashSet<_>>().into_iter().collect()),
                };
                anyhow::ensure!(!rpms.is_empty(), "module '{mod_name}:{stream}' has no packages to install");
                let module_paths = OverlayPaths::detect()?;
                rum_transaction::module_state::enable(&module_paths, &mod_name, &stream)?;
                println!("Enabled module stream '{mod_name}:{stream}'.");
                install(&cli, &paths, &rpms, false).await?;
            }
            ModuleAction::Remove { name } => {
                let (_, _, _, _, modules, module_defaults, ..) = load_candidates(&cli).await?;
                let (rest, profile) = name.split_once('/').map(|(r, p)| (r.to_string(), Some(p.to_string()))).unwrap_or_else(|| (name.clone(), None));
                let (mod_name, stream) = rest
                    .split_once(':')
                    .map(|(n, s)| (n.to_string(), s.to_string()))
                    .or_else(|| module_defaults.get(&rest).map(|s| (rest.clone(), s.clone())))
                    .with_context(|| format!("module '{rest}' has no default stream; specify one explicitly as 'name:stream'"))?;
                let m = modules.iter().find(|m| m.name == mod_name && m.stream == stream).with_context(|| format!("module '{mod_name}:{stream}' not found in any enabled repo"))?;
                let rpms: Vec<String> = match &profile {
                    Some(p) => m.profiles.iter().find(|(n, _)| n == p).map(|(_, rpms)| rpms.clone()).with_context(|| format!("module '{mod_name}:{stream}' has no profile '{p}'"))?,
                    None => m.profiles.iter().flat_map(|(_, rpms)| rpms.iter().cloned()).collect::<std::collections::HashSet<_>>().into_iter().collect(),
                };
                anyhow::ensure!(!rpms.is_empty(), "module '{mod_name}:{stream}' has no packages to remove");
                let overlay_for_scope = OverlayContext::load(&paths).context("loading overlay context")?;
                let removed: Vec<&Package> = overlay_for_scope.base.iter().chain(&overlay_for_scope.overlay).filter(|p| rpms.iter().any(|n| *n == p.nevra.name)).collect();
                let scope = removed_dependency_closure_names(&removed, &overlay_for_scope);
                rum_transaction::apply_remove(&paths, &rpms, false, &protected_packages(&cli), cli.assume_yes)?;
                if cli.main_conf.clean_requirements_on_remove && !cli.no_autoremove {
                    autoremove_ex(&cli, &paths, false, Some(&scope))?;
                }
            }
        },
        Command::Advisory { action } => advisory(&cli, &paths, action).await?,
        Command::Offline => anyhow::bail!("offline transactions are not yet implemented: rum has no offline-transaction staging or systemd integration"),
        Command::OfflineUpgrade => anyhow::bail!("offline-upgrade is not yet implemented: see 'rum offline'"),
        Command::OfflineDistrosync => anyhow::bail!("offline-distrosync is not yet implemented: see 'rum offline'"),
        Command::Reposync { download_path, newest_only, norepopath } => reposync(&cli, download_path.as_deref(), *newest_only, *norepopath).await?,
        Command::Repomanage { path, new, keep } => repomanage(path, *new, *keep)?,
        Command::Repoclosure { pkg } => repoclosure(&cli, pkg).await?,
        Command::BuildDep { spec, define, dry_run } => builddep(&cli, &paths, spec, define, *dry_run).await?,
        Command::Replay => anyhow::bail!("replay is not yet implemented: rum's history log is view-only, not replayable"),
        Command::Do => anyhow::bail!("do is not yet implemented"),
        Command::Automatic { timer, downloadupdates, no_downloadupdates, installupdates, no_installupdates } => {
            let overrides = AutomaticOverrides {
                timer: *timer,
                download_updates: if *downloadupdates { Some(true) } else if *no_downloadupdates { Some(false) } else { None },
                apply_updates: if *installupdates { Some(true) } else if *no_installupdates { Some(false) } else { None },
            };
            let mut cli = cli;
            automatic(&mut cli, &paths, overrides).await?
        }
        Command::Completions { shell } => {
            let mut cmd = <Cli as clap::CommandFactory>::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(*shell, &mut cmd, name, &mut std::io::stdout());
        }
    }
    Ok(())
}

/// True when running as `uid 0` — the only case where rum's writes to
/// system paths (rpmdb, `/etc/yum.repos.d`, `/var/lib/rakuos`) will actually
/// succeed.
fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Returns `Some(display name)` for every subcommand that writes to real
/// system state — the overlay/base rpmdb via an `rpm` transaction, repo
/// files under `/etc/yum.repos.d`, or rum's own state dir under
/// `/var/lib/rakuos` (history log, versionlock, module state) — so `main`
/// can reject it up front instead of failing partway through a transaction
/// with a raw permission-denied error. Read-only/query commands (`list`,
/// `search`, `info`, `repoquery`, `changelog`, `history`, `paths`, ...)
/// return `None` and run unprivileged, same as dnf.
fn command_needs_root(cmd: &Command) -> Option<&'static str> {
    match cmd {
        Command::Install { .. } => Some("install"),
        Command::Remove { .. } => Some("remove"),
        Command::Upgrade { .. } => Some("upgrade"),
        Command::SystemUpgrade { .. } => Some("system-upgrade"),
        Command::Reinstall { .. } => Some("reinstall"),
        Command::Downgrade { .. } => Some("downgrade"),
        Command::DistroSync { .. } => Some("distro-sync"),
        Command::Autoremove { .. } => Some("autoremove"),
        Command::Swap { .. } => Some("swap"),
        Command::Mark { .. } => Some("mark"),
        Command::DebuginfoInstall { .. } => Some("debuginfo-install"),
        Command::Clean { .. } => Some("clean"),
        Command::Copr { action: CoprAction::Enable { .. } | CoprAction::Disable { .. } } => Some("copr"),
        Command::Versionlock { action: VersionlockAction::Add { .. } | VersionlockAction::Delete { .. } | VersionlockAction::Clear } => Some("versionlock"),
        Command::ConfigManager { .. } => Some("config-manager"),
        Command::Group { action: GroupAction::Install { .. } | GroupAction::Remove { .. } | GroupAction::Upgrade { .. } } => Some("group"),
        Command::Environment { action: EnvironmentAction::Install { .. } | EnvironmentAction::Remove { .. } | EnvironmentAction::Upgrade { .. } } => Some("environment"),
        Command::Module { action: ModuleAction::Enable { .. } | ModuleAction::Disable { .. } | ModuleAction::Reset { .. } | ModuleAction::Install { .. } | ModuleAction::Remove { .. } } => Some("module"),
        Command::History { action: HistoryAction::Undo { .. } | HistoryAction::Redo { .. } | HistoryAction::Rollback { .. } } => Some("history"),
        _ => None,
    }
}

/// `true` for a root-gated command that was also passed `--dry-run` — see
/// the root check in `main`, which exempts these since they don't write
/// anything. Commands with no `dry_run` field at all (`clean`, `copr`,
/// `versionlock`, `config-manager`, `group`, `environment`, `module`,
/// `swap`, `mark`) always mutate real state regardless of any flag, so
/// they're not listed here.
fn command_is_dry_run(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Install { dry_run: true, .. }
            | Command::Remove { dry_run: true, .. }
            | Command::Upgrade { dry_run: true, .. }
            | Command::SystemUpgrade { dry_run: true, .. }
            | Command::Reinstall { dry_run: true, .. }
            | Command::Downgrade { dry_run: true, .. }
            | Command::DistroSync { dry_run: true, .. }
            | Command::Autoremove { dry_run: true }
            | Command::DebuginfoInstall { dry_run: true, .. }
            | Command::History { action: HistoryAction::Undo { dry_run: true, .. } | HistoryAction::Redo { dry_run: true, .. } | HistoryAction::Rollback { dry_run: true, .. } }
    )
}

/// Fetches every enabled repo's package list, deduplicated by whatever the
/// caller does with it — shared by `install`, `upgrade`, and `search` so
/// they all see the same repo set the same way. `cli.refresh` forces a
/// re-download ignoring each repo's `metadata_expire=`; `cli.cacheonly`
/// forbids any network access, erroring if nothing is cached; repo metadata
/// is cached under `<cache_dir>/repos` (kept separate from
/// `download_all`'s package-download cache in the same `cache_dir`).
/// `cli.repo`/`--enablerepo`/`--disablerepo` narrow which repos are used at
/// all (see [`effective_repo_configs`]); `cli.exclude` drops matching
/// package names from the candidate pool entirely, so the resolver never
/// considers them (dnf's `-x`/`--exclude`).
async fn load_candidates(cli: &Cli) -> Result<(Vec<RepoConfig>, Vec<Package>, Vec<rum_repo::Group>, Vec<rum_repo::Environment>, Vec<rum_repo::Module>, std::collections::HashMap<String, String>, Vec<Package>, Vec<rum_repo::Advisory>)> {
    load_candidates_with_vars(cli, detect_vars(cli), cli.refresh).await
}

/// Same as [`load_candidates`], but forces `refresh` on regardless of
/// `cli.refresh`/`metadata_expire=` — for callers like `check-upgrade`
/// where reporting against stale cached metadata is worse than useless.
async fn load_candidates_force_refresh(cli: &Cli) -> Result<(Vec<RepoConfig>, Vec<Package>, Vec<rum_repo::Group>, Vec<rum_repo::Environment>, Vec<rum_repo::Module>, std::collections::HashMap<String, String>, Vec<Package>, Vec<rum_repo::Advisory>)> {
    load_candidates_with_vars(cli, detect_vars(cli), true).await
}

/// `RepoVars::detect()`, with `--forcearch` applied on top — dnf5's
/// `--forcearch` overrides `$basearch` repo-URL expansion *and* every
/// resolver call's `host_arch`, so every caller that would otherwise call
/// `RepoVars::detect()` directly should go through this instead.
fn detect_vars(cli: &Cli) -> rum_repo::RepoVars {
    let mut vars = rum_repo::RepoVars::detect_with_varsdir(cli.main_conf.varsdir.as_deref());
    if let Some(arch) = &cli.forcearch {
        vars.basearch = arch.clone();
    }
    vars
}

/// Same as [`detect_vars`], but just the `basearch` — for the many call
/// sites that only need `host_arch` for the resolver, not a full
/// [`RepoVars`](rum_repo::RepoVars).
fn detect_host_arch(cli: &Cli) -> String {
    cli.forcearch.clone().unwrap_or_else(|| rum_repo::RepoVars::detect().basearch)
}

/// Same as [`load_candidates`], but against an explicitly supplied
/// [`RepoVars`](rum_repo::RepoVars) instead of the host's own detected
/// `$releasever`/`$basearch` — the hook `system-upgrade download
/// --releasever=X` needs to point repo metadata loading at a *different*
/// major release's repos without touching the running system's own view.
async fn load_candidates_with_vars(cli: &Cli, vars: rum_repo::RepoVars, refresh: bool) -> Result<(Vec<RepoConfig>, Vec<Package>, Vec<rum_repo::Group>, Vec<rum_repo::Environment>, Vec<rum_repo::Module>, std::collections::HashMap<String, String>, Vec<Package>, Vec<rum_repo::Advisory>)> {
    let mut all_configs = rum_repo::load_repo_configs(&cli.repo_dir, &vars)
        .with_context(|| format!("loading repo configs from {}", cli.repo_dir.display()))?;
    apply_metadata_expire_default(&mut all_configs, &cli.main_conf);
    // `--repofrompath name,url` (dnf): an ad-hoc, enabled-for-this-run-only
    // repo — added directly to the in-memory config list rather than
    // written to `cli.repo_dir`, since (unlike `config-manager addrepo`)
    // it's not meant to persist past this invocation.
    for spec in &cli.repofrompath {
        let (id, url) = spec.split_once(',').with_context(|| format!("--repofrompath '{spec}' must be 'name,url'"))?;
        all_configs.push(RepoConfig {
            id: id.to_string(),
            name: id.to_string(),
            base_url: Some(url.trim_end_matches('/').to_string()),
            mirrorlist: None,
            metalink: None,
            gpgcheck: false,
            gpgkeys: Vec::new(),
            metadata_expire: rum_repo::DEFAULT_METADATA_EXPIRE,
            enabled: true,
            priority: rum_core::default_repo_priority(),
            cost: rum_core::default_repo_cost(),
            exclude: Vec::new(),
            includepkgs: Vec::new(),
            skip_if_unavailable: None,
            proxy: None,
            proxy_username: None,
            proxy_password: None,
            username: None,
            password: None,
            sslcacert: None,
            sslclientcert: None,
            sslclientkey: None,
        });
    }
    let repo_configs = effective_repo_configs(cli, all_configs);
    anyhow::ensure!(!repo_configs.is_empty(), "no enabled repos found in {}", cli.repo_dir.display());

    let repo_cache_root = cli.cache_dir.join("repos");
    let client = build_client(cli)?;
    let mut candidates = Vec::new();
    let mut groups = Vec::new();
    let mut environments = Vec::new();
    let mut modules = Vec::new();
    let mut module_defaults = std::collections::HashMap::new();
    let mut advisories = Vec::new();
    eprintln!("Updating and loading repositories:");
    // Each repo's metadata load (mirror probe + repomd + primary.xml +
    // comps/modules) is an independent round of network round-trips, so
    // fan them out across repos concurrently instead of awaiting them one
    // at a time — with a dozen-plus enabled repos (common on rakuos images)
    // sequential loading was the dominant wall-clock cost of every install.
    // Results are still applied in original config order below so output
    // and error attribution stay deterministic.
    let mut load_set = tokio::task::JoinSet::new();
    for (idx, cfg) in repo_configs.iter().cloned().enumerate() {
        let client = client.clone();
        let repo_cache_root = repo_cache_root.clone();
        let cacheonly = cli.cacheonly;
        load_set.spawn(async move {
            let result = rum_repo::load_repo_ex(&client, &cfg, &repo_cache_root, refresh, cacheonly).await;
            (idx, cfg, result)
        });
    }
    let mut loaded: Vec<Option<(RepoConfig, Result<rum_repo::Repo>)>> = (0..repo_configs.len()).map(|_| None).collect();
    while let Some(joined) = load_set.join_next().await {
        let (idx, cfg, result) = joined.context("repo metadata load task panicked")?;
        loaded[idx] = Some((cfg, result));
    }
    for slot in loaded {
        let (cfg, result) = slot.expect("every repo index is spawned exactly once");
        match result {
            Ok(repo) => {
                candidates.extend(repo.packages);
                groups.extend(repo.groups);
                environments.extend(repo.environments);
                modules.extend(repo.modules);
                module_defaults.extend(repo.module_defaults);
                advisories.extend(repo.advisories);
            }
            // `skip_if_unavailable=` (dnf default: false): an unreachable
            // repo shouldn't abort every other repo's resolution too, as
            // long as the admin has explicitly opted into that risk. A
            // repo's own `skip_if_unavailable=` overrides the `[main]`
            // default when set, matching dnf's per-section semantics.
            Err(e) if cfg.skip_if_unavailable.unwrap_or(cli.main_conf.skip_if_unavailable) => {
                eprintln!("warning: skipping unavailable repo '{}': {e:#}", cfg.id);
            }
            Err(e) => return Err(e).with_context(|| format!("loading repo '{}'", cfg.id)),
        }
    }
    eprintln!("Repositories loaded.");
    // Kept from before the exact-NEVRA collapse below, for download-fallback
    // purposes only: when a package's own repo 404s mid-download, this is
    // what lets `download_all_ex` retry an *identical* build mirrored under
    // another repo id, rather than only ever falling back to a different
    // (and possibly transaction-breaking) EVR. The resolver itself must
    // never see these duplicates — only `alt_candidates` downstream does.
    let raw_candidates_for_fallback = candidates.clone();
    // The exact same build (identical NEVRA) can legitimately be listed by
    // more than one enabled repo (e.g. a package present in both `fedora`
    // and `updates`, or two mirrored/overlapping repos). Left as separate
    // `Package` entries, each becomes its own SAT variable, which silently
    // defeats the resolver's own Conflicts/at-most-one-per-version clauses:
    // a clause attached to one duplicate's variable does nothing to stop
    // the solver satisfying demand via the *other* duplicate's variable
    // (this is exactly how `nodejs22-bin` and `nodejs24-bin` — each
    // `Conflicts: alternative-for(nodejs-bin)` against the other — ended up
    // selected simultaneously). Collapse to one candidate per NEVRA,
    // keeping whichever copy sorts first by repo priority (dnf convention:
    // lower wins) so this doesn't change which repo a package's metadata
    // is attributed to.
    {
        let mut best: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (i, pkg) in candidates.iter().enumerate() {
            let key = pkg.nevra.to_string();
            match best.get(&key) {
                Some(&j) if (candidates[j].repo_priority, candidates[j].repo_cost) <= (pkg.repo_priority, pkg.repo_cost) => {}
                _ => {
                    best.insert(key, i);
                }
            }
        }
        let mut keep: Vec<bool> = vec![false; candidates.len()];
        for &i in best.values() {
            keep[i] = true;
        }
        let mut i = 0;
        candidates.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
    }
    let mut raw_candidates_for_fallback = raw_candidates_for_fallback;
    if !cli.exclude.is_empty() {
        candidates.retain(|pkg| !cli.exclude.iter().any(|pat| glob_match(pat, &pkg.nevra.name)));
        raw_candidates_for_fallback.retain(|pkg| !cli.exclude.iter().any(|pat| glob_match(pat, &pkg.nevra.name)));
    }
    // `includepkgs=` (dnf: when non-empty, only these names may be
    // considered as resolver candidates at all — including as a
    // dependency target, same as dnf's own repo-level allow-list).
    if !cli.main_conf.includepkgs.is_empty() {
        candidates.retain(|pkg| cli.main_conf.includepkgs.iter().any(|pat| glob_match(pat, &pkg.nevra.name)));
        raw_candidates_for_fallback.retain(|pkg| cli.main_conf.includepkgs.iter().any(|pat| glob_match(pat, &pkg.nevra.name)));
    }
    // Per-repo `exclude=`/`includepkgs=` (dnf: a `.repo` section's own
    // exclude/includepkgs only ever narrows candidates sourced from *that*
    // repo, layered on top of the global lists above — a package excluded
    // globally stays excluded everywhere, but a per-repo exclude on repo A
    // doesn't touch an identically-named build offered by repo B).
    let repo_exclude: std::collections::HashMap<&str, &[String]> =
        repo_configs.iter().filter(|c| !c.exclude.is_empty()).map(|c| (c.id.as_str(), c.exclude.as_slice())).collect();
    let repo_include: std::collections::HashMap<&str, &[String]> =
        repo_configs.iter().filter(|c| !c.includepkgs.is_empty()).map(|c| (c.id.as_str(), c.includepkgs.as_slice())).collect();
    if !repo_exclude.is_empty() {
        let keep = |pkg: &Package| !repo_exclude.get(pkg.repo_id.as_str()).is_some_and(|pats| pats.iter().any(|pat| glob_match(pat, &pkg.nevra.name)));
        candidates.retain(keep);
        raw_candidates_for_fallback.retain(keep);
    }
    if !repo_include.is_empty() {
        let keep = |pkg: &Package| repo_include.get(pkg.repo_id.as_str()).is_none_or(|pats| pats.iter().any(|pat| glob_match(pat, &pkg.nevra.name)));
        candidates.retain(keep);
        raw_candidates_for_fallback.retain(keep);
    }
    // `--from-repo`/`--from-vendor`: narrows candidates, same shape as the
    // `--exclude`/`includepkgs` filters above. `raw_candidates_for_fallback`
    // deliberately keeps every repo's copy regardless (download-fallback
    // needs to see a mirrored build even if it's not the one the resolver
    // was allowed to pick).
    candidates = apply_from_filters(cli, candidates);

    // Modularity filtering: a package that belongs to some module:stream
    // build (i.e. its NEVRA appears in a module's `artifacts`) is only a
    // real candidate if that module's *active* stream (an explicit `module
    // enable`, or the repo's own `modulemd-defaults` otherwise) is the one
    // this build belongs to — same as dnf hiding every non-active stream's
    // RPMs from plain `install`/`list`/resolution. A `module disable`
    // blocks every stream outright. Non-modular packages (the vast
    // majority of any repo) are never touched by any of this.
    if !modules.is_empty() {
        let module_paths = rum_overlay::OverlayPaths::detect().unwrap_or_else(|_| rum_overlay::OverlayPaths {
            mode: rum_overlay::OverlayMode::Standalone,
            overlay_upper: std::path::PathBuf::new(),
            state_dir: std::path::PathBuf::from("/var/lib/rum"),
            installroot: None,
        });
        let enabled_streams = rum_transaction::module_state::enabled(&module_paths).unwrap_or_default();
        let disabled_modules = rum_transaction::module_state::disabled(&module_paths).unwrap_or_default();

        let mut artifact_module: std::collections::HashMap<(String, String, String), (String, String)> = std::collections::HashMap::new();
        for m in &modules {
            for a in &m.artifacts {
                if let Some(n) = rum_core::Nevra::parse_nevra(a) {
                    let evr = n.evr();
                    artifact_module.insert((n.name, evr, n.arch), (m.name.clone(), m.stream.clone()));
                }
            }
        }

        candidates.retain(|pkg| {
            let Some((mod_name, mod_stream)) = artifact_module.get(&(pkg.nevra.name.clone(), pkg.nevra.evr(), pkg.nevra.arch.clone())) else {
                return true;
            };
            if disabled_modules.contains(mod_name) {
                return false;
            }
            let active_stream = enabled_streams.get(mod_name).or_else(|| module_defaults.get(mod_name));
            active_stream.is_some_and(|s| s == mod_stream)
        });
    }

    Ok((repo_configs, candidates, groups, environments, modules, module_defaults, raw_candidates_for_fallback, advisories))
}

/// dnf-style glob support for package-name arguments (`rum remove
/// 'firefox*'`, `rum upgrade 'kernel*'`) — every command below otherwise
/// only ever does an exact, literal name match. Expansion happens once, up
/// front, against `pool` (the relevant "what names actually exist right
/// now" list for that command — installed packages for remove/upgrade/
/// reinstall/downgrade/distro-sync/check-upgrade/mark, available repo
/// candidates for install), so every existing exact-match code path
/// downstream (including its error messages) keeps working unmodified on
/// the expanded, literal result.
///
/// A pattern with no glob metacharacter is passed through completely
/// unchanged, even if it matches nothing in `pool` — that keeps the
/// existing "package 'x' is not installed" / "nothing provides 'x'" error
/// paths intact for plain typos. A pattern that *is* a glob is expanded
/// against `pool` (deduplicated, first-match order) and, if it matches
/// nothing at all, dropped with a warning instead of falling through to
/// become a nonsensical literal name.
fn expand_name_patterns(patterns: &[String], pool: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for pat in patterns {
        if !rum_core::is_glob_pattern(pat) {
            out.push(pat.clone());
            continue;
        }
        let mut matched = false;
        for &name in pool {
            if rum_core::glob_match(pat, name) && !out.iter().any(|n| n == name) {
                out.push(name.to_string());
                matched = true;
            }
        }
        if !matched {
            println!("No match for pattern: {pat}");
        }
    }
    out
}

/// Prints an informational note for any name in `names` that resolves to a
/// base-image package but has no overlay copy — these are silently excluded
/// from the overlay-only candidate pool by every mutating command, so
/// without this the user would otherwise see "nothing to do" with no
/// explanation for why an installed package was skipped.
fn warn_base_only_names(names: &[String], overlay: &OverlayContext, verb: &str) {
    for name in names {
        let in_overlay = overlay.overlay.iter().any(|p| &p.nevra.name == name);
        let in_base = overlay.base.iter().any(|p| &p.nevra.name == name);
        if in_base && !in_overlay {
            println!("'{name}' is provided by the base image; rum will not layer a copy into the overlay to {verb} it — skipping.");
        }
    }
}

/// Finds, for each installed package matching `names` (or every installed
/// package if `names` is empty), the highest-EVR same-name candidate across
/// enabled repos that's strictly newer than what's installed, then runs it
/// through the same download/verify/apply pipeline as `install` — `rpm -U`
/// upgrades in place, it doesn't need a separate code path from install.
///
/// Always restricted to `overlay.overlay` — never `overlay.base` — even
/// when a name is given explicitly. The base image is read-only, so an
/// "upgrade" of a base package can't upgrade anything in place; it would
/// silently layer a duplicate copy into the overlay, permanently shadowing
/// the base package from then on with no way back short of a soft reset.
/// dnf, working from a single merged "installed" view, has no equivalent
/// distinction to make — this is rum's whole reason to exist, so it must
/// never be an accident a bare `rum upgrade <name>` falls into. A name that
/// only exists in the base image is reported, not silently dropped.
/// `rum system-upgrade` — mode-aware, see the `Command::SystemUpgrade` doc
/// comment for the full split. Standalone dispatches to the
/// download/reboot/execute/clean/log/status offline-transaction machinery
/// in `offline.rs`; Split runs an ordinary overlay upgrade followed by a
/// `bootc upgrade` staging check, and refuses every Standalone-only
/// subcommand.
async fn system_upgrade(cli: &Cli, paths: &OverlayPaths, action: Option<&SystemUpgradeAction>, dry_run: bool) -> Result<()> {
    match &paths.mode {
        OverlayMode::Split { .. } => {
            anyhow::ensure!(
                action.is_none(),
                "system-upgrade subcommands are Standalone-only — this is a RakuOS overlay system. \
                 A new major release arrives as a new base image; run `rum system-upgrade` with no \
                 subcommand to upgrade overlay packages and stage it."
            );
            upgrade(cli, paths, &[], dry_run, &AdvisoryUpgradeFilter::NONE).await.context("upgrading overlay packages")?;
            if dry_run {
                return Ok(());
            }
            println!("Checking for a new base image...");
            let status = std::process::Command::new("bootc").arg("upgrade").status();
            match status {
                Ok(s) if s.success() => {}
                Ok(s) => eprintln!("warning: `bootc upgrade` exited with status {s}"),
                Err(e) => eprintln!("warning: couldn't run `bootc upgrade`: {e}"),
            }
            Ok(())
        }
        OverlayMode::Standalone => match action {
            None => anyhow::bail!(
                "system-upgrade needs a subcommand in Standalone mode, e.g. \
                 `rum system-upgrade download --releasever=45`. See `rum system-upgrade --help`."
            ),
            Some(SystemUpgradeAction::Download { releasever, no_downgrade, packages }) => {
                system_upgrade_download(cli, paths, releasever, *no_downgrade, packages).await
            }
            Some(SystemUpgradeAction::Reboot { poweroff }) => system_upgrade_reboot(*poweroff),
            Some(SystemUpgradeAction::Execute) => system_upgrade_execute(cli, paths).await,
            Some(SystemUpgradeAction::Clean) => offline::clean().context("cleaning offline transaction state"),
            Some(SystemUpgradeAction::Log) => system_upgrade_log(),
            Some(SystemUpgradeAction::Status) => system_upgrade_status(),
        },
    }
}

async fn system_upgrade_download(cli: &Cli, paths: &OverlayPaths, target_releasever: &str, no_downgrade: bool, packages: &[String]) -> Result<()> {
    let system_vars = detect_vars(cli);
    anyhow::ensure!(
        system_vars.releasever != target_releasever,
        "need a --releasever greater than the current system version ({})",
        system_vars.releasever
    );

    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let target_vars = rum_repo::RepoVars { releasever: target_releasever.to_string(), basearch: system_vars.basearch.clone(), custom: system_vars.custom.clone() };
    println!("Downloading everything needed to upgrade to release {target_releasever}...");
    let (repo_configs, candidates, _groups, _environments, _modules, _module_defaults, fallback_candidates, _advisories) = load_candidates_with_vars(cli, target_vars, cli.refresh).await.context("loading target-release repositories")?;

    let installed_names: Vec<String> = overlay.base.iter().chain(&overlay.overlay).map(|p| p.nevra.name.clone()).collect();
    let names = if packages.is_empty() { installed_names } else { packages.to_vec() };

    let options = resolve_options(cli, paths);
    let host_arch = system_vars.basearch.clone();
    let plan = if no_downgrade {
        rum_resolver::resolve_upgrade(&names, &candidates, &overlay, &host_arch, &options).context("resolving upgrade transaction")?
    } else {
        rum_resolver::resolve_distro_sync(&names, &candidates, &overlay, &host_arch, &options).context("resolving distro-sync transaction")?
    };

    anyhow::ensure!(!plan.to_install.is_empty(), "nothing to download — no upgrade candidates found for release {target_releasever}");

    println!("Downloading {} package(s):", plan.to_install.len());
    for pkg in &plan.to_install {
        println!("  {} ({})", pkg.nevra, pkg.repo_id);
    }

    let client = build_client(cli)?;
    let dest = offline::packages_dir();
    std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    let downloaded = rum_transaction::download_all_ex_with_metadata_root(&client, &plan.to_install, &dest, &cli.cache_dir, cli.main_conf.max_parallel_downloads, false, &repo_configs, &fallback_candidates)
        .await
        .context("downloading packages")?;

    let cmd_line = format!(
        "rum system-upgrade download --releasever={target_releasever}{}",
        if no_downgrade { " --no-downgrade" } else { "" }
    );
    let state = offline::State {
        status: offline::Status::DownloadComplete,
        system_releasever: system_vars.releasever,
        target_releasever: target_releasever.to_string(),
        cmd_line,
        poweroff_after: false,
        packages: downloaded.iter().map(|d| d.rpm_path.clone()).collect(),
    };
    offline::write_state(&state)?;
    offline::install_unit().context("installing rum-system-upgrade.service")?;

    println!("Download complete! Use `rum system-upgrade reboot` to start the upgrade.");
    Ok(())
}

fn system_upgrade_reboot(poweroff: bool) -> Result<()> {
    let state = offline::load_state()?.context("no offline transaction is stored — run `rum system-upgrade download --releasever=X` first")?;
    anyhow::ensure!(
        matches!(state.status, offline::Status::DownloadComplete | offline::Status::Ready),
        "system is not ready for the offline transaction (status: {:?})",
        state.status
    );
    anyhow::ensure!(offline::packages_dir().is_dir(), "downloaded-package directory {} does not exist", offline::packages_dir().display());
    anyhow::ensure!(
        offline::unit_is_wanted(),
        "rum-system-upgrade.service is not wanted by system-update.target — re-run `rum system-upgrade download` to reinstall it"
    );

    println!("The system will now reboot to upgrade to release version {}.", state.target_releasever);
    offline::create_trigger_symlink()?;

    let mut state = state;
    state.status = offline::Status::Ready;
    state.poweroff_after = poweroff;
    offline::write_state(&state)?;

    offline::systemctl_reboot(poweroff)
}

/// Internal — invoked by `rum-system-upgrade.service` during the
/// `system-update.target` boot phase. Applies every downloaded RPM via a
/// single `rpm` transaction, entirely offline (no repo/network access —
/// the packages were already fetched and gpg-verified during `download`).
async fn system_upgrade_execute(cli: &Cli, paths: &OverlayPaths) -> Result<()> {
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "system-upgrade execute must run as root");
    anyhow::ensure!(Path::new(offline::MAGIC_SYMLINK).is_symlink(), "trigger file does not exist, exiting");

    let mut state = offline::load_state()?.context("no offline transaction state found")?;
    anyhow::ensure!(state.status == offline::Status::Ready, "use `rum system-upgrade reboot` to begin the transaction");

    println!("Starting system upgrade. This will take a while.");
    offline::remove_trigger_symlink()?;

    state.status = offline::Status::TransactionIncomplete;
    offline::write_state(&state)?;

    anyhow::ensure!(!state.packages.is_empty(), "stored transaction has no packages");
    let dbpath = match &paths.mode {
        OverlayMode::Split { overlay_rpmdb, .. } => Some(overlay_rpmdb.clone()),
        OverlayMode::Standalone => None,
    };
    let mut rpm_args = vec!["--upgrade".to_string(), "--replacepkgs".to_string(), "--replacefiles".to_string()];
    if !cli.main_conf.gpgcheck {
        rpm_args.push("--nosignature".to_string());
    }
    if let Some(dbpath) = &dbpath {
        rpm_args.push("--dbpath".to_string());
        rpm_args.push(dbpath.display().to_string());
    }
    for pkg in &state.packages {
        rpm_args.push(pkg.display().to_string());
    }
    let status = std::process::Command::new("rpm").args(&rpm_args).status().context("running rpm to apply the offline transaction")?;

    if !status.success() {
        eprintln!("Transaction failed: rpm exited with {status}");
        anyhow::bail!("offline transaction failed");
    }

    println!("Transaction complete! Cleaning up and rebooting...");
    let poweroff_after = state.poweroff_after;
    offline::clean()?;
    offline::systemctl_reboot(poweroff_after)
}

fn system_upgrade_log() -> Result<()> {
    let status = std::process::Command::new("journalctl").args(["-u", "rum-system-upgrade.service"]).status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => anyhow::bail!("couldn't read logs via journalctl — is systemd-journald running?"),
    }
}

fn system_upgrade_status() -> Result<()> {
    match offline::load_state()? {
        None => println!("No offline transaction is stored."),
        Some(state) => match state.status {
            offline::Status::DownloadIncomplete => println!("No offline transaction is stored."),
            offline::Status::DownloadComplete | offline::Status::Ready => {
                println!("An offline transaction was initiated by the following command:");
                println!("\t{}", state.cmd_line);
                println!("Run `rum system-upgrade reboot` to reboot and perform the offline transaction.");
            }
            offline::Status::TransactionIncomplete => {
                println!("An offline transaction was started, but it did not finish. Run `rum system-upgrade log` for more information.");
                println!("The command that initiated the transaction was:");
                println!("\t{}", state.cmd_line);
            }
        },
    }
    Ok(())
}

async fn upgrade(cli: &Cli, paths: &OverlayPaths, names: &[String], dry_run: bool, advisory_filter: &AdvisoryUpgradeFilter<'_>) -> Result<()> {
    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let (repo_configs, mut candidates, _groups, _environments, _modules, _module_defaults, fallback_candidates, advisories) = load_candidates(cli).await?;

    let installed_names: Vec<&str> = overlay.base.iter().chain(&overlay.overlay).map(|p| p.nevra.name.as_str()).collect();
    let names = &expand_name_patterns(names, &installed_names);

    let locked = rum_transaction::versionlock::list_names(paths)?;
    warn_base_only_names(names, &overlay, "upgrade");
    let installed: Vec<&Package> = overlay.overlay.iter().filter(|pkg| names.is_empty() || names.contains(&pkg.nevra.name)).filter(|pkg| !locked.contains(&pkg.nevra.name)).collect();

    // Named explicitly (never `upgrade_all`) even for a bare `rum upgrade`,
    // so versionlock's per-name exclusion (already applied to `installed`
    // above) carries through — `SOLVER_SOLVABLE_ALL` has no equivalent
    // per-name skip.
    let upgrade_names: Vec<String> = installed.iter().map(|p| p.nevra.name.clone()).collect();
    if upgrade_names.is_empty() {
        println!("Nothing to upgrade — everything is already at the newest available version.");
        return Ok(());
    }

    // `--advisory=`/`--security`/`--bugfix`/`--enhancement`/
    // `--advisory-severity=`: narrows which *candidate builds* the resolver
    // is even allowed to pick as an upgrade target for a named package —
    // any candidate NEVRA not covered by a matching advisory is dropped
    // from the pool before resolution, same as dnf5's own advisory-filtered
    // upgrade. Dependencies pulled in transitively (not one of
    // `upgrade_names` itself) are left untouched, since an advisory covers
    // the packages it patches, not their whole dependency closure.
    if advisory_filter.is_active() {
        let covered: std::collections::HashSet<&str> =
            advisories.iter().filter(|a| advisory_filter.matches(a)).flat_map(|a| a.packages.iter().map(String::as_str)).collect();
        candidates.retain(|c| !upgrade_names.contains(&c.nevra.name) || covered.contains(c.nevra.to_string().as_str()));
    }

    let mut options = resolve_options(cli, paths);
    options.allow_erasing = cli.allowerasing;
    let host_arch = detect_host_arch(cli);
    let plan = rum_resolver::resolve_upgrade(&upgrade_names, &candidates, &overlay, &host_arch, &options).context("resolving dependencies")?;
    let to_install = plan.to_install;

    if to_install.is_empty() {
        println!("Nothing to upgrade — everything is already at the newest available version.");
        return Ok(());
    }

    println!("Upgrading {} package(s):", to_install.len());
    for pkg in &to_install {
        println!("  {} ({})", pkg.nevra, pkg.repo_id);
    }

    // `plan.to_erase` came straight out of libsolv's own solve (see
    // `resolve_upgrade`'s `allow_erasing`/`protected_names` jobs) — it
    // already respects protected packages, unlike the old
    // `compute_erasures` Rust-side pass this used to call here.
    let to_erase = plan.to_erase;
    if !to_erase.is_empty() {
        println!("Erasing (would otherwise break with this upgrade):");
        for pkg in &to_erase {
            println!("  {}", pkg.nevra);
        }
        let erase_names: Vec<String> = to_erase.iter().map(|p| format!("{}.{}", p.nevra.name, p.nevra.arch)).collect();
        rum_transaction::apply_remove_ex(paths, &erase_names, dry_run, &protected_packages(cli), true, cli.assume_yes).context("erasing conflicting packages")?;
    }

    if dry_run {
        println!("Transaction test succeeded.");
        return Ok(());
    }

    let client = build_client(cli)?;
    if cli.downloadonly {
        return download_only(cli, &client, &to_install, &repo_configs, &fallback_candidates).await;
    }

    if !rum_transaction::confirm_transaction_ex(cli.assume_yes, cli.assume_no, cli.default_yes)? {
        println!("Operation aborted.");
        return Ok(());
    }

    rum_transaction::import_repo_keys(&client, paths, &repo_configs).await.context("importing repo gpg keys")?;
    let downloaded = rum_transaction::download_all_ex(&client, &to_install, &cli.cache_dir, cli.main_conf.max_parallel_downloads, true, &repo_configs, &fallback_candidates).await.context("downloading packages")?;
    // Upgrades of already-installed packages keep whatever reason they
    // already had — treat every upgraded package as User here rather than
    // guessing, since `apply_install` has no prior-reason lookup to fall
    // back on and an upgrade is never how a package first gets pulled in.
    let explicit_names: Vec<String> = to_install.iter().map(|p| p.nevra.name.clone()).collect();
    let downloaded = apply_install_with_signature_retry(&client, downloaded, &to_install, &cli.cache_dir, cli.main_conf.max_parallel_downloads, &repo_configs, &fallback_candidates, |d| {
        rum_transaction::apply_install_ex(paths, d, &repo_configs, dry_run, &explicit_names, cli.no_gpgchecks, cli.allowerasing, setopt_noscripts(cli), &cli.main_conf.installonlypkgs, cli.assume_yes, true)
    })
    .await?;
    cleanup_downloaded(cli, &downloaded, dry_run);
    Ok(())
}

/// Installs the `BuildRequires` of a `.spec` file (or the equivalent
/// `Requires` embedded in a `.src.rpm`'s own header) using the system rpm
/// tooling to do the actual macro-aware parsing — `rpmspec` for a `.spec`,
/// `rpm -qp` for a `.src.rpm` (an srpm's BuildRequires are stored as plain
/// header Requires, which is also how dnf's own `builddep` reads them).
async fn builddep(cli: &Cli, paths: &OverlayPaths, spec: &Path, define: &[String], dry_run: bool) -> Result<()> {
    anyhow::ensure!(spec.is_file(), "no such file: {}", spec.display());

    let is_srpm = spec.extension().is_some_and(|e| e == "rpm");
    let mut cmd = std::process::Command::new(if is_srpm { "rpm" } else { "rpmspec" });
    if is_srpm {
        cmd.args(["-qp", "--requires"]);
    } else {
        cmd.args(["-q", "--buildrequires"]);
    }
    for d in define {
        cmd.arg("--define").arg(d);
    }
    cmd.arg(spec);

    let out = cmd.output().with_context(|| format!("running {} on {}", if is_srpm { "rpm -qp --requires" } else { "rpmspec -q --buildrequires" }, spec.display()))?;
    anyhow::ensure!(
        out.status.success(),
        "{} failed on {}:\n{}",
        if is_srpm { "rpm -qp --requires" } else { "rpmspec -q --buildrequires" },
        spec.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );

    // Each line is a capability, optionally with a version constraint
    // ("foo >= 1.2.3") — rum's resolver matches by bare name/capability, so
    // drop the constraint. `rpmlib(...)` features are virtual, always
    // satisfied by rpm itself, and never resolve to a real package.
    let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("rpmlib("))
        .map(|l| l.split_whitespace().next().unwrap_or(l).to_string())
        .collect();

    if names.is_empty() {
        println!("No build dependencies found for {}.", spec.display());
        return Ok(());
    }

    println!("Installing build dependencies for {}:", spec.display());
    for name in &names {
        println!("  {name}");
    }
    install(cli, paths, &names, dry_run).await
}

async fn install(cli: &Cli, paths: &OverlayPaths, packages: &[String], dry_run: bool) -> Result<()> {
    install_ex(cli, paths, packages, dry_run, false).await
}

/// Applies a [`history::Inverse`] (from `undo`/`rollback`) by reusing the
/// plain `install`/`remove` pipelines — same confirmation prompt,
/// `--dry-run` handling, versionlock, kernel/protected-package guards, and
/// history recording (undoing a transaction is itself recorded as a new
/// transaction, same as dnf5). `--nodeps` on the remove side matches how
/// `sync_packages` erases conflicting packages elsewhere in this file: undo
/// already knows exactly which names it wants gone, a second rpm-level
/// dependency check on an overlay-only rpmdb view can only be redundant or
/// wrong (see the comment on `apply_remove_ex`'s `--nodeps` call site).
async fn apply_history_inverse(cli: &Cli, paths: &OverlayPaths, inverse: history::Inverse, dry_run: bool) -> Result<()> {
    if inverse.to_remove.is_empty() && inverse.to_install.is_empty() {
        println!("Nothing to do.");
        return Ok(());
    }
    if !inverse.to_remove.is_empty() {
        rum_transaction::apply_remove_ex(paths, &inverse.to_remove, dry_run, &protected_packages(cli), true, cli.assume_yes).context("undoing install(s)")?;
    }
    if !inverse.to_install.is_empty() {
        install(cli, paths, &inverse.to_install, dry_run).await.context("undoing remove(s)")?;
    }
    Ok(())
}

/// Full form of [`install`] with `force_allow_erasing` — used only by
/// `Command::Swap`, which needs conflict-clearing erasure to happen
/// regardless of whether the user passed `--allowerasing` on the swap
/// invocation itself: a swap that can't clear a conflicting sibling package
/// out of the way (e.g. `libavfilter`'s dependency on `ocl-icd` conflicting
/// with an already-installed `OpenCL-ICD-Loader`) isn't a swap at all, the
/// same way dnf's own `swap` doesn't gate this behind a separate flag.
async fn install_ex(cli: &Cli, paths: &OverlayPaths, packages: &[String], dry_run: bool, force_allow_erasing: bool) -> Result<()> {
    anyhow::ensure!(!packages.is_empty(), "no packages given");

    // URL .rpm arguments (dnf's `dnf install https://.../foo.rpm`) — fetch
    // them into the local-rpm cache first, then fall through to the same
    // local-file install path below. Anything that isn't a local file and
    // doesn't look like a URL is left in `packages` to be resolved as a
    // package name.
    let (url_rpms, packages): (Vec<String>, Vec<String>) =
        packages.iter().cloned().partition(|p| (p.starts_with("http://") || p.starts_with("https://")) && p.ends_with(".rpm"));
    let mut downloaded_url_rpms = Vec::new();
    if !url_rpms.is_empty() {
        let client = build_client(cli)?;
        std::fs::create_dir_all(rum_overlay::LOCAL_RPM_CACHE).with_context(|| format!("creating {}", rum_overlay::LOCAL_RPM_CACHE))?;
        for url in &url_rpms {
            let filename = url.rsplit('/').next().filter(|f| !f.is_empty()).unwrap_or("download.rpm");
            let dest = Path::new(rum_overlay::LOCAL_RPM_CACHE).join(filename);
            println!("Downloading {url}...");
            let resp = rum_repo::get_with_retry(&client, url).await?.error_for_status().with_context(|| format!("fetching {url}"))?;
            let bytes = resp.bytes().await.with_context(|| format!("reading {url}"))?;
            tokio::fs::write(&dest, &bytes).await.with_context(|| format!("writing {}", dest.display()))?;
            downloaded_url_rpms.push(dest.to_string_lossy().into_owned());
        }
    }

    // Local .rpm file arguments install directly from their own embedded
    // header rather than going through repo-candidate resolution — rum's
    // resolver only ever knows about repo metadata, it has no notion of
    // "here's a file, not a name" the way dnf's local-rpm install does.
    let (rpm_files, packages): (Vec<String>, Vec<String>) =
        packages.iter().cloned().partition(|p| p.ends_with(".rpm") && Path::new(p).is_file());
    let rpm_files: Vec<String> = downloaded_url_rpms.into_iter().chain(rpm_files).collect();
    if !rpm_files.is_empty() {
        install_local_rpm_files(cli, paths, &rpm_files, dry_run, force_allow_erasing).await?;
    }
    if packages.is_empty() {
        return Ok(());
    }
    let packages = packages.as_slice();

    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let (repo_configs, mut candidates, groups, environments, _modules, _module_defaults, fallback_candidates, _advisories) = load_candidates(cli).await?;
    let client = build_client(cli)?;
    let host_arch = detect_host_arch(cli);

    // versionlock (dnf5 parity): a locked name must never resolve to a
    // candidate whose EVR/arch doesn't match what it was locked at, even
    // via a plain `install` — dnf's versionlock plugin folds its excludes
    // into the package sack consulted by every resolve, not just
    // `upgrade`/`distro-sync`. `resolve_ex` (via `options.locked_names`)
    // already enforces this at the libsolv pool level for every path
    // (direct and transitive), but filtering `candidates` here too keeps a
    // locked-and-mismatched name out of `expand_groups`/name-pattern
    // expansion below, so e.g. a bare `rum install sqlite-libs` with a
    // stale lock reports "nothing available" instead of silently no-op'ing
    // through the pool filter with a confusing later error.
    let locked = rum_transaction::versionlock::list(paths)?;
    if !locked.is_empty() {
        candidates.retain(|pkg| match locked.iter().find(|l| l.name == pkg.nevra.name) {
            Some(lock) => lock.allows(&pkg.nevra.evr(), &pkg.nevra.arch),
            None => true,
        });
    }

    let packages = expand_groups(packages, &groups, &environments)?;
    let candidate_names: Vec<&str> = candidates.iter().map(|c| c.nevra.name.as_str()).collect();
    let packages = expand_name_patterns(&packages, &candidate_names);
    let packages = &packages;

    let packages: Vec<String> = if cli.skip_unavailable {
        let (available, missing): (Vec<String>, Vec<String>) =
            packages.iter().cloned().partition(|name| candidates.iter().any(|c| c.nevra.name == *name || c.provides.iter().any(|p| p.name == *name)));
        for name in &missing {
            println!("Skipping unavailable package: {name}");
        }
        available
    } else {
        packages.to_vec()
    };
    anyhow::ensure!(!packages.is_empty(), "no packages left to install after --skip-unavailable filtering");

    // A name already tracked in packages.list *and* actually present in the
    // overlay is a repeat, explicit `install` of something the user already
    // has — treat that as "repair this and whatever it depends on", not a
    // silent no-op the way an already-satisfied dependency is. Only
    // meaningful under `Split`: `Standalone` environments have no
    // packages.list at all.
    //
    // Requiring actual overlay presence (not just packages.list membership)
    // matters because packages.list survives a soft reset while the overlay
    // itself is wiped — after a soft reset (or on first boot, where
    // packages.list is pre-seeded from factory before anything is
    // installed) every name is "tracked" but none are installed yet. Routing
    // those through `reinstall_with_deps` would fail immediately (nothing to
    // reinstall) and abort the whole install, which `rakuos-overlay-sync`
    // then silently swallows — leaving the overlay never actually rebuilt.
    let (already_listed, packages): (Vec<String>, Vec<String>) = if matches!(paths.mode, OverlayMode::Split { .. }) {
        packages.into_iter().partition(|name| {
            rum_overlay::packages_list_contains(name) && overlay.overlay.iter().any(|p| p.nevra.name == *name)
        })
    } else {
        (Vec::new(), packages)
    };
    if !already_listed.is_empty() {
        reinstall_with_deps(cli, paths, &already_listed, dry_run).await?;
    }
    if packages.is_empty() {
        return Ok(());
    }
    let packages = packages.as_slice();

    let mut options = resolve_options(cli, paths);
    options.allow_erasing |= force_allow_erasing;
    // `force_allow_erasing` is only ever set by `swap`'s install half — a
    // swap must actually land the named package even if some
    // differently-named already-installed package (typically a base-image
    // one the swap's remove half couldn't touch) already Provides the same
    // capability. See `ResolveOptions::force_names`.
    if force_allow_erasing {
        options.force_names.extend(packages.iter().cloned());
    }
    let plan = if cli.skip_unavailable {
        // The earlier name-or-provides pre-filter is only a heuristic: a
        // candidate can list `name` in its `Provides` (a loose text match)
        // without that name being resolvable by `rum-solv`'s own
        // `install_names` job, which only matches literal package names —
        // e.g. `heif-pixbuf-loader` shows up in a provides scan but isn't
        // an actual package name or a capability `rum-solv` interns, so it
        // was passing the pre-filter as "available" and then still hard-
        // failing the whole install with "nothing provides" from inside
        // the solver. Retry against the solver's own verdict instead: drop
        // exactly the name it says it can't provide and try again, so
        // "unavailable" means whatever the solver itself can't satisfy,
        // not what a text scan over Provides guesses.
        let mut packages: Vec<String> = packages.to_vec();
        loop {
            match rum_resolver::resolve_ex(&packages, &candidates, &overlay, &host_arch, &options) {
                Ok(plan) => break plan,
                Err(e) => {
                    let msg = e.to_string();
                    let culprit = msg.strip_prefix("nothing provides '").and_then(|rest| rest.strip_suffix('\'')).and_then(|name| {
                        let bare = name.split('.').next().unwrap_or(name);
                        packages.iter().find(|p| p.as_str() == name || p.as_str() == bare).cloned()
                    });
                    match culprit {
                        Some(name) => {
                            println!("Skipping unavailable package: {name}");
                            packages.retain(|p| *p != name);
                            anyhow::ensure!(!packages.is_empty(), "no packages left to install after --skip-unavailable filtering");
                        }
                        None => return Err(e).context("resolving dependencies"),
                    }
                }
            }
        }
    } else {
        rum_resolver::resolve_ex(packages, &candidates, &overlay, &host_arch, &options).context("resolving dependencies")?
    };

    for name in &plan.skipped {
        println!("Skipping broken package: {name}");
    }

    if plan.to_install.is_empty() {
        println!("Nothing to do — every requested package is already satisfied:");
        for s in &plan.already_satisfied {
            println!("  {} (via {:?})", s.dependency.name, s.origin);
        }
        return Ok(());
    }

    // Classify each planned package against whatever's currently installed
    // under the same (name, arch) so this pre-download preview uses the same
    // Installing/Upgrading/Downgrading/Reinstalling table as the post-download
    // summary in `apply_install_ex`, instead of a flat name list.
    let installed_by_name: std::collections::HashMap<(&str, &str), &Package> =
        overlay.base.iter().chain(&overlay.overlay).map(|p| ((p.nevra.name.as_str(), p.nevra.arch.as_str()), p)).collect();
    let entries: Vec<rum_transaction::summary::Entry> = plan
        .to_install
        .iter()
        .map(|pkg| match installed_by_name.get(&(pkg.nevra.name.as_str(), pkg.nevra.arch.as_str())) {
            Some(old) => {
                let action = match rum_core::EvrCompare::compare_evr(pkg.nevra.evr().as_str(), &old.nevra.evr()) {
                    std::cmp::Ordering::Equal => rum_transaction::summary::Action::Reinstalling,
                    std::cmp::Ordering::Greater => rum_transaction::summary::Action::Upgrading,
                    std::cmp::Ordering::Less => rum_transaction::summary::Action::Downgrading,
                };
                rum_transaction::summary::Entry::new(action, pkg).replacing(old)
            }
            None => rum_transaction::summary::Entry::new(rum_transaction::summary::Action::Installing, pkg),
        })
        .collect();
    rum_transaction::summary::print_transaction_summary(&entries);
    if !plan.already_satisfied.is_empty() {
        println!("Skipped ({} already satisfied dependencies):", plan.already_satisfied.len());
    }
    if !plan.to_obsolete.is_empty() {
        // rpm -U erases these automatically as part of the same
        // transaction (see Plan::to_obsolete's doc comment) — this is
        // purely a heads-up before the transaction runs, matching dnf's
        // "Obsoleting" line in its own plan summary.
        println!("Obsoleting:");
        for pkg in &plan.to_obsolete {
            println!("  {}", pkg.nevra);
        }
    }
    if !plan.to_erase.is_empty() {
        // Unlike `to_obsolete`, rpm won't erase these on its own — they're
        // conflicting overlay packages `--allowerasing` opted to clear out
        // of the way (see `Plan::to_erase`'s doc comment), so rum issues an
        // explicit `rpm -e` for them itself, before the install transaction
        // runs (same remove-then-install ordering `swap` uses, and for the
        // same reason: installing first would hit the very file/name
        // conflict this whole path exists to route around).
        println!("Erasing (conflicts with the above):");
        for pkg in &plan.to_erase {
            println!("  {}", pkg.nevra);
        }
        let erase_names: Vec<String> = plan.to_erase.iter().map(|p| format!("{}.{}", p.nevra.name, p.nevra.arch)).collect();
        rum_transaction::apply_remove_ex(paths, &erase_names, dry_run, &protected_packages(cli), true, cli.assume_yes).context("erasing conflicting packages")?;
    }

    // A dry run only resolves and simulates — the transaction summary
    // printed above (plus any erase-conflict lines) is the entire answer to
    // "what would this do", so stop here instead of actually fetching every
    // planned package's real `.rpm` over the network just to throw the
    // bytes away. This is also what lets a dry run stay unprivileged (no
    // GPG-key import into the real overlay dbpath, no real rpm transaction
    // test against it).
    if dry_run {
        println!("Transaction test succeeded.");
        return Ok(());
    }

    if cli.downloadonly {
        return download_only(cli, &client, &plan.to_install, &repo_configs, &fallback_candidates).await;
    }

    // Ask before spending any time/bandwidth on the download below — the
    // summary above already has everything needed to decide, so there's no
    // reason to make the user wait through a full `download_all_ex` just to
    // find out `apply_install_ex` was going to ask the same question anyway.
    if !rum_transaction::confirm_transaction_ex(cli.assume_yes, cli.assume_no, cli.default_yes)? {
        println!("Operation aborted.");
        return Ok(());
    }

    rum_transaction::import_repo_keys(&client, paths, &repo_configs).await.context("importing repo gpg keys")?;
    let downloaded = rum_transaction::download_all_ex(&client, &plan.to_install, &cli.cache_dir, cli.main_conf.max_parallel_downloads, true, &repo_configs, &fallback_candidates).await.context("downloading packages")?;
    // Only the names the user actually typed count as `User`-reasoned;
    // everything else in `plan.to_install` was pulled in purely to satisfy
    // a dependency of those, so it's recorded as `Dependency` — the
    // distinction `autoremove` needs later.
    let downloaded = apply_install_with_signature_retry(&client, downloaded, &plan.to_install, &cli.cache_dir, cli.main_conf.max_parallel_downloads, &repo_configs, &fallback_candidates, |d| {
        rum_transaction::apply_install_ex(paths, d, &repo_configs, dry_run, packages, cli.no_gpgchecks, options.allow_erasing, setopt_noscripts(cli), &cli.main_conf.installonlypkgs, cli.assume_yes, true)
    })
    .await?;
    cleanup_downloaded(cli, &downloaded, dry_run);
    prune_installonly(cli, paths)?;
    // Only the explicitly-requested top-level names, not the whole
    // dependency closure — same scope rakuos-core's `rakupkg` used,
    // since packages.list exists purely so `rakuos-overlay-sync` knows
    // what to re-request; dependencies get pulled back in by the
    // resolver automatically.
    if matches!(paths.mode, OverlayMode::Split { .. }) {
        for name in packages {
            rum_overlay::packages_list_add(name)?;
        }
    }
    Ok(())
}

/// Installs local `.rpm` files directly via their own embedded header
/// (name/EVR/arch/provides/requires) rather than through rum's repo-backed
/// resolver, which has no notion of "here's a file, not a name". In `Split`
/// overlay mode, the *newest* copy of each installed file is cached under
/// `LOCAL_RPM_CACHE` and its name recorded in `LOCAL_RPM_LIST` — replayed
/// by `rakuos-overlay-sync` after a reset/first boot the same way it
/// replays `PACKAGES_LIST`, just reinstalling the cached file instead of
/// re-resolving a repo name.
async fn install_local_rpm_files(cli: &Cli, paths: &OverlayPaths, rpm_files: &[String], dry_run: bool, force_replace: bool) -> Result<()> {
    let mut downloaded = Vec::new();
    for path in rpm_files {
        let package = rum_rpmdb::query_local_file(Path::new(path)).with_context(|| format!("reading RPM header from {path}"))?;
        downloaded.push(rum_transaction::DownloadedPackage { package, rpm_path: PathBuf::from(path) });
    }

    // Plain `install` (not `reinstall`) of a local file already installed at
    // the exact same NEVRA is a silent no-op under dnf — e.g. akmods'
    // `find /var/cache/akmods -name '*.rpm' | xargs rum install -y` runs
    // right after akmods' own internal `reinstall` already put the same rpm
    // in place, so this second pass would otherwise hand rpm a package it
    // refuses as "already installed" (exit 2) and fail the whole build.
    if !force_replace {
        let overlay = OverlayContext::load(paths).context("loading overlay context")?;
        downloaded.retain(|d| {
            let already = overlay.base.iter().chain(&overlay.overlay).any(|p| p.nevra == d.package.nevra);
            if already {
                println!("Package {} is already installed.", d.package.nevra);
            }
            !already
        });
    }
    if downloaded.is_empty() {
        return Ok(());
    }

    let names: Vec<String> = downloaded.iter().map(|d| d.package.nevra.name.clone()).collect();
    println!("Installing {} local RPM file(s): {}", downloaded.len(), names.join(", "));
    rum_transaction::apply_install_ex(paths, &downloaded, &[], dry_run, &names, cli.no_gpgchecks, force_replace, setopt_noscripts(cli), &[], cli.assume_yes, false).context("applying rpm transaction")?;

    if !dry_run && matches!(paths.mode, OverlayMode::Split { .. }) {
        std::fs::create_dir_all(rum_overlay::LOCAL_RPM_CACHE).with_context(|| format!("creating {}", rum_overlay::LOCAL_RPM_CACHE))?;
        for d in &downloaded {
            let dest = Path::new(rum_overlay::LOCAL_RPM_CACHE).join(format!("{}.rpm", d.package.nevra.name));
            std::fs::copy(&d.rpm_path, &dest).with_context(|| format!("caching {} to {}", d.rpm_path.display(), dest.display()))?;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644)).with_context(|| format!("setting permissions on {}", dest.display()))?;
            rum_overlay::local_rpm_list_add(&d.package.nevra.name)?;
        }
    }
    Ok(())
}

/// Reinstalls each of `names` (same EVR) plus every already-installed
/// overlay package in their `Requires` closure — a repeated `install` of
/// something already in packages.list means "make sure this and what it
/// depends on are intact", not a no-op.
async fn reinstall_with_deps(cli: &Cli, paths: &OverlayPaths, names: &[String], dry_run: bool) -> Result<()> {
    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let mut closure: std::collections::HashSet<String> = names.iter().cloned().collect();
    let mut frontier: Vec<String> = names.to_vec();
    while let Some(name) = frontier.pop() {
        let Some(pkg) = overlay.overlay.iter().find(|p| p.nevra.name == name) else { continue };
        for dep in &pkg.requires {
            if let Some(dep_pkg) = overlay.overlay.iter().find(|p| p.nevra.name == dep.name || p.provides.iter().any(|prov| prov.name == dep.name)) {
                if closure.insert(dep_pkg.nevra.name.clone()) {
                    frontier.push(dep_pkg.nevra.name.clone());
                }
            }
        }
    }
    let names: Vec<String> = closure.into_iter().collect();
    println!("Already tracked — reinstalling {} and its overlay dependencies...", names.len());
    sync_packages(cli, paths, &names, dry_run, SyncMode::Reinstall).await
}

/// `installonly_limit=` (dnf default: 3) — after an install that may have
/// added another coexisting version of an `installonlypkgs=` name (kernel
/// family, by default), erase the oldest excess overlay-owned versions of
/// that name so the count settles back at the limit. Only ever touches
/// overlay-owned installs: a `Base` copy lives on the read-only image and
/// rum has no way to remove it regardless.
fn prune_installonly(cli: &Cli, paths: &OverlayPaths) -> Result<()> {
    let limit = cli.main_conf.installonly_limit;
    if limit == 0 || cli.main_conf.installonlypkgs.is_empty() {
        return Ok(());
    }
    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let host_arch = detect_host_arch(cli);
    let running_kernel = running_kernel_evr(&host_arch);
    let mut by_name: std::collections::BTreeMap<String, Vec<&Package>> = std::collections::BTreeMap::new();
    for pkg in &overlay.overlay {
        if rum_core::name_matches_any(&pkg.nevra.name, &cli.main_conf.installonlypkgs) {
            by_name.entry(pkg.nevra.name.clone()).or_default().push(pkg);
        }
    }
    for (_name, mut versions) in by_name {
        if versions.len() <= limit as usize {
            continue;
        }
        versions.sort_by(|a, b| a.nevra.evr().as_str().compare_evr(&b.nevra.evr()));
        let excess = versions.len() - limit as usize;
        // Same rule as dnf5's `installonly_cmp` (see `running_kernel_evr`'s
        // doc comment): the currently-running build must never end up
        // among the oldest-`excess` slice pruned here, no matter where its
        // EVR happens to sort — walk oldest-to-newest and skip over it,
        // taking the next-oldest survivor in its place instead, so the
        // pruned count still settles at `limit`.
        let to_remove: Vec<&Package> = versions.iter().filter(|p| running_kernel.as_deref() != Some(format!("{}-{}", p.nevra.version, p.nevra.release).as_str())).take(excess).copied().collect();
        if to_remove.is_empty() {
            continue;
        }
        let to_remove: Vec<String> = to_remove.iter().map(|p| p.nevra.to_string()).collect();
        println!("installonly_limit={limit}: pruning {} old package(s):", to_remove.len());
        for n in &to_remove {
            println!("  {n}");
        }
        rum_transaction::apply_remove(paths, &to_remove, false, &protected_packages(cli), cli.assume_yes)?;
    }
    Ok(())
}

/// `keepcache=` support: dnf's default is to delete a package's cached
/// `.rpm` once it's been successfully installed (it already did its job;
/// keeping it around is opt-in disk usage). Best-effort — a failed removal
/// just leaves the file cached, same as if `keepcache=1` were set.
fn cleanup_downloaded(cli: &Cli, downloaded: &[rum_transaction::DownloadedPackage], dry_run: bool) {
    if dry_run || cli.main_conf.keepcache {
        return;
    }
    for pkg in downloaded {
        let _ = std::fs::remove_file(&pkg.rpm_path);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SyncMode {
    /// Same EVR as installed — repair a corrupted/modified install.
    Reinstall,
    /// Highest available EVR strictly older than installed.
    Downgrade,
    /// Highest available EVR, whichever direction that is from installed.
    DistroSync,
}

/// Shared implementation for `reinstall`/`downgrade`/`distro-sync`: same
/// installed-package-matching and download/verify/apply pipeline as
/// [`upgrade`], just with a different candidate-EVR rule and `rpm
/// --replacepkgs --oldpackage` so same-or-older EVRs aren't rejected as a
/// no-op.
async fn sync_packages(cli: &Cli, paths: &OverlayPaths, names: &[String], dry_run: bool, mode: SyncMode) -> Result<()> {
    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let (repo_configs, candidates, _groups, _environments, _modules, _module_defaults, fallback_candidates, _advisories) = load_candidates(cli).await?;

    let installed_names: Vec<&str> = overlay.base.iter().chain(&overlay.overlay).map(|p| p.nevra.name.as_str()).collect();
    let names = &expand_name_patterns(names, &installed_names);

    let locked = rum_transaction::versionlock::list_names(paths)?;
    warn_base_only_names(names, &overlay, match mode { SyncMode::Reinstall => "reinstall", SyncMode::Downgrade => "downgrade", SyncMode::DistroSync => "distro-sync" });
    let installed: Vec<&Package> = overlay
        .overlay
        .iter()
        .filter(|pkg| names.is_empty() || names.contains(&pkg.nevra.name))
        .filter(|pkg| mode == SyncMode::Reinstall || !locked.contains(&pkg.nevra.name))
        .collect();
    if !names.is_empty() {
        for name in names {
            let in_overlay = installed.iter().any(|p| &p.nevra.name == name);
            let in_base = overlay.base.iter().any(|p| &p.nevra.name == name);
            anyhow::ensure!(in_overlay || in_base, "package '{name}' is not installed");
        }
    }

    let mut options = resolve_options(cli, paths);
    options.allow_erasing = true;
    let host_arch = detect_host_arch(cli);

    let (to_install, to_erase) = if mode == SyncMode::DistroSync {
        // dnf5's `add_distro_sync` (`SOLVER_DISTUPGRADE`): let libsolv pick
        // whatever exactly matches the enabled repos now, rather than
        // Rust re-deciding "any different EVR, highest" by hand — this
        // also runs the pick through a real dependency-satisfiability
        // solve and Split mode's base-package lock, instead of trusting
        // an EVR-only comparison.
        let names: Vec<String> = installed.iter().map(|p| p.nevra.name.clone()).collect();
        let plan = rum_resolver::resolve_distro_sync(&names, &candidates, &overlay, &host_arch, &options).context("resolving dependencies")?;
        (plan.to_install, plan.to_erase)
    } else {
        // Reinstall/Downgrade need a specific EVR rule ("exact same
        // build" / "highest strictly older build") that libsolv's job
        // model has no direct equivalent for — that selection stays rum's
        // own policy, picked here in Rust — but the actual install still
        // goes through libsolv via a pinned job (`resolve_pinned`) rather
        // than being trusted as-is, so it still gets a real Requires
        // closure check and the base-package lock.
        let mut targets = Vec::new();
        for inst in &installed {
            let same_name = candidates.iter().filter(|c| c.nevra.name == inst.nevra.name);
            let picked = match mode {
                SyncMode::Reinstall => same_name.filter(|c| c.nevra.evr().as_str().compare_evr(&inst.nevra.evr()) == std::cmp::Ordering::Equal).next(),
                SyncMode::Downgrade => same_name
                    .filter(|c| c.nevra.evr().as_str().compare_evr(&inst.nevra.evr()) == std::cmp::Ordering::Less)
                    .max_by(|a, b| a.nevra.evr().as_str().compare_evr(b.nevra.evr().as_str())),
                SyncMode::DistroSync => unreachable!(),
            };
            if let Some(pkg) = picked {
                targets.push(pkg.clone());
            }
        }
        if targets.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            let plan = rum_resolver::resolve_pinned(&targets, &candidates, &overlay, &host_arch, &options).context("resolving dependencies")?;
            (plan.to_install, plan.to_erase)
        }
    };

    if to_install.is_empty() {
        let verb = match mode {
            SyncMode::Reinstall => "reinstall",
            SyncMode::Downgrade => "downgrade",
            SyncMode::DistroSync => "sync",
        };
        println!("Nothing to {verb} — no matching candidate found.");
        return Ok(());
    }

    println!("{} {} package(s):", match mode { SyncMode::Reinstall => "Reinstalling", SyncMode::Downgrade => "Downgrading", SyncMode::DistroSync => "Syncing" }, to_install.len());
    for pkg in &to_install {
        println!("  {} ({})", pkg.nevra, pkg.repo_id);
    }

    if !to_erase.is_empty() {
        println!("Erasing (would otherwise break with this transaction):");
        for pkg in &to_erase {
            println!("  {}", pkg.nevra);
        }
        let erase_names: Vec<String> = to_erase.iter().map(|p| format!("{}.{}", p.nevra.name, p.nevra.arch)).collect();
        rum_transaction::apply_remove_ex(paths, &erase_names, dry_run, &protected_packages(cli), true, cli.assume_yes).context("erasing conflicting packages")?;
    }

    if dry_run {
        println!("Transaction test succeeded.");
        return Ok(());
    }

    let client = build_client(cli)?;
    if cli.downloadonly {
        return download_only(cli, &client, &to_install, &repo_configs, &fallback_candidates).await;
    }

    if !rum_transaction::confirm_transaction_ex(cli.assume_yes, cli.assume_no, cli.default_yes)? {
        println!("Operation aborted.");
        return Ok(());
    }

    rum_transaction::import_repo_keys(&client, paths, &repo_configs).await.context("importing repo gpg keys")?;
    let downloaded = rum_transaction::download_all_ex(&client, &to_install, &cli.cache_dir, cli.main_conf.max_parallel_downloads, true, &repo_configs, &fallback_candidates).await.context("downloading packages")?;
    let explicit_names: Vec<String> = to_install.iter().map(|p| p.nevra.name.clone()).collect();
    let downloaded = apply_install_with_signature_retry(&client, downloaded, &to_install, &cli.cache_dir, cli.main_conf.max_parallel_downloads, &repo_configs, &fallback_candidates, |d| {
        rum_transaction::apply_install_ex(paths, d, &repo_configs, dry_run, &explicit_names, cli.no_gpgchecks, true, setopt_noscripts(cli), &cli.main_conf.installonlypkgs, cli.assume_yes, true)
    })
    .await?;
    cleanup_downloaded(cli, &downloaded, dry_run);
    Ok(())
}

/// Removes every overlay package rum recorded as [`Reason::Dependency`]
/// that nothing currently installed (base or overlay) still `Requires`.
/// Packages with no recorded reason at all (installed before history
/// tracking existed) are treated as `User` — never a removal candidate —
/// same as dnf treats packages it has no yum/dnf history for.
fn autoremove(cli: &Cli, paths: &OverlayPaths, dry_run: bool) -> Result<()> {
    autoremove_ex(cli, paths, dry_run, None)
}

/// Every name transitively reachable via `Requires`/`Recommends` starting
/// from `removed` — computed against `overlay` *before* removal, since the
/// removed packages' own dependency edges are gone once they're actually
/// erased. Feeds `autoremove_ex`'s `scope`, so `clean_requirements_on_remove=`
/// only sweeps what the just-removed packages themselves pulled in, not
/// unrelated pre-existing orphans elsewhere in the system (dnf5's
/// `SOLVER_CLEANDEPS` is scoped the same way, folded into the same solve).
fn removed_dependency_closure_names(removed: &[&Package], overlay: &OverlayContext) -> std::collections::HashSet<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut frontier: Vec<String> = Vec::new();
    for pkg in removed {
        for d in pkg.requires.iter().chain(pkg.recommends.iter()) {
            for leaf in rum_resolver::boolean_dep_leaf_names(&d.name) {
                if seen.insert(leaf.clone()) {
                    frontier.push(leaf);
                }
            }
        }
    }
    while let Some(name) = frontier.pop() {
        let Some(pkg) = overlay.overlay.iter().chain(&overlay.base).find(|p| p.nevra.name == name || p.provides.iter().any(|pr| pr.name == name)) else {
            continue;
        };
        for d in pkg.requires.iter().chain(pkg.recommends.iter()) {
            for leaf in rum_resolver::boolean_dep_leaf_names(&d.name) {
                if seen.insert(leaf.clone()) {
                    frontier.push(leaf);
                }
            }
        }
    }
    seen
}

/// `scope`, when given, restricts the sweep to names reachable from
/// [`removed_dependency_closure_names`] — dnf5 parity for
/// `clean_requirements_on_remove=`: dnf folds `SOLVER_CLEANDEPS` into the
/// same solve as the remove itself, so it only ever sweeps the
/// just-removed packages' own now-unneeded dependency subtree, never an
/// unrelated pre-existing orphan elsewhere in the system. `None` (used by
/// the standalone `rum autoremove` command) keeps the old unscoped
/// full-system sweep. Scoping can only narrow the candidate set the
/// unscoped sweep would already compute, never widen it — so this can't
/// remove anything a full `rum autoremove` wouldn't also remove, only
/// leaves unrelated orphans for that explicit sweep to catch later.
/// The full autoremove wave-sweep: resolves the whole transitive removal
/// set (packages recorded as `Reason::Dependency` that nothing remaining
/// still requires, across however many waves it takes for removing one
/// wave to orphan the next) before anything is actually removed, rather
/// than one pass over the pre-removal snapshot. `required_names` only
/// reflects what's *still* installed after subtracting waves already
/// picked — a single pass over the original snapshot only ever catches the
/// outermost leaves (e.g. steam's own direct deps), leaving everything
/// *those* leaves in turn required (often nearly its whole multilib
/// dependency tree) behind forever, since nothing ever re-checks them once
/// the leaves are gone. Shared by [`autoremove_ex`] (which actually removes
/// the result) and `rum list --autoremove` (which only reports it).
fn compute_autoremove_candidates(
    paths: &OverlayPaths,
    overlay: &OverlayContext,
    protected_names: &[String],
    scope: Option<&std::collections::HashSet<String>>,
) -> Vec<Package> {
    let mut remaining: Vec<&Package> = overlay.overlay.iter().collect();
    let mut candidates: Vec<Package> = Vec::new();
    loop {
        // `d.name` may itself be a boolean/rich-dependency expression (e.g.
        // `(python3.14dist(pyscard) < 3~~ with python3.14dist(pyscard) >= 2)`)
        // rather than a plain capability name — expand those to their real
        // leaf names so a package required only via a rich dependency isn't
        // wrongly treated as orphaned.
        // Weak deps (Recommends) matter here too: a package installed only
        // because something else `Recommends` it (dnf/rum's default — see
        // `rum-resolver`'s recommends best-effort install) is still tracked
        // as `Reason::Dependency`, exactly like a hard-`Requires` pull. If
        // this only looked at `requires`, every Recommends-only package
        // (mpv pulled in by a codec Recommends, libvirt-daemon-kvm/
        // -config-network/-driver-storage pulled in by virt-manager's
        // Recommends on libvirt-daemon, etc.) would look "unrequired" on
        // the very first pass and get swept, then cascade into removing
        // *their* deps in the next wave — live-reproduced removing 80
        // packages including firefox/virt-manager/steam's own runtime deps
        // on a `rum remove feishin`.
        let required_names: std::collections::HashSet<String> = overlay
            .base
            .iter()
            .chain(remaining.iter().copied())
            .flat_map(|pkg| pkg.requires.iter().chain(pkg.recommends.iter()).flat_map(|d| rum_resolver::boolean_dep_leaf_names(&d.name)))
            .collect();

        let wave: Vec<&Package> = remaining
            .iter()
            .copied()
            .filter(|pkg| history::reason_of(paths, &pkg.nevra.name) == Some(Reason::Dependency))
            .filter(|pkg| !required_names.contains(pkg.nevra.name.as_str()) && !pkg.provides.iter().any(|p| required_names.contains(p.name.as_str())))
            .filter(|pkg| scope.is_none_or(|s| s.contains(&pkg.nevra.name)))
            .filter(|pkg| !rum_core::name_matches_any(&pkg.nevra.name, protected_names))
            .collect();

        if wave.is_empty() {
            break;
        }
        let wave_nevras: std::collections::HashSet<&rum_core::Nevra> = wave.iter().map(|p| &p.nevra).collect();
        remaining.retain(|p| !wave_nevras.contains(&p.nevra));
        candidates.extend(wave.into_iter().cloned());
    }
    candidates
}

fn autoremove_ex(cli: &Cli, paths: &OverlayPaths, dry_run: bool, scope: Option<&std::collections::HashSet<String>>) -> Result<()> {
    // `OverlayMode::Standalone` (distrobox/podman, and every image-build
    // chroot — neither ever has `/var/lib/rakuos/base-rpmdb`, see
    // `OverlayPaths::detect`) used to refuse this sweep entirely: a package
    // inherited from a parent image layer (e.g. the kernel, already
    // installed by an earlier `FROM`-base build stage) has no
    // `reasons.json` entry of its own in *this* container's fresh state
    // dir, and an older, looser version of the wave filter below treated
    // that untracked state as sweepable — live-reproduced during the COSMIC
    // image build, where `rum remove -y google-noto-color-emoji-fonts`
    // cascaded into removing 456 packages including the kernel. The wave
    // filter now requires a *literal* `Some(Reason::Dependency)` match
    // (recorded only when `rum` itself, in this container, actually
    // installed the package as someone else's transitive pull), so an
    // untracked inherited package reads as `None` and is never a candidate
    // — the actual bug is fixed at its source, so there's nothing left for
    // a blanket mode-based refusal to protect against. `protected_names`
    // below is the same defense-in-depth dnf5 relies on via its
    // `/etc/dnf/protected.d/*.conf` (rum, rpm-adjacent tooling, and
    // whatever an image's own `rum.conf` adds via `protected_packages=`)
    // for anything that should never be swept regardless of reason
    // tracking.
    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let protected_names = protected_packages(cli);
    let candidates = compute_autoremove_candidates(paths, &overlay, &protected_names, scope);
    let candidates: Vec<&Package> = candidates.iter().collect();

    if candidates.is_empty() {
        println!("Nothing to autoremove.");
        return Ok(());
    }

    println!("Removing {} unneeded dependency package(s):", candidates.len());
    for pkg in &candidates {
        println!("  {}", pkg.nevra);
    }
    // `name.arch` rather than a bare name — a multilib dependency (e.g.
    // steam pulling in both `libnsl.x86_64` and `libnsl.i686`) produces two
    // candidates sharing one name, and `rpm -e libnsl` errors as ambiguous
    // ("specifies multiple packages") instead of removing both. `rpm`
    // accepts the `name.arch` qualifier to disambiguate, same as `dnf`'s own
    // NEVRA-qualified erase.
    let names: Vec<String> = candidates.iter().map(|p| format!("{}.{}", p.nevra.name, p.nevra.arch)).collect();
    rum_transaction::apply_remove(paths, &names, dry_run, &protected_packages(cli), cli.assume_yes)?;
    Ok(())
}

/// Checks every base+overlay installed package's `Requires` against what's
/// actually provided by the combined installed set, reporting anything
/// unmet. This is purely an installed-vs-installed consistency check (rpmdb
/// corruption, a manually-forced install, a botched removal) — it does not
/// compare against repo metadata the way `check-upgrade` does.
fn pkg_provides(pkg: &Package, capability: &str) -> bool {
    pkg.nevra.name == capability || pkg.provides.iter().any(|d| d.name == capability)
}

fn pkg_requires(pkg: &Package, capability: &str) -> bool {
    pkg.requires.iter().any(|d| d.name == capability)
}

/// `rum provides <capability>` — reports every installed package that
/// provides it, then (network permitting) every repo candidate too, same
/// two-part "Installed/Available" shape dnf's own `provides` output has.
async fn provides(cli: &Cli, paths: &OverlayPaths, capability: &str) -> Result<()> {
    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let mut found = false;
    for pkg in overlay.base.iter().chain(&overlay.overlay) {
        if pkg_provides(pkg, capability) {
            println!("{} : installed", pkg.nevra);
            found = true;
        }
    }
    if let Ok((_, candidates, _groups, _environments, ..)) = load_candidates(cli).await {
        for pkg in &candidates {
            if pkg_provides(pkg, capability) {
                println!("{} : {}", pkg.nevra, pkg.repo_id);
                found = true;
            }
        }
    }
    if !found {
        println!("No matches found for '{capability}'.");
    }
    Ok(())
}

/// `rum advisory list/info/summary` (dnf5 alias `updateinfo`). Buckets an
/// advisory's *covered packages* against the live overlay/rpmdb view the
/// same way [`check_upgrade`] does: `--available` (default) is packages not
/// installed at that exact EVR, `--updates` is an installed package with a
/// strictly newer candidate EVR, `--installed` is an installed package at
/// that exact EVR. An advisory qualifies for a bucket if *any* of its
/// covered packages does — matching dnf5's own collection-level semantics,
/// since one advisory commonly spans multiple sub-packages of the same
/// source build.
async fn advisory(cli: &Cli, paths: &OverlayPaths, action: &AdvisoryAction) -> Result<()> {
    let (filters, mode) = match action {
        AdvisoryAction::List { filters } => (filters, "list"),
        AdvisoryAction::Info { filters } => (filters, "info"),
        AdvisoryAction::Summary { filters } => (filters, "summary"),
    };

    let (_, candidates, _groups, _environments, _modules, _module_defaults, _fallback, advisories) = load_candidates(cli).await?;
    anyhow::ensure!(!advisories.is_empty(), "no advisory (updateinfo) data in enabled repos");

    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let installed: Vec<&Package> = overlay.base.iter().chain(&overlay.overlay).collect();
    let installed_nevras: std::collections::HashSet<String> = installed.iter().map(|p| p.nevra.to_string()).collect();

    let bucket_of = |nevra_str: &str| -> Option<&'static str> {
        let pkg_nevra = rum_core::Nevra::parse_nevra(nevra_str)?;
        if installed_nevras.contains(nevra_str) {
            return Some("installed");
        }
        let is_update = installed.iter().any(|inst| {
            inst.nevra.name == pkg_nevra.name && inst.nevra.arch == pkg_nevra.arch && pkg_nevra.evr().as_str().compare_evr(&inst.nevra.evr()) == std::cmp::Ordering::Greater
        });
        if is_update {
            return Some("updates");
        }
        Some("available")
    };

    let want_severity = filters.severity.as_deref().map(rum_repo::updateinfo::Severity::parse_cli);
    let want_kind = filters.bugfix || filters.security || filters.enhancement || filters.newpackage;
    let name_pool: Vec<&str> = candidates.iter().map(|p| p.nevra.name.as_str()).chain(installed.iter().map(|p| p.nevra.name.as_str())).collect();
    let want_names = expand_name_patterns(&filters.packages, &name_pool);

    let want_any_bucket = filters.installed || filters.updates || filters.all;
    let matched: Vec<(&rum_repo::Advisory, Vec<&str>)> = advisories
        .iter()
        .filter_map(|adv| {
            if !filters.advisory.is_empty() && !filters.advisory.contains(&adv.id) {
                return None;
            }
            if want_kind {
                let ok = (filters.bugfix && adv.kind == rum_repo::updateinfo::AdvisoryKind::Bugfix)
                    || (filters.security && adv.kind == rum_repo::updateinfo::AdvisoryKind::Security)
                    || (filters.enhancement && adv.kind == rum_repo::updateinfo::AdvisoryKind::Enhancement)
                    || (filters.newpackage && adv.kind == rum_repo::updateinfo::AdvisoryKind::Newpackage);
                if !ok {
                    return None;
                }
            }
            if let Some(want) = want_severity {
                if adv.severity < want {
                    return None;
                }
            }
            let mut buckets: Vec<&str> = adv.packages.iter().filter_map(|n| bucket_of(n)).collect();
            buckets.sort_unstable();
            buckets.dedup();
            if !want_names.is_empty() {
                let covers_wanted = adv.packages.iter().filter_map(|n| rum_core::Nevra::parse_nevra(n)).any(|n| want_names.iter().any(|w| *w == n.name));
                if !covers_wanted {
                    return None;
                }
            }
            let selected_buckets: Vec<&str> = if filters.all {
                buckets.clone()
            } else if want_any_bucket {
                buckets.iter().copied().filter(|b| (*b == "installed" && filters.installed) || (*b == "updates" && filters.updates)).collect()
            } else {
                // Default: --available.
                buckets.iter().copied().filter(|b| *b == "available").collect()
            };
            if selected_buckets.is_empty() {
                return None;
            }
            Some((adv, selected_buckets))
        })
        .collect();

    match mode {
        "summary" => {
            let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
            for (adv, _) in &matched {
                *counts.entry(adv.kind.as_str()).or_default() += 1;
            }
            if counts.is_empty() {
                println!("No advisories match the given filters.");
            } else {
                for (kind, count) in &counts {
                    println!("{count:>4} {kind}");
                }
                println!("{:>4} total", matched.len());
            }
        }
        "info" => {
            if matched.is_empty() {
                println!("No advisories match the given filters.");
            }
            for (adv, _) in &matched {
                println!("Advisory ID    : {}", adv.id);
                println!("Type           : {}", adv.kind.as_str());
                println!("Severity       : {}", adv.severity.as_str());
                if !adv.issued.is_empty() {
                    println!("Issued         : {}", adv.issued);
                }
                println!("Title          : {}", adv.title);
                if !adv.description.is_empty() {
                    println!("Description    : {}", adv.description);
                }
                println!("Packages       :");
                for pkg in &adv.packages {
                    println!("  {pkg}");
                }
                println!();
            }
        }
        _ => {
            if matched.is_empty() {
                println!("No advisories match the given filters.");
            } else {
                println!("{:<24} {:<12} {:<10} Title", "Advisory", "Type", "Severity");
                for (adv, _) in &matched {
                    println!("{:<24} {:<12} {:<10} {}", adv.id, adv.kind.as_str(), adv.severity.as_str(), adv.title);
                }
            }
        }
    }
    Ok(())
}

/// `rum check-upgrade`/`check-update` — same candidate selection as
/// [`upgrade`], printed instead of applied. Always overlay-only, even with
/// explicit names — a base-image package can never be upgraded through the
/// overlay, so it must never be reported as having an update available
/// (software centers/daemons poll this to decide what to show/offer).
///
/// This runs the real `rum_resolver::resolve_upgrade` solve (the same path
/// `rum upgrade` itself uses) and reports `Plan::to_upgrade` rather than
/// doing an independent per-name version comparison — a separate raw
/// "installed vs newest-in-repo" diff would drift from what `rum upgrade`
/// actually does, e.g. it used to still list an i686 build whose x86_64
/// sibling is base-owned (and therefore locked across all arches in Split
/// mode — see `rum-solv`'s `build_pool_locked`) even though the real
/// upgrade silently skips it.
async fn check_upgrade(cli: &Cli, paths: &OverlayPaths, names: &[String], json: bool) -> Result<()> {
    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    // dnf's own check-upgrade always hits the network for fresh metadata
    // (unlike list/info, which are happy with a stale cache) — an update
    // check that silently reports against last week's metadata is worse
    // than useless, so force a refresh regardless of metadata_expire=.
    let (_, candidates, _groups, _environments, ..) = load_candidates_force_refresh(cli).await?;

    let installed_names: Vec<&str> = overlay.base.iter().chain(&overlay.overlay).map(|p| p.nevra.name.as_str()).collect();
    let names = &expand_name_patterns(names, &installed_names);

    let locked = rum_transaction::versionlock::list_names(paths)?;
    warn_base_only_names(names, &overlay, "check-upgrade");
    let installed: Vec<&Package> = overlay.overlay.iter().filter(|pkg| names.is_empty() || names.contains(&pkg.nevra.name)).filter(|pkg| !locked.contains(&pkg.nevra.name)).collect();

    let upgrade_names: Vec<String> = installed.iter().map(|p| p.nevra.name.clone()).collect();
    let mut any = false;
    let mut updates = Vec::new();
    if !upgrade_names.is_empty() {
        let mut options = resolve_options(cli, paths);
        options.allow_erasing = cli.allowerasing;
        let host_arch = detect_host_arch(cli);
        let plan = rum_resolver::resolve_upgrade(&upgrade_names, &candidates, &overlay, &host_arch, &options).context("resolving dependencies")?;

        for newer in &plan.to_upgrade {
            // Match the installed package this upgrade replaces — same
            // name/arch when available (the common case), otherwise fall
            // back to name only (libsolv can move a package to a different
            // arch, e.g. syncing a multilib sibling's EVR).
            let inst = installed
                .iter()
                .find(|p| p.nevra.name == newer.nevra.name && p.nevra.arch == newer.nevra.arch)
                .or_else(|| installed.iter().find(|p| p.nevra.name == newer.nevra.name));
            let Some(inst) = inst else { continue };

            any = true;
            if json {
                updates.push(serde_json::json!({
                    "name": inst.nevra.name,
                    "current_version": inst.nevra.evr(),
                    "version": newer.nevra.evr(),
                    "arch": newer.nevra.arch,
                    "repo": newer.repo_id,
                }));
            } else {
                println!("{} -> {} ({})", inst.nevra, newer.nevra, newer.repo_id);
            }
        }
    }
    if json {
        println!("{}", serde_json::json!({ "updates": updates }));
    } else if !any {
        println!("Nothing to upgrade.");
    }
    Ok(())
}

/// Arguments for [`repoquery`] — bundled into a struct since dnf5's
/// `repoquery` has grown enough independent filters/output modes that a
/// positional-argument list stopped being readable.
struct RepoqueryArgs<'a> {
    pattern: Option<&'a str>,
    installed: bool,
    available: bool,
    whatprovides: Option<&'a str>,
    whatrequires: Option<&'a str>,
    whatconflicts: Option<&'a str>,
    whatobsoletes: Option<&'a str>,
    whatrecommends: Option<&'a str>,
    whatenhances: Option<&'a str>,
    whatsuggests: Option<&'a str>,
    whatsupplements: Option<&'a str>,
    recursive: bool,
    arch: Option<&'a str>,
    duplicates: bool,
    info: bool,
    queryformat: Option<&'a str>,
    requires: bool,
    provides: bool,
    conflicts: bool,
    obsoletes: bool,
    recommends: bool,
    enhances: bool,
    suggests: bool,
    supplements: bool,
    srpm: bool,
    location: bool,
    nvr: bool,
    envra: bool,
    extras: bool,
    upgrades: bool,
    querytags: bool,
}

fn pkg_conflicts(pkg: &Package, capability: &str) -> bool {
    pkg.conflicts.iter().any(|d| d.name == capability)
}

fn pkg_obsoletes(pkg: &Package, capability: &str) -> bool {
    pkg.obsoletes.iter().any(|d| d.name == capability)
}

fn pkg_recommends(pkg: &Package, capability: &str) -> bool {
    pkg.recommends.iter().any(|d| d.name == capability)
}

fn pkg_enhances(pkg: &Package, capability: &str) -> bool {
    pkg.enhances.iter().any(|d| d.name == capability)
}

fn pkg_suggests(pkg: &Package, capability: &str) -> bool {
    pkg.suggests.iter().any(|d| d.name == capability)
}

fn pkg_supplements(pkg: &Package, capability: &str) -> bool {
    pkg.supplements.iter().any(|d| d.name == capability)
}

/// `--extras`: true if no candidate in `candidates` shares this installed
/// package's name — i.e. it isn't available from any configured repo.
/// Matched by name only (not arch/EVR), same as dnf's extras semantics.
fn pkg_is_extra(pkg: &Package, candidates: &[Package]) -> bool {
    !candidates.iter().any(|c| c.nevra.name == pkg.nevra.name)
}

/// `--upgrades`: true if some same-name/same-arch candidate has a strictly
/// greater EVR than this installed package.
fn pkg_has_upgrade(pkg: &Package, candidates: &[Package]) -> bool {
    candidates
        .iter()
        .any(|c| c.nevra.name == pkg.nevra.name && c.nevra.arch == pkg.nevra.arch && c.nevra.evr().as_str().compare_evr(&pkg.nevra.evr()) == std::cmp::Ordering::Greater)
}

/// Every capability name a package's own identity could be matched against
/// as a *provider* — its own name/self-provide plus every explicit
/// `Provides:` entry. Used to grow the frontier one hop at a time for
/// `--recursive --whatrequires`.
fn pkg_provided_names(pkg: &Package) -> impl Iterator<Item = &str> {
    std::iter::once(pkg.nevra.name.as_str()).chain(pkg.provides.iter().map(|d| d.name.as_str()))
}

/// dnf-style `--queryformat`/`--qf` substitution: replaces every
/// `%{tag}` occurrence with that field's value. Unrecognized tags are left
/// as-is (matches rpm's own `--queryformat` behavior for unknown tags,
/// rather than erroring on a typo).
fn apply_queryformat(fmt: &str, pkg: &Package) -> String {
    let mut out = String::with_capacity(fmt.len());
    let mut rest = fmt;
    while let Some(start) = rest.find("%{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            break;
        };
        let tag = &after[..end];
        let value = match tag {
            "name" => pkg.nevra.name.clone(),
            "epoch" => pkg.nevra.epoch.to_string(),
            "version" => pkg.nevra.version.clone(),
            "release" => pkg.nevra.release.clone(),
            "evr" => pkg.nevra.evr(),
            "arch" => pkg.nevra.arch.clone(),
            "nevra" => pkg.nevra.to_string(),
            "summary" => pkg.summary.clone(),
            "repoid" => pkg.repo_id.clone(),
            "size" => pkg.download_size.to_string(),
            "installsize" => pkg.install_size.to_string(),
            "location" => pkg.location.clone(),
            other => {
                out.push_str("%{");
                out.push_str(other);
                out.push('}');
                rest = &after[end + 1..];
                continue;
            }
        };
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

async fn repoquery(cli: &Cli, paths: &OverlayPaths, args: RepoqueryArgs<'_>) -> Result<()> {
    if args.querytags {
        for tag in ["name", "epoch", "version", "release", "evr", "arch", "nevra", "summary", "repoid", "size", "installsize", "location"] {
            println!("{tag}");
        }
        return Ok(());
    }

    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let installed_set: Vec<Package> = overlay.base.iter().chain(&overlay.overlay).cloned().collect();
    let candidates: Vec<Package> = if args.available || !args.installed || args.extras || args.upgrades {
        load_candidates(cli).await.map(|(_, c, ..)| c).unwrap_or_default()
    } else {
        Vec::new()
    };

    // `--extras`/`--upgrades` are queries over the installed set compared
    // against repo candidates — they don't make sense layered on top of
    // `--available`'s "repo candidates only" pool, so they force
    // installed-only regardless of `--installed`/`--available`, matching
    // dnf5's own semantics.
    let installed_only_query = args.extras || args.upgrades;
    let mut pool: Vec<Package> = Vec::new();
    if installed_only_query || args.installed || !args.available {
        pool.extend(installed_set.iter().cloned());
    }
    if !installed_only_query && (args.available || !args.installed) {
        pool.extend(candidates.iter().cloned());
    }

    // `--recursive --whatrequires`: BFS over the reverse-dependency graph.
    // A naive re-scan of the whole pool per round is O(pool * frontier)
    // and blows up badly on a broad capability like `glibc` (near-total
    // fanout, thousands of rounds) — instead build a `Requires` name ->
    // requiring-package-indices index once up front, so each round is
    // O(frontier * avg requirers) via direct lookups.
    let recursive_whatrequires: Option<std::collections::HashSet<usize>> = if args.recursive {
        args.whatrequires.map(|cap| {
            let mut requires_index: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
            for (i, pkg) in pool.iter().enumerate() {
                for dep in &pkg.requires {
                    requires_index.entry(dep.name.as_str()).or_default().push(i);
                }
            }
            let mut visited: std::collections::HashSet<usize> = std::collections::HashSet::new();
            let mut frontier: Vec<String> = vec![cap.to_string()];
            loop {
                let newly_matched: Vec<usize> = frontier
                    .iter()
                    .filter_map(|f| requires_index.get(f.as_str()))
                    .flatten()
                    .copied()
                    .filter(|i| !visited.contains(i))
                    .collect::<std::collections::HashSet<usize>>()
                    .into_iter()
                    .collect();
                if newly_matched.is_empty() {
                    break;
                }
                let mut next_frontier = Vec::new();
                for &i in &newly_matched {
                    visited.insert(i);
                    next_frontier.extend(pkg_provided_names(&pool[i]).map(str::to_string));
                }
                frontier = next_frontier;
            }
            visited
        })
    } else {
        None
    };

    // `--duplicates`: only keep name+arch groups with more than one
    // distinct EVR present in the (already filtered-by-everything-else)
    // matched pool — same as dnf5's own "multiple builds available" report.
    let duplicate_names: Option<std::collections::HashSet<(String, String)>> = if args.duplicates {
        let mut evrs: std::collections::HashMap<(String, String), std::collections::HashSet<String>> = std::collections::HashMap::new();
        for pkg in &pool {
            evrs.entry((pkg.nevra.name.clone(), pkg.nevra.arch.clone())).or_default().insert(pkg.nevra.evr());
        }
        Some(evrs.into_iter().filter(|(_, set)| set.len() > 1).map(|(k, _)| k).collect())
    } else {
        None
    };

    // A bare pattern can itself carry dnf's `name.arch` disambiguation
    // syntax (`rum repoquery libatomic.i686`) — without this, patterns
    // were glob-matched only against the plain package name, so an
    // exact-arch query for a real, available package silently returned
    // nothing (the dot-suffix was matched as part of the name and never
    // matched anything). Only split on `.` when the suffix is an arch
    // actually present in the pool, so package names that legitimately
    // contain a dot aren't misparsed.
    let (pattern_name, pattern_arch): (Option<&str>, Option<&str>) = match args.pattern {
        Some(pat) => match pat.rsplit_once('.') {
            Some((base, arch)) if pool.iter().any(|p| p.nevra.arch == arch) => (Some(base), Some(arch)),
            _ => (Some(pat), None),
        },
        None => (None, None),
    };

    let matches: Vec<&Package> = pool
        .iter()
        .enumerate()
        .filter(|(_, pkg)| pattern_name.is_none_or(|pat| glob_match(pat, &pkg.nevra.name)))
        .filter(|(_, pkg)| pattern_arch.is_none_or(|a| pkg.nevra.arch == a))
        .filter(|(_, pkg)| args.arch.is_none_or(|a| glob_match(a, &pkg.nevra.arch)))
        .filter(|(_, pkg)| args.whatprovides.is_none_or(|cap| pkg_provides(pkg, cap)))
        .filter(|(i, pkg)| match &recursive_whatrequires {
            Some(visited) => visited.contains(i),
            None => args.whatrequires.is_none_or(|cap| pkg_requires(pkg, cap)),
        })
        .filter(|(_, pkg)| args.whatconflicts.is_none_or(|cap| pkg_conflicts(pkg, cap)))
        .filter(|(_, pkg)| args.whatobsoletes.is_none_or(|cap| pkg_obsoletes(pkg, cap)))
        .filter(|(_, pkg)| args.whatrecommends.is_none_or(|cap| pkg_recommends(pkg, cap)))
        .filter(|(_, pkg)| args.whatenhances.is_none_or(|cap| pkg_enhances(pkg, cap)))
        .filter(|(_, pkg)| args.whatsuggests.is_none_or(|cap| pkg_suggests(pkg, cap)))
        .filter(|(_, pkg)| args.whatsupplements.is_none_or(|cap| pkg_supplements(pkg, cap)))
        .filter(|(_, pkg)| duplicate_names.as_ref().is_none_or(|dups| dups.contains(&(pkg.nevra.name.clone(), pkg.nevra.arch.clone()))))
        .filter(|(_, pkg)| !args.srpm || pkg.nevra.arch == "src")
        .filter(|(_, pkg)| !args.extras || pkg_is_extra(pkg, &candidates))
        .filter(|(_, pkg)| !args.upgrades || pkg_has_upgrade(pkg, &candidates))
        .map(|(_, pkg)| pkg)
        .collect();

    for pkg in matches {
        if let Some(fmt) = args.queryformat {
            println!("{}", apply_queryformat(fmt, pkg));
        } else if args.info {
            println!("Name         : {}", pkg.nevra.name);
            println!("Version      : {}", pkg.nevra.evr());
            println!("Architecture : {}", pkg.nevra.arch);
            println!("Repository   : {}", if pkg.repo_id.is_empty() { "installed" } else { &pkg.repo_id });
            println!("Size         : {}", rum_core::format_size(pkg.install_size));
            println!("Summary      : {}", pkg.summary);
            println!();
        } else if args.requires {
            for d in &pkg.requires {
                println!("{}: {}", pkg.nevra, d.name);
            }
        } else if args.provides {
            for d in &pkg.provides {
                println!("{}: {}", pkg.nevra, d.name);
            }
        } else if args.conflicts {
            for d in &pkg.conflicts {
                println!("{}: {}", pkg.nevra, d.name);
            }
        } else if args.obsoletes {
            for d in &pkg.obsoletes {
                println!("{}: {}", pkg.nevra, d.name);
            }
        } else if args.recommends {
            for d in &pkg.recommends {
                println!("{}: {}", pkg.nevra, d.name);
            }
        } else if args.enhances {
            for d in &pkg.enhances {
                println!("{}: {}", pkg.nevra, d.name);
            }
        } else if args.suggests {
            for d in &pkg.suggests {
                println!("{}: {}", pkg.nevra, d.name);
            }
        } else if args.supplements {
            for d in &pkg.supplements {
                println!("{}: {}", pkg.nevra, d.name);
            }
        } else if args.location {
            println!("{}", pkg.location);
        } else if args.envra {
            println!("{}:{}-{}-{}.{}", pkg.nevra.epoch, pkg.nevra.name, pkg.nevra.version, pkg.nevra.release, pkg.nevra.arch);
        } else if args.nvr {
            println!("{}-{}-{}", pkg.nevra.name, pkg.nevra.version, pkg.nevra.release);
        } else {
            // `--nevra` is the same as the bare default (NEVRA is already
            // what's printed with no output-mode flag at all) — kept as an
            // explicit, documented flag for parity with dnf5's own
            // `--nevra`, not because it changes behavior.
            println!("{}", pkg.nevra);
        }
    }
    Ok(())
}

/// `rum info <name>` — installed copy (if any) first, then every matching
/// repo candidate, same "Installed Packages" / "Available Packages"
/// sectioning dnf uses.
async fn info(cli: &Cli, paths: &OverlayPaths, name: &str) -> Result<()> {
    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let mut printed = false;
    for pkg in overlay.base.iter().chain(&overlay.overlay) {
        if pkg.nevra.name == name {
            println!("Name         : {}", pkg.nevra.name);
            println!("Version      : {}", pkg.nevra.evr());
            println!("Architecture : {}", pkg.nevra.arch);
            println!("Repository   : installed");
            println!("Summary      : {}", pkg.summary);
            println!();
            printed = true;
        }
    }
    if let Ok((_, candidates, _groups, _environments, ..)) = load_candidates(cli).await {
        for pkg in &candidates {
            if pkg.nevra.name == name {
                println!("Name         : {}", pkg.nevra.name);
                println!("Version      : {}", pkg.nevra.evr());
                println!("Architecture : {}", pkg.nevra.arch);
                println!("Repository   : {}", pkg.repo_id);
                println!("Summary      : {}", pkg.summary);
                println!();
                printed = true;
            }
        }
    }
    if !printed {
        println!("No package named '{name}' found.");
    }
    Ok(())
}

/// `rum leaves` — overlay-installed packages nothing else installed
/// `Requires` (by name or by any of its `Provides`).
fn leaves(paths: &OverlayPaths) -> Result<()> {
    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let required_names: std::collections::HashSet<&str> =
        overlay.base.iter().chain(&overlay.overlay).flat_map(|pkg| pkg.requires.iter().map(|d| d.name.as_str())).collect();

    for pkg in &overlay.overlay {
        let is_leaf = !required_names.contains(pkg.nevra.name.as_str()) && !pkg.provides.iter().any(|p| required_names.contains(p.name.as_str()));
        if is_leaf {
            println!("{}", pkg.nevra);
        }
    }
    Ok(())
}

/// `rum clean [all|packages|metadata]` — `packages` removes downloaded
/// `.rpm`s from under `cache_dir/repos/<repo>-<hash>/packages/` (matching
/// the hashed per-repo cache dir [`rum_transaction::download_all_ex`] now
/// shares with the metadata cache — see its doc comment; a flat `.rpm`
/// directly under `cache_dir` is also removed for compatibility with `rum
/// download --destdir` output left in the default cache dir), `metadata`
/// removes `cache_dir/repos` (repo metadata cache) wholesale, `all` does
/// both. Walks `dir` removing every `.rpm` file, recursing into every
/// subdirectory including `repos` — safe to share that walk with the
/// metadata cache since metadata files (`repomd.xml`, `primary.xml`,
/// `modules.yaml`, ...) never carry a `.rpm` extension, so this never
/// touches them; `rum clean metadata` removes the rest of `repos` itself.
/// Returns the count removed.
fn remove_rpms_recursive(dir: &Path) -> u64 {
    let mut removed = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            removed += remove_rpms_recursive(&path);
        } else if path.extension().is_some_and(|e| e == "rpm") && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn clean(cli: &Cli, what: CleanWhat) -> Result<()> {
    let mut removed = 0u64;
    if matches!(what, CleanWhat::Packages | CleanWhat::All) {
        removed += remove_rpms_recursive(&cli.cache_dir);
    }
    if matches!(what, CleanWhat::Metadata | CleanWhat::All) {
        let repos_dir = cli.cache_dir.join("repos");
        if matches!(what, CleanWhat::All) {
            // `all` really does mean everything, packages included.
            if repos_dir.exists() {
                std::fs::remove_dir_all(&repos_dir).with_context(|| format!("removing {}", repos_dir.display()))?;
                removed += 1;
            }
        } else {
            // Metadata-only: each per-repo dir under `repos/` holds
            // `repomd.xml`/`primary.xml`/`baseurl.txt`/`comps.xml`/etc.
            // directly, plus a nested `packages/` dir with downloaded RPMs
            // (see `rum_repo::repo_cache_dir`). A blanket `remove_dir_all`
            // on the whole per-repo dir would also delete `packages/` —
            // wiping out anything staged there for offline reinstall (e.g.
            // `rakuos reset-overlay --soft`'s pre-download step) even
            // though only the metadata was asked to be cleared.
            for entry in std::fs::read_dir(&repos_dir).into_iter().flatten().flatten() {
                let repo_dir = entry.path();
                if !repo_dir.is_dir() {
                    continue;
                }
                for file in std::fs::read_dir(&repo_dir).into_iter().flatten().flatten() {
                    let path = file.path();
                    if path.is_dir() {
                        continue;
                    }
                    if std::fs::remove_file(&path).is_ok() {
                        removed += 1;
                    }
                }
            }
        }
    }
    println!("Removed {removed} cache item(s).");
    Ok(())
}

/// `rum download` — resolves like `install` but only downloads, into
/// `destdir` (defaulting to `--cache-dir`), never touching the rpmdb.
async fn download(cli: &Cli, paths: &OverlayPaths, packages: &[String], destdir: Option<&Path>, srpm: bool, releasever: Option<&str>) -> Result<()> {
    anyhow::ensure!(!packages.is_empty(), "no packages given");
    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let (repo_configs, candidates, _groups, _environments, _modules, _module_defaults, fallback_candidates, _advisories) = match releasever {
        Some(rv) => {
            let mut vars = detect_vars(cli);
            vars.releasever = rv.to_string();
            load_candidates_with_vars(cli, vars, cli.refresh).await?
        }
        None => load_candidates(cli).await?,
    };
    let host_arch = detect_host_arch(cli);

    let to_install = if srpm {
        // A source package has no installable Requires closure to run
        // through the resolver — dnf5's own `--srpm` (`DownloadCommand::
        // run`, `download.cpp`) skips dependency resolution entirely and
        // just looks the name up directly against `arch == "src"`. Same
        // here: pick the newest `src`-arch candidate per requested name,
        // straight out of whatever source repo the caller enabled (e.g.
        // Fedora's `rawhide-source` via `--enablerepo`).
        let mut picked = Vec::new();
        for name in packages {
            let best = candidates
                .iter()
                .filter(|p| p.nevra.name == *name && p.nevra.arch == "src")
                .max_by(|a, b| a.nevra.evr().as_str().compare_evr(&b.nevra.evr()));
            match best {
                Some(pkg) => picked.push(pkg.clone()),
                None if cli.main_conf.skip_if_unavailable => {
                    eprintln!("warning: no source package found for '{name}'");
                }
                None => anyhow::bail!("no source package \"{name}\" available; enable its source repo (e.g. --enablerepo=*-source)"),
            }
        }
        picked
    } else {
        // Unlike `install`, `download` never touches the rpmdb — there's
        // no reason an already-installed package should short-circuit to
        // "already satisfied" and get skipped here the way it does for
        // `install`. A caller asking to download package X wants X's RPM
        // sitting in the cache regardless of whether it happens to be
        // installed right now (e.g. `rakuos reset-overlay --soft` pre-
        // populating rum's package cache from the overlay rpmdb before
        // wiping it — every one of those names *is* currently installed).
        // `force_names` (the same mechanism `swap`'s install half uses)
        // makes the resolver actually pick a real candidate instead of
        // treating the name as already met.
        let mut options = resolve_options(cli, paths);
        options.force_names.extend(packages.iter().cloned());
        let plan = rum_resolver::resolve_ex(packages, &candidates, &overlay, &host_arch, &options).context("resolving dependencies")?;
        plan.to_install
    };

    if to_install.is_empty() {
        println!("Nothing to download — every requested package is already satisfied.");
        return Ok(());
    }

    let per_repo = destdir.is_none();
    let dest = destdir.unwrap_or(&cli.cache_dir);
    let client = build_client(cli)?;
    // `dest` may be a caller-supplied `--destdir` distinct from
    // `cli.cache_dir` — repo metadata (and the `baseurl.txt` the same-repo
    // mirror-fallback needs) was cached under the latter by `load_candidates`
    // above, not `dest`, so that's what must be passed as the metadata root.
    let downloaded = rum_transaction::download_all_ex_with_metadata_root(&client, &to_install, dest, &cli.cache_dir, cli.main_conf.max_parallel_downloads, per_repo, &repo_configs, &fallback_candidates).await.context("downloading packages")?;
    for d in &downloaded {
        println!("{}", d.rpm_path.display());
    }
    Ok(())
}

/// `rum reposync` — downloads every candidate package from the enabled
/// (`--repo`/`--repoid`-filtered) repos to disk, same file layout
/// `download`/`install` already use (per-repo-id subdirectories by
/// default, `download_all_ex` handles the actual fetch/checksum work).
async fn reposync(cli: &Cli, download_path: Option<&Path>, newest_only: bool, norepopath: bool) -> Result<()> {
    let (repo_configs, mut candidates, _groups, _environments, ..) = load_candidates(cli).await?;
    anyhow::ensure!(!repo_configs.is_empty(), "no enabled repos found");

    if newest_only {
        use std::collections::HashMap;
        let mut best: HashMap<(String, String), usize> = HashMap::new();
        for (i, pkg) in candidates.iter().enumerate() {
            match best.get(&(pkg.nevra.name.clone(), pkg.nevra.arch.clone())) {
                Some(&j) if candidates[j].nevra.evr().as_str().compare_evr(&pkg.nevra.evr()).is_ge() => {}
                _ => {
                    best.insert((pkg.nevra.name.clone(), pkg.nevra.arch.clone()), i);
                }
            }
        }
        let mut keep: Vec<usize> = best.into_values().collect();
        keep.sort_unstable();
        candidates = keep.into_iter().map(|i| candidates[i].clone()).collect();
    }

    let dest = download_path.unwrap_or(&cli.cache_dir);
    let client = build_client(cli)?;
    // Deliberately `&[]`, not `&repo_configs`: reposync's destination is
    // routinely a user-specified mirror/export directory (`--download-path`)
    // rather than rum's own internal package cache, so it keeps the plain
    // `<dest>/<repo_id>/packages/` layout instead of nesting under the
    // opaque hashed dirs the real cache uses (see `download_all_ex`'s doc).
    let downloaded = rum_transaction::download_all_ex(&client, &candidates, dest, cli.main_conf.max_parallel_downloads, !norepopath, &[], &[]).await.context("downloading packages")?;
    for d in &downloaded {
        println!("{}", d.rpm_path.display());
    }
    println!("reposync: downloaded {} package(s) to {}", downloaded.len(), dest.display());
    Ok(())
}

/// `rum repomanage <path>` — dnf-utils' `repomanage`: scans a plain
/// directory of `.rpm` *files* (not repo metadata) and reports which are
/// "old" (all but the newest `--keep` versions of each name+arch) or, with
/// `--new`, the newest `--keep` themselves. Header info comes from `rpm -qp`
/// directly against each file, the same "no --dbpath needed, it's just
/// reading the file's own header" pattern `changelog` uses for installed
/// packages.
fn repomanage(dir: &Path, new: bool, keep: u32) -> Result<()> {
    anyhow::ensure!(dir.is_dir(), "'{}' is not a directory", dir.display());

    let mut rpm_paths = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).with_context(|| format!("reading directory {}", d.display()))? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rpm") {
                rpm_paths.push(path);
            }
        }
    }

    // name, epoch:version-release, arch, path — extracted straight from
    // each file's own header, no rpmdb involved.
    struct Entry {
        name: String,
        evr: String,
        arch: String,
        path: PathBuf,
    }
    let mut entries = Vec::new();
    for path in rpm_paths {
        let out = std::process::Command::new("rpm")
            .args(["-qp", "--qf", "%{NAME}\t%|EPOCH?{%{EPOCH}}:{0}|:%{VERSION}-%{RELEASE}\t%{ARCH}", "--nosignature"])
            .arg(&path)
            .output()
            .with_context(|| format!("running rpm -qp on {}", path.display()))?;
        if !out.status.success() {
            eprintln!("warning: skipping {}: rpm -qp failed", path.display());
            continue;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut parts = text.trim().splitn(3, '\t');
        let (Some(name), Some(evr), Some(arch)) = (parts.next(), parts.next(), parts.next()) else {
            eprintln!("warning: skipping {}: couldn't parse rpm header", path.display());
            continue;
        };
        entries.push(Entry { name: name.to_string(), evr: evr.to_string(), arch: arch.to_string(), path });
    }

    use std::collections::HashMap;
    let mut groups: HashMap<(String, String), Vec<Entry>> = HashMap::new();
    for e in entries {
        groups.entry((e.name.clone(), e.arch.clone())).or_default().push(e);
    }

    let mut selected: Vec<PathBuf> = Vec::new();
    for group in groups.values_mut() {
        group.sort_by(|a, b| b.evr.as_str().compare_evr(a.evr.as_str()));
        let keep = keep as usize;
        let (newest, older) = if group.len() > keep { group.split_at(keep) } else { (group.as_slice(), &group[group.len()..]) };
        let picked = if new { newest } else { older };
        selected.extend(picked.iter().map(|e| e.path.clone()));
    }
    selected.sort();
    for p in &selected {
        println!("{}", p.display());
    }
    Ok(())
}

/// `rum repoclosure` — checks every candidate from the enabled repos (or
/// just the `--pkg`-filtered subset) has every `Requires` satisfiable by
/// something in the full candidate pool. See
/// [`rum_resolver::check_closure`] for the actual check.
async fn repoclosure(cli: &Cli, pkg_patterns: &[String]) -> Result<()> {
    let (repo_configs, candidates, _groups, _environments, ..) = load_candidates(cli).await?;
    anyhow::ensure!(!repo_configs.is_empty(), "no enabled repos found");
    let host_arch = detect_host_arch(cli);
    let unresolved = rum_resolver::check_closure(&candidates, pkg_patterns, &host_arch);
    if unresolved.is_empty() {
        println!("repoclosure: no unresolved dependencies found");
        return Ok(());
    }
    for (nevra, missing) in &unresolved {
        println!("package: {nevra}");
        for m in missing {
            println!("  unresolved deps:");
            println!("    {m}");
        }
    }
    anyhow::bail!("repoclosure found {} package(s) with unresolved dependencies", unresolved.len());
}

/// `rum changelog <name>...` — `rpm -q --changelog` against whichever
/// dbpath (base or overlay) actually has the package installed; installed
/// packages only, same as dnf's own `changelog` without `--upgrades`.
fn changelog(paths: &OverlayPaths, packages: &[String]) -> Result<()> {
    let dbpaths: Vec<Option<&Path>> = match &paths.mode {
        rum_overlay::OverlayMode::Split { base_snapshot_rpmdb, overlay_rpmdb } => vec![Some(base_snapshot_rpmdb.as_path()), Some(overlay_rpmdb.as_path())],
        rum_overlay::OverlayMode::Standalone => vec![None],
    };
    for name in packages {
        let mut found = false;
        for dbpath in &dbpaths {
            let mut cmd = std::process::Command::new("rpm");
            if let Some(p) = dbpath {
                cmd.arg("--dbpath").arg(p);
            }
            let output = cmd.arg("-q").arg("--changelog").arg(name).output().context("spawning rpm -q --changelog")?;
            if output.status.success() {
                found = true;
                print!("{}", String::from_utf8_lossy(&output.stdout));
            }
        }
        if !found {
            println!("{name}: not installed");
        }
    }
    Ok(())
}

/// `rum needs-restarting` — scans every readable `/proc/<pid>/maps` for a
/// mapped file whose entry is suffixed `(deleted)` (the kernel's own marker
/// for "this mapped file no longer exists at that path," almost always
/// because a package update replaced it out from under a running process),
/// and reports the affected PIDs. Doesn't attempt dnf's fuller "which
/// systemd unit owns this and should be restarted" classification — that
/// needs a live systemd/D-Bus connection rum has no other reason to depend
/// on.
fn needs_restarting() -> Result<()> {
    let mut any = false;
    for entry in std::fs::read_dir("/proc").context("reading /proc")?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let maps_path = entry.path().join("maps");
        let Ok(maps) = std::fs::read_to_string(&maps_path) else { continue };
        let deleted: Vec<&str> = maps.lines().filter(|l| l.ends_with("(deleted)")).filter_map(|l| l.split_whitespace().last()).collect();
        if !deleted.is_empty() {
            any = true;
            let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
            println!("{pid} : {}", comm.trim());
            for d in deleted {
                println!("    {d}");
            }
        }
    }
    if !any {
        println!("No processes need restarting.");
    }
    Ok(())
}

fn check(paths: &OverlayPaths, installonlypkgs: &[String], dependencies: bool, duplicates: bool, obsoleted: bool) -> Result<()> {
    let overlay = OverlayContext::load(paths).context("loading overlay context")?;
    let all: Vec<&Package> = overlay.base.iter().chain(&overlay.overlay).collect();

    let mut problems = 0;

    if dependencies {
        for pkg in &all {
            for req in &pkg.requires {
                if req.name.starts_with("rpmlib(") {
                    continue;
                }
                if !overlay.satisfied(req, Some(pkg.nevra.arch.as_str())) {
                    problems += 1;
                    println!("{}: missing require \"{}\"", pkg.nevra, req.name);
                }
            }
            for conflict in &pkg.conflicts {
                for other in &all {
                    if std::ptr::eq(*pkg, *other) || other.nevra.name == pkg.nevra.name {
                        continue;
                    }
                    if conflict.name == other.nevra.name || other.provides.iter().any(|p| p.name == conflict.name && conflict.satisfied_by_evr(&other.nevra.evr())) {
                        problems += 1;
                        println!("{}: installed conflict \"{}\" from \"{}\"", pkg.nevra, conflict.name, other.nevra);
                    }
                }
            }
        }
    }

    if obsoleted {
        for pkg in &all {
            for other in &all {
                if std::ptr::eq(*pkg, *other) || other.nevra.name == pkg.nevra.name {
                    continue;
                }
                let hit = other.obsoletes.iter().any(|ob| {
                    ob.name == pkg.nevra.name || pkg.provides.iter().any(|p| p.name == ob.name && ob.satisfied_by_evr(&pkg.nevra.evr()))
                });
                if hit {
                    problems += 1;
                    println!("{}: obsoleted by \"{}\" from \"{}\"", pkg.nevra, other.nevra, other.repo_id);
                }
            }
        }
    }

    if duplicates {
        let mut by_name_arch: std::collections::BTreeMap<(&str, &str), Vec<&Package>> = std::collections::BTreeMap::new();
        for pkg in &all {
            by_name_arch.entry((pkg.nevra.name.as_str(), pkg.nevra.arch.as_str())).or_default().push(pkg);
        }
        for ((name, _arch), pkgs) in &by_name_arch {
            if pkgs.len() < 2 || rum_core::name_matches_any(name, installonlypkgs) {
                continue;
            }
            for pkg in pkgs {
                problems += 1;
                println!("{}: duplicate with \"{}\"", pkg.nevra, name);
            }
        }
    }

    if problems == 0 {
        println!("No problems found.");
    } else {
        anyhow::bail!("{problems} problem(s) found");
    }
    Ok(())
}

/// `[commands]`/`[emitters]` from `automatic.conf` — mirrors dnf5's
/// `ConfigAutomaticCommands`/`ConfigAutomaticEmitters`. Email/D-Bus/command
/// emitters and the `[email]`/`[command]`/`[command_email]` sections aren't
/// implemented (`stdio`/`motd` only) — those need MTA/D-Bus plumbing this
/// codebase has no other use for, out of proportion with what a RakuOS
/// system (which has its own update surfacing) actually needs from this
/// command.
struct AutomaticConfig {
    upgrade_type: String,
    random_sleep: u32,
    network_online_timeout: i32,
    download_updates: bool,
    apply_updates: bool,
    reboot: String,
    reboot_command: String,
    emit_via: Vec<String>,
    emit_no_updates: bool,
}

impl Default for AutomaticConfig {
    fn default() -> Self {
        Self {
            upgrade_type: "default".to_string(),
            random_sleep: 0,
            network_online_timeout: 60,
            download_updates: true,
            apply_updates: false,
            reboot: "never".to_string(),
            reboot_command: "shutdown -r +5 'Rebooting after applying package updates'".to_string(),
            emit_via: vec!["stdio".to_string()],
            emit_no_updates: false,
        }
    }
}

/// Reads `automatic.conf` from dnf5's search path (`/usr` overridden by
/// `/etc/dnf`) plus a RakuOS-specific `/etc/rum/automatic.conf` on top (read
/// last, so it wins on any key collision) — same "later file wins per-key"
/// layering `pre_configure()` in dnf5's `automatic_plugin` uses, just
/// without the installroot/host-config indirection rum doesn't need here.
fn load_automatic_config() -> AutomaticConfig {
    let mut cfg = AutomaticConfig::default();
    for path in ["/usr/share/dnf5/dnf5-plugins/automatic.conf", "/etc/dnf/dnf5-plugins/automatic.conf", "/etc/dnf/automatic.conf", "/etc/rum/automatic.conf"] {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let mut section = String::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = name.trim().to_string();
                continue;
            }
            let Some((key, val)) = line.split_once('=') else { continue };
            let (key, val) = (key.trim(), val.trim());
            match (section.as_str(), key) {
                ("commands", "upgrade_type") => cfg.upgrade_type = val.to_string(),
                ("commands", "random_sleep") => cfg.random_sleep = val.parse().unwrap_or(cfg.random_sleep),
                ("commands", "network_online_timeout") => cfg.network_online_timeout = val.parse().unwrap_or(cfg.network_online_timeout),
                ("commands", "download_updates") => cfg.download_updates = val.eq_ignore_ascii_case("yes") || val == "1" || val.eq_ignore_ascii_case("true"),
                ("commands", "apply_updates") => cfg.apply_updates = val.eq_ignore_ascii_case("yes") || val == "1" || val.eq_ignore_ascii_case("true"),
                ("commands", "reboot") => cfg.reboot = val.to_string(),
                ("commands", "reboot_command") => cfg.reboot_command = val.to_string(),
                ("emitters", "emit_via") => cfg.emit_via = val.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
                ("emitters", "emit_no_updates") => cfg.emit_no_updates = val.eq_ignore_ascii_case("yes") || val == "1" || val.eq_ignore_ascii_case("true"),
                _ => {}
            }
        }
    }
    cfg
}

struct AutomaticOverrides {
    timer: bool,
    download_updates: Option<bool>,
    apply_updates: Option<bool>,
}

/// Best-effort equivalent of dnf5's `wait_for_network`: tries a plain TCP
/// connect to each enabled repo's host (baseurl/mirrorlist/metalink, in that
/// order) once a second until `timeout` elapses or one succeeds. `timeout <=
/// 0` skips the check entirely, same as dnf5.
fn wait_for_network(repo_configs: &[RepoConfig], timeout: i32) {
    if timeout <= 0 {
        return;
    }
    let hosts: Vec<(String, u16)> = repo_configs
        .iter()
        .filter(|r| r.enabled)
        .filter_map(|r| r.base_url.as_deref().or(r.mirrorlist.as_deref()).or(r.metalink.as_deref()))
        .filter_map(|u| reqwest::Url::parse(u).ok())
        .filter_map(|u| Some((u.host_str()?.to_string(), u.port_or_known_default().unwrap_or(443))))
        .collect();
    if hosts.is_empty() {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout.max(0) as u64);
    loop {
        for (host, port) in &hosts {
            if let Ok(mut addrs) = (host.as_str(), *port).to_socket_addrs() {
                if let Some(addr) = addrs.next() {
                    if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(1)).is_ok() {
                        return;
                    }
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("warning: no network connection detected after {timeout}s, proceeding anyway");
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// dnf-automatic/dnf5-automatic equivalent: resolve an upgrade
/// non-interactively, optionally download it, optionally apply it — driven
/// by `automatic.conf` rather than requiring per-invocation CLI flags (see
/// `Command::Automatic`'s doc comment). Meant to run from a periodic timer,
/// not interactively.
async fn automatic(cli: &mut Cli, paths: &OverlayPaths, overrides: AutomaticOverrides) -> Result<()> {
    let mut config = load_automatic_config();
    if let Some(v) = overrides.download_updates {
        config.download_updates = v;
    }
    if let Some(v) = overrides.apply_updates {
        config.apply_updates = v;
    }

    if overrides.timer && config.random_sleep > 0 {
        // No `rand` crate dependency for one call site — a coarse
        // nanosecond-clock-driven jitter is good enough for its actual
        // purpose (spreading a timer fan-out across mirrors), not
        // cryptographic unpredictability.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let delay = nanos % (config.random_sleep + 1);
        std::thread::sleep(std::time::Duration::from_secs(delay as u64));
    }

    let vars = detect_vars(cli);
    let repo_configs = rum_repo::load_repo_configs(&cli.repo_dir, &vars).with_context(|| format!("loading repo configs from {}", cli.repo_dir.display()))?;
    wait_for_network(&repo_configs, config.network_online_timeout);

    // Non-interactive by construction: a timer/cron invocation has no
    // terminal to prompt on, and `download_updates`/`apply_updates` already
    // gate everything meaningful this command does.
    cli.assume_yes = true;
    cli.downloadonly = !config.apply_updates;

    let ran = if config.download_updates || config.apply_updates {
        let result = match config.upgrade_type.as_str() {
            "distro-sync" => sync_packages(cli, paths, &[], false, SyncMode::DistroSync).await,
            "security" => {
                let filter = AdvisoryUpgradeFilter { ids: &[], security: true, bugfix: false, enhancement: false, severity: None };
                upgrade(cli, paths, &[], false, &filter).await
            }
            _ => {
                let filter = AdvisoryUpgradeFilter { ids: &[], security: false, bugfix: false, enhancement: false, severity: None };
                upgrade(cli, paths, &[], false, &filter).await
            }
        };
        if let Err(e) = &result {
            eprintln!("Transaction failed: {e:#}");
        }
        Some(result)
    } else {
        None
    };
    let success = !matches!(ran, Some(Err(_)));

    if config.emit_no_updates || !success || ran.is_some() {
        for emitter in &config.emit_via {
            match emitter.as_str() {
                "stdio" => { /* upgrade()/sync_packages() above already printed the transaction to stdout — nothing extra to emit. */ }
                "motd" => {
                    let msg = if success { "rum automatic: transaction applied successfully.\n" } else { "rum automatic: transaction failed — see logs.\n" };
                    let _ = std::fs::write("/etc/motd", msg);
                }
                other => eprintln!("warning: unknown automatic.conf emit_via emitter \"{other}\" (only stdio/motd are implemented)"),
            }
        }
    }

    if !success {
        anyhow::bail!("automatic transaction failed");
    }

    if config.apply_updates && ran.is_some() && config.reboot != "never" {
        // `when-needed` would normally only reboot if a "reboot suggested"
        // package (kernel etc.) was part of the transaction; rum doesn't
        // track that classification, so it's folded into `when-changed`'s
        // behavior (reboot on any applied transaction) rather than silently
        // never rebooting for it.
        let status = std::process::Command::new("sh").arg("-c").arg(&config.reboot_command).status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => anyhow::bail!("reboot command exited with status {s}"),
            Err(e) => anyhow::bail!("failed to run reboot command: {e}"),
        }
    }

    Ok(())
}

fn repo_list(cli: &Cli, all: bool) -> Result<()> {
    let vars = detect_vars(cli);
    let configs = rum_repo::load_repo_configs(&cli.repo_dir, &vars).with_context(|| format!("loading repo configs from {}", cli.repo_dir.display()))?;
    for cfg in &configs {
        if cfg.enabled || all {
            println!("{:<20} {}", cfg.id, if cfg.enabled { "enabled" } else { "disabled" });
        }
    }
    Ok(())
}

fn repo_info(cli: &Cli, id: &str) -> Result<()> {
    let vars = detect_vars(cli);
    let configs = rum_repo::load_repo_configs(&cli.repo_dir, &vars).with_context(|| format!("loading repo configs from {}", cli.repo_dir.display()))?;
    let cfg = configs.iter().find(|c| c.id == id).with_context(|| format!("no repo '{id}' found"))?;
    println!("Id           : {}", cfg.id);
    println!("Enabled      : {}", cfg.enabled);
    println!("Base URL     : {}", cfg.base_url.as_deref().unwrap_or("-"));
    println!("Mirrorlist   : {}", cfg.mirrorlist.as_deref().unwrap_or("-"));
    println!("Metalink     : {}", cfg.metalink.as_deref().unwrap_or("-"));
    println!("GPG check    : {}", cfg.gpgcheck);
    println!("Metadata exp.: {}s", cfg.metadata_expire);
    Ok(())
}
