//! Hand-written FFI declarations against librepo's public C API
//! (`/usr/include/librepo/*.h`, librepo 1.20 as shipped on this machine) and
//! the small slice of GLib's `GError` ABI librepo's error convention needs.
//! See the crate-level `Cargo.toml` comment for why this isn't bindgen
//! output like `rum-solv`'s `sys.rs` is.
#![allow(non_camel_case_types)]
#![allow(dead_code)]

use libc::{c_char, c_double, c_int, c_long, c_void};

/// Opaque librepo handle (`typedef struct _LrHandle LrHandle;`, handle.h).
/// Never constructed on the Rust side — only ever seen behind a pointer
/// returned by `lr_handle_init`.
#[repr(C)]
pub struct LrHandle {
    _private: [u8; 0],
}

/// `gboolean` (glib/gtypes.h: `typedef gint gboolean;`, `gint` is a plain
/// `c_int`).
pub type gboolean = c_int;

/// `GQuark` (glib/gquark.h: `typedef guint32 GQuark;`).
pub type GQuark = u32;

/// GLib's `GError` (glib/gerror.h) — stable public layout, safe to mirror
/// directly:
/// ```c
/// struct _GError { GQuark domain; gint code; gchar *message; };
/// ```
#[repr(C)]
pub struct GError {
    pub domain: GQuark,
    pub code: c_int,
    pub message: *mut c_char,
}

/// `LrHandleOption` (handle.h) — a plain C enum, so its values are just
/// 0, 1, 2, ... in declaration order. Only the subset this crate actually
/// sets is named; the rest of librepo's enum still occupies those ordinal
/// slots so this must stay in the exact order handle.h declares them.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum LrHandleOption {
    Update = 0,
    Urls = 1,
    Mirrorlist = 2,
    MirrorlistUrl = 3,
    MetalinkUrl = 4,
    Local = 5,
    Httpauth = 6,
    Userpwd = 7,
    Proxy = 8,
    Proxyport = 9,
    Proxytype = 10,
    Proxyauth = 11,
    Proxyuserpwd = 12,
    Progresscb = 13,
    Progressdata = 14,
    Maxspeed = 15,
    Destdir = 16,
    Repotype = 17,
    Connecttimeout = 18,
    Ignoremissing = 19,
    Interruptible = 20,
    Useragent = 21,
    Fetchmirrors = 22,
    Maxmirrortries = 23,
    Maxparalleldownloads = 24,
    Maxdownloadspermirror = 25,
    Varsub = 26,
    Fastestmirror = 27,
    Fastestmirrorcache = 28,
    Fastestmirrormaxage = 29,
    Fastestmirrorcb = 30,
    Fastestmirrordata = 31,
    Lowspeedtime = 32,
    Lowspeedlimit = 33,
    Gpgcheck = 34,
    Checksum = 35,
    Yumdlist = 36,
    Yumblist = 37,
    Hmfcb = 38,
    Sslverifypeer = 39,
    Sslverifyhost = 40,
    Ipresolve = 41,
    Allowedmirrorfailures = 42,
    Adaptivemirrorsorting = 43,
    Gnupghomedir = 44,
    Fastestmirrortimeout = 45,
    Httpheader = 46,
    Offline = 47,
    Sslclientcert = 48,
    Sslclientkey = 49,
    Sslcacert = 50,
    Httpauthmethods = 51,
    Proxyauthmethods = 52,
    Ftpuseepsv = 53,
    Yumslist = 54,
    Cachedir = 55,
    Preservetime = 56,
    Onetimeflag = 57,
    Sslverifystatus = 58,
    ProxySslverifypeer = 59,
    ProxySslverifyhost = 60,
    ProxySslclientcert = 61,
    ProxySslclientkey = 62,
    ProxySslcacert = 63,
    Username = 64,
    Password = 65,
}

/// `LrRepotype` (types.h) — passed as `LRO_REPOTYPE`'s vararg. `lr_download_packages`
/// requires this be set to `LR_YUMREPO` on any handle whose `LRO_URLS` it
/// resolves package targets against (confirmed against this librepo build:
/// omitting it fails every batch target with "Bad repo type", even though
/// the single-target `lr_download_target`/`lr_download_url` calls never
/// needed it).
pub const LR_YUMREPO: c_int = 1 << 1;

/// `LrProgressCb` (types.h): `int (*)(void *clientp, double total, double now)`.
pub type LrProgressCb = Option<extern "C" fn(clientp: *mut c_void, total_to_download: c_double, now_downloaded: c_double) -> c_int>;

/// `LrTransferStatus` (types.h) — passed to `LrEndCb`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LrTransferStatus {
    Successful = 0,
    Alreadyexists = 1,
    Error = 2,
}

/// `LrEndCb` (types.h): `int (*)(void *clientp, LrTransferStatus status, const char *msg)`.
pub type LrEndCb = Option<extern "C" fn(clientp: *mut c_void, status: LrTransferStatus, msg: *const c_char) -> c_int>;

/// `LrMirrorFailureCb` (types.h): `int (*)(void *clientp, const char *msg, const char *url)`.
pub type LrMirrorFailureCb = Option<extern "C" fn(clientp: *mut c_void, msg: *const c_char, url: *const c_char) -> c_int>;

/// `LrChecksumType` (checksum.h) — sorted by hash quality, matches librepo's
/// declaration order exactly.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum LrChecksumType {
    Unknown = 0,
    Md5 = 1,
    Sha1 = 2,
    Sha224 = 3,
    Sha256 = 4,
    Sha384 = 5,
    Sha512 = 6,
}

/// Opaque `LrPackageTarget` (package_downloader.h) — this crate never reads
/// its fields directly (unlike `LrDownloadTarget`, whose `err`/`rcode` we
/// read back after a single-target download); per-target results instead
/// come back through the `LrEndCb`/`LrMirrorFailureCb` trampolines, so a
/// zero-sized opaque type is enough here.
#[repr(C)]
pub struct LrPackageTarget {
    _private: [u8; 0],
}

/// `LrPackageDownloadFlag` (package_downloader.h).
pub const LR_PACKAGEDOWNLOAD_FAILFAST: c_int = 1 << 0;

/// GLib singly-linked list node (glib/gslist.h) — stable public layout,
/// safe to construct directly rather than going through `g_slist_prepend`
/// for every node.
#[repr(C)]
pub struct GSList {
    pub data: *mut c_void,
    pub next: *mut GSList,
}

extern "C" {
    pub fn lr_handle_init() -> *mut LrHandle;
    pub fn lr_handle_free(handle: *mut LrHandle);

    /// Genuine C varargs (`lr_handle_setopt(LrHandle *, GError **, LrHandleOption, ...)`,
    /// handle.h) — every call site below must pass exactly the type the
    /// matching `LRO_*` doc comment in handle.h specifies (`c_long` for
    /// "(long)" options, not `c_int`; `*const c_char` for "(char *)"; a raw
    /// `*const *const c_char` for "(char **)"), since C varargs are not
    /// type-checked and a mismatched size corrupts the stack/registers.
    pub fn lr_handle_setopt(handle: *mut LrHandle, err: *mut *mut GError, option: LrHandleOption, ...) -> gboolean;

    /// Single-URL-to-fd convenience wrapper (downloader.h). Respects every
    /// option set on `handle` (timeouts, low-speed abort, TLS, user agent,
    /// proxy, mirror list if any) but not per-target resume/progress — that
    /// needs the fuller `LrDownloadTarget` API below.
    pub fn lr_download_url(handle: *mut LrHandle, url: *const c_char, fd: c_int, err: *mut *mut GError) -> gboolean;

    pub fn g_error_free(error: *mut GError);
}

/// `LrDownloadTarget` (downloadtarget.h) — only the fields this crate reads
/// or writes are given real types; the rest keep the same in-memory size via
/// explicit padding fields, since bindgen isn't generating this and every
/// field's C declaration order/type must be reproduced exactly for the
/// struct layout to match librepo's compiled definition.
#[repr(C)]
pub struct LrDownloadTarget {
    pub handle: *mut LrHandle,
    pub path: *mut c_char,
    pub baseurl: *mut c_char,
    pub fd: c_int,
    pub fn_: *mut c_char,
    pub checksums: *mut c_void,
    pub expectedsize: i64,
    pub origsize: i64,
    pub resume: gboolean,
    pub progresscb: LrProgressCb,
    pub cbdata: *mut c_void,
    pub endcb: *mut c_void,
    pub mirrorfailurecb: *mut c_void,
    pub chunk: *mut c_void,
    pub byterangestart: i64,
    pub byterangeend: i64,
    pub no_cache: gboolean,
    pub usedmirror: *mut c_char,
    pub effectiveurl: *mut c_char,
    pub rcode: c_int,
    pub err: *mut c_char,
    pub userdata: *mut c_void,
    pub is_zchunk: gboolean,
    pub range: *mut c_char,
    pub zck_dl: *mut c_void,
    pub zck_header_size: i64,
    pub total_to_download: c_double,
    pub downloaded: c_double,
}

extern "C" {
    pub fn lr_downloadtarget_new(
        handle: *mut LrHandle,
        path: *const c_char,
        baseurl: *const c_char,
        fd: c_int,
        fn_: *const c_char,
        possiblechecksums: *mut c_void,
        expectedsize: i64,
        resume: gboolean,
        progresscb: LrProgressCb,
        cbdata: *mut c_void,
        endcb: *mut c_void,
        mirrorfailurecb: *mut c_void,
        userdata: *mut c_void,
        byterangestart: i64,
        byterangeend: i64,
        range: *mut c_char,
        no_cache: gboolean,
        is_zchunk: gboolean,
    ) -> *mut LrDownloadTarget;

    pub fn lr_downloadtarget_free(target: *mut LrDownloadTarget);

    pub fn lr_download_target(target: *mut LrDownloadTarget, err: *mut *mut GError) -> gboolean;

    /// `lr_packagetarget_new_v3` (package_downloader.h) — the exact
    /// constructor dnf5's `PackageDownloader::download()`
    /// (`libdnf5/repo/package_downloader.cpp`) uses to build each
    /// `LrPackageTarget` before batching them into one `lr_download_packages`
    /// call.
    pub fn lr_packagetarget_new_v3(
        handle: *mut LrHandle,
        relative_url: *const c_char,
        dest: *const c_char,
        checksum_type: LrChecksumType,
        checksum: *const c_char,
        expectedsize: i64,
        base_url: *const c_char,
        resume: gboolean,
        progresscb: LrProgressCb,
        cbdata: *mut c_void,
        endcb: LrEndCb,
        mirrorfailurecb: LrMirrorFailureCb,
        byterangestart: i64,
        byterangeend: i64,
        err: *mut *mut GError,
    ) -> *mut LrPackageTarget;

    pub fn lr_packagetarget_free(target: *mut LrPackageTarget);

    /// Downloads every target in `targets` (a `GSList` of `LrPackageTarget*`)
    /// in one call — librepo's own parallel, mirror-failover-aware batch
    /// downloader (matches `LRO_MAXPARALLELDOWNLOADS` set on `handle`),
    /// exactly what dnf5 uses instead of looping single-target downloads
    /// itself.
    pub fn lr_download_packages(targets: *mut GSList, flags: c_int, err: *mut *mut GError) -> gboolean;

    pub fn g_slist_prepend(list: *mut GSList, data: *mut c_void) -> *mut GSList;
    pub fn g_slist_free(list: *mut GSList);

    /// `lr_fastestmirror` (fastestmirror.h) — sorts `*list` (a `GSList` of
    /// `char*` mirror URLs) in place by measured connection speed, the exact
    /// primitive `lr_handle_perform`/`lr_download_packages` call internally
    /// when `LRO_FASTESTMIRROR` is enabled on a handle. Reads `handle`'s
    /// `LRO_FASTESTMIRRORCACHE`/`LRO_FASTESTMIRRORMAXAGE`/
    /// `LRO_FASTESTMIRRORTIMEOUT`/proxy/connect-timeout options; `handle` may
    /// be `NULL` to use librepo's built-in defaults for all of those.
    pub fn lr_fastestmirror(handle: *mut LrHandle, list: *mut *mut GSList, err: *mut *mut GError) -> gboolean;
}

pub const LRE_OK: c_int = 0;
pub const LR_TRUE: gboolean = 1;

// The `long` options in `LrHandleOption`'s doc comments (CONNECTTIMEOUT,
// LOWSPEEDTIME, LOWSPEEDLIMIT, SSLVERIFYPEER/HOST) all take a plain C
// `long`, i.e. `c_long` here — kept as a type alias use-site reminder next
// to the extern block rather than re-explained at every call site.
pub type c_long_opt = c_long;
