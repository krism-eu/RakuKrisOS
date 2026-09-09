#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// Hand-written mirror of libsolv's `struct s_Solvable` (solvable.h).
/// bindgen cannot generate this one (see the comment in build.rs), so we
/// blocklist its broken opaque stub and define the real, stable, public-ABI
/// layout here instead. Matches the `#else` (non-`LIBSOLV_SOLVABLE_PREPEND_DEP`)
/// branch, which is libsolv's default.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct Solvable {
    pub name: Id,
    pub arch: Id,
    pub evr: Id,
    pub vendor: Id,
    pub repo: *mut Repo,
    pub provides: Offset,
    pub obsoletes: Offset,
    pub conflicts: Offset,
    pub requires: Offset,
    pub recommends: Offset,
    pub suggests: Offset,
    pub supplements: Offset,
    pub enhances: Offset,
}

// `pool_id2solvable`/`queue_push2` are `static inline` in libsolv's headers
// (no linkable symbol in libsolv.so itself), so bindgen can't bind them
// directly. Rather than auto-generating a wrapper for every static inline
// function libsolv happens to declare (fragile across libsolv versions —
// see the comment in generated/static_fns.c), we hand-wrap just the two we
// actually call and declare their real symbols here.
extern "C" {
    #[link_name = "pool_id2solvable__extern"]
    pub fn pool_id2solvable(pool: *const Pool, p: Id) -> *mut Solvable;
    #[link_name = "queue_push2__extern"]
    pub fn queue_push2(q: *mut Queue, id1: Id, id2: Id);
    #[link_name = "queue_push__extern"]
    pub fn queue_push(q: *mut Queue, id: Id);
    /// Pointer to the start of a `0`-terminated run of solvable ids that
    /// provide capability/name id `d` (own name or a `Provides:` entry) —
    /// same lookup libsolv's own provides-based jobs (`SOLVER_SOLVABLE_PROVIDES`)
    /// use internally. Requires `pool_createwhatprovides` to have already
    /// run; the returned pointer is only valid until the next call that
    /// mutates the pool's provides index.
    #[link_name = "pool_whatprovides_ptr__extern"]
    pub fn pool_whatprovides_ptr(pool: *mut Pool, d: Id) -> *const Id;
}
