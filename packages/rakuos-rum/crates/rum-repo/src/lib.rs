//! Fetches and parses standard yum/dnf repo metadata (`repomd.xml` +
//! `primary.xml[.gz]`) — this is the "rum" half of the name: rum still
//! consumes plain yum-compatible repos, it just resolves and applies them
//! itself instead of handing the job to dnf.

pub mod comps;
pub mod copr;
pub mod modulemd;
mod primary;
pub mod updateinfo;

use anyhow::{bail, Context, Result};
use rum_core::Package;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub use comps::{Environment, Group};
pub use modulemd::Module;
pub use primary::parse_primary_xml;
pub use updateinfo::{parse_updateinfo_xml, Advisory};
pub use net::{
    default_net_options, download_packages_batch, download_to_file_with_retry, download_to_file_with_retry_ex, fastest_mirror_sort, fastestmirror_enabled, get_bytes_with_retry, get_with_retry,
    set_default_net_options, set_fastestmirror, set_max_attempts, set_minrate, set_stall_timeout, verify_checksum, with_net_options, BatchOutcome, BatchSpec, NetOptions,
};

/// Builds the [`NetOptions`] a repo's own `.repo`-file settings ask for —
/// `proxy=`/`username=`/`password=`/`sslcacert=`/`sslclientcert=`/
/// `sslclientkey=`, matching dnf's per-section option names — falling back
/// to the global `rum.conf` defaults (via [`NetOptions::or_default`]) for
/// anything this repo didn't set itself, same precedence dnf5's
/// `ConfigRepo`/`ConfigMain` `OptionChild` inheritance gives proxy settings.
/// Callers wrap a repo's network calls in
/// `rum_repo::with_net_options(rum_repo::net_options_for_repo(cfg), async { ... })`.
pub fn net_options_for_repo(cfg: &RepoConfig) -> NetOptions {
    NetOptions {
        proxy: cfg.proxy.clone(),
        proxy_userpwd: match (&cfg.proxy_username, &cfg.proxy_password) {
            (Some(u), p) => Some(format!("{u}:{}", p.as_deref().unwrap_or(""))),
            (None, _) => None,
        },
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        sslcacert: cfg.sslcacert.clone(),
        sslclientcert: cfg.sslclientcert.clone(),
        sslclientkey: cfg.sslclientkey.clone(),
    }
    .or_default()
}

mod net;

/// `group_package_types=`: process-wide, set once near the top of `run()`
/// (mirroring [`set_fastestmirror`]/[`set_max_attempts`] etc.) so
/// `load_repo_ex`'s comps parsing can read it without threading a new
/// parameter through every call site between `main` and `comps.rs`.
static GROUP_PACKAGE_TYPES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

pub fn set_group_package_types(types: Vec<String>) {
    let _ = GROUP_PACKAGE_TYPES.set(types);
}

fn group_package_types() -> &'static [String] {
    GROUP_PACKAGE_TYPES.get_or_init(|| vec!["mandatory".to_string(), "default".to_string()])
}

/// A configured package source, analogous to a `.repo` file `[section]`.
#[derive(Debug, Clone)]
pub struct RepoConfig {
    pub id: String,
    /// `name=` from the `.repo` file (dnf's human-readable repo
    /// description, e.g. `"Fedora 44 - x86_64 - Updates"`) — used in
    /// dnf-style progress output instead of the bare `id`. Falls back to
    /// `id` when unset, same as dnf itself does.
    pub name: String,
    /// Base URL, no trailing slash (e.g. `https://repo.rakuos.org/rakuos/44`),
    /// with `$releasever`/`$basearch` already expanded. `None` when the repo
    /// only configured `mirrorlist=`/`metalink=` — [`load_repo`] resolves
    /// one of those to a concrete base URL at fetch time, since doing so
    /// requires a network round-trip and can't happen during the plain INI
    /// parse in [`load_repo_configs`].
    pub base_url: Option<String>,
    /// `mirrorlist=` URL: a plain-text, one-URL-per-line list of mirror
    /// base URLs (optionally with `repodata/repomd.xml` appended, same as
    /// dnf/yum mirrorlists commonly do).
    pub mirrorlist: Option<String>,
    /// `metalink=` URL: an RFC 5854 metalink XML document (as served by
    /// `mirrors.fedoraproject.org`) whose `<url>` entries point directly at
    /// a mirror's `repomd.xml`.
    pub metalink: Option<String>,
    /// Whether downloaded packages from this repo must carry a valid
    /// signature from a key in `gpgkey` before `rum-transaction` will
    /// install them. Defaults to `true` to match dnf/yum's default —
    /// unlike dnf, rum refuses to silently treat a missing `gpgcheck=`
    /// line as "off".
    pub gpgcheck: bool,
    /// `gpgkey=` URL(s) (space-separated in the `.repo` file, same as
    /// dnf), used to `rpm --import` before checking signatures.
    pub gpgkeys: Vec<String>,
    /// `metadata_expire=` in seconds: how long cached `repomd.xml`/
    /// `primary.xml` stay trusted before [`load_repo`] re-fetches them.
    /// `u64::MAX` represents `-1`/`never` (dnf's "cache never expires on
    /// its own" setting). Defaults to 172800 (48h), matching dnf's own
    /// built-in default for repos that don't set it explicitly.
    pub metadata_expire: u64,
    /// `enabled=` from the `.repo` file (defaults to `1`/true). Disabled
    /// repos are still returned by [`load_repo_configs`] (unlike before)
    /// so `--enable-repo`/`config-manager --set-enabled` can turn one on
    /// for a single run/permanently without needing to re-parse the file —
    /// callers that just want "what dnf would use by default" should
    /// filter on this themselves.
    pub enabled: bool,
    /// `priority=`: dnf convention, lower number wins. Packages from a
    /// lower-priority-number repo are preferred over an equally-satisfying
    /// candidate from a higher-priority-number repo; only when the
    /// preferred repo has no satisfying candidate at all does resolution
    /// fall back to a worse-priority one. Defaults to 99, dnf's own default
    /// for a repo that doesn't set `priority=`.
    pub priority: u32,
    /// `cost=`: dnf convention, lower number wins — but only ever consulted
    /// as a *tiebreaker* among repos already tied on `priority`, unlike
    /// `priority` itself which overrides version/newness. Real-world use:
    /// Fedora's own `fedora-updates-archive.repo` sets `cost=10000` (vs. the
    /// default 1000) specifically so an archive mirror that duplicates
    /// current repos' content is only ever reached for when nothing else
    /// has the package, rather than being picked arbitrarily whenever it
    /// happens to list the exact same build. Defaults to 1000, dnf's own
    /// default for a repo that doesn't set `cost=`.
    pub cost: u32,
    /// `exclude=`/`excludepkgs=`: per-repo name/glob blocklist, applied on
    /// top of (not instead of) the global `--exclude`/`exclude=` list from
    /// `rum.conf`/the CLI. dnf allows this per-section — e.g. a COPR repo
    /// that ships a `kernel` package you never want pulled from it even
    /// though the global exclude list doesn't mention it.
    pub exclude: Vec<String>,
    /// `includepkgs=`: per-repo allowlist — when non-empty, only names/globs
    /// matching this list may be sourced from this specific repo (other
    /// repos are unaffected). Applied on top of the global
    /// `main_conf.includepkgs` list, same as `exclude` above.
    pub includepkgs: Vec<String>,
    /// `skip_if_unavailable=`: per-repo override of `rum.conf`'s
    /// `[main]` setting of the same name — `None` means "inherit the
    /// global default", matching dnf's own per-section override semantics.
    /// An unreachable repo with this effectively `true` is skipped with a
    /// warning instead of failing the whole operation.
    pub skip_if_unavailable: Option<bool>,
    /// `proxy=`: this repo's own proxy URL, overriding `rum.conf`'s global
    /// `proxy=` for requests to this repo only (dnf's per-section
    /// `proxy=`, `config_repo.cpp`'s `OptionChild<OptionString> proxy`).
    /// `None` means "inherit the global default" — same as leaving it unset
    /// in the `.repo` file, unlike `main`'s own `proxy=_none_` magic value
    /// which explicitly disables proxying; a repo has no equivalent "force
    /// off" spelling here since dnf's own format doesn't define one.
    pub proxy: Option<String>,
    pub proxy_username: Option<String>,
    pub proxy_password: Option<String>,
    /// `username=`/`password=`: HTTP basic/digest credentials for repos
    /// that require authentication to fetch metadata/packages at all
    /// (dnf's per-section `username=`/`password=`).
    pub username: Option<String>,
    pub password: Option<String>,
    /// `sslcacert=`: a private CA bundle to trust for this repo, in
    /// addition to the system trust store — needed for a repo served
    /// behind a CA `LRO_SSLVERIFYPEER`'s default trust store doesn't know.
    pub sslcacert: Option<String>,
    /// `sslclientcert=`/`sslclientkey=`: client certificate/key for repos
    /// requiring mTLS.
    pub sslclientcert: Option<String>,
    pub sslclientkey: Option<String>,
}

/// dnf's default `metadata_expire` when a repo doesn't set one.
pub const DEFAULT_METADATA_EXPIRE: u64 = 172_800;

/// A repo with its parsed package list, ready for the resolver.
pub struct Repo {
    pub id: String,
    pub base_url: String,
    pub packages: Vec<Package>,
    /// Comps groups (`<group>` entries), if this repo shipped a `type="group"`/
    /// `"group_gz"` data entry in `repomd.xml` — empty otherwise (not every
    /// repo has comps data, and its absence isn't an error).
    pub groups: Vec<Group>,
    /// Comps environments (`<environment>` entries) from the same data
    /// entry as `groups` — empty under the same conditions.
    pub environments: Vec<Environment>,
    /// Module-stream metadata (`modulemd` documents), if this repo shipped
    /// a `type="modules"` data entry in `repomd.xml` — empty otherwise (not
    /// every repo is modular).
    pub modules: Vec<Module>,
    /// module name -> default stream, from this repo's `modulemd-defaults`
    /// documents.
    pub module_defaults: HashMap<String, String>,
    /// Security/bugfix/enhancement advisories, if this repo shipped a
    /// `type="updateinfo"` data entry in `repomd.xml` — empty otherwise (not
    /// every repo publishes advisory data, e.g. most third-party repos).
    pub advisories: Vec<Advisory>,
}

/// Pre-transform snapshot of everything [`load_repo_ex`] parses out of a
/// repo's XML/YAML metadata — `packages`' `location` is still the bare
/// repo-relative href (not yet joined with `base_url`) and `repo_id`/
/// `repo_priority`/`repo_cost` aren't stamped, same shape [`parse_primary_xml`]
/// itself returns, so [`ParsedCache::into_repo`] can apply `cfg`'s current
/// values rather than baking in whatever they were the moment this was
/// cached. Serialized to `parsed.json` next to the raw XML/YAML cache files
/// as a faster-to-load snapshot — analogous to libdnf5 keeping a `.solv`/
/// `.solvx` binary alongside the raw metadata it was built from, just with
/// plain JSON instead of a libsolv SAT pool.
#[derive(serde::Serialize, serde::Deserialize)]
struct ParsedCache {
    base_url: String,
    packages: Vec<Package>,
    groups: Vec<Group>,
    environments: Vec<Environment>,
    modules: Vec<Module>,
    module_defaults: HashMap<String, String>,
    #[serde(default)]
    advisories: Vec<Advisory>,
}

impl ParsedCache {
    fn into_repo(mut self, cfg: &RepoConfig) -> Repo {
        for pkg in &mut self.packages {
            pkg.repo_id = cfg.id.clone();
            pkg.repo_priority = cfg.priority;
            pkg.repo_cost = cfg.cost;
            if !pkg.location.is_empty() {
                pkg.location = format!("{}/{}", self.base_url, pkg.location);
            }
        }
        for advisory in &mut self.advisories {
            advisory.repo_id = cfg.id.clone();
        }
        Repo { id: cfg.id.clone(), base_url: self.base_url, packages: self.packages, groups: self.groups, environments: self.environments, modules: self.modules, module_defaults: self.module_defaults, advisories: self.advisories }
    }
}

/// Downloads `repomd.xml`, finds the `primary` data entry, downloads and
/// decompresses it, and parses every `<package>` into a [`Package`] with
/// `location` resolved to a full download URL and `repo_id` stamped.
///
/// If `cfg.base_url` isn't set, first resolves one from `cfg.mirrorlist` or
/// `cfg.metalink` (mirrorlist checked first if both are somehow set, same
/// precedence dnf uses).
///
/// `cache_root` is checked first (a per-repo subdirectory keyed by both the
/// repo id and a hash of its configured URL(s), same scheme dnf5 uses under
/// `/var/cache/libdnf5` — the hash suffix means a repo whose `baseurl`/
/// `mirrorlist`/`metalink` changes gets a fresh cache automatically instead
/// of serving stale data from an unrelated source under the same id). If
/// the cached `repomd.xml` is younger than `cfg.metadata_expire`, no
/// network request happens at all — same behavior as `dnf` when metadata
/// hasn't expired. `refresh` forces a re-fetch regardless of cache age,
/// for `rum makecache`/`--refresh`.
pub async fn load_repo(client: &reqwest::Client, cfg: &RepoConfig, cache_root: &Path, refresh: bool) -> Result<Repo> {
    load_repo_ex(client, cfg, cache_root, refresh, false).await
}

/// Same as [`load_repo`], with `cache_only` (dnf's `-C`/`--cacheonly`)
/// additionally forcing a network-free run: whatever's cached is used
/// regardless of `metadata_expire=` age (stale is still better than a
/// network round-trip the caller explicitly asked to skip), and a
/// completely missing cache is a hard error rather than falling through to
/// a fetch.
/// Fetches and decompresses `repomd.xml`/`primary.xml` from a single
/// candidate `mirror`, with no fallback of its own — [`load_repo_ex`] loops
/// this over every mirror in the repo's mirrorlist/metalink until one
/// succeeds. Returns `(repomd_xml, primary_href, primary_xml, bytes_fetched)`.
async fn fetch_repomd_and_primary(client: &reqwest::Client, mirror: &str) -> Result<(String, String, String, u64)> {
    let repomd_url = format!("{mirror}/repodata/repomd.xml");
    let repomd_resp = get_with_retry(client, &repomd_url)
        .await?
        .error_for_status()
        .with_context(|| format!("{repomd_url} returned an error status"))?
        .bytes()
        .await?;
    let mut fetched = repomd_resp.len() as u64;
    let repomd_xml = String::from_utf8_lossy(&repomd_resp).into_owned();

    let primary_href = find_data_href(&repomd_xml, "primary").with_context(|| format!("no <data type=\"primary\"> entry in {repomd_url}"))?;
    let primary_url = format!("{mirror}/{primary_href}");
    let compressed = get_bytes_with_retry(client, &primary_url).await?;
    fetched += compressed.len() as u64;

    let xml = decompress(&primary_href, &compressed)?;
    Ok((repomd_xml, primary_href, xml, fetched))
}

/// Verifies `repomd.xml`'s detached GPG signature (`repomd.xml.asc`, the
/// same file dnf/createrepo_c produce) against `cfg.gpgkeys`, matching
/// dnf5's `repo_gpgcheck=` check — dnf5 gets this from librepo directly
/// (`LRO_GPGCHECK`/`LRO_GNUPGHOMEDIR`, `repo_downloader.cpp`) since its
/// metadata fetch goes through librepo's own internal downloader; rum's
/// metadata fetch instead goes through this crate's own retry loop
/// ([`fetch_repomd_and_primary`]), so this shells out to `gpg`/`gpgv`
/// directly against a scratch keyring built from `cfg.gpgkeys`, entirely
/// independent of rpm's own keyring (`rum-transaction::import_repo_keys`
/// imports the same keys there too, for *package* signature checks — this
/// is the repomd-metadata-trust check dnf5 calls `repo_gpgcheck`, distinct
/// from and in addition to that).
///
/// Not every repo publishes a `repomd.xml.asc` even with `gpgcheck=1` set —
/// that's simply "this repo doesn't sign its metadata", not a verification
/// failure, so a missing `.asc` is treated as nothing to check rather than
/// an error (same as dnf5's `repo_gpgcheck` defaulting to off in practice
/// for such repos). A signature that *is* published but doesn't verify
/// against any configured key fails closed.
async fn verify_repomd_signature(client: &reqwest::Client, cfg: &RepoConfig, mirror: &str, repomd_xml: &str) -> Result<()> {
    if !cfg.gpgcheck || cfg.gpgkeys.is_empty() {
        return Ok(());
    }
    let sig_url = format!("{mirror}/repodata/repomd.xml.asc");
    let sig_bytes = match get_with_retry(client, &sig_url).await.and_then(|r| r.error_for_status().map_err(Into::into)) {
        Ok(resp) => resp.bytes().await?,
        Err(_) => return Ok(()),
    };

    let mut key_texts = Vec::with_capacity(cfg.gpgkeys.len());
    for key_url in &cfg.gpgkeys {
        let bytes = get_bytes_with_retry(client, key_url).await.with_context(|| format!("fetching gpgkey {key_url}"))?;
        key_texts.push(bytes);
    }

    let repomd_bytes = repomd_xml.as_bytes().to_vec();
    tokio::task::spawn_blocking(move || verify_repomd_signature_blocking(&repomd_bytes, &sig_bytes, &key_texts))
        .await
        .context("gpg verification task panicked")?
}

/// The actual `gpg`/`gpgv` shell-outs behind [`verify_repomd_signature`],
/// isolated in its own scratch homedir (never rpm's or the invoking user's
/// real `~/.gnupg`) so this never reads or writes any trust state beyond
/// `cfg.gpgkeys` for this one check: `gpg --import` each configured key
/// into it, `gpg --export` them back out to a plain keyring file (the
/// format `gpgv` itself understands — it doesn't read a homedir's keybox
/// directly), then `gpgv --keyring` that file against the signature/data
/// pair. The whole homedir is removed afterward regardless of outcome.
fn verify_repomd_signature_blocking(repomd_bytes: &[u8], sig_bytes: &[u8], key_texts: &[Vec<u8>]) -> Result<()> {
    use std::io::Write;

    let tmp = std::env::temp_dir().join(format!("rum-repomd-gpg-{}-{:x}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()));
    std::fs::create_dir_all(&tmp).context("creating temp gpg homedir")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // gpg refuses to use a homedir it considers insecurely permissioned.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)).context("setting temp gpg homedir permissions")?;
    }

    let result = (|| -> Result<()> {
        for (i, key) in key_texts.iter().enumerate() {
            let key_path = tmp.join(format!("key{i}.asc"));
            std::fs::write(&key_path, key)?;
            let status = std::process::Command::new("gpg").arg("--homedir").arg(&tmp).arg("--batch").arg("--quiet").arg("--import").arg(&key_path).status().context("spawning gpg --import")?;
            anyhow::ensure!(status.success(), "gpg --import failed for gpgkey #{i}");
        }

        let export = std::process::Command::new("gpg").arg("--homedir").arg(&tmp).arg("--batch").arg("--export").output().context("spawning gpg --export")?;
        anyhow::ensure!(export.status.success(), "gpg --export failed");
        let keyring_path = tmp.join("keyring.gpg");
        std::fs::File::create(&keyring_path).and_then(|mut f| f.write_all(&export.stdout)).context("writing scratch keyring")?;

        let sig_path = tmp.join("repomd.xml.asc");
        let data_path = tmp.join("repomd.xml");
        std::fs::write(&sig_path, sig_bytes)?;
        std::fs::write(&data_path, repomd_bytes)?;

        let status = std::process::Command::new("gpgv").arg("--keyring").arg(&keyring_path).arg(&sig_path).arg(&data_path).status().context("spawning gpgv")?;
        anyhow::ensure!(status.success(), "repomd.xml signature verification failed (gpgv rejected it)");
        Ok(())
    })();

    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// Applies `cfg`'s proxy/auth/TLS-client settings ([`net_options_for_repo`])
/// to every librepo handle [`load_repo_ex_inner`] creates while fetching
/// this repo's metadata, then delegates — kept as a thin wrapper so the
/// actual loading logic isn't nested inside a closure.
pub async fn load_repo_ex(client: &reqwest::Client, cfg: &RepoConfig, cache_root: &Path, refresh: bool, cache_only: bool) -> Result<Repo> {
    with_net_options(net_options_for_repo(cfg), load_repo_ex_inner(client, cfg, cache_root, refresh, cache_only)).await
}

async fn load_repo_ex_inner(client: &reqwest::Client, cfg: &RepoConfig, cache_root: &Path, refresh: bool, cache_only: bool) -> Result<Repo> {
    let cache_dir = repo_cache_dir(cache_root, cfg);
    let repomd_cache_path = cache_dir.join("repomd.xml");
    let primary_cache_path = cache_dir.join("primary.xml");
    let baseurl_cache_path = cache_dir.join("baseurl.txt");
    let comps_cache_path = cache_dir.join("comps.xml");
    let modules_cache_path = cache_dir.join("modules.yaml");
    let updateinfo_cache_path = cache_dir.join("updateinfo.xml");
    let parsed_cache_path = cache_dir.join("parsed.json");

    if !refresh && (cache_only || cache_is_fresh(&repomd_cache_path, cfg.metadata_expire)) {
        // Fast path: a previously-parsed snapshot (libdnf5's `.solv`/
        // `.solvx` cache serves the same purpose — skip re-parsing
        // primary.xml/comps.xml/modules.yaml on every single invocation
        // when nothing about the repo's content has changed since the last
        // fetch). Falls through to the raw-XML parse below on any read/
        // deserialize failure — this file is an optimization, never the
        // only copy of the data (primary.xml etc. are still written
        // alongside it).
        if let Some(repo) = std::fs::read(&parsed_cache_path).ok().and_then(|bytes| serde_json::from_slice::<ParsedCache>(&bytes).ok()).map(|cached| cached.into_repo(cfg)) {
            tracing::debug!(repo = %cfg.id, cache = %cache_dir.display(), packages = repo.packages.len(), groups = repo.groups.len(), environments = repo.environments.len(), modules = repo.modules.len(), "loaded repo metadata from parsed cache");
            return Ok(repo);
        }

        if let (Ok(xml), Ok(base_url)) = (std::fs::read_to_string(&primary_cache_path), std::fs::read_to_string(&baseurl_cache_path)) {
            let base_url = base_url.trim().to_string();
            let packages = parse_primary_xml(&xml)?;
            let comps = std::fs::read_to_string(&comps_cache_path).map(|xml| comps::parse_comps_xml_with_types(&xml, group_package_types())).unwrap_or_default();
            let module_data = std::fs::read_to_string(&modules_cache_path).map(|yaml| modulemd::parse_modules_yaml(&yaml)).unwrap_or_default();
            let advisories = std::fs::read_to_string(&updateinfo_cache_path).map(|xml| parse_updateinfo_xml(&xml)).unwrap_or_default();

            let cached = ParsedCache { base_url, packages, groups: comps.groups, environments: comps.environments, modules: module_data.modules, module_defaults: module_data.defaults, advisories };
            if let Err(e) = std::fs::write(&parsed_cache_path, serde_json::to_vec(&cached).unwrap_or_default()) {
                tracing::warn!(repo = %cfg.id, cache = %parsed_cache_path.display(), error = %e, "failed to write parsed metadata cache");
            }

            let repo = cached.into_repo(cfg);
            tracing::debug!(repo = %cfg.id, cache = %cache_dir.display(), packages = repo.packages.len(), groups = repo.groups.len(), environments = repo.environments.len(), modules = repo.modules.len(), "loaded repo metadata from cache");
            return Ok(repo);
        }
        // Cached repomd.xml looked fresh but primary.xml/baseurl.txt are
        // missing or unreadable (partial write, manual tampering) — fall
        // through to a real fetch rather than failing outright, unless the
        // caller explicitly forbade network access.
        anyhow::ensure!(!cache_only, "no cached metadata for repo '{}' and --cacheonly forbids fetching it", cfg.id);
    }
    anyhow::ensure!(!cache_only, "no cached metadata for repo '{}' and --cacheonly forbids fetching it", cfg.id);

    // Matches dnf's own default behavior: silent on a cache hit, one
    // dnf5-style bracketed progress line per repo when metadata actually
    // has to come over the network — timed/sized across every metadata
    // file this repo needs (repomd + primary, plus comps/modules below),
    // not just the initial `repomd.xml` probe.
    let fetch_start = std::time::Instant::now();
    let mut fetched_bytes: u64 = 0;

    // A mirror can have current repodata/repomd.xml while lagging on the
    // primary.xml it points at (or vice versa) — mirror sync is per-file,
    // not atomic across a whole tree. Probing repomd.xml with a HEAD
    // (resolve_base_url) isn't enough to catch that, so on any failure
    // fetching *or parsing* this mirror's metadata, fall through to the
    // next candidate mirror instead of failing the whole repo outright.
    let mirrors = resolve_ranked_mirrors(client, cfg, cache_root, refresh).await?;
    let ordered_mirrors: Vec<String> = if mirrors.len() > 1 {
        match resolve_base_url(client, cfg, cache_root).await {
            Ok(preferred) => {
                let mut ordered = vec![preferred.clone()];
                ordered.extend(mirrors.into_iter().filter(|m| *m != preferred));
                ordered
            }
            Err(_) => mirrors,
        }
    } else {
        mirrors
    };

    let mut fetched: Option<(String, String, String, String, u64)> = None;
    let mut last_err: Option<anyhow::Error> = None;
    for mirror in &ordered_mirrors {
        match fetch_repomd_and_primary(client, mirror).await {
            Ok((repomd_xml, primary_href, xml, bytes)) => {
                fetched = Some((mirror.clone(), repomd_xml, primary_href, xml, bytes));
                break;
            }
            Err(e) => {
                tracing::warn!(repo = %cfg.id, mirror = %mirror, error = %e, "mirror metadata fetch failed, trying next mirror");
                last_err = Some(e);
            }
        }
    }
    let (base_url, repomd_xml, _primary_href, xml, primary_bytes) = fetched.ok_or_else(|| match last_err {
        Some(e) => e.context(format!("repo '{}': all {} mirror(s) failed", cfg.id, ordered_mirrors.len())),
        None => anyhow::anyhow!("repo '{}': no mirrors available", cfg.id),
    })?;
    fetched_bytes += primary_bytes;
    verify_repomd_signature(client, cfg, &base_url, &repomd_xml).await.with_context(|| format!("verifying repomd.xml signature for repo '{}'", cfg.id))?;

    // Comps groups are optional and not every repo ships them (`group_gz`
    // is the common compressed form, `group` the rare plain-XML one) —
    // absence is not an error, just an empty group list.
    let group_xml = {
        let href = find_data_href(&repomd_xml, "group_gz").or_else(|| find_data_href(&repomd_xml, "group"));
        match href {
            Some(href) => {
                let url = format!("{base_url}/{href}");
                match get_with_retry(client, &url).await.and_then(|r| r.error_for_status().map_err(Into::into)) {
                    Ok(resp) => match resp.bytes().await {
                        Ok(bytes) => {
                            fetched_bytes += bytes.len() as u64;
                            decompress(&href, &bytes).unwrap_or_default()
                        }
                        Err(_) => String::new(),
                    },
                    Err(e) => {
                        tracing::debug!(repo = %cfg.id, error = %e, "failed to fetch comps group data, continuing without it");
                        String::new()
                    }
                }
            }
            None => String::new(),
        }
    };
    let comps = comps::parse_comps_xml_with_types(&group_xml, group_package_types());

    // Module-stream metadata is optional and rare outside Fedora/RHEL-
    // style repos with modularity content (`type="modules"`, always
    // compressed in practice) — absence is not an error, just no modules.
    let modules_yaml = {
        let href = find_data_href(&repomd_xml, "modules");
        match href {
            Some(href) => {
                let url = format!("{base_url}/{href}");
                match get_with_retry(client, &url).await.and_then(|r| r.error_for_status().map_err(Into::into)) {
                    Ok(resp) => match resp.bytes().await {
                        Ok(bytes) => {
                            fetched_bytes += bytes.len() as u64;
                            decompress(&href, &bytes).unwrap_or_default()
                        }
                        Err(_) => String::new(),
                    },
                    Err(e) => {
                        tracing::debug!(repo = %cfg.id, error = %e, "failed to fetch modules data, continuing without it");
                        String::new()
                    }
                }
            }
            None => String::new(),
        }
    };
    let module_data = modulemd::parse_modules_yaml(&modules_yaml);

    // Advisory (updateinfo) data is optional and, unlike comps/modules,
    // often quite large (Fedora's `updates` repo ships tens of thousands of
    // advisories) — still fetched unconditionally alongside primary.xml
    // since `rum advisory`/`upgrade --advisory=` need it and there's no
    // cheaper way to know in advance whether a repo has it.
    let updateinfo_xml = {
        let href = find_data_href(&repomd_xml, "updateinfo");
        match href {
            Some(href) => {
                let url = format!("{base_url}/{href}");
                match get_with_retry(client, &url).await.and_then(|r| r.error_for_status().map_err(Into::into)) {
                    Ok(resp) => match resp.bytes().await {
                        Ok(bytes) => {
                            fetched_bytes += bytes.len() as u64;
                            decompress(&href, &bytes).unwrap_or_default()
                        }
                        Err(_) => String::new(),
                    },
                    Err(e) => {
                        tracing::debug!(repo = %cfg.id, error = %e, "failed to fetch updateinfo data, continuing without it");
                        String::new()
                    }
                }
            }
            None => String::new(),
        }
    };
    let advisories = parse_updateinfo_xml(&updateinfo_xml);

    if let Err(e) = write_cache(&cache_dir, &repomd_xml, &xml, &base_url, &group_xml, &modules_yaml, &updateinfo_xml) {
        // Caching is an optimization, not a correctness requirement — a
        // read-only cache_dir (e.g. a locked-down container) shouldn't
        // block the install from proceeding.
        tracing::warn!(repo = %cfg.id, cache = %cache_dir.display(), error = %e, "failed to write repo metadata cache");
    }

    // dnf5's own bracketed `label 100% | speed | size | time` progress
    // line, printed once per repo that actually needed a network fetch
    // (a cache hit returns above and never reaches here) — the label is
    // truncated/padded to a fixed column width the same way dnf5 clips a
    // long repo `name=` rather than reflowing the whole table per repo.
    const LABEL_WIDTH: usize = 39;
    let label: String = cfg.name.chars().take(LABEL_WIDTH).collect();
    eprintln!(
        " {label:<LABEL_WIDTH$} 100% | {:>10} | {:>10} | {}",
        rum_core::format_speed(fetched_bytes, fetch_start.elapsed()),
        rum_core::format_size(fetched_bytes),
        rum_core::format_duration(fetch_start.elapsed())
    );

    let packages = parse_primary_xml(&xml)?;
    let cached = ParsedCache { base_url, packages, groups: comps.groups, environments: comps.environments, modules: module_data.modules, module_defaults: module_data.defaults, advisories };
    if let Err(e) = std::fs::write(cache_dir.join("parsed.json"), serde_json::to_vec(&cached).unwrap_or_default()) {
        tracing::warn!(repo = %cfg.id, error = %e, "failed to write parsed metadata cache");
    }

    let repo = cached.into_repo(cfg);
    tracing::info!(repo = %cfg.id, base_url = %repo.base_url, packages = repo.packages.len(), groups = repo.groups.len(), environments = repo.environments.len(), modules = repo.modules.len(), advisories = repo.advisories.len(), "loaded repo metadata");
    Ok(repo)
}

/// Per-repo cache directory: `<cache_root>/<id>-<hash>`, where `hash` is a
/// short digest of everything that identifies *which* metadata this repo
/// id refers to (`base_url`/`mirrorlist`/`metalink`). Matches dnf5's own
/// `/var/cache/libdnf5/<id>-<hash>` layout — the hash suffix means pointing
/// a repo id at a different URL (or a different repo reusing the same id)
/// naturally gets its own cache slot instead of colliding with stale data.
pub fn repo_cache_dir(cache_root: &Path, cfg: &RepoConfig) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cfg.base_url.hash(&mut hasher);
    cfg.mirrorlist.hash(&mut hasher);
    cfg.metalink.hash(&mut hasher);
    cache_root.join(format!("{}-{:016x}", cfg.id, hasher.finish()))
}

/// The exact mirror base URL a repo's cached `Package.location`s were built
/// from (`baseurl.txt`, written by [`load_repo_ex`]/`write_cache`). For a
/// `mirrorlist=`/`metalink=` repo this is whichever single mirror answered
/// at metadata-fetch time — which may be hours old (persists across
/// `metadata_expire`) or simply absent from a *fresh* [`resolve_mirrors`]
/// call, since Fedora's metalink endpoint returns a geo-sorted/randomized
/// subset per request rather than a stable full list. Callers that need to
/// strip a package's `location` down to its bare `href` (to replay it
/// against a different mirror) must use *this* value, not a prefix search
/// over a freshly re-resolved mirror list — that search silently finds
/// nothing when the two calls returned disjoint mirror sets, which is
/// exactly what disabled the same-repo 404 fallback for `gtk-vnc2` on the
/// stock Fedora metalink repo.
pub fn cached_base_url(cache_root: &Path, cfg: &RepoConfig) -> Option<String> {
    std::fs::read_to_string(repo_cache_dir(cache_root, cfg).join("baseurl.txt")).ok().map(|s| s.trim().to_string())
}

/// Is the cached `repomd.xml` at `repomd_cache_path` still within
/// `metadata_expire` seconds of its last write? `u64::MAX` (from `-1`/
/// `never`) means "yes, always" once the file exists at all.
fn cache_is_fresh(repomd_cache_path: &Path, metadata_expire: u64) -> bool {
    let Ok(meta) = std::fs::metadata(repomd_cache_path) else { return false };
    if metadata_expire == u64::MAX {
        return true;
    }
    let Ok(modified) = meta.modified() else { return false };
    SystemTime::now().duration_since(modified).map(|age| age < Duration::from_secs(metadata_expire)).unwrap_or(false)
}

fn write_cache(cache_dir: &Path, repomd_xml: &str, primary_xml: &str, base_url: &str, group_xml: &str, modules_yaml: &str, updateinfo_xml: &str) -> Result<()> {
    std::fs::create_dir_all(cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;
    std::fs::write(cache_dir.join("repomd.xml"), repomd_xml)?;
    std::fs::write(cache_dir.join("primary.xml"), primary_xml)?;
    std::fs::write(cache_dir.join("baseurl.txt"), base_url)?;
    if !group_xml.is_empty() {
        std::fs::write(cache_dir.join("comps.xml"), group_xml)?;
    }
    if !modules_yaml.is_empty() {
        std::fs::write(cache_dir.join("modules.yaml"), modules_yaml)?;
    }
    if !updateinfo_xml.is_empty() {
        std::fs::write(cache_dir.join("updateinfo.xml"), updateinfo_xml)?;
    }
    Ok(())
}

/// Resolves `cfg` down to a single concrete base URL: uses `base_url`
/// directly if configured, otherwise fetches `mirrorlist`/`metalink` and
/// picks the first working mirror. Mirrors are tried in list order, each
/// verified with a `HEAD` against its `repodata/repomd.xml` — mirror lists
/// are frequently stale (dead mirrors, geo-routing hiccups), so silently
/// trusting entry #1 the way a naive implementation would is a common
/// source of flaky installs.
/// Every candidate mirror base URL for `cfg`, unprobed and in the order
/// its `mirrorlist=`/`metalink=` listed them (or a single-element vec for
/// a plain `baseurl=` repo). Used both by [`resolve_base_url`] (which
/// probes these to pick the metadata mirror) and, on a package `.rpm`
/// download 404, by `rum-transaction` to retry the same file against the
/// rest of this list — a mirror can be current on `repomd.xml`/
/// `primary.xml` while still lagging on an individual package's rsync, or
/// vice versa, so package-level retries deliberately don't reuse the
/// metadata-time probe result.
pub async fn resolve_mirrors(client: &reqwest::Client, cfg: &RepoConfig) -> Result<Vec<String>> {
    if let Some(base_url) = &cfg.base_url {
        return Ok(vec![base_url.trim_end_matches('/').to_string()]);
    }
    let (text, is_metalink) = fetch_mirror_source(client, cfg).await?;
    let mirrors = if is_metalink { parse_metalink(&text) } else { parse_mirrorlist(&text) };
    anyhow::ensure!(!mirrors.is_empty(), "repo '{}': mirrorlist/metalink returned no usable mirror URLs", cfg.id);
    Ok(mirrors)
}

async fn fetch_mirror_source(client: &reqwest::Client, cfg: &RepoConfig) -> Result<(String, bool)> {
    if let Some(mirrorlist_url) = &cfg.mirrorlist {
        let text = get_with_retry(client, mirrorlist_url).await?.error_for_status()?.text().await?;
        Ok((text, false))
    } else if let Some(metalink_url) = &cfg.metalink {
        let text = get_with_retry(client, metalink_url).await?.error_for_status()?.text().await?;
        Ok((text, true))
    } else {
        bail!("repo '{}' has no baseurl, mirrorlist, or metalink configured", cfg.id)
    }
}

/// `resolve_mirrors`, but caches the raw `mirrorlist=`/`metalink=` response
/// to `<cache_root>/<repo>-<hash>/{mirrorlist.txt,metalink.xml}` and reuses
/// it while younger than `cfg.metadata_expire` — the same file dnf5 itself
/// caches under `/var/cache/libdnf5/<repo>-<hash>/metalink.xml`. Without
/// this, every call re-fetched the document over the network (Fedora's
/// metalink endpoint alone was hit twice per package download: once here,
/// once more inside `resolve_base_url`'s own `resolve_mirrors` call) and,
/// since that endpoint returns a geo-sorted/randomized subset per request,
/// two calls a few hundred milliseconds apart needn't even agree on which
/// mirrors exist — caching makes the mirror *list* stable for the same
/// window `repomd.xml`/`primary.xml` already stay trusted for, while
/// `resolve_base_url`'s per-mirror `HEAD` probe still happens live every
/// time so a since-gone-dead mirror in that cached list is still avoided.
pub async fn resolve_mirrors_cached(client: &reqwest::Client, cfg: &RepoConfig, cache_root: &Path, refresh: bool) -> Result<Vec<String>> {
    if let Some(base_url) = &cfg.base_url {
        return Ok(vec![base_url.trim_end_matches('/').to_string()]);
    }
    let cache_dir = repo_cache_dir(cache_root, cfg);
    let is_metalink = cfg.mirrorlist.is_none() && cfg.metalink.is_some();
    let cache_path = cache_dir.join(if is_metalink { "metalink.xml" } else { "mirrorlist.txt" });

    if !refresh && cache_is_fresh(&cache_path, cfg.metadata_expire) {
        if let Ok(text) = std::fs::read_to_string(&cache_path) {
            let mirrors = if is_metalink { parse_metalink(&text) } else { parse_mirrorlist(&text) };
            if !mirrors.is_empty() {
                return Ok(mirrors);
            }
        }
    }

    let (text, is_metalink) = fetch_mirror_source(client, cfg).await?;
    let mirrors = if is_metalink { parse_metalink(&text) } else { parse_mirrorlist(&text) };
    anyhow::ensure!(!mirrors.is_empty(), "repo '{}': mirrorlist/metalink returned no usable mirror URLs", cfg.id);

    if let Err(e) = std::fs::create_dir_all(&cache_dir).and_then(|_| std::fs::write(&cache_path, &text)) {
        // Same as the repomd/primary cache: an optimization, not a
        // correctness requirement.
        tracing::warn!(repo = %cfg.id, cache = %cache_path.display(), error = %e, "failed to write mirror list cache");
    }
    Ok(mirrors)
}

/// Probes `cfg`'s mirror list with a `HEAD /repodata/repomd.xml` against
/// each candidate in order, returning the first that responds — the same
/// selection [`load_repo_ex`] uses to pick which mirror to actually load
/// metadata from. Exposed so a package download can re-probe for the
/// *currently* best-responding mirror instead of trusting a `baseurl.txt`
/// that may have been written hours/days ago (under `metadata_expire=`)
/// and could since have gone stale or slow. Uses [`resolve_mirrors_cached`]
/// for the candidate list itself — only the per-mirror liveness probe below
/// is ever live on every call.
pub async fn resolve_base_url(client: &reqwest::Client, cfg: &RepoConfig, cache_root: &Path) -> Result<String> {
    let mirrors = resolve_ranked_mirrors(client, cfg, cache_root, false).await?;
    if mirrors.len() == 1 {
        return Ok(mirrors.into_iter().next().unwrap());
    }

    for mirror in &mirrors {
        let probe_url = format!("{mirror}/repodata/repomd.xml");
        match client.head(&probe_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(repo = %cfg.id, mirror = %mirror, "selected mirror");
                return Ok(mirror.clone());
            }
            Ok(resp) => tracing::debug!(repo = %cfg.id, mirror = %mirror, status = %resp.status(), "mirror probe failed, trying next"),
            Err(e) => tracing::debug!(repo = %cfg.id, mirror = %mirror, error = %e, "mirror probe errored, trying next"),
        }
    }
    bail!("repo '{}': none of {} mirror(s) responded to a repomd.xml probe", cfg.id, mirrors.len())
}

/// `resolve_mirrors_cached`, but the returned list is ranked fastest-first
/// by librepo's own `lr_fastestmirror` ([`fastest_mirror_sort`]) — the
/// exact mechanism dnf5 uses (`LRO_FASTESTMIRROR`/`LRO_FASTESTMIRRORCACHE`,
/// `libdnf5/repo/repo_downloader.cpp`), including its caching: results are
/// persisted to a single `fastestmirror.cache` file shared by every repo
/// under `cache_root` (not a per-repo file — mirrors shared across repos,
/// common for e.g. Fedora, get their measured speed reused immediately) and
/// remain valid for librepo's own default max age (30 days), so `refresh`
/// only controls whether the raw mirror list itself
/// ([`resolve_mirrors_cached`]) is re-fetched — the ranking cache is
/// librepo's to manage from here on.
pub async fn resolve_ranked_mirrors(client: &reqwest::Client, cfg: &RepoConfig, cache_root: &Path, refresh: bool) -> Result<Vec<String>> {
    let mirrors = resolve_mirrors_cached(client, cfg, cache_root, refresh).await?;
    if mirrors.len() <= 1 {
        return Ok(mirrors);
    }
    Ok(fastest_mirror_sort(mirrors, cache_root.join("fastestmirror.cache")).await)
}

/// Plain-text yum mirrorlist format: one URL per line, blank lines and
/// `#`-comments ignored. A trailing `/repodata/repomd.xml` (some mirrorlist
/// providers include it, some don't) is stripped so every entry ends up as
/// a bare base URL, same shape as a `baseurl=` value.
fn parse_mirrorlist(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.trim_end_matches('/').trim_end_matches("/repodata/repomd.xml").to_string())
        .collect()
}

/// RFC 5854 metalink XML, the format `mirrors.fedoraproject.org` serves:
/// `<metalink><files><file><resources><url ...>https://mirror/.../repomd.xml</url>...`.
/// Each `<url>` points directly at a mirror's `repomd.xml`, so the base URL
/// is that with the trailing `/repodata/repomd.xml` stripped.
fn parse_metalink(xml: &str) -> Vec<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_url = false;
    let mut urls = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"url" => in_url = true,
            Ok(Event::End(e)) if e.local_name().as_ref() == b"url" => in_url = false,
            Ok(Event::Text(t)) if in_url => {
                if let Ok(text) = t.unescape() {
                    let url = text.trim().trim_end_matches("/repodata/repomd.xml").to_string();
                    if url.starts_with("http://") || url.starts_with("https://") {
                        urls.push(url);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    urls
}

/// Minimal `repomd.xml` scan for a `<data type="wanted_type">` entry's
/// `<location href="…"/>` — `repomd.xml` is small and flat enough that a
/// full streaming parse isn't worth the extra code. Used for `"primary"`
/// and `"group"`/`"group_gz"` (comps).
fn find_data_href(xml: &str, wanted_type: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_wanted = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let local = e.local_name();
                let name = String::from_utf8_lossy(local.as_ref()).to_string();
                if name == "data" {
                    in_wanted = e.attributes().flatten().any(|a| a.key.local_name().as_ref() == b"type" && a.value.as_ref() == wanted_type.as_bytes());
                } else if name == "location" && in_wanted {
                    if let Some(href) = e.attributes().flatten().find(|a| a.key.local_name().as_ref() == b"href") {
                        return Some(String::from_utf8_lossy(&href.value).to_string());
                    }
                }
            }
            Ok(Event::End(e)) => {
                if e.local_name().as_ref() == b"data" {
                    in_wanted = false;
                }
            }
            Ok(Event::Eof) => return None,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

fn decompress(href: &str, data: &[u8]) -> Result<String> {
    use std::io::Read;

    if href.ends_with(".gz") {
        let mut out = String::new();
        flate2::read::GzDecoder::new(data).read_to_string(&mut out).context("gunzip primary.xml.gz")?;
        Ok(out)
    } else if href.ends_with(".xz") {
        let mut out = String::new();
        xz2::read::XzDecoder::new(data).read_to_string(&mut out).context("un-xz primary.xml.xz")?;
        Ok(out)
    } else if href.ends_with(".zst") {
        let mut out = String::new();
        zstd::stream::Decoder::new(data).context("opening zstd stream")?.read_to_string(&mut out).context("un-zstd primary.xml.zst")?;
        Ok(out)
    } else {
        Ok(String::from_utf8(data.to_vec())?)
    }
}

/// Loads repo metadata from an already-decompressed local `primary.xml`
/// file on disk — used by tests and by `rum` subcommands that operate on a
/// pre-fetched cache.
pub fn load_primary_file(path: &Path, repo_id: &str) -> Result<Vec<Package>> {
    let xml = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut packages = parse_primary_xml(&xml)?;
    for pkg in &mut packages {
        pkg.repo_id = repo_id.to_string();
    }
    Ok(packages)
}

pub use rum_core::glob_match;

/// The `$releasever`/`$basearch` (and a handful of other common dnf repo
/// variables) values to substitute into `.repo` file URLs. dnf reads
/// releasever from `/etc/os-release`'s `VERSION_ID` and basearch from
/// `uname -m` (with the same i386-family normalization dnf does); rum does
/// the same so existing Fedora/RakuOS repo files resolve identically.
pub struct RepoVars {
    pub releasever: String,
    pub basearch: String,
    /// User-defined `$myvar`-style substitution vars, read from one file
    /// per var (filename = var name, file contents = value, first line
    /// only, trailing newline stripped) under dnf's own `varsdir=` search
    /// path: `/etc/dnf/vars/`, `/etc/yum/vars/`, and then — taking priority
    /// over both, matching dnf5's own "explicit `varsdir=` wins" precedence
    /// — `rum.conf`'s `varsdir=` if set.
    pub custom: std::collections::HashMap<String, String>,
}

impl RepoVars {
    /// Detects releasever from `/etc/os-release` and basearch from
    /// `uname -m`. Falls back to `"44"`/`"x86_64"` (RakuOS's current
    /// baseline) if either can't be read — better to resolve against
    /// *something* sane than fail every repo load outright. No custom vars
    /// — use [`RepoVars::detect_with_varsdir`] to also pick those up.
    pub fn detect() -> Self {
        Self::detect_with_varsdir(None)
    }

    /// Same as [`RepoVars::detect`], but also reads `$myvar`-style custom
    /// vars from the standard `/etc/dnf/vars/`+`/etc/yum/vars/` search path
    /// plus `extra_varsdir` (`rum.conf`'s `varsdir=`, if set — takes
    /// priority over the two standard dirs on a name collision).
    pub fn detect_with_varsdir(extra_varsdir: Option<&Path>) -> Self {
        let releasever = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|contents| {
                contents.lines().find_map(|l| l.strip_prefix("VERSION_ID=")).map(|v| v.trim_matches('"').to_string())
            })
            .unwrap_or_else(|| "44".to_string());

        let basearch = std::process::Command::new("uname")
            .arg("-m")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .map(|arch| match arch.as_str() {
                "i386" | "i486" | "i586" => "i686".to_string(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "x86_64".to_string());

        let mut custom = HashMap::new();
        for dir in [Path::new("/etc/dnf/vars"), Path::new("/etc/yum/vars")].into_iter().chain(extra_varsdir) {
            read_varsdir(dir, &mut custom);
        }

        Self { releasever, basearch, custom }
    }

    fn expand(&self, s: &str) -> String {
        let mut out = s.replace("$releasever", &self.releasever).replace("$basearch", &self.basearch);
        for (name, value) in &self.custom {
            out = out.replace(&format!("${name}"), value);
        }
        out
    }
}

/// Reads every regular file directly under `dir` into `vars` (filename =
/// var name, first line of contents = value) — missing/unreadable `dir` is
/// silently skipped, same as dnf5 treats an absent varsdir as "no custom
/// vars from here", not an error.
fn read_varsdir(dir: &Path, vars: &mut HashMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else { continue };
        if !file_type.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
        if let Ok(contents) = std::fs::read_to_string(entry.path()) {
            vars.insert(name, contents.lines().next().unwrap_or("").to_string());
        }
    }
}

/// Reads every enabled repo out of standard dnf/yum `.repo` INI files in
/// `dir` (normally `/etc/yum.repos.d`) — rum deliberately reuses this
/// format rather than inventing its own, so existing repo configs (RakuOS's
/// own, Fedora's, Terra's, RPM Fusion's, …) keep working unmodified.
/// `mirrorlist=`/`metalink=`-only repos are kept (unlike a plain `baseurl=`
/// repo, they don't get a concrete `base_url` here — [`load_repo`] resolves
/// one at fetch time, since that requires a network round-trip).
pub fn load_repo_configs(dir: &Path, vars: &RepoVars) -> Result<Vec<RepoConfig>> {
    let mut repos = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(repos),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };

    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("repo") {
            continue;
        }
        repos.extend(parse_repo_file(&std::fs::read_to_string(&path)?, vars));
    }
    Ok(repos)
}

/// Minimal `config-manager --set-enabled`/`--add-repo` equivalent: edits
/// `.repo` INI files directly rather than going through [`load_repo_configs`]
/// and rewriting everything, so untouched sections/comments/formatting in
/// the file survive byte-for-byte.
pub mod config_manager {
    use super::RepoVars;
    use anyhow::{Context, Result};
    use std::path::Path;

    /// Flips `enabled=` for `id`'s section to `0`/`1` in whichever `.repo`
    /// file under `dir` actually defines it, adding the line if the section
    /// had no explicit `enabled=` before (dnf's default is enabled, so
    /// enabling an already-implicitly-enabled repo is a no-op write).
    pub fn set_enabled(dir: &Path, id: &str, enabled: bool) -> Result<()> {
        let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("repo") {
                continue;
            }
            let contents = std::fs::read_to_string(&path)?;
            if let Some(updated) = set_enabled_in_text(&contents, id, enabled) {
                std::fs::write(&path, updated).with_context(|| format!("writing {}", path.display()))?;
                return Ok(());
            }
        }
        anyhow::bail!("no repo section named '[{id}]' found under {}", dir.display());
    }

    fn set_enabled_in_text(contents: &str, id: &str, enabled: bool) -> Option<String> {
        let header = format!("[{id}]");
        let lines: Vec<&str> = contents.lines().collect();
        let start = lines.iter().position(|l| l.trim() == header)?;
        let end = lines[start + 1..].iter().position(|l| l.trim_start().starts_with('[')).map(|i| start + 1 + i).unwrap_or(lines.len());

        let mut out: Vec<String> = lines[..start + 1].iter().map(|s| s.to_string()).collect();
        let mut wrote = false;
        for line in &lines[start + 1..end] {
            if line.trim_start().starts_with("enabled") && line.contains('=') {
                out.push(format!("enabled={}", if enabled { 1 } else { 0 }));
                wrote = true;
            } else {
                out.push(line.to_string());
            }
        }
        if !wrote {
            out.push(format!("enabled={}", if enabled { 1 } else { 0 }));
        }
        out.extend(lines[end..].iter().map(|s| s.to_string()));
        Some(out.join("\n") + "\n")
    }

    /// Writes a brand-new `<id>.repo` file under `dir` with a single
    /// `baseurl=` section — the common case (`dnf config-manager --add-repo
    /// <url>` for a plain repo, not a `.repo`-file URL dnf would instead
    /// download verbatim).
    pub fn add_repo(dir: &Path, id: &str, baseurl: &str, vars: &RepoVars) -> Result<std::path::PathBuf> {
        let _ = vars; // kept for symmetry with load_repo_configs's $releasever/$basearch expansion; baseurl is stored unexpanded, same as a hand-written .repo file.
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{id}.repo"));
        let contents = format!("[{id}]\nname={id}\nbaseurl={baseurl}\nenabled=1\ngpgcheck=1\n");
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }
}

#[derive(Default)]
struct RawSection {
    name: Option<String>,
    base_url: Option<String>,
    mirrorlist: Option<String>,
    metalink: Option<String>,
    enabled: bool,
    gpgcheck: bool,
    gpgkeys: Vec<String>,
    metadata_expire: Option<String>,
    priority: Option<String>,
    cost: Option<String>,
    exclude: Vec<String>,
    includepkgs: Vec<String>,
    skip_if_unavailable: Option<bool>,
    proxy: Option<String>,
    proxy_username: Option<String>,
    proxy_password: Option<String>,
    username: Option<String>,
    password: Option<String>,
    sslcacert: Option<String>,
    sslclientcert: Option<String>,
    sslclientkey: Option<String>,
}

fn parse_repo_file(contents: &str, vars: &RepoVars) -> Vec<RepoConfig> {
    let mut repos = Vec::new();
    let mut id: Option<String> = None;
    let mut section = RawSection { enabled: true, gpgcheck: true, ..Default::default() };

    let flush = |id: &Option<String>, section: &RawSection, out: &mut Vec<RepoConfig>| {
        let Some(id) = id else { return };
        if section.base_url.is_none() && section.mirrorlist.is_none() && section.metalink.is_none() {
            return;
        }
        out.push(RepoConfig {
            id: id.clone(),
            name: section.name.clone().map(|n| vars.expand(&n)).unwrap_or_else(|| id.clone()),
            base_url: section.base_url.as_ref().map(|u| vars.expand(u).trim_end_matches('/').to_string()),
            mirrorlist: section.mirrorlist.as_ref().map(|u| vars.expand(u)),
            metalink: section.metalink.as_ref().map(|u| vars.expand(u)),
            gpgcheck: section.gpgcheck,
            gpgkeys: section.gpgkeys.iter().map(|k| vars.expand(k)).collect(),
            metadata_expire: section.metadata_expire.as_deref().map(parse_metadata_expire).unwrap_or(DEFAULT_METADATA_EXPIRE),
            enabled: section.enabled,
            priority: section.priority.as_deref().and_then(|v| strip_inline_comment(v).parse().ok()).unwrap_or(99),
            cost: section.cost.as_deref().and_then(|v| strip_inline_comment(v).parse().ok()).unwrap_or(1000),
            exclude: section.exclude.iter().map(|p| vars.expand(p)).collect(),
            includepkgs: section.includepkgs.iter().map(|p| vars.expand(p)).collect(),
            skip_if_unavailable: section.skip_if_unavailable,
            proxy: section.proxy.as_ref().map(|v| vars.expand(v)),
            proxy_username: section.proxy_username.as_ref().map(|v| vars.expand(v)),
            proxy_password: section.proxy_password.as_ref().map(|v| vars.expand(v)),
            username: section.username.as_ref().map(|v| vars.expand(v)),
            password: section.password.as_ref().map(|v| vars.expand(v)),
            sslcacert: section.sslcacert.as_ref().map(|v| vars.expand(v)),
            sslclientcert: section.sslclientcert.as_ref().map(|v| vars.expand(v)),
            sslclientkey: section.sslclientkey.as_ref().map(|v| vars.expand(v)),
        });
    };

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(new_id) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            flush(&id, &section, &mut repos);
            id = Some(new_id.to_string());
            section = RawSection { enabled: true, gpgcheck: true, ..Default::default() };
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            let (key, val) = (key.trim(), val.trim());
            match key {
                "name" => section.name = Some(val.to_string()),
                "baseurl" => section.base_url = Some(val.to_string()),
                "mirrorlist" => section.mirrorlist = Some(val.to_string()),
                "metalink" => section.metalink = Some(val.to_string()),
                "enabled" => section.enabled = val != "0",
                "gpgcheck" => section.gpgcheck = val != "0",
                "gpgkey" => section.gpgkeys = val.split_whitespace().map(str::to_string).collect(),
                "metadata_expire" => section.metadata_expire = Some(val.to_string()),
                "priority" => section.priority = Some(val.to_string()),
                "cost" => section.cost = Some(val.to_string()),
                "exclude" | "excludepkgs" => section.exclude.extend(val.split_whitespace().map(str::to_string)),
                "includepkgs" => section.includepkgs.extend(val.split_whitespace().map(str::to_string)),
                "skip_if_unavailable" => section.skip_if_unavailable = Some(val == "1" || val.eq_ignore_ascii_case("true")),
                "proxy" => section.proxy = if val.is_empty() || val == "_none_" { None } else { Some(val.to_string()) },
                "proxy_username" => section.proxy_username = Some(val.to_string()),
                "proxy_password" => section.proxy_password = Some(val.to_string()),
                "username" => section.username = Some(val.to_string()),
                "password" => section.password = Some(val.to_string()),
                "sslcacert" => section.sslcacert = Some(val.to_string()),
                "sslclientcert" => section.sslclientcert = Some(val.to_string()),
                "sslclientkey" => section.sslclientkey = Some(val.to_string()),
                _ => {}
            }
        }
    }
    flush(&id, &section, &mut repos);
    repos
}

/// Strips a trailing `# comment` from an INI value — dnf's own `.repo`
/// parser (Python's `configparser`) doesn't support inline comments either,
/// but some hand-edited/vendored `.repo` files carry them anyway (e.g.
/// `cost=10000 # default is 1000`); tolerating them here costs nothing and
/// avoids a numeric field silently falling back to its default.
fn strip_inline_comment(val: &str) -> &str {
    val.split('#').next().unwrap_or(val).trim()
}

/// Parses a `metadata_expire=` value the way dnf does: `-1` or `never`
/// means "don't expire on its own" (`u64::MAX` here), a bare integer is
/// seconds, and a trailing `s`/`m`/`h`/`d` suffix scales it (`6h` = 21600).
/// Anything unparseable falls back to [`DEFAULT_METADATA_EXPIRE`] rather
/// than failing the whole repo file over one malformed value.
fn parse_metadata_expire(raw: &str) -> u64 {
    let raw = raw.trim();
    if raw == "-1" || raw.eq_ignore_ascii_case("never") {
        return u64::MAX;
    }
    let (num, mult) = match raw.chars().last() {
        Some('s') => (&raw[..raw.len() - 1], 1),
        Some('m') => (&raw[..raw.len() - 1], 60),
        Some('h') => (&raw[..raw.len() - 1], 3600),
        Some('d') => (&raw[..raw.len() - 1], 86400),
        _ => (raw, 1),
    };
    num.trim().parse::<u64>().map(|n| n.saturating_mul(mult)).unwrap_or(DEFAULT_METADATA_EXPIRE)
}

/// Parses dnf's `minrate=`/`bandwidth=`-style byte-count values: a plain
/// number of bytes, or one with a `k`/`M`/`G` suffix (case-insensitive,
/// 1024-based — dnf5's own `str_to_bytes` convention). Falls back to 1000
/// (dnf's own `minrate=` default) on anything unparseable.
fn parse_bytes(raw: &str) -> u64 {
    let raw = raw.trim();
    let (num, mult) = match raw.chars().last() {
        Some('k') | Some('K') => (&raw[..raw.len() - 1], 1024u64),
        Some('m') | Some('M') => (&raw[..raw.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&raw[..raw.len() - 1], 1024 * 1024 * 1024),
        _ => (raw, 1),
    };
    num.trim().parse::<f64>().map(|n| (n * mult as f64) as u64).unwrap_or(1000)
}

/// Parses `/etc/rum/rum.conf`'s `[main]` section — same INI format and (as
/// far as rum implements it) the same option names as dnf's `/etc/dnf/
/// dnf.conf`, so an admin who knows dnf.conf already knows rum.conf. Only
/// the options rum actually has a use for are recognized; anything else
/// (dnf options rum has no equivalent behavior for) is silently ignored
/// rather than erroring, same as dnf itself does with unknown plugin-only
/// keys.
pub mod main_config {
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};

    pub const DEFAULT_CONF_PATH: &str = "/etc/rum/rum.conf";

    #[derive(Debug, Clone)]
    pub struct MainConfig {
        pub cachedir: Option<PathBuf>,
        pub reposdir: Option<PathBuf>,
        /// `keepcache=`: keep downloaded `.rpm`s in the cache after a
        /// successful transaction instead of deleting them. dnf defaults
        /// this to `false`.
        pub keepcache: bool,
        pub assumeyes: bool,
        /// `gpgcheck=`: dnf's default is `true`; unlike dnf, rum already
        /// refuses to treat a *repo's* missing `gpgcheck=` as "off" (see
        /// [`crate::RepoConfig::gpgcheck`]), so this only supplies the
        /// process-wide `--no-gpgchecks` default.
        pub gpgcheck: bool,
        /// `best=`: dnf's "require the best available version to resolve
        /// cleanly, don't silently fall back to an older one" toggle.
        /// dnf5's default is `false` (backtrack to an older satisfiable
        /// candidate rather than fail); wired into `rum-solv`'s
        /// `SOLVER_FORCEBEST` job flag on install/upgrade/distro-sync jobs
        /// (`SolveRequest::best`), matching dnf5's own
        /// `GoalPrivate::add_install`/`add_upgrade`/`add_distro_sync`.
        pub best: bool,
        pub exclude: Vec<String>,
        pub includepkgs: Vec<String>,
        /// `metadata_expire=`: fallback used by repos with no `metadata_expire=`
        /// of their own, in place of [`crate::DEFAULT_METADATA_EXPIRE`].
        pub metadata_expire: Option<u64>,
        pub retries: u32,
        pub timeout: u64,
        /// `minrate=`: minimum sustained throughput (bytes/sec, `k`/`M`/`G`
        /// suffixes accepted, 1024-based) below which a transfer is
        /// considered stalled and aborted/retried — matches dnf's own
        /// `minrate=`/`timeout=` pair, which together set libcurl's
        /// `CURLOPT_LOW_SPEED_LIMIT`/`CURLOPT_LOW_SPEED_TIME` (see
        /// `PackageDownloader`/`librepo.cpp` in dnf5). dnf's default is
        /// 1000 B/s. Without this, a connection crawling at a few bytes/sec
        /// (observed live: a mirror stuck at "12 B/s eta 23w") never trips
        /// a naive "zero progress" stall check and hangs indefinitely
        /// instead of failing over to another attempt/mirror.
        pub minrate: u64,
        pub max_parallel_downloads: u32,
        pub skip_if_unavailable: bool,
        pub clean_requirements_on_remove: bool,
        pub installroot: Option<PathBuf>,
        /// `protected_packages=`: names (or globs) that `rum remove`/
        /// `autoremove`/`swap` refuse to erase, dnf's protected-packages
        /// plugin behavior. `rum` itself is always implicitly protected on
        /// top of this list, regardless of config — see
        /// [`crate::ALWAYS_PROTECTED`].
        pub protected_packages: Vec<String>,
        /// `installonlypkgs=`: names (or globs) allowed to have multiple
        /// versions installed side by side instead of the resolver treating
        /// a same-name/different-EVR candidate as replacing the installed
        /// one — the kernel package family is the canonical example.
        /// Defaults to dnf's own built-in list.
        pub installonlypkgs: Vec<String>,
        /// `installonly_limit=`: how many versions of an `installonlypkgs=`
        /// package to keep installed at once; older ones beyond this count
        /// are pruned after a successful install (dnf default: 3).
        pub installonly_limit: u32,
        /// `install_weak_deps=`: whether `Recommends` are installed
        /// best-effort alongside a hard `Requires` closure. dnf's default is
        /// `true`; `Suggests` are never auto-installed regardless of this
        /// setting (matching dnf, which has no config to change that).
        pub install_weak_deps: bool,
        /// `skip_broken=`: if the full requested `install` set can't be
        /// resolved together, drop whichever individual requested names are
        /// responsible and proceed with the rest, instead of failing the
        /// whole transaction over one bad/renamed/unavailable name. dnf's
        /// default is `false`; also settable per-run via `--skip-broken`.
        pub skip_broken: bool,
        /// `multilib_policy=`: `"best"` (dnf5's default) lets a bare `install
        /// foo` job span every arch build of `foo` and leaves the solver's
        /// own policy to pick one; `"all"` instead installs every distinct
        /// arch build side by side. Any other value is rejected at parse
        /// time, matching dnf5's own `RuntimeError` for an unrecognized
        /// policy (`libdnf5/base/goal.cpp`).
        pub multilib_policy: String,
        /// `obsoletes=`: when an `install` name matches nothing literal or
        /// `Provides`-d, widen the search to packages that `Obsoletes` it,
        /// so e.g. `rum install <renamed-away-name>` still resolves to the
        /// replacement instead of failing. dnf's default is `true`.
        pub obsoletes: bool,
        /// `proxy=`: global default proxy URL (e.g.
        /// `http://proxy.example.com:3128`) used for every repo that doesn't
        /// set its own repo-level `proxy=` (rum has no repo-level proxy yet,
        /// so this is the only proxy knob for now). `"_none_"` (dnf's own
        /// magic value) explicitly disables proxying, same as leaving it
        /// unset.
        pub proxy: Option<String>,
        pub proxy_username: Option<String>,
        pub proxy_password: Option<String>,
        /// `fastestmirror=`: dnf5's default is `true` — race mirrors by
        /// measured download throughput and prefer the fastest (see
        /// [`crate::rank_mirrors`]). `false` uses mirrors in whatever order
        /// the mirrorlist/metalink document listed them, skipping the probe
        /// round-trip entirely.
        pub fastestmirror: bool,
        /// `varsdir=`: extra directory to read `$myvar`-style custom repo
        /// vars from, on top of the standard `/etc/dnf/vars/`+
        /// `/etc/yum/vars/` search path — see [`crate::RepoVars`].
        pub varsdir: Option<PathBuf>,
        /// `disable_excludes=`: a comma/space-separated list of scopes
        /// (`"main"` and/or `"*"`, dnf also accepts individual repo ids but
        /// rum's `exclude=`/`--exclude` is main-config/CLI-scoped only, so
        /// there's no per-repo list to selectively disable) that turn off
        /// applying `exclude=`/`--exclude` filtering. Empty (dnf's default)
        /// applies excludes normally.
        pub disable_excludes: Vec<String>,
        /// `protect_running_kernel=`: dnf's default is `true` — refuse to
        /// `remove`/`autoremove` the `kernel`-family package matching the
        /// currently-running `uname -r`, on top of whatever
        /// `protected_packages=` already lists explicitly.
        pub protect_running_kernel: bool,
        /// `sslverify=`: dnf's default is `true` — verify TLS certificates
        /// on HTTPS repo/download connections. `false` matches dnf's escape
        /// hatch for a repo behind a broken/self-signed cert chain; only
        /// meant for trusted internal mirrors, never for a public repo.
        pub sslverify: bool,
        /// `ip_resolve=`: `"ipv4"`/`"ipv6"`/`"whatever"` (dnf's default) —
        /// restricts DNS resolution to one address family, matching dnf5's
        /// `CURLOPT_IPRESOLVE`. Useful on networks with a broken/black-holed
        /// IPv6 route where "happy eyeballs" AAAA-first delays make
        /// downloads slow to start even though IPv4 works fine.
        pub ip_resolve: String,
        /// `user_agent=`: overrides the `User-Agent` header rum sends on
        /// every repo/download HTTP request (default: `rum/<version>`).
        pub user_agent: Option<String>,
        /// `history_record=`: dnf's default is `true` — whether
        /// install/remove/upgrade transactions get appended to rum's own
        /// history log at all (`rum history`/`rum history undo` both read
        /// from it). `false` matches dnf's use case of a throwaway/CI root
        /// where transaction history isn't worth persisting.
        pub history_record: bool,
        /// `username=`/`password=`: global default HTTP basic-auth
        /// credentials used for every repo request that doesn't set its own
        /// (rum has no repo-level username/password yet, so this is the
        /// only auth knob for now), matching dnf's fallback pair.
        pub username: Option<String>,
        pub password: Option<String>,
        /// `group_package_types=`: comma/space-separated `packagereq`
        /// `type=` values (`mandatory`/`default`/`optional`/`conditional`)
        /// that count as "in" a comps group for `@group`/`group install`
        /// purposes. dnf's default is `mandatory,default`.
        pub group_package_types: Vec<String>,
        /// `diskspacecheck=`: dnf's default is `true` — before applying a
        /// transaction, verify the target filesystem has enough free space
        /// for the packages being installed, aborting instead of failing
        /// mid-transaction with a half-written rpmdb.
        pub diskspacecheck: bool,
        /// `assumeno=`: config-file equivalent of `--assumeno` — answer
        /// every confirmation prompt "no" instead of "yes", aborting the
        /// transaction. dnf's default is `false`.
        pub assumeno: bool,
        /// `defaultyes=`: if enabled, the default answer to user
        /// confirmation prompts is "yes" instead of "no" — i.e. hitting
        /// Enter on an empty line accepts. Not to be confused with
        /// `assumeyes`, which skips the prompt entirely. dnf's default is
        /// `false`.
        pub defaultyes: bool,
    }

    /// dnf's own default `installonlypkgs=` list (`/etc/dnf/dnf.conf`'s
    /// built-in default, not something any `.conf` file has to spell out).
    pub const DEFAULT_INSTALLONLYPKGS: &[&str] = &[
        "kernel",
        "kernel-core",
        "kernel-devel",
        "kernel-modules",
        "kernel-modules-core",
        "kernel-modules-extra",
        "kernel-debug",
        "kernel-debug-core",
        "kernel-debug-devel",
        "kernel-debug-modules",
        "kernel-debug-modules-core",
        "kernel-debug-modules-extra",
        "kernel-debug-uname-r",
        "kernel-uname-r",
        "kernel-PAE",
        "kernel-rt",
        "kernel-rt-core",
        "kernel-rt-devel",
        "kernel-rt-modules",
        "kernel-rt-modules-extra",
        "installonlypkg(kernel)",
        "installonlypkg(kernel-module)",
        "installonlypkg(vm)",
    ];

    /// Packages `rum` refuses to remove no matter what `protected_packages=`
    /// says — removing your own package manager out from under yourself
    /// isn't a config-file toggle.
    pub const ALWAYS_PROTECTED: &[&str] = &["rum"];

    impl Default for MainConfig {
        fn default() -> Self {
            MainConfig {
                cachedir: None,
                reposdir: None,
                keepcache: false,
                assumeyes: false,
                gpgcheck: true,
                best: true,
                exclude: Vec::new(),
                includepkgs: Vec::new(),
                metadata_expire: None,
                retries: 10,
                timeout: 30,
                minrate: 1000,
                max_parallel_downloads: 3,
                skip_if_unavailable: false,
                clean_requirements_on_remove: true,
                installroot: None,
                protected_packages: Vec::new(),
                installonlypkgs: DEFAULT_INSTALLONLYPKGS.iter().map(|s| s.to_string()).collect(),
                installonly_limit: 3,
                install_weak_deps: true,
                skip_broken: false,
                multilib_policy: "best".to_string(),
                obsoletes: true,
                proxy: None,
                proxy_username: None,
                proxy_password: None,
                fastestmirror: true,
                varsdir: None,
                disable_excludes: Vec::new(),
                protect_running_kernel: true,
                sslverify: true,
                ip_resolve: "whatever".to_string(),
                user_agent: None,
                history_record: true,
                username: None,
                password: None,
                group_package_types: vec!["mandatory".to_string(), "default".to_string()],
                diskspacecheck: true,
                assumeno: false,
                defaultyes: false,
            }
        }
    }

    /// Loads `path`, falling back to [`MainConfig::default`] if it doesn't
    /// exist — rum works with no `/etc/rum/rum.conf` at all, same as dnf
    /// works with defaults if `/etc/dnf/dnf.conf` is missing its `[main]`
    /// section.
    pub fn load(path: &Path) -> Result<MainConfig> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(parse(&s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MainConfig::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn parse(contents: &str) -> MainConfig {
        let mut cfg = MainConfig::default();
        let mut in_main = false;
        let mut installonlypkgs_overridden = false;
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                in_main = section == "main";
                continue;
            }
            if !in_main {
                continue;
            }
            let Some((key, val)) = line.split_once('=') else { continue };
            apply_kv(&mut cfg, key.trim(), val.trim(), &mut installonlypkgs_overridden);
        }
        cfg
    }

    fn apply_kv(cfg: &mut MainConfig, key: &str, val: &str, installonlypkgs_overridden: &mut bool) {
        match key {
            "cachedir" => cfg.cachedir = Some(PathBuf::from(val)),
            "reposdir" => cfg.reposdir = Some(PathBuf::from(val.split(':').next().unwrap_or(val))),
            "keepcache" => cfg.keepcache = val == "1" || val.eq_ignore_ascii_case("true"),
            "assumeyes" => cfg.assumeyes = val == "1" || val.eq_ignore_ascii_case("true"),
            "gpgcheck" => cfg.gpgcheck = val != "0" && !val.eq_ignore_ascii_case("false"),
            "best" => cfg.best = val == "1" || val.eq_ignore_ascii_case("true"),
            "exclude" | "excludepkgs" => cfg.exclude.extend(val.split_whitespace().map(str::to_string)),
            "includepkgs" => cfg.includepkgs.extend(val.split_whitespace().map(str::to_string)),
            "metadata_expire" => cfg.metadata_expire = Some(super::parse_metadata_expire(val)),
            "retries" => {
                if let Ok(n) = val.parse() {
                    cfg.retries = n;
                }
            }
            "timeout" => {
                if let Ok(n) = val.parse() {
                    cfg.timeout = n;
                }
            }
            "minrate" => cfg.minrate = super::parse_bytes(val),
            "max_parallel_downloads" => {
                if let Ok(n) = val.parse() {
                    cfg.max_parallel_downloads = n;
                }
            }
            "skip_if_unavailable" => cfg.skip_if_unavailable = val == "1" || val.eq_ignore_ascii_case("true"),
            "clean_requirements_on_remove" => cfg.clean_requirements_on_remove = val == "1" || val.eq_ignore_ascii_case("true"),
            "installroot" => cfg.installroot = Some(PathBuf::from(val)),
            "protected_packages" => cfg.protected_packages.extend(val.split_whitespace().map(str::to_string)),
            "installonlypkgs" => {
                // Explicit `installonlypkgs=` replaces dnf's built-in
                // default list entirely (matching dnf's own semantics)
                // rather than appending to it — only the first
                // occurrence clears the default.
                if !*installonlypkgs_overridden {
                    cfg.installonlypkgs.clear();
                    *installonlypkgs_overridden = true;
                }
                cfg.installonlypkgs.extend(val.split_whitespace().map(str::to_string));
            }
            "installonly_limit" => {
                if let Ok(n) = val.parse() {
                    cfg.installonly_limit = n;
                }
            }
            "install_weak_deps" => cfg.install_weak_deps = val == "1" || val.eq_ignore_ascii_case("true"),
            "skip_broken" => cfg.skip_broken = val == "1" || val.eq_ignore_ascii_case("true"),
            "multilib_policy" if val == "all" || val == "best" => cfg.multilib_policy = val.to_string(),
            "obsoletes" => cfg.obsoletes = val == "1" || val.eq_ignore_ascii_case("true"),
            "proxy" => cfg.proxy = if val.is_empty() || val == "_none_" { None } else { Some(val.to_string()) },
            "proxy_username" => cfg.proxy_username = if val.is_empty() { None } else { Some(val.to_string()) },
            "proxy_password" => cfg.proxy_password = if val.is_empty() { None } else { Some(val.to_string()) },
            "fastestmirror" => cfg.fastestmirror = val == "1" || val.eq_ignore_ascii_case("true"),
            "varsdir" => cfg.varsdir = if val.is_empty() { None } else { Some(PathBuf::from(val)) },
            "disable_excludes" => cfg.disable_excludes.extend(val.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).map(str::to_string)),
            "protect_running_kernel" => cfg.protect_running_kernel = val == "1" || val.eq_ignore_ascii_case("true"),
            "sslverify" => cfg.sslverify = val == "1" || val.eq_ignore_ascii_case("true"),
            "ip_resolve" if val == "ipv4" || val == "ipv6" || val == "whatever" => cfg.ip_resolve = val.to_string(),
            "user_agent" => cfg.user_agent = if val.is_empty() { None } else { Some(val.to_string()) },
            "history_record" => cfg.history_record = val == "1" || val.eq_ignore_ascii_case("true"),
            "username" => cfg.username = if val.is_empty() { None } else { Some(val.to_string()) },
            "password" => cfg.password = if val.is_empty() { None } else { Some(val.to_string()) },
            "group_package_types" => cfg.group_package_types = val.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).map(str::to_string).collect(),
            "diskspacecheck" => cfg.diskspacecheck = val == "1" || val.eq_ignore_ascii_case("true"),
            "assumeno" => cfg.assumeno = val == "1" || val.eq_ignore_ascii_case("true"),
            "defaultyes" => cfg.defaultyes = val == "1" || val.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }

    /// Applies `--setopt key=value` overrides on top of an already-loaded
    /// `MainConfig`, same key set as `[main]` in the config file (dnf5's
    /// `--setopt` is a generic ad hoc override of any config key, not a
    /// fixed allowlist of CLI flags). Unknown keys — including repo-scoped
    /// `<repoid>.key=value` and the `tsflags=` pseudo-key rum special-cases
    /// separately in rum-cli — are silently ignored here, matching `parse`'s
    /// own unknown-key handling.
    pub fn apply_setopt(cfg: &mut MainConfig, setopt: &[String]) {
        let mut installonlypkgs_overridden = false;
        for kv in setopt {
            let Some((key, val)) = kv.split_once('=') else { continue };
            apply_kv(cfg, key.trim(), val.trim(), &mut installonlypkgs_overridden);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_main_section() {
            let cfg = parse("[main]\ncachedir=/var/cache/rum\nkeepcache=1\nassumeyes=True\ngpgcheck=0\nexclude=kernel* firefox\nretries=5\ntimeout=15\nbest=1\n\n[some-repo]\ncachedir=/should/not/apply\n");
            assert_eq!(cfg.cachedir, Some(PathBuf::from("/var/cache/rum")));
            assert!(cfg.keepcache);
            assert!(cfg.assumeyes);
            assert!(!cfg.gpgcheck);
            assert_eq!(cfg.exclude, vec!["kernel*", "firefox"]);
            assert_eq!(cfg.retries, 5);
            assert_eq!(cfg.timeout, 15);
            assert_eq!(cfg.minrate, 1000);
            assert!(cfg.best);
        }

        #[test]
        fn parses_new_main_options() {
            let cfg = parse("[main]\ndisable_excludes=main,foo\nprotect_running_kernel=0\nsslverify=0\nip_resolve=ipv4\nuser_agent=custom-agent\nhistory_record=0\n");
            assert_eq!(cfg.disable_excludes, vec!["main", "foo"]);
            assert!(!cfg.protect_running_kernel);
            assert!(!cfg.sslverify);
            assert_eq!(cfg.ip_resolve, "ipv4");
            assert_eq!(cfg.user_agent, Some("custom-agent".to_string()));
            assert!(!cfg.history_record);
        }

        #[test]
        fn ip_resolve_rejects_unknown_value() {
            let cfg = parse("[main]\nip_resolve=bogus\n");
            assert_eq!(cfg.ip_resolve, "whatever");
        }

        #[test]
        fn minrate_kv_overrides_default() {
            let cfg = parse("[main]\nminrate=5k\n");
            assert_eq!(cfg.minrate, 5 * 1024);
        }

        #[test]
        fn missing_file_returns_default() {
            let cfg = load(Path::new("/nonexistent/rum.conf.does.not.exist")).unwrap();
            assert!(!cfg.keepcache);
            assert!(cfg.gpgcheck);
        }

        #[test]
        fn default_has_dnf_installonlypkgs_and_limit() {
            let cfg = MainConfig::default();
            assert!(cfg.installonlypkgs.iter().any(|p| p == "kernel"));
            assert_eq!(cfg.installonly_limit, 3);
            assert!(cfg.install_weak_deps);
            assert!(cfg.protected_packages.is_empty());
        }

        #[test]
        fn parses_protected_and_installonly_settings() {
            let cfg = parse("[main]\nprotected_packages=glibc systemd\ninstallonlypkgs=kernel kernel-core mypkg\ninstallonly_limit=5\ninstall_weak_deps=0\n");
            assert_eq!(cfg.protected_packages, vec!["glibc", "systemd"]);
            assert_eq!(cfg.installonlypkgs, vec!["kernel", "kernel-core", "mypkg"]);
            assert_eq!(cfg.installonly_limit, 5);
            assert!(!cfg.install_weak_deps);
        }

        #[test]
        fn setopt_overrides_loaded_config() {
            let mut cfg = parse("[main]\ninstall_weak_deps=1\nretries=10\n");
            apply_setopt(&mut cfg, &["install_weak_deps=false".to_string(), "retries=2".to_string(), "unknownkey=whatever".to_string()]);
            assert!(!cfg.install_weak_deps);
            assert_eq!(cfg.retries, 2);
        }

        #[test]
        fn setopt_installonlypkgs_replaces_not_appends() {
            let mut cfg = parse("[main]\ninstallonlypkgs=kernel\n");
            apply_setopt(&mut cfg, &["installonlypkgs=foo bar".to_string()]);
            assert_eq!(cfg.installonlypkgs, vec!["foo", "bar"]);
        }

        #[test]
        fn parses_proxy_settings() {
            let cfg = parse("[main]\nproxy=http://proxy.example.com:3128\nproxy_username=alice\nproxy_password=hunter2\n");
            assert_eq!(cfg.proxy.as_deref(), Some("http://proxy.example.com:3128"));
            assert_eq!(cfg.proxy_username.as_deref(), Some("alice"));
            assert_eq!(cfg.proxy_password.as_deref(), Some("hunter2"));
        }

        #[test]
        fn proxy_none_magic_value_disables_proxy() {
            let cfg = parse("[main]\nproxy=_none_\n");
            assert_eq!(cfg.proxy, None);
        }

        #[test]
        fn parses_varsdir() {
            let cfg = parse("[main]\nvarsdir=/etc/rum/vars\n");
            assert_eq!(cfg.varsdir.as_deref(), Some(Path::new("/etc/rum/vars")));
            assert_eq!(MainConfig::default().varsdir, None);
        }

        #[test]
        fn fastestmirror_defaults_true_and_parses() {
            assert!(MainConfig::default().fastestmirror);
            let cfg = parse("[main]\nfastestmirror=false\n");
            assert!(!cfg.fastestmirror);
            let cfg = parse("[main]\nfastestmirror=1\n");
            assert!(cfg.fastestmirror);
        }

        #[test]
        fn parses_username_password_diskspacecheck_assumeno_group_package_types() {
            let cfg = MainConfig::default();
            assert_eq!(cfg.group_package_types, vec!["mandatory", "default"]);
            assert!(cfg.diskspacecheck);
            assert!(!cfg.assumeno);
            assert_eq!(cfg.username, None);

            let cfg = parse("[main]\nusername=alice\npassword=hunter2\ngroup_package_types=mandatory,default,optional\ndiskspacecheck=0\nassumeno=1\n");
            assert_eq!(cfg.username.as_deref(), Some("alice"));
            assert_eq!(cfg.password.as_deref(), Some("hunter2"));
            assert_eq!(cfg.group_package_types, vec!["mandatory", "default", "optional"]);
            assert!(!cfg.diskspacecheck);
            assert!(cfg.assumeno);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> RepoVars {
        RepoVars { releasever: "44".to_string(), basearch: "x86_64".to_string(), custom: HashMap::new() }
    }

    #[test]
    fn parses_repo_ini() {
        let repos = parse_repo_file(
            "[rakuos]\nname=RakuOS\nbaseurl=https://repo.rakuos.org/rakuos/$releasever/\nenabled=1\ngpgcheck=1\ngpgkey=https://repo.rakuos.org/pubkey.gpg\n\n[disabled-repo]\nbaseurl=https://example.com/x\nenabled=0\n",
            &vars(),
        );
        // Disabled repos are still returned (so `--enable-repo`/
        // `config-manager --set-enabled` can act on them) — filtering by
        // `enabled` is the caller's job, not the parser's.
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].id, "rakuos");
        assert_eq!(repos[0].base_url.as_deref(), Some("https://repo.rakuos.org/rakuos/44"));
        assert!(repos[0].gpgcheck);
        assert!(repos[0].enabled);
        assert_eq!(repos[0].gpgkeys, vec!["https://repo.rakuos.org/pubkey.gpg"]);
        assert_eq!(repos[1].id, "disabled-repo");
        assert!(!repos[1].enabled);
        assert_eq!(repos[0].metadata_expire, DEFAULT_METADATA_EXPIRE);
    }

    #[test]
    fn parses_metadata_expire_suffixes() {
        assert_eq!(parse_metadata_expire("6h"), 21_600);
        assert_eq!(parse_metadata_expire("7d"), 604_800);
        assert_eq!(parse_metadata_expire("120"), 120);
        assert_eq!(parse_metadata_expire("120s"), 120);
        assert_eq!(parse_metadata_expire("30m"), 1800);
        assert_eq!(parse_metadata_expire("-1"), u64::MAX);
        assert_eq!(parse_metadata_expire("never"), u64::MAX);
        assert_eq!(parse_metadata_expire("garbage"), DEFAULT_METADATA_EXPIRE);
    }

    #[test]
    fn parses_minrate_suffixes() {
        assert_eq!(parse_bytes("1000"), 1000);
        assert_eq!(parse_bytes("10k"), 10 * 1024);
        assert_eq!(parse_bytes("10K"), 10 * 1024);
        assert_eq!(parse_bytes("1M"), 1024 * 1024);
        assert_eq!(parse_bytes("1G"), 1024 * 1024 * 1024);
        assert_eq!(parse_bytes("garbage"), 1000);
    }

    #[test]
    fn repo_ini_metadata_expire_overrides_default() {
        let repos = parse_repo_file("[fedora]\nbaseurl=https://example.com/os\nenabled=1\nmetadata_expire=6h\n", &vars());
        assert_eq!(repos[0].metadata_expire, 21_600);
    }

    #[test]
    fn repo_ini_priority_parses_and_defaults() {
        let repos = parse_repo_file(
            "[fedora]\nbaseurl=https://example.com/os\nenabled=1\npriority=10\n\n[rpmfusion]\nbaseurl=https://example.com/rf\nenabled=1\n",
            &vars(),
        );
        assert_eq!(repos[0].priority, 10);
        assert_eq!(repos[1].priority, 99);
    }

    #[test]
    fn cache_is_fresh_respects_metadata_expire() {
        let dir = std::env::temp_dir().join(format!("rum-repo-cache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let repomd = dir.join("repomd.xml");
        std::fs::write(&repomd, "x").unwrap();

        assert!(cache_is_fresh(&repomd, 3600));
        assert!(!cache_is_fresh(&repomd, 0));
        assert!(cache_is_fresh(&repomd, u64::MAX));
        assert!(!cache_is_fresh(&dir.join("missing.xml"), 3600));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn expands_basearch() {
        let repos = parse_repo_file("[fedora]\nbaseurl=https://example.com/$basearch/os\nenabled=1\ngpgcheck=0\n", &vars());
        assert_eq!(repos[0].base_url.as_deref(), Some("https://example.com/x86_64/os"));
        assert!(!repos[0].gpgcheck);
    }

    #[test]
    fn expands_custom_vars() {
        let mut v = vars();
        v.custom.insert("myrepo".to_string(), "internal".to_string());
        let repos = parse_repo_file("[custom]\nbaseurl=https://example.com/$myrepo/$basearch\nenabled=1\ngpgcheck=0\n", &v);
        assert_eq!(repos[0].base_url.as_deref(), Some("https://example.com/internal/x86_64"));
    }

    #[test]
    fn reads_custom_vars_from_varsdir() {
        let dir = std::env::temp_dir().join(format!("rum-varsdir-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("myrepo"), "internal-mirror\n").unwrap();
        let vars = RepoVars::detect_with_varsdir(Some(&dir));
        assert_eq!(vars.custom.get("myrepo").map(String::as_str), Some("internal-mirror"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keeps_mirrorlist_only_repo_without_a_base_url() {
        let repos = parse_repo_file("[nobase]\nmirrorlist=https://example.com/$releasever/mirrorlist\nenabled=1\n", &vars());
        assert_eq!(repos.len(), 1);
        assert!(repos[0].base_url.is_none());
        assert_eq!(repos[0].mirrorlist.as_deref(), Some("https://example.com/44/mirrorlist"));
    }

    #[test]
    fn keeps_metalink_only_repo_without_a_base_url() {
        let repos = parse_repo_file("[nobase]\nmetalink=https://example.com/metalink\nenabled=1\n", &vars());
        assert_eq!(repos.len(), 1);
        assert!(repos[0].base_url.is_none());
        assert_eq!(repos[0].metalink.as_deref(), Some("https://example.com/metalink"));
    }

    #[test]
    fn parses_mirrorlist_text() {
        let text = "# comment\nhttp://mirror1.example.com/repo/\n\nhttp://mirror2.example.com/repo/repodata/repomd.xml\n";
        let mirrors = parse_mirrorlist(text);
        assert_eq!(mirrors, vec!["http://mirror1.example.com/repo", "http://mirror2.example.com/repo"]);
    }

    #[test]
    fn parses_metalink_xml() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<metalink version="3.0" xmlns="http://www.metalinker.org/">
  <files>
    <file name="repomd.xml">
      <resources maxconnections="1">
        <url protocol="https" type="https" location="US" preference="100">https://mirror1.example.com/repo/repodata/repomd.xml</url>
        <url protocol="http" type="http" location="US" preference="99">http://mirror2.example.com/repo/repodata/repomd.xml</url>
      </resources>
    </file>
  </files>
</metalink>"#;
        let mirrors = parse_metalink(xml);
        assert_eq!(mirrors, vec!["https://mirror1.example.com/repo", "http://mirror2.example.com/repo"]);
    }
}
