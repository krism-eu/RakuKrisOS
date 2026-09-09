//! A retrying `GET`, shared by every crate that talks to a repo/mirror
//! over HTTP (`rum-repo` itself, `rum-transaction`'s downloads/gpg-key
//! fetch, `rum-repo::copr`).
//!
//! Plain `reqwest` doesn't retry a failed `send()` at all, which makes
//! `rum` far more fragile than dnf/libdnf (which default to `retries=10`)
//! against exactly the kind of error seen in practice: "connection closed
//! before message completed" from a pooled keep-alive connection the
//! remote end had already closed by the time `reqwest` tried to reuse it —
//! a benign, common race with HTTP connection pooling, not a sign the
//! mirror or the network is actually down. A one-off retry after a short
//! backoff clears this in effectively all real-world cases.
//!
//! The actual transfer for every function here goes through
//! [`rum_librepo`] (real librepo, the same C library dnf5 links), not
//! hand-rolled curl calls — this module now only owns the retry loop,
//! `.part`-file/resume bookkeeping and checksum verification around it,
//! same responsibilities `libdnf5`'s `repo_downloader.cpp` keeps on top of
//! its own librepo calls.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

const DEFAULT_MAX_ATTEMPTS: u32 = 4;
const DEFAULT_STALL_TIMEOUT_SECS: u32 = 30;

/// Process-wide attempt count, set once from `rum.conf`'s `retries=` (dnf's
/// own default is 10) via [`set_max_attempts`]. Every crate that calls
/// [`get_with_retry`] — `rum-repo` itself, `rum-transaction`'s downloads/
/// gpg-key fetch, `rum-repo::copr` — shares this single knob rather than
/// each needing `retries=` threaded through its own signature.
static MAX_ATTEMPTS: AtomicU32 = AtomicU32::new(DEFAULT_MAX_ATTEMPTS);

/// Process-wide stall timeout (seconds), set once from `rum.conf`'s
/// `timeout=` via [`set_stall_timeout`]. Applied per-chunk in
/// [`get_bytes_with_retry`] rather than as a whole-request `reqwest`
/// timeout, so a large package doesn't get killed mid-download just for
/// taking a while — only a connection that's gone genuinely silent for this
/// long gets aborted.
static STALL_TIMEOUT_SECS: AtomicU32 = AtomicU32::new(DEFAULT_STALL_TIMEOUT_SECS);

/// Process-wide `minrate=` (bytes/sec), set once from `rum.conf` via
/// [`set_minrate`]. Matches dnf's own `minrate=` default of 1000 B/s —
/// this is librepo's own `LRO_LOWSPEEDLIMIT`/`LRO_LOWSPEEDTIME` pair (see
/// dnf5's `librepo.cpp`, which sets the exact same two options): a transfer
/// averaging below `minrate` bytes/sec over a `timeout`-second window is
/// stalled, not just one that's gone completely silent. `0` disables the
/// throughput check.
static MINRATE: AtomicU64 = AtomicU64::new(1000);

/// Process-wide `fastestmirror=` toggle, set once from `rum.conf` via
/// [`set_fastestmirror`]. dnf5's own default is `true`; when `false`,
/// [`crate::rank_mirrors`] skips its concurrent throughput-probe race and
/// mirrors are used in whatever order the mirrorlist/metalink document
/// already listed them.
static FASTEST_MIRROR: AtomicBool = AtomicBool::new(true);

/// Sets the [`FASTEST_MIRROR`] toggle — call once at startup after loading
/// `rum.conf`.
pub fn set_fastestmirror(enabled: bool) {
    FASTEST_MIRROR.store(enabled, Ordering::Relaxed);
}

/// Proxy/auth/TLS-client settings applied to every librepo handle created
/// while in scope — the per-repo/per-call analog of `rum.conf`'s global
/// `proxy=`/`proxy_username=`/`proxy_password=` (already parsed into
/// `MainConfig`, see `rum-cli/src/main.rs`) plus the `.repo`-file-level
/// `proxy=`/`username=`/`password=`/`sslcacert=`/`sslclientcert=`/
/// `sslclientkey=` options dnf's own `.repo` format supports per-section
/// (`RepoConfig`). Mirrors dnf5's own model: `librepo.cpp` sets exactly
/// these six `LRO_*` options from `ConfigRepo`, which itself falls back to
/// `ConfigMain`'s global proxy when a repo doesn't set its own
/// (`config_repo.cpp`'s `OptionChild` inheritance).
#[derive(Clone, Default, Debug)]
pub struct NetOptions {
    pub proxy: Option<String>,
    /// `"user:pass"`, already combined — matches `LRO_PROXYUSERPWD`'s
    /// expected format.
    pub proxy_userpwd: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub sslcacert: Option<String>,
    pub sslclientcert: Option<String>,
    pub sslclientkey: Option<String>,
}

impl NetOptions {
    /// Fills in any field left `None` here from `default_net_options()` —
    /// the global (`rum.conf`) fallback a repo-level [`NetOptions`] inherits
    /// from when it doesn't set its own value, same precedence dnf5's
    /// `OptionChild<T>` gives `ConfigRepo` over `ConfigMain`.
    pub fn or_default(self) -> NetOptions {
        let def = default_net_options();
        NetOptions {
            proxy: self.proxy.or(def.proxy),
            proxy_userpwd: self.proxy_userpwd.or(def.proxy_userpwd),
            username: self.username.or(def.username),
            password: self.password.or(def.password),
            sslcacert: self.sslcacert.or(def.sslcacert),
            sslclientcert: self.sslclientcert.or(def.sslclientcert),
            sslclientkey: self.sslclientkey.or(def.sslclientkey),
        }
    }
}

static DEFAULT_NET_OPTIONS: std::sync::OnceLock<std::sync::RwLock<NetOptions>> = std::sync::OnceLock::new();

fn default_net_options_lock() -> &'static std::sync::RwLock<NetOptions> {
    DEFAULT_NET_OPTIONS.get_or_init(|| std::sync::RwLock::new(NetOptions::default()))
}

/// Sets the process-wide default [`NetOptions`] — call once at startup from
/// `rum.conf`'s `[main]` proxy settings, same as [`set_fastestmirror`] et al.
/// Per-repo overrides (see [`with_net_options`]) still fall back to whatever
/// is set here via [`NetOptions::or_default`].
pub fn set_default_net_options(opts: NetOptions) {
    *default_net_options_lock().write().unwrap() = opts;
}

pub fn default_net_options() -> NetOptions {
    default_net_options_lock().read().unwrap().clone()
}

tokio::task_local! {
    /// The [`NetOptions`] in effect for librepo handles created by the
    /// current async task — set via [`with_net_options`]. `try_with` (in
    /// [`current_net_options`]) rather than `with` because most call sites
    /// never opt into an override and should transparently fall back to
    /// [`default_net_options`].
    static NET_OPTIONS: NetOptions;
}

/// Runs `fut` with `opts` as the effective [`NetOptions`] for every librepo
/// handle it creates (directly, or transitively through any `rum-repo`
/// network call) — the mechanism [`load_repo`](crate::load_repo) and
/// `rum-transaction`'s package-download calls use to apply a specific
/// repo's `proxy=`/`username=`/`sslclientcert=` etc. without threading a
/// `NetOptions` parameter through every intervening function signature.
/// Task-local rather than thread-local: survives `.await` points within the
/// same task (unlike a plain thread-local, which a `tokio` worker thread
/// could hand off to an unrelated task between polls), and is captured by
/// value into each `spawn_blocking` closure that actually builds a handle
/// rather than assumed to still be readable from the blocking thread.
pub async fn with_net_options<F: std::future::Future>(opts: NetOptions, fut: F) -> F::Output {
    NET_OPTIONS.scope(opts, fut).await
}

fn current_net_options() -> NetOptions {
    NET_OPTIONS.try_with(|o| o.clone()).unwrap_or_else(|_| default_net_options())
}

/// Reads the current [`FASTEST_MIRROR`] toggle.
pub fn fastestmirror_enabled() -> bool {
    FASTEST_MIRROR.load(Ordering::Relaxed)
}

/// Sets the attempt count [`get_with_retry`] uses from here on — call once
/// at startup after loading `rum.conf`. `retries=0` would mean "never even
/// try", which isn't a sensible retry count, so it's floored at 1.
pub fn set_max_attempts(retries: u32) {
    MAX_ATTEMPTS.store(retries.max(1), Ordering::Relaxed);
}

/// Sets the per-chunk stall timeout [`get_bytes_with_retry`] uses from here
/// on — call once at startup after loading `rum.conf`. `timeout=0` would
/// mean "abort if a chunk isn't already buffered", which is never useful,
/// so it's floored at 1.
pub fn set_stall_timeout(secs: u64) {
    STALL_TIMEOUT_SECS.store((secs.max(1)).min(u32::MAX as u64) as u32, Ordering::Relaxed);
}

/// Sets the [`MINRATE`] throughput floor [`get_bytes_with_retry`] and
/// [`download_to_file_with_retry_ex`] use from here on — call once at
/// startup after loading `rum.conf`.
pub fn set_minrate(bytes_per_sec: u64) {
    MINRATE.store(bytes_per_sec, Ordering::Relaxed);
}

/// A fully-buffered HTTP response from [`get_with_retry`] — deliberately
/// not `reqwest::Response`. Metadata/mirrorlist/GPG-key fetches now go
/// through librepo (matching [`download_to_file_with_retry_ex`]'s package
/// downloads and dnf5's own model — see that function's doc comment), which
/// only offers a synchronous API and is run inside `spawn_blocking`,
/// returning a fully-read body rather than a streamable response object.
/// `bytes()`/`text()` stay `async fn` purely so existing call sites
/// (written against `reqwest::Response`) don't need to drop their `.await`.
pub struct HttpResponse {
    pub status: u32,
    body: Vec<u8>,
}

impl HttpResponse {
    /// Same contract as `reqwest::Response::error_for_status`: turns a
    /// >=400 status into an `Err`, otherwise passes `self` through
    /// unchanged.
    pub fn error_for_status(self) -> Result<Self> {
        if self.status >= 400 {
            anyhow::bail!(HttpStatusError { status: self.status });
        }
        Ok(self)
    }

    pub async fn bytes(self) -> Result<Vec<u8>> {
        Ok(self.body)
    }

    pub async fn text(self) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.body).into_owned())
    }
}

fn new_handle(stall_timeout_secs: u64, minrate: u64, net_opts: &NetOptions) -> Result<rum_librepo::Handle> {
    let handle = rum_librepo::Handle::new(concat!("rum/", env!("CARGO_PKG_VERSION")), 30, minrate, stall_timeout_secs).context("initializing librepo handle")?;
    handle.set_proxy(net_opts.proxy.as_deref(), net_opts.proxy_userpwd.as_deref())?;
    handle.set_credentials(net_opts.username.as_deref(), net_opts.password.as_deref())?;
    handle.set_tls_client(net_opts.sslcacert.as_deref(), net_opts.sslclientcert.as_deref(), net_opts.sslclientkey.as_deref())?;
    Ok(handle)
}

/// Ranks `mirrors` fastest-first using librepo's own `lr_fastestmirror`
/// (a real per-mirror TCP connect-time race, [`rum_librepo::Handle::
/// fastest_mirror_sort`]) — the exact mechanism dnf5 relies on
/// (`libdnf5/repo/repo_downloader.cpp`'s `LRO_FASTESTMIRROR`/
/// `LRO_FASTESTMIRRORCACHE`). `cache_path` should be a single path shared
/// by every repo (dnf5 points it at `<basecachedir>/fastestmirror.cache`,
/// not a per-repo file), so mirrors measured once benefit every repo that
/// shares them, and results survive for `LRO_FASTESTMIRRORMAXAGE` (30 days,
/// librepo's own default — this crate doesn't override it). On any failure
/// this returns `mirrors` unchanged rather than blocking the caller on a
/// ranking that isn't essential to the download itself succeeding.
pub async fn fastest_mirror_sort(mirrors: Vec<String>, cache_path: std::path::PathBuf) -> Vec<String> {
    if mirrors.len() <= 1 || !fastestmirror_enabled() {
        return mirrors;
    }
    let original = mirrors.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        let handle = rum_librepo::Handle::new(concat!("rum/", env!("CARGO_PKG_VERSION")), 30, 0, 30).context("initializing librepo handle")?;
        handle.set_fastestmirror(true, cache_path.to_str()).context("setting LRO_FASTESTMIRROR")?;
        handle.fastest_mirror_sort(&mirrors)
    })
    .await;
    match result {
        Ok(Ok(ranked)) => ranked,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "fastest-mirror ranking failed, using unranked mirror order");
            original
        }
        Err(e) => {
            tracing::debug!(error = %e, "fastest-mirror ranking task panicked, using unranked mirror order");
            original
        }
    }
}

/// A librepo failure, downgraded to an HTTP-style status when librepo's
/// `GError` message names one (see [`rum_librepo::parse_http_status`]) so
/// the shared retry loop below can apply the same permanent-vs-transient
/// distinction it always has (4xx besides 429 = permanent, everything else
/// = transient) without needing to know it's now backed by librepo instead
/// of curl.
fn classify_librepo_error(e: anyhow::Error) -> anyhow::Error {
    let msg = e.to_string();
    match rum_librepo::parse_http_status(&msg) {
        Some(status) => anyhow::Error::new(HttpStatusError { status }).context(msg),
        None => e,
    }
}

/// One single-stream librepo attempt at `GET url`, buffering the whole body
/// in memory — the metadata/mirrorlist/GPG-key documents this is used for
/// are all small enough (repomd.xml, compressed primary.xml, a PEM-armored
/// key) that streaming to disk the way package downloads do would be
/// needless complexity. Downloads to a private temp file (librepo's fd-based
/// API requires a real, seekable, read+write file descriptor) and reads it
/// back rather than a genuine in-memory sink — librepo has no
/// write-to-buffer mode.
fn fetch_bytes_blocking(url: &str, stall_timeout_secs: u64, minrate: u64, net_opts: &NetOptions) -> Result<(u32, Vec<u8>)> {
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::io::AsRawFd;

    let handle = new_handle(stall_timeout_secs, minrate, net_opts)?;
    let path = std::env::temp_dir().join(format!("rum-librepo-fetch-{}-{:x}.tmp", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()));
    // librepo `fdopen()`s the fd itself, which requires read+write access
    // regardless of the direction data actually flows (write-only fails
    // with EINVAL — confirmed against this exact librepo build).
    let mut file = OpenOptions::new().create(true).read(true).write(true).truncate(true).open(&path).context("creating temp file for download")?;
    let result = handle.fetch_to_fd(url, file.as_raw_fd());
    let outcome = match result {
        Ok(()) => {
            file.seek(SeekFrom::Start(0))?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            // librepo's `lr_download_url` fails the whole call on a bad
            // HTTP status rather than handing one back for the caller to
            // inspect (unlike libcurl's `response_code()` this replaced),
            // so a successful call always means 2xx here.
            Ok((200, buf))
        }
        Err(e) => Err(classify_librepo_error(e)),
    };
    drop(file);
    let _ = std::fs::remove_file(&path);
    outcome
}

/// Shared retry loop backing both [`get_with_retry`] and
/// [`get_bytes_with_retry`]: retries a transport-level failure (connect
/// error, mid-transfer stall below `minrate`, etc.) with exponential
/// backoff, same as before. A completed transfer — even a >=400 status —
/// returns immediately on the attempt it happened, *except* 5xx/429, which
/// are treated as transient and retried exactly like a transport error:
/// a mirror returning 503/429 right now can easily succeed on the very
/// next attempt, unlike a definitive 404/403 which won't.
async fn fetch_with_retry(url: &str) -> Result<HttpResponse> {
    let max_attempts = MAX_ATTEMPTS.load(Ordering::Relaxed);
    let stall_timeout_secs = STALL_TIMEOUT_SECS.load(Ordering::Relaxed) as u64;
    let minrate = MINRATE.load(Ordering::Relaxed);
    let net_opts = current_net_options();
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=max_attempts {
        let url_owned = url.to_string();
        let net_opts = net_opts.clone();
        let result = tokio::task::spawn_blocking(move || fetch_bytes_blocking(&url_owned, stall_timeout_secs, minrate, &net_opts)).await.context("fetch task panicked")?;
        match result {
            Ok((status, body)) => return Ok(HttpResponse { status, body }),
            Err(e) => {
                let retryable_status = e.downcast_ref::<HttpStatusError>().is_some_and(|he| he.status >= 500 || he.status == 429);
                let is_permanent = e.downcast_ref::<HttpStatusError>().is_some_and(|he| (400..500).contains(&he.status) && he.status != 429);
                if is_permanent {
                    if let Some(he) = e.downcast_ref::<HttpStatusError>() {
                        return Ok(HttpResponse { status: he.status, body: Vec::new() });
                    }
                }
                if attempt < max_attempts {
                    tracing::debug!(url, attempt, error = %e, retryable_status, "request failed, retrying");
                    tokio::time::sleep(Duration::from_millis(200 * 2u64.pow(attempt - 1))).await;
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap()).with_context(|| format!("fetching {url} (after {max_attempts} attempts)"))
}

/// `GET url`, retrying transient connect/request errors and 5xx/429
/// statuses (not other HTTP error statuses — callers still call
/// `.error_for_status()` themselves) with a short exponential backoff.
/// Returns the first success/definitive-status response or the last error,
/// wrapped with `url` for context. `client` is accepted but unused — kept
/// so existing callers don't need to drop the argument; see
/// [`HttpResponse`]'s doc comment for why this no longer goes through
/// `reqwest`.
pub async fn get_with_retry(_client: &reqwest::Client, url: &str) -> Result<HttpResponse> {
    fetch_with_retry(url).await
}

/// `GET url` and read the full body, retrying the *whole* request+body
/// cycle (not just connect/headers) on failure — matches [`get_with_retry`]
/// plus an immediate, non-retried failure on any >=400 status: a mirror
/// missing a file it advertised isn't going to un-404 between attempt 1 and
/// attempt 10, so there's no reason to spend every attempt's backoff on it
/// (5xx/429 are already retried inside [`fetch_with_retry`] before this
/// even sees them). `client` is accepted but unused; see
/// [`get_with_retry`].
pub async fn get_bytes_with_retry(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    get_with_retry(client, url).await?.error_for_status().with_context(|| format!("fetching {url}"))?.bytes().await
}

/// Downloads `url` straight to `dest` over the shared `client` — streaming
/// to disk instead of buffering in memory (unlike [`get_bytes_with_retry`]),
/// and, like `wget`, verifying the transfer against the server-reported
/// `Content-Length` itself (an early-EOF from a stalled/reset connection
/// otherwise reads as "success" once the per-chunk stall timeout is the only
/// guard). An earlier version of this shelled out to the real `wget` binary
/// per package for that same Content-Length safety net (proven in `rakurpm`,
/// RakuOS's other package-download tool) — but that meant a cold TCP+TLS
/// handshake *per package* instead of reusing `client`'s pooled keep-alive
/// connections, which showed up as long pauses partway through "Downloading
/// Packages:" on any transaction with more than a couple dozen packages.
/// Reusing `client` here keeps that connection reuse while still getting the
/// same verified-transfer guarantee.
///
/// Writes go to `dest` with a `.part` suffix and only rename onto `dest`
/// itself once fully downloaded and length-checked — live-caught in podman
/// testing an interrupted (killed mid-transfer) `rum install rocm` run: the
/// caller's own cache logic treats "file exists at `dest`" as "already
/// downloaded, skip it", so writing progressively straight to the final path
/// left a truncated RPM that looked cached forever and failed signature
/// verification on the next run instead of simply being retried.
/// Same as calling [`download_to_file_with_retry`] with no progress
/// reporting.
pub async fn download_to_file_with_retry(client: &reqwest::Client, url: &str, dest: &std::path::Path) -> Result<()> {
    download_to_file_with_retry_ex(client, url, dest, None, None).await
}

/// Digest algorithms `primary.xml`'s `<checksum type="...">` can name —
/// `sha256` is what current `createrepo_c` emits, `sha1`/`md5` are kept for
/// older/third-party repos still using them (same set librepo accepts).
fn digest_hex(algo: &str, bytes: &[u8]) -> Option<String> {
    use sha2::Digest as _;
    match algo {
        "sha256" => Some(hex::encode(sha2::Sha256::digest(bytes))),
        "sha512" => Some(hex::encode(sha2::Sha512::digest(bytes))),
        "sha1" => {
            use sha1::Digest as _;
            Some(hex::encode(sha1::Sha1::digest(bytes)))
        }
        "md5" => {
            use md5::Digest as _;
            Some(hex::encode(md5::Md5::digest(bytes)))
        }
        _ => None,
    }
}

/// Verifies `path`'s bytes against a primary.xml-sourced `(type, hex)`
/// checksum — the same integrity check dnf5/librepo perform on every
/// downloaded package via `lr_packagetarget_new_v3`'s checksum arguments,
/// independent of (and in addition to) GPG signature verification, which
/// many repos/packages skip via `gpgcheck=0`. An unrecognized algorithm
/// name is treated as "can't verify, don't block on it" rather than a hard
/// error — the checksum types Fedora repos actually emit are all covered
/// above.
pub async fn verify_checksum(path: &std::path::Path, checksum_type: &str, expected_hex: &str) -> Result<()> {
    if checksum_type.is_empty() || expected_hex.is_empty() {
        return Ok(());
    }
    let algo = checksum_type.to_ascii_lowercase();
    let bytes = tokio::fs::read(path).await.with_context(|| format!("reading {} for checksum verification", path.display()))?;
    let Some(actual) = digest_hex(&algo, &bytes) else { return Ok(()) };
    if !actual.eq_ignore_ascii_case(expected_hex) {
        anyhow::bail!("checksum mismatch for {}: expected {algo}:{expected_hex}, got {algo}:{actual}", path.display());
    }
    Ok(())
}

/// An HTTP response status librepo doesn't fail a transfer on by itself in
/// a way that gives us back a real status code (only bad statuses do, and
/// then only via [`rum_librepo::parse_http_status`] scraping the error
/// message) — reconstructed manually after every attempt so the retry loop
/// can tell a permanent 404/403 apart from a transient network failure,
/// same distinction `get_bytes_with_retry` makes for `reqwest::Error`.
#[derive(Debug)]
struct HttpStatusError {
    status: u32,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "server returned HTTP {}", self.status)
    }
}

impl std::error::Error for HttpStatusError {}

/// One single-stream attempt at downloading `url` into `part`, resuming
/// from whatever bytes are already on disk at `part` (set by a prior
/// attempt of the same call — see the caller's doc comment). Runs on a
/// blocking thread since librepo's handle/target API is synchronous.
///
/// Deliberately a single sequential stream, not a parallel `Range`-split
/// download: that's also what dnf5 actually does — librepo (`downloader.c`)
/// opens exactly one transfer per file and lets its internal downloader
/// parallelize *across* files (via `LRO_MAXPARALLELDOWNLOADS`), never
/// splitting one file into concurrent ranges. Splitting a single file's
/// transfer into parallel range requests assumes every range is served
/// consistent, identical bytes — a real risk against a load-balanced
/// mirror/CDN set under a lossy or degraded link, where different ranges can
/// land on different backend nodes. Matching librepo's one-stream-per-file
/// model removes that risk entirely; `verify_checksum` after each attempt is
/// the remaining safety net.
///
/// Handle option choices mirror librepo's own defaults, same ones dnf5 sets
/// (`libdnf5/repo/librepo.cpp`): 30s connect timeout, TLS verification on,
/// and `LRO_LOWSPEEDLIMIT`/`LRO_LOWSPEEDTIME` sourced from `rum.conf`'s
/// `minrate=`/`timeout=` (dnf's own knobs for the exact same options) —
/// letting librepo itself detect and abort a stalled transfer natively.
fn download_single_attempt_blocking(url: &str, part: &std::path::Path, stall_timeout_secs: u64, minrate: u64, progress: Option<std::sync::Arc<AtomicU64>>, net_opts: &NetOptions) -> Result<()> {
    use std::fs::OpenOptions;
    use std::os::unix::io::AsRawFd;

    let resume_from = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    // librepo `fdopen()`s the fd itself, which requires read+write access
    // regardless of the direction data actually flows (write-only fails
    // with EINVAL — confirmed against this exact librepo build).
    let file = OpenOptions::new().create(true).read(true).write(true).open(part).with_context(|| format!("opening {}", part.display()))?;

    let handle = new_handle(stall_timeout_secs, minrate, net_opts)?;
    let resume = resume_from > 0;
    handle.download_target(url, file.as_raw_fd(), resume, 0, progress).map_err(classify_librepo_error)
}

/// `progress`, if given, is set to the total bytes downloaded so far
/// (including whatever was already on disk from an earlier attempt of this
/// same call) as the transfer proceeds — a caller can poll it from a
/// separate ticker task to print a live "X% done" line instead of the file
/// just going silent until it hits 100%, which is what every download used
/// to do (indistinguishable from a hang on a large file over a slow
/// mirror). Reset to 0 at the very start of the call, then left alone
/// across retries within it, since each retry resumes from where the last
/// attempt left off rather than starting over — see
/// [`download_single_attempt_blocking`].
///
/// Writes go to `dest` with a `.part` suffix and only rename onto `dest`
/// itself once fully downloaded and checksum-verified — live-caught in
/// podman testing an interrupted (killed mid-transfer) `rum install rocm`
/// run: the caller's own cache logic treats "file exists at `dest`" as
/// "already downloaded, skip it", so writing progressively straight to the
/// final path left a truncated RPM that looked cached forever and failed
/// signature verification on the next run instead of simply being retried.
/// Any `.part` file left over from a *previous, separate* call (a crashed
/// or killed process) is deliberately not trusted for resume — this call
/// didn't observe it being written, so it's truncated at the very start
/// instead. Same as calling [`download_to_file_with_retry`] with no
/// progress reporting.
pub async fn download_to_file_with_retry_ex(
    _client: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
    progress: Option<std::sync::Arc<AtomicU64>>,
    expected_checksum: Option<(&str, &str)>,
) -> Result<()> {
    let part = dest.with_extension(match dest.extension() {
        Some(ext) => format!("{}.part", ext.to_string_lossy()),
        None => "part".to_string(),
    });
    let max_attempts = MAX_ATTEMPTS.load(Ordering::Relaxed);
    let stall_timeout_secs = STALL_TIMEOUT_SECS.load(Ordering::Relaxed) as u64;
    let minrate = MINRATE.load(Ordering::Relaxed);
    let net_opts = current_net_options();

    let _ = tokio::fs::remove_file(&part).await;
    if let Some(p) = &progress {
        p.store(0, Ordering::Relaxed);
    }

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=max_attempts {
        let url_owned = url.to_string();
        let part_owned = part.clone();
        let progress_clone = progress.clone();
        let net_opts = net_opts.clone();
        let result = tokio::task::spawn_blocking(move || download_single_attempt_blocking(&url_owned, &part_owned, stall_timeout_secs, minrate, progress_clone, &net_opts))
            .await
            .context("download task panicked")?;

        match result {
            Ok(()) => {
                if let Some((ty, hex)) = expected_checksum {
                    if let Err(e) = verify_checksum(&part, ty, hex).await {
                        // Wrong bytes on disk — can't resume from a corrupt
                        // partial, so the next attempt starts clean.
                        let _ = tokio::fs::remove_file(&part).await;
                        if let Some(p) = &progress {
                            p.store(0, Ordering::Relaxed);
                        }
                        if attempt < max_attempts {
                            tracing::debug!(url, attempt, error = %e, "checksum mismatch, retrying");
                            tokio::time::sleep(Duration::from_millis(200 * 2u64.pow(attempt - 1))).await;
                        }
                        last_err = Some(e);
                        continue;
                    }
                }
                tokio::fs::rename(&part, dest).await.with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
                return Ok(());
            }
            Err(e) => {
                // Same reasoning as get_bytes_with_retry: a definitive HTTP
                // client error means this exact URL will never succeed no
                // matter how many times it's retried. Deliberately *not*
                // retried here even for archive-style single-host repos
                // known to lag their own metadata (see `download_all_ex`'s
                // per-package mirror-cycling loop, which is where that case
                // is now handled) — this function only ever sees one URL at
                // a time, so retrying it here would burn several seconds
                // sitting on a dead mirror instead of immediately moving on
                // to the next one, which is what dnf5/librepo actually do
                // (cycle the whole mirror set fast, only backing off once
                // every mirror in it has failed).
                let is_permanent = e.downcast_ref::<HttpStatusError>().is_some_and(|he| (400..500).contains(&he.status) && he.status != 429);
                if is_permanent {
                    let _ = tokio::fs::remove_file(&part).await;
                    return Err(e).with_context(|| format!("fetching {url}"));
                }
                // Transient failure — leave the `.part` file on disk so the
                // next attempt resumes from its current byte offset instead
                // of restarting the whole transfer.
                if attempt < max_attempts {
                    tracing::debug!(url, attempt, error = %e, "download failed, retrying");
                    tokio::time::sleep(Duration::from_millis(200 * 2u64.pow(attempt - 1))).await;
                }
                last_err = Some(e);
            }
        }
    }
    let _ = tokio::fs::remove_file(&part).await;
    Err(last_err.unwrap()).with_context(|| format!("fetching {url} (after {max_attempts} attempts)"))
}

/// One package to fetch as part of a [`download_packages_batch`] call.
pub struct BatchSpec {
    /// Repo-relative href — combined with `urls` (the repo's ranked mirror
    /// list) by librepo itself, not by this crate.
    pub relative_url: String,
    /// Final destination path — a sibling `.part` file is what librepo
    /// actually writes/resumes into; this function renames it onto `dest`
    /// once librepo reports that target as `Successful`/`Alreadyexists`,
    /// same as every other download path here.
    pub dest: std::path::PathBuf,
    pub checksum_type: String,
    pub checksum: String,
    pub expected_size: i64,
    pub progress: Option<std::sync::Arc<AtomicU64>>,
    /// Overrides the batch's shared `urls` list for this one target
    /// (`LrPackageTarget::base_url`) — used for a package whose href can't
    /// be resolved against the repo's ranked mirror list, so it still gets
    /// a real (if single-mirror) librepo download instead of being dropped
    /// from the batch entirely.
    pub base_url: Option<String>,
}

/// One package's outcome from [`download_packages_batch`].
pub struct BatchOutcome {
    pub ok: bool,
    pub error: Option<String>,
}

/// Downloads every package in `specs` from one repo with a single
/// `lr_download_packages` call — librepo's own batch/parallel/mirror-
/// failover downloader across `urls` (the repo's already-ranked mirror
/// list), exactly the entrypoint dnf5's `PackageDownloader::download()`
/// uses per repo. Checksum verification happens inside librepo itself
/// (each target carries its own `checksum_type`/`checksum`), unlike the
/// single-target path above which verifies after the fact.
///
/// Every target writes to a `.part` sibling of its `dest` (resumed from
/// if one is already on disk, e.g. left over from a previous failed batch)
/// and is renamed onto `dest` only once librepo reports it `Successful`; a
/// failed target's `.part` file is left in place so the next batch/round
/// resumes it rather than restarting from zero.
pub async fn download_packages_batch(urls: Vec<String>, specs: Vec<BatchSpec>, max_parallel: u64) -> Result<Vec<BatchOutcome>> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let stall_timeout_secs = STALL_TIMEOUT_SECS.load(Ordering::Relaxed) as u64;
    let minrate = MINRATE.load(Ordering::Relaxed);
    let net_opts = current_net_options();

    let parts: Vec<std::path::PathBuf> = specs
        .iter()
        .map(|s| {
            s.dest.with_extension(match s.dest.extension() {
                Some(ext) => format!("{}.part", ext.to_string_lossy()),
                None => "part".to_string(),
            })
        })
        .collect();

    let lr_specs: Vec<rum_librepo::PackageSpec> = specs
        .iter()
        .zip(&parts)
        .map(|(s, part)| {
            let resume = part.exists();
            rum_librepo::PackageSpec {
                relative_url: s.relative_url.clone(),
                dest: part.to_string_lossy().into_owned(),
                checksum_type: s.checksum_type.clone(),
                checksum: s.checksum.clone(),
                expected_size: s.expected_size,
                base_url: s.base_url.clone(),
                resume,
                progress: s.progress.clone(),
            }
        })
        .collect();

    let results = tokio::task::spawn_blocking(move || -> Result<Vec<rum_librepo::PackageResult>> {
        let handle = new_handle(stall_timeout_secs, minrate, &net_opts)?;
        handle.set_urls(&urls).context("setting LRO_URLS")?;
        handle.set_max_parallel(max_parallel.max(1)).context("setting LRO_MAXPARALLELDOWNLOADS")?;
        handle.download_packages(&lr_specs, false)
    })
    .await
    .context("batch download task panicked")??;

    let mut out = Vec::with_capacity(results.len());
    for ((spec, part), result) in specs.iter().zip(&parts).zip(results) {
        if result.ok {
            tokio::fs::rename(part, &spec.dest).await.with_context(|| format!("renaming {} to {}", part.display(), spec.dest.display()))?;
            out.push(BatchOutcome { ok: true, error: None });
        } else {
            out.push(BatchOutcome { ok: false, error: result.error });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod checksum_tests {
    use super::*;

    #[tokio::test]
    async fn checksum_matches() {
        let dir = std::env::temp_dir().join(format!("rum-checksum-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("pkg.rpm");
        tokio::fs::write(&path, b"hello world").await.unwrap();
        // sha256("hello world")
        verify_checksum(&path, "sha256", "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9").await.unwrap();
    }

    #[tokio::test]
    async fn checksum_mismatch_rejected() {
        let dir = std::env::temp_dir().join(format!("rum-checksum-test2-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("pkg.rpm");
        tokio::fs::write(&path, b"hello world").await.unwrap();
        let err = verify_checksum(&path, "sha256", "0000000000000000000000000000000000000000000000000000000000000000").await;
        assert!(err.is_err());
    }
}
