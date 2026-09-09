#include "wrapper.h"

// Hand-maintained, not bindgen output: libsolv's headers declare dozens of
// `static inline` helpers (hash.h, queue.h, pool.h, ...), and bindgen's
// wrap_static_fns dumps a non-inline `<name>__extern` wrapper for every one
// it can see reachable from wrapper.h — regardless of whether we call it.
// That's fine on the libsolv version it was generated against, but static
// inline functions are not part of libsolv's stable ABI: EL10 (libsolv
// 0.7.33) simply doesn't declare `allochashtable`/friends that a newer
// Fedora libsolv does, so a vendored full dump fails to compile there with
// "implicit declaration of function 'allochashtable'". Only wrap the two
// static inline functions rum-solv actually calls (see src/lib.rs) — both
// have existed in libsolv since the beginning, so this is stable across
// every libsolv version we target.

Solvable * pool_id2solvable__extern(const Pool *pool, Id p) { return pool_id2solvable(pool, p); }
void queue_push2__extern(Queue *q, Id id1, Id id2) { queue_push2(q, id1, id2); }
void queue_push__extern(Queue *q, Id id) { queue_push(q, id); }
Id * pool_whatprovides_ptr__extern(Pool *pool, Id d) { return pool_whatprovides_ptr(pool, d); }
