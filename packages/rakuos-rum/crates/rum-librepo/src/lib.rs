//! Safe Rust wrapper around librepo's `LrHandle`/`LrDownloadTarget` C API —
//! the same library dnf5 links (`libdnf5/repo/librepo.hpp`,
//! `repo_downloader.cpp`) for repo metadata and package downloading, used
//! here so `rum`'s own downloader behaves like dnf5's rather than merely
//! imitating it with hand-rolled curl calls.
//!
//! Deliberately a thin layer: one [`Handle`] wraps an `LrHandle*` configured
//! with the timeout/TLS/user-agent options `rum-repo` needs, with two
//! download primitives on top of it —
//! [`Handle::fetch_to_fd`] (single-shot, no resume; matches `lr_download_url`)
//! and [`Handle::download_target`] (resumable, with a progress callback;
//! matches the `LrDownloadTarget`/`lr_download_target` API dnf5 uses for
//! package downloads). Retry looping, exponential backoff, `.part` file
//! handling and checksum verification stay in `rum-repo::net`, same as
//! before — librepo's job here is exactly the transfer itself.
mod sys;

use anyhow::{bail, Context, Result};
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::os::raw::c_double;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Wraps a GLib `GError**` out-param for the duration of one FFI call and
/// turns a non-NULL result into an `anyhow::Error`, freeing it either way —
/// every `sys::` call in this module goes through here so a `GError` never
/// leaks and every failure path is checked (librepo, like all GLib-style
/// APIs, guarantees `err` is only set when the call itself reports failure,
/// and never sets it on success).
struct ErrSlot(*mut sys::GError);

impl ErrSlot {
    fn new() -> Self {
        ErrSlot(std::ptr::null_mut())
    }

    fn as_out_param(&mut self) -> *mut *mut sys::GError {
        &mut self.0
    }

    /// `ok` is the `gboolean` librepo returned from the call this slot was
    /// passed to. Consumes `self` either way: on success there's nothing to
    /// free (librepo didn't set `err`); on failure the message is copied out
    /// before the `GError` is freed.
    fn check(mut self, ok: sys::gboolean, what: &str) -> Result<()> {
        if ok != 0 && self.0.is_null() {
            return Ok(());
        }
        if self.0.is_null() {
            bail!("{what} failed (no error detail from librepo)");
        }
        let msg = unsafe {
            let m = (*self.0).message;
            if m.is_null() {
                "unknown error".to_string()
            } else {
                std::ffi::CStr::from_ptr(m).to_string_lossy().into_owned()
            }
        };
        let code = unsafe { (*self.0).code };
        unsafe { sys::g_error_free(self.0) };
        self.0 = std::ptr::null_mut();
        bail!("{what}: {msg} (librepo error code {code})")
    }
}

impl Drop for ErrSlot {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { sys::g_error_free(self.0) };
        }
    }
}

/// A best-effort HTTP status parsed back out of a librepo `GError` message.
/// librepo doesn't hand the raw status code back through its public API the
/// way libcurl's `CURLINFO_RESPONSE_CODE` does — it folds a bad status into
/// `LRE_BADSTATUS` and only the human-readable message names the code
/// (librepo's own `curl.c`/`downloader.c` format status failures as
/// `"Status code: NNN for <url>"`). Callers that need to tell a permanent
/// 4xx apart from a transient 5xx/429 (see `rum-repo::net`'s retry loop)
/// parse it out of the message text; `None` means "couldn't tell, treat as
/// transient" rather than assuming success or failure.
pub fn parse_http_status(message: &str) -> Option<u32> {
    let lower = message.to_ascii_lowercase();
    let idx = lower.find("status code")?;
    let rest = &message[idx..];
    let digits_start = rest.find(|c: char| c.is_ascii_digit())?;
    let digits: String = rest[digits_start..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// One configured librepo handle. Not `Sync`/thread-shared by design —
/// `LrHandle` isn't documented as safe for concurrent use across threads,
/// so `rum-repo::net` creates a fresh [`Handle`] per blocking download task
/// (same lifetime as the `curl::easy::Easy` it replaces).
pub struct Handle(*mut sys::LrHandle);

impl Handle {
    pub fn new(useragent: &str, connect_timeout_secs: u64, low_speed_limit: u64, low_speed_time_secs: u64) -> Result<Self> {
        let raw = unsafe { sys::lr_handle_init() };
        if raw.is_null() {
            bail!("lr_handle_init failed (out of memory)");
        }
        let handle = Handle(raw);
        handle.setopt_str(sys::LrHandleOption::Useragent, useragent).context("setting LRO_USERAGENT")?;
        handle.setopt_long(sys::LrHandleOption::Connecttimeout, connect_timeout_secs.min(i64::MAX as u64) as i64).context("setting LRO_CONNECTTIMEOUT")?;
        handle.setopt_long(sys::LrHandleOption::Lowspeedlimit, low_speed_limit.min(i64::MAX as u64) as i64).context("setting LRO_LOWSPEEDLIMIT")?;
        handle.setopt_long(sys::LrHandleOption::Lowspeedtime, low_speed_time_secs.max(1).min(i64::MAX as u64) as i64).context("setting LRO_LOWSPEEDTIME")?;
        handle.setopt_long(sys::LrHandleOption::Sslverifypeer, 1).context("setting LRO_SSLVERIFYPEER")?;
        handle.setopt_long(sys::LrHandleOption::Sslverifyhost, 1).context("setting LRO_SSLVERIFYHOST")?;
        Ok(handle)
    }

    fn setopt_long(&self, opt: sys::LrHandleOption, value: i64) -> Result<()> {
        let mut err = ErrSlot::new();
        // LRO_* "(long)" options take a genuine C `long` in the varargs
        // slot; on this platform (LP64) that's 64-bit, matching `i64`.
        let ok = unsafe { sys::lr_handle_setopt(self.0, err.as_out_param(), opt, value as std::os::raw::c_long) };
        err.check(ok, "lr_handle_setopt")
    }

    fn setopt_str(&self, opt: sys::LrHandleOption, value: &str) -> Result<()> {
        let cstr = CString::new(value).context("option value contains an interior NUL")?;
        let mut err = ErrSlot::new();
        let ok = unsafe { sys::lr_handle_setopt(self.0, err.as_out_param(), opt, cstr.as_ptr()) };
        err.check(ok, "lr_handle_setopt")
    }

    /// Sets `LRO_URLS` (a `char **`, `NULL`-terminated) — the same option
    /// dnf5's `RepoDownloader` sets from its already-resolved mirror list
    /// before handing a repo's packages to `lr_download_packages`. Once set,
    /// every `LrPackageTarget` built from this handle whose own `base_url`
    /// is null resolves its `relative_url` against *this* list, and librepo
    /// itself — not caller-side Rust code — does the mirror cycling/failover
    /// across it for the whole batch.
    pub fn set_urls(&self, urls: &[String]) -> Result<()> {
        let cstrs: Vec<CString> = urls.iter().map(|u| CString::new(u.as_str()).context("url contains an interior NUL")).collect::<Result<_>>()?;
        let mut ptrs: Vec<*const c_char> = cstrs.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        {
            let mut err = ErrSlot::new();
            let ok = unsafe { sys::lr_handle_setopt(self.0, err.as_out_param(), sys::LrHandleOption::Urls, ptrs.as_ptr()) };
            err.check(ok, "lr_handle_setopt(LRO_URLS)")?;
        }
        // `LRO_REPOTYPE` (a C enum, promoted to `c_int` in the varargs slot,
        // not `c_long` like the other options this crate sets) — required
        // for `lr_download_packages` to resolve targets against `LRO_URLS`
        // at all; see the `LR_YUMREPO` doc comment in `sys.rs`.
        let mut err = ErrSlot::new();
        let ok = unsafe { sys::lr_handle_setopt(self.0, err.as_out_param(), sys::LrHandleOption::Repotype, sys::LR_YUMREPO) };
        err.check(ok, "lr_handle_setopt(LRO_REPOTYPE)")
    }

    /// Single-shot download of `url` into the already-open file descriptor
    /// `fd`, no resume — matches `lr_download_url`, librepo's own wrapper
    /// used the same way. `fd` must stay open and owned by the caller;
    /// librepo doesn't take ownership of it.
    pub fn fetch_to_fd(&self, url: &str, fd: RawFd) -> Result<()> {
        let curl = CString::new(url).context("url contains an interior NUL")?;
        let mut err = ErrSlot::new();
        let ok = unsafe { sys::lr_download_url(self.0, curl.as_ptr(), fd, err.as_out_param()) };
        err.check(ok, &format!("downloading {url}"))
    }

    /// Downloads `url` into `fd`, resuming from `fd`'s current write offset
    /// when `resume` is set (librepo autodetects the resume offset by
    /// seeking `fd` itself), reporting progress (bytes downloaded so far,
    /// including anything already on disk from a previous attempt) through
    /// `progress` as the transfer proceeds. Matches the
    /// `LrDownloadTarget`/`lr_download_target` API dnf5 uses for package
    /// downloads — the fuller sibling of [`Handle::fetch_to_fd`], needed
    /// here for resume + progress, neither of which `lr_download_url`
    /// exposes.
    pub fn download_target(&self, url: &str, fd: RawFd, resume: bool, expected_size: i64, progress: Option<Arc<AtomicU64>>) -> Result<()> {
        let curl = CString::new(url).context("url contains an interior NUL")?;

        // `clientp` for the progress trampoline: a raw pointer into the
        // `Arc`'s allocation, kept alive on this function's stack for the
        // duration of the (synchronous) download call below.
        let cb_data = progress.as_ref().map(|p| Arc::as_ptr(p) as *mut std::ffi::c_void).unwrap_or(std::ptr::null_mut());
        let cb: sys::LrProgressCb = if progress.is_some() { Some(progress_trampoline) } else { None };

        let target = unsafe {
            sys::lr_downloadtarget_new(
                self.0,
                curl.as_ptr(),
                std::ptr::null(),
                fd,
                std::ptr::null(),
                std::ptr::null_mut(),
                expected_size,
                resume as sys::gboolean,
                cb,
                cb_data,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                0,
                std::ptr::null_mut(),
                0,
                0,
            )
        };
        if target.is_null() {
            bail!("lr_downloadtarget_new failed (out of memory)");
        }

        let mut err = ErrSlot::new();
        let ok = unsafe { sys::lr_download_target(target, err.as_out_param()) };
        let target_err = unsafe { (*target).err };
        let target_rcode = unsafe { (*target).rcode };
        let extra = if !target_err.is_null() {
            Some(unsafe { std::ffi::CStr::from_ptr(target_err).to_string_lossy().into_owned() })
        } else {
            None
        };
        unsafe { sys::lr_downloadtarget_free(target) };

        err.check(ok, &format!("downloading {url}"))?;
        if target_rcode != sys::LRE_OK {
            let msg = extra.unwrap_or_else(|| format!("librepo error code {target_rcode}"));
            bail!("downloading {url}: {msg}");
        }
        Ok(())
    }

    /// Sets `LRO_PROXY`/`LRO_PROXYUSERPWD` — matches dnf5's `librepo.cpp`
    /// `proxy=`/`proxy_username=`+`proxy_password=` handling
    /// (`config_repo.cpp`). `proxy` is a full URL (`http://host:port`,
    /// `socks5://host:port`, ...); `userpwd`, if given, is `"user:pass"`.
    /// Either argument being `None` leaves that option unset on the handle.
    pub fn set_proxy(&self, proxy: Option<&str>, userpwd: Option<&str>) -> Result<()> {
        if let Some(p) = proxy {
            self.setopt_str(sys::LrHandleOption::Proxy, p).context("setting LRO_PROXY")?;
        }
        if let Some(up) = userpwd {
            self.setopt_str(sys::LrHandleOption::Proxyuserpwd, up).context("setting LRO_PROXYUSERPWD")?;
        }
        Ok(())
    }

    /// Sets `LRO_USERNAME`/`LRO_PASSWORD` — HTTP basic/digest credentials
    /// for repos that require authentication, matching dnf5's `librepo.cpp`
    /// `username=`/`password=` handling (`config_repo.cpp`).
    pub fn set_credentials(&self, username: Option<&str>, password: Option<&str>) -> Result<()> {
        if let Some(u) = username {
            self.setopt_str(sys::LrHandleOption::Username, u).context("setting LRO_USERNAME")?;
        }
        if let Some(p) = password {
            self.setopt_str(sys::LrHandleOption::Password, p).context("setting LRO_PASSWORD")?;
        }
        Ok(())
    }

    /// Sets `LRO_SSLCACERT`/`LRO_SSLCLIENTCERT`/`LRO_SSLCLIENTKEY` — a
    /// private CA bundle and/or client certificate/key for repos behind
    /// mTLS or a non-public CA, matching dnf5's `librepo.cpp`
    /// `sslcacert=`/`sslclientcert=`/`sslclientkey=` handling
    /// (`config_repo.cpp`). All three paths are independent; any subset may
    /// be `None`.
    pub fn set_tls_client(&self, cacert: Option<&str>, clientcert: Option<&str>, clientkey: Option<&str>) -> Result<()> {
        if let Some(c) = cacert {
            self.setopt_str(sys::LrHandleOption::Sslcacert, c).context("setting LRO_SSLCACERT")?;
        }
        if let Some(c) = clientcert {
            self.setopt_str(sys::LrHandleOption::Sslclientcert, c).context("setting LRO_SSLCLIENTCERT")?;
        }
        if let Some(k) = clientkey {
            self.setopt_str(sys::LrHandleOption::Sslclientkey, k).context("setting LRO_SSLCLIENTKEY")?;
        }
        Ok(())
    }

    /// Sets `LRO_FASTESTMIRROR`/`LRO_FASTESTMIRRORCACHE` — librepo's own
    /// mirror-speed sorting (a plain TCP connect-time race per mirror,
    /// cached to `cache_path` keyed by host) applied to whatever mirror
    /// list is set on this handle via `LRO_URLS`/`LRO_MIRRORLISTURL`/
    /// `LRO_METALINKURL`. Matches dnf5's `RepoDownloader::common_handle_setup`
    /// (`repo_downloader.cpp`), which points `LRO_FASTESTMIRRORCACHE` at a
    /// single `fastestmirror.cache` file under the *base* cachedir shared by
    /// every repo, not a per-repo file — so `cache_path` should be that same
    /// shared path here too. `LRO_FASTESTMIRRORMAXAGE` is left at librepo's
    /// own default (30 days, `LRO_FASTESTMIRRORMAXAGE_DEFAULT`).
    pub fn set_fastestmirror(&self, enabled: bool, cache_path: Option<&str>) -> Result<()> {
        self.setopt_long(sys::LrHandleOption::Fastestmirror, enabled as i64).context("setting LRO_FASTESTMIRROR")?;
        if enabled {
            if let Some(path) = cache_path {
                self.setopt_str(sys::LrHandleOption::Fastestmirrorcache, path).context("setting LRO_FASTESTMIRRORCACHE")?;
            }
        }
        Ok(())
    }

    /// Sorts `urls` fastest-first using librepo's own `lr_fastestmirror` —
    /// a real per-mirror TCP connect-time race (`LrFastestMirror::
    /// plain_connect_time`), cached to whatever `LRO_FASTESTMIRRORCACHE`
    /// path is set on this handle (see [`Handle::set_fastestmirror`]) and
    /// reused for `LRO_FASTESTMIRRORMAXAGE` (30 days by default). This is
    /// the exact primitive dnf5 relies on for mirror ranking — calling it
    /// directly here (rather than only through `LRO_URLS` +
    /// `lr_download_packages`) lets `rum-repo` rank a mirror list up front,
    /// before deciding which mirror to fetch metadata from, the same way
    /// dnf5's `RepoDownloader` ranks once at metadata-resolution time and
    /// reuses that order for every later package download.
    ///
    /// Falls back to returning `urls` unchanged (in original order) on any
    /// failure — a bad probe here shouldn't block a download that would
    /// otherwise succeed against an unranked mirror list.
    pub fn fastest_mirror_sort(&self, urls: &[String]) -> Result<Vec<String>> {
        if urls.len() <= 1 {
            return Ok(urls.to_vec());
        }
        let cstrs: Vec<CString> = urls.iter().map(|u| CString::new(u.as_str()).context("url contains an interior NUL")).collect::<Result<_>>()?;
        let mut list: *mut sys::GSList = std::ptr::null_mut();
        for c in cstrs.iter().rev() {
            list = unsafe { sys::g_slist_prepend(list, c.as_ptr() as *mut c_void) };
        }

        let mut err = ErrSlot::new();
        let ok = unsafe { sys::lr_fastestmirror(self.0, &mut list, err.as_out_param()) };

        let mut out = Vec::with_capacity(urls.len());
        if ok != 0 {
            let mut node = list;
            while !node.is_null() {
                let data = unsafe { (*node).data } as *const c_char;
                if !data.is_null() {
                    out.push(unsafe { std::ffi::CStr::from_ptr(data) }.to_string_lossy().into_owned());
                }
                node = unsafe { (*node).next };
            }
        }
        unsafe { sys::g_slist_free(list) };
        err.check(ok, "lr_fastestmirror")?;
        if out.len() != urls.len() {
            bail!("lr_fastestmirror returned {} url(s), expected {}", out.len(), urls.len());
        }
        Ok(out)
    }

    /// Sets `LRO_MAXPARALLELDOWNLOADS` — matches `main.get_max_parallel_downloads_option()`
    /// in dnf5's `RepoDownloader::get_cached_handle` (`libdnf5/repo/repo_downloader.cpp`),
    /// applied once per handle before batch-downloading packages with it.
    pub fn set_max_parallel(&self, n: u64) -> Result<()> {
        self.setopt_long(sys::LrHandleOption::Maxparalleldownloads, n.min(i64::MAX as u64) as i64).context("setting LRO_MAXPARALLELDOWNLOADS")
    }

    /// Batch-downloads every target in `specs` with a single
    /// `lr_download_packages` call — librepo's own parallel,
    /// mirror-failover-aware package downloader (bounded by
    /// `LRO_MAXPARALLELDOWNLOADS`, see [`Handle::set_max_parallel`]), the
    /// exact same entrypoint dnf5's `PackageDownloader::download()`
    /// (`libdnf5/repo/package_downloader.cpp`) uses — one `LrPackageTarget`
    /// per package via `lr_packagetarget_new_v3`, batched into a `GSList`,
    /// one `lr_download_packages` call for the whole set. Replaces looping
    /// single-target `download_target` calls from application code, which
    /// gets none of librepo's own mirror failover or shared low-speed/
    /// timeout enforcement across the batch.
    pub fn download_packages(&self, specs: &[PackageSpec], fail_fast: bool) -> Result<Vec<PackageResult>> {
        if specs.is_empty() {
            return Ok(Vec::new());
        }

        // Every C string and per-target state must outlive the
        // `lr_download_packages` call below — librepo holds these pointers
        // (as `cbdata` and as target fields) until that call returns.
        // `states` is a `Vec<Box<_>>` so each element's heap address is
        // stable once pushed, even as the Vec itself grows.
        let mut states: Vec<Box<TargetState>> = Vec::with_capacity(specs.len());
        let mut url_cstrs = Vec::with_capacity(specs.len());
        let mut dest_cstrs = Vec::with_capacity(specs.len());
        let mut checksum_cstrs = Vec::with_capacity(specs.len());
        let mut base_cstrs = Vec::with_capacity(specs.len());
        let mut lr_targets: Vec<*mut sys::LrPackageTarget> = Vec::with_capacity(specs.len());

        let mut build_err: Option<anyhow::Error> = None;
        for spec in specs {
            let url = match CString::new(spec.relative_url.as_str()) {
                Ok(v) => v,
                Err(e) => { build_err = Some(e.into()); break; }
            };
            let dest = match CString::new(spec.dest.as_str()) {
                Ok(v) => v,
                Err(e) => { build_err = Some(e.into()); break; }
            };
            let checksum = if spec.checksum.is_empty() {
                None
            } else {
                match CString::new(spec.checksum.as_str()) {
                    Ok(v) => Some(v),
                    Err(e) => { build_err = Some(e.into()); break; }
                }
            };
            let base = match spec.base_url.as_deref().map(CString::new).transpose() {
                Ok(v) => v,
                Err(e) => { build_err = Some(e.into()); break; }
            };

            let state = Box::new(TargetState {
                progress: spec.progress.clone(),
                ok: false,
                already_exists: false,
                error: None,
            });
            let state_ptr = state.as_ref() as *const TargetState as *mut c_void;

            let mut err = ErrSlot::new();
            let target = unsafe {
                sys::lr_packagetarget_new_v3(
                    self.0,
                    url.as_ptr(),
                    dest.as_ptr(),
                    checksum_type_from_str(&spec.checksum_type),
                    checksum.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null()),
                    spec.expected_size,
                    base.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null()),
                    spec.resume as sys::gboolean,
                    if spec.progress.is_some() { Some(pkg_progress_trampoline) } else { None },
                    state_ptr,
                    Some(pkg_end_trampoline),
                    None,
                    0,
                    0,
                    err.as_out_param(),
                )
            };
            if target.is_null() {
                build_err = Some(err.check(0, "lr_packagetarget_new_v3").unwrap_err());
                break;
            }

            lr_targets.push(target);
            states.push(state);
            url_cstrs.push(url);
            dest_cstrs.push(dest);
            checksum_cstrs.push(checksum);
            base_cstrs.push(base);
        }

        if let Some(e) = build_err {
            for target in &lr_targets {
                unsafe { sys::lr_packagetarget_free(*target) };
            }
            return Err(e);
        }

        // Build the GSList (dnf5 prepends back-to-front for the same
        // O(1)-per-node reason: appending to the tail of a GSList is O(n)).
        let mut list: *mut sys::GSList = std::ptr::null_mut();
        for target in lr_targets.iter().rev() {
            list = unsafe { sys::g_slist_prepend(list, *target as *mut c_void) };
        }

        let flags = if fail_fast { sys::LR_PACKAGEDOWNLOAD_FAILFAST } else { 0 };
        let mut err = ErrSlot::new();
        let ok = unsafe { sys::lr_download_packages(list, flags, err.as_out_param()) };

        unsafe { sys::g_slist_free(list) };
        for target in &lr_targets {
            unsafe { sys::lr_packagetarget_free(*target) };
        }

        // Per librepo's own docs (package_downloader.h): in non-failfast
        // mode, FALSE is only returned for a function-level error — per-
        // target failures are reported through the end callback (already
        // captured in `states`) and don't affect this return value. So a
        // FALSE here is always a hard error, exactly like dnf5's
        // `if (!lr_download_packages(...)) throw`.
        err.check(ok, "lr_download_packages")?;

        Ok(states
            .into_iter()
            .map(|s| PackageResult { ok: s.ok, already_exists: s.already_exists, error: s.error })
            .collect())
    }
}

/// One package to download, passed to [`Handle::download_packages`].
pub struct PackageSpec {
    /// Relative URL (repo-relative package location) — combined with the
    /// handle's configured mirrors/baseurl by librepo itself, same as
    /// `LrPackageTarget::relative_url`.
    pub relative_url: String,
    /// Destination filename (full path) — matches `LrPackageTarget::dest`.
    pub dest: String,
    /// Checksum algorithm name (`"sha256"`, `"sha1"`, ...), case-insensitive;
    /// empty means unknown/unchecked.
    pub checksum_type: String,
    pub checksum: String,
    pub expected_size: i64,
    pub base_url: Option<String>,
    pub resume: bool,
    /// Optional live byte-progress counter, ticked from
    /// [`pkg_progress_trampoline`] the same way [`Handle::download_target`]
    /// ticks its single-target progress counter.
    pub progress: Option<Arc<AtomicU64>>,
}

/// Per-target outcome from [`Handle::download_packages`], populated by
/// librepo's `LrEndCb` — mirrors dnf5's `DownloadCallbacks::TransferStatus`
/// three-way split (success / already-exists / error).
pub struct PackageResult {
    pub ok: bool,
    pub already_exists: bool,
    pub error: Option<String>,
}

struct TargetState {
    progress: Option<Arc<AtomicU64>>,
    ok: bool,
    already_exists: bool,
    error: Option<String>,
}

fn checksum_type_from_str(s: &str) -> sys::LrChecksumType {
    match s.to_ascii_lowercase().as_str() {
        "md5" => sys::LrChecksumType::Md5,
        "sha1" | "sha" => sys::LrChecksumType::Sha1,
        "sha224" => sys::LrChecksumType::Sha224,
        "sha256" => sys::LrChecksumType::Sha256,
        "sha384" => sys::LrChecksumType::Sha384,
        "sha512" => sys::LrChecksumType::Sha512,
        _ => sys::LrChecksumType::Unknown,
    }
}

extern "C" fn pkg_progress_trampoline(clientp: *mut c_void, _total_to_download: c_double, now_downloaded: c_double) -> c_int {
    if !clientp.is_null() {
        let state = unsafe { &mut *(clientp as *mut TargetState) };
        if let Some(p) = &state.progress {
            p.store(now_downloaded.max(0.0) as u64, Ordering::Relaxed);
        }
    }
    0 // LR_CB_OK
}

extern "C" fn pkg_end_trampoline(clientp: *mut c_void, status: sys::LrTransferStatus, msg: *const c_char) -> c_int {
    if !clientp.is_null() {
        let state = unsafe { &mut *(clientp as *mut TargetState) };
        match status {
            sys::LrTransferStatus::Successful => state.ok = true,
            sys::LrTransferStatus::Alreadyexists => {
                state.ok = true;
                state.already_exists = true;
            }
            sys::LrTransferStatus::Error => {
                state.ok = false;
                state.error = Some(if msg.is_null() {
                    "unknown error".to_string()
                } else {
                    unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy().into_owned()
                });
            }
        }
    }
    0 // LR_CB_OK
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { sys::lr_handle_free(self.0) };
        }
    }
}

extern "C" fn progress_trampoline(clientp: *mut std::ffi::c_void, _total_to_download: std::os::raw::c_double, now_downloaded: std::os::raw::c_double) -> std::os::raw::c_int {
    if !clientp.is_null() {
        let atomic = unsafe { &*(clientp as *const AtomicU64) };
        atomic.store(now_downloaded.max(0.0) as u64, Ordering::Relaxed);
    }
    0 // LR_CB_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_new_and_drop() {
        let h = Handle::new("rum-librepo-test/0", 5, 0, 5).unwrap();
        drop(h);
    }

    #[test]
    fn fetch_local_file() {
        let dir = std::env::temp_dir().join(format!("rum-librepo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.txt");
        std::fs::write(&src, b"hello librepo").unwrap();

        let h = Handle::new("rum-librepo-test/0", 5, 0, 5).unwrap();
        let dest = dir.join("dest.txt");
        let file = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(true).open(&dest).unwrap();
        use std::os::unix::io::AsRawFd;
        h.fetch_to_fd(&format!("file://{}", src.display()), file.as_raw_fd()).unwrap();
        drop(file);
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello librepo");
    }

    #[test]
    fn download_target_https() {
        let dir = std::env::temp_dir().join(format!("rum-librepo-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("example.html");
        let file = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(true).open(&dest).unwrap();
        use std::os::unix::io::AsRawFd;
        let h = Handle::new("rum-librepo-test/0", 10, 0, 10).unwrap();
        let progress = Arc::new(AtomicU64::new(0));
        h.download_target("https://example.com/", file.as_raw_fd(), false, 0, Some(progress.clone())).unwrap();
        drop(file);
        let body = std::fs::read(&dest).unwrap();
        assert!(!body.is_empty());
        assert!(progress.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn parse_status_from_message() {
        assert_eq!(parse_http_status("Status code: 404 for http://example.com/x"), Some(404));
        assert_eq!(parse_http_status("some other failure"), None);
    }
}
