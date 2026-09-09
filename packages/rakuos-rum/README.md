# rakuos-rum

**rum** = RakuOS + yum. A native, overlay-aware RPM package manager for
RakuOS, written in Rust, intended to fully replace dnf rather than wrap it.

## Why not just keep wrapping dnf?

RakuOS overlays `/usr` (see `rakuos-core`'s `90rakuos-overlay` dracut
module). The base image is a read-only OSTree/composefs deployment, and
everything installed on top lands in an overlayfs upperdir. dnf has no
concept of that split, it sees one merged rpmdb and resolves against it
exactly like a normal, non-overlay system. That means every dnf-based
install RakuOS has ever done has been blind to whether a dependency is
already sitting on the free, shared, image-provided base layer, versus
whether it's about to duplicate that dependency's files into the novel,
per-machine, `/var`-backed overlay upperdir.

rum's whole reason to exist is closing that gap. It resolves dependencies
against an explicit base-image-vs-overlay diff, so installing a package
only ever writes to the overlay what the overlay doesn't already have. It
also refuses to ever quietly layer a base-image package into the overlay,
even if you ask for it by name (see `upgrade`/`reinstall`/`downgrade`/
`distro-sync` below): if a package only exists in the base image, rum
reports that and leaves it alone rather than duplicating it.

## Why rum ends up much lighter than dnf

rum isn't a port of dnf's code, it's a from-scratch reimplementation of the
commands and flags people actually use, built around what RakuOS needs.
There's no 1:1 porting of libdnf5's internals, so none of the weight that
comes along with carrying that over.

It also isn't reinventing rpm. dnf/libdnf reimplement a lot of what rpm
already does itself (its own transaction ordering, its own dependency
bookkeeping, its own package database handling in places), which is a
large part of why libdnf5 is such a big dependency. rum takes the opposite
approach: rpm stays the backbone. Actual transaction application (cpio
payload extraction, scriptlets, triggers, digest verification) is left to
`rpm` itself, rum only owns resolution and the base/overlay split on top
of it. Less code to carry means less code to maintain and fewer places
for RakuOS-specific bugs to hide.

rum accomplishes all of this in about 11,000 lines of Rust — a fraction of
dnf5/libdnf5's sprawling C++ codebase.

## Architecture

Still speaks plain yum/dnf-compatible repo metadata (`repomd.xml` +
`primary.xml`) and reads standard `/etc/yum.repos.d/*.repo` files. rum
changes how packages get resolved and applied, not the repo ecosystem
underneath it. Existing RakuOS, Fedora, Terra, RPM Fusion, etc. repos work
unmodified.

```
crates/
  rum-core        NEVRA identity, dependency expressions, rpm's version-
                   comparison algorithm (rpmvercmp, epoch-aware EVR compare)
  rum-rpmdb        Queries an on-disk rpmdb (shells to `rpm`, doesn't
                   reimplement rpmdb's on-disk format)
  rum-overlay      The differentiator: maintains rum's own overlay rpmdb
                   (/var/lib/rakuos/rum-rpmdb), entirely separate from the
                   base image's own rpmdb snapshot
                   (/var/lib/rakuos/base-rpmdb). The two are unioned, never
                   merged/diffed, so there's no "is the merged view stale"
                   question to answer. Falls back to a single plain rpmdb
                   (rpm's own default) with no base/overlay distinction at
                   all when no RakuOS overlay is present (distrobox/podman,
                   image-build environments), see `OverlayMode`.
  rum-repo         Fetches/parses repomd.xml + primary.xml, loads .repo
                   INI files, caches metadata under /var/cache/rum/repos
                   (dnf5-style <id>-<hash> per-repo dirs) honoring each
                   repo's metadata_expire= before re-fetching, enables/
                   disables Copr repos (rum-repo::copr), and loads
                   /etc/rum/rum.conf. Its actual transfers (rum-repo::net)
                   go through rum-librepo.
  rum-librepo      FFI bindings to librepo (the same C library dnf5 uses
                   for repo metadata/package downloading); backs every
                   HTTP(S) transfer rum-repo::net makes
  rum-resolver     Dependency resolver backed by rum-solv/libsolv; consults
                   rum-overlay before ever considering a repo candidate for
                   a dependency
  rum-solv         FFI bindings to libsolv; builds a Pool from rum's
                   overlay/base/candidate package data and drives the solve
  rum-transaction  Downloads resolved packages, applies them via `rpm -U`
                   / `rpm -e` against rum's own overlay dbpath (or rpm's
                   default dbpath outside a RakuOS overlay system), plus
                   history, versionlock, and GPG key handling
  rum-cli          `rum` binary and all its subcommands
  rum-dnf-shim     `dnf`/`dnf5` compatibility shims, exec into `rum`
  rum-zypper-shim  `zypper` compatibility shim, execs into `rum`
```

## Command status

rum aims for full dnf5 CLI parity over time. Most day-to-day commands are
implemented; a handful of larger subsystems (modularity, advisories,
offline transactions) are still stubs that print a "not implemented"
message rather than doing something wrong silently.

**Implemented:**

- `install`, `remove` (aliases `in`, `rm`)
- `upgrade`, `system-upgrade` (aliases `up`, `update`)
- `reinstall`, `downgrade`, `distro-sync` (aliases `rei`, `dg`, `dsync`)
- `autoremove`, `swap`, `mark`, `check`
- `list`, `search` (aliases `ls`, `se`)
- `provides`, `check-upgrade` (alias `check-update`), `repoquery` (alias `rq`)
- `info` (alias `if`), `leaves`, `origin`, `paths`
- `makecache` (alias `mc`), `copr enable`/`copr disable`
- `clean`, `download`, `debuginfo-install`, `changelog`, `needs-restarting`
- `repo list`/`repo info` (and the `repolist`/`repoinfo` top-level aliases)
- `history`, `versionlock`, `config-manager`
- `group list`/`group info`/`group install`/`group remove`, and `@group-id`
  install syntax (e.g. `install @fonts @hardware-support`) — parses
  `comps.xml`/`group_gz` from repo metadata; a group's `mandatory` and
  `default` packagereqs are pulled in, matching dnf's own default `group
  install` set (`optional` members are skipped)
- `environment list`/`environment info`/`environment install`/`environment
  remove`, and `@^environment-id` install syntax — parsed from the same
  comps data as `group`; an environment's `<grouplist>` groups are pulled in
  (not `<optionlist>`), matching dnf's own default `environment install` set
- `reposync` — mirrors an enabled repo's (or `--repo`-selected repos')
  packages to local disk via `--download-path`/`-p`, with `--newest-only`
  and `--norepopath`
- `repomanage` — scans a local directory of `.rpm` files (not repo
  metadata) and reports old/`--new` versions per name+arch, honoring
  `--keep`
- `repoclosure` — checks every candidate package's `Requires` against the
  full candidate pool of the enabled repos, optionally restricted to a
  `--pkg` glob; reports anything unresolved
- `module list`/`module info`/`module enable`/`module disable`/`module
  reset` — parses `modules.yaml` (modulemd v2, `type="modules"` repo data)
  for module-stream metadata; `enable`/`disable`/`reset` persist state that
  filters every other command's candidate pool (an enabled stream's RPMs
  become visible, a disabled module's are blocked outright, everything else
  falls back to the repo's own `modulemd-defaults` stream if it has one),
  matching dnf's own "only the active stream's packages are installable"
  modularity behavior

`upgrade`, `reinstall`, `downgrade`, and `distro-sync` only ever touch
overlay-installed packages, even when you name a base-image package
explicitly. Naming one prints a message telling you it's provided by the
base image and won't be layered; it does not error out or silently do
nothing.

**Not yet implemented (stubs that print a message and exit):**

- `advisory`/`updateinfo` (updateinfo.xml isn't fetched or parsed)
- `offline`, `offline-upgrade`, `offline-distrosync` (no reboot-time
  transaction staging)
- `builddep`/`build-dep` (no spec-file BuildRequires parsing)
- `replay`, `do`

**Deliberately not reimplemented:** actual RPM transaction application
(cpio payload extraction, scriptlets, triggers, low-level digest
verification). rum shells out to `rpm` itself for that step, the same way
dnf5/zypper sit on top of librpm rather than reimplementing it. Re-owning
that logic is a security and format-fragility liability for very little
upside over trusting rpm to do it right.

## Drop-in dnf replacement on non-RakuOS distros

rum isn't tied to RakuOS's overlay system. [`OverlayPaths::detect()`](crates/rum-overlay/src/lib.rs)
falls back to `Standalone` mode whenever it doesn't find a RakuOS base-image
rpmdb snapshot (`/var/lib/rakuos/base-rpmdb`) — which is every non-RakuOS
system. In `Standalone` mode there's no base/overlay split at all: rum reads
and writes rpm's own default rpmdb, exactly like dnf does, with no
RakuOS-specific behavior left to trip over. Point it at a plain Fedora,
CentOS Stream, RHEL, or other dnf-compatible system's `/etc/yum.repos.d`
and it resolves and installs against that system directly.

Two ways to use it as dnf's replacement:

- **Directly**: `rum install httpd`, `rum remove foo`, etc. work as-is on
  any system with standard yum/dnf repo metadata — rum reads
  `repomd.xml`/`primary.xml` and `.repo` files unmodified, so existing
  Fedora, EPEL, RPM Fusion, Copr, and other third-party repos work without
  any changes.
- **As `dnf`/`dnf5` itself**: the `rum-dnf-shim` crate builds `dnf`/`dnf5`
  binaries that translate the handful of places dnf's grammar diverges from
  rum's (dnf4's positional `list <keyword>`, single-word `groupinstall`/
  `groupremove`/`grouplist`/`groupinfo`, `mark remove`) into rum's own CLI,
  then process-replaces (`exec`) into `rum` — so exit codes, stdio, and
  signal handling all behave exactly as if `rum` had been invoked directly.
  Every verb/flag rum already aliases 1:1 with dnf/dnf5 (the large majority)
  passes through untouched.
- **As `zypper` itself**: the `rum-zypper-shim` crate builds a `zypper`
  binary the same way, for openSUSE/SLE-family systems. It translates
  zypper's own verb spellings (`dist-upgrade`/`dup` → `distro-sync`,
  `list-updates`/`lu` → `check-upgrade`, `what-provides`/`wp` → `provides`,
  `verify`/`ve` → `check`, `packages`/`pa` → `list`, `repos`/`lr` →
  `repo list`, `refresh`/`ref` → `makecache`), `-n`/`--non-interactive` →
  `-y`, `--no-gpg-checks` → `--no-gpgchecks`, `--root`/`-R` →
  `--installroot`, and `addrepo`/`modifyrepo`'s positional argument order
  (zypper takes `<url> <alias>`; rum's `config-manager --add-repo` takes
  `alias=url`) into rum's own CLI, then process-replaces into `rum` the
  same way. `removerepo`, `source-install`, and interactive `shell` mode
  have no rum equivalent and print a clear error instead of silently doing
  nothing.

Installing these shim binaries ahead of (or in place of) the system's real
`dnf`/`dnf5`/`zypper` on `$PATH` lets existing scripts, Ansible playbooks,
and muscle-memory commands keep working unmodified against rum instead.

Since rum only owns dependency resolution and shells out to real `rpm` for
transaction application, its resolver decisions are held to the same
constraints rpm itself would enforce, so this isn't a "mostly works"
approximation of dnf, it's a resolver aimed at full dnf5 CLI parity (see
**Command status** above) sitting on the same trusted `rpm`
backbone dnf itself uses.

## Resolver behavior

Dependency resolution is backed by [libsolv](https://github.com/openSUSE/libsolv)
itself — the same SAT-solving library dnf5 and zypper use — via the
[`rum-solv`](crates/rum-solv/src/lib.rs) crate's FFI bindings, not a
hand-rolled solver. `rum-resolver` builds a libsolv `Pool` from rum's own
overlay/base/candidate package data (rather than handing libsolv rpmdb/repo
files directly), so it covers the cases that matter day to day:

- Consults the overlay/base split before ever considering a repo
  candidate, so a dependency already satisfied by the base image never
  gets pulled into the overlay — every base solvable is locked in the
  pool before each solve, RakuOS's Split-mode "never erase a base
  package" guarantee.
- Rich/boolean dependency syntax (`(A if B)`, `(A if B else C)`,
  `(A unless B)`, `(A or B)`, `(A and B)`, `(A with B)`, `(A without B)`)
  is parsed natively by libsolv (`pool_parserpmrichdep`, from
  `libsolvext`) rather than being translated by rum itself.
  RakuOS ships no SELinux userspace, so `rum-resolver` strips
  `Requires` on `selinux-policy`, `policycoreutils`, any `-selinux`
  subpackage, and a couple of other known-broken repo-metadata
  dependencies out of a package's requirements before it ever reaches
  libsolv, rather than treating them as unsatisfiable (see
  `is_selinux_ignored`/`is_broken_repo_dep_ignored` in
  [`rum-resolver`](crates/rum-resolver/src/lib.rs)).
- Conflicts, Obsoletes, multilib version consistency, and backtracking to
  the next-best provider when the highest-EVR candidate leads to a dead
  end are all handled by libsolv's solver directly.
- Reports already-installed packages a transaction will obsolete via
  `Plan::to_obsolete`. rpm itself auto-erases them during `-U`; this is
  just for plan-summary visibility before the transaction runs.

`protected_packages=` is enforced on removal. `installonlypkgs=`/
`installonly_limit=`/`install_weak_deps=` (`rum.conf`) and `allow_erasing`/
`force_names` are accepted as [`ResolveOptions`](crates/rum-resolver/src/lib.rs)
but not yet translated into libsolv jobs/flags — only the plain `Requires`
closure and the base-package erasure guard above are wired up against the
libsolv backend so far. See [`rum-repo`'s `main_config`](crates/rum-repo/src/lib.rs)
for where these are parsed from `rum.conf`.

## Repo metadata and caching

`primary.xml`/`repomd.xml`/`.repo` parsing handles gzip/xz/zstd metadata
compression, `$releasever`/`$basearch` expansion, and mirrorlist/metalink
resolution with a live `HEAD` probe against each candidate mirror's
`repomd.xml` before committing to it. GPG key import and signature
verification run before install (`rpm --import` / `rpm -K`), gated
per-repo by `gpgcheck=`.

Repo metadata caching is modeled on dnf5's `/var/cache/libdnf5` layout:
per-repo `<id>-<hash>` cache dirs under `/var/cache/rum/repos`,
`metadata_expire=` parsing with `s`/`m`/`h`/`d` suffixes and `-1`/`never`
support, a cache hit makes zero network calls, and `--refresh` or
`rum makecache` forces an unconditional re-fetch.

Copr repo enable/disable auto-detects the right chroot from
`/etc/os-release`'s `ID_LIKE`/`ID` and `VERSION_ID`.

## Output

rum prints dnf-style progress by default, not gated behind `RUST_LOG`/
`-v`: `<repo>: refreshing repository metadata` only when a repo's cache
actually needed a network fetch (silent on a cache hit, same as dnf),
`Downloading Packages:` with a `(n/total): <file>.rpm` line per package
actually fetched, `Importing GPG key for repo '<id>': <url>` before
`rpm --import`, and `Running transaction` / `Running transaction test`
immediately before handing off to `rpm -Uvh`/`rpm -ev`. rpm's own `-v`
output (per-package hashmarks, `Preparing...`, `Running scriptlet:`, etc.)
prints directly since the child process inherits rum's stdout/stderr, so
scriptlet output is visible exactly as it would be running `rpm` by hand.

Every HTTP `GET` (repo metadata, mirrorlist/metalink, package downloads,
GPG keys, Copr repo files) retries on a transient connect/send error
before giving up, using a short backoff. Plain `reqwest` doesn't retry at
all, unlike dnf/libdnf's own `retries=` default, which made rum noticeably
more fragile against a common, usually-harmless failure: a pooled
keep-alive connection the remote end had already closed by the time
`reqwest` tried to reuse it ("connection closed before message
completed"). The retry count is configurable via `retries=` in
`/etc/rum/rum.conf` (default 4).

## Configuration

`/etc/rum/rum.conf` follows the same `[main]` section format as
`/etc/dnf/dnf.conf`. Recognized options: `cachedir`, `reposdir`,
`keepcache`, `assumeyes`, `gpgcheck`, `exclude`, `includepkgs`,
`metadata_expire`, `retries`, `timeout`, `max_parallel_downloads`,
`skip_if_unavailable`, `clean_requirements_on_remove`, `installroot`.
`best=` is parsed but not currently wired to any resolver behavior: rum's
resolver already always backtracks to the next-best provider on a
conflict, so there's no separate best=0/best=1 mode to map it onto.

`exclude`/`excludepkgs`, `includepkgs`, and `skip_if_unavailable` can also
be set per repo in a `.repo` file's own section (same as dnf), layered on
top of `rum.conf`'s `[main]` defaults rather than replacing them: a
per-repo `excludepkgs=` only ever drops candidates sourced from that one
repo, an identically-named build from another enabled repo is unaffected;
a per-repo `skip_if_unavailable=1` lets one flaky/optional repo (e.g. a
Copr) be skipped on failure without changing the default for every other
configured repo.

## Building

`rum-solv` links against libsolv/libsolvext, so building requires
`libsolv-devel` on top of a Rust toolchain (`dnf install libsolv-devel`, or
equivalent). Its FFI bindings are committed pre-generated
(`crates/rum-solv/generated/`) rather than produced by `bindgen` at every
build, so ordinary builds don't need `clang-devel`/`libclang` at all — this
keeps `clang-devel`'s LLVM version out of the RPM `BuildRequires`, which
matters on distros (e.g. EL10) where it can conflict with the LLVM version
our own `rust` package is pinned to. Maintainers refresh the vendored
bindings after a libsolv upgrade with:

```
cargo build -p rum-solv --features regen-bindings
```

which needs `clang-devel` installed and rewrites
`crates/rum-solv/generated/{bindings.rs,static_fns.c}` in place.

Repo/package downloading (`rum-librepo`) links against
[librepo](https://github.com/rpm-software-management/librepo) — the same C
library dnf5 uses — plus GLib, which librepo's own API is built on. Building
requires `librepo-devel` and `glib2-devel` (`dnf install librepo-devel
glib2-devel`, or equivalent). Unlike `rum-solv`, `rum-librepo`'s FFI layer
(`crates/rum-librepo/src/sys.rs`) is hand-written directly against
`/usr/include/librepo/*.h` rather than bindgen output — librepo's
`lr_handle_setopt` is a genuine C varargs function and its error convention
is GLib's `GError**`, neither of which bindgen turns into a usable Rust
signature — so there's no `regen-bindings` feature or vendored bindings file
to refresh here.

```
cargo build --workspace
cargo test --workspace
```

## Usage (early/unstable)

```
rum install <pkg> [<pkg>...]   # resolve + install, skipping base-satisfied deps
rum install --dry-run <pkg>    # resolve + validate only (rpm --test)
rum remove <pkg> [<pkg>...]
rum upgrade [<pkg>...]         # upgrade overlay-installed packages only
rum system-upgrade             # upgrade every overlay-installed package; never touches the read-only base image
rum list [filter]              # list installed packages, optionally name-filtered
rum search <query>             # search enabled repos' name+summary
rum origin <pkg>               # Base / Overlay / Installed / NotInstalled
rum paths                      # print the overlay paths / mode rum is using
rum makecache                  # force-refresh cached repo metadata for every enabled repo
rum copr enable <owner>/<project> [chroot]  # enable a Copr repo (auto-detects chroot)
rum copr disable <owner>/<project>          # remove a previously-enabled Copr repo
```

`--refresh` is also accepted on `install`/`upgrade`/`system-upgrade`/`search`
to force that one command's repo-metadata fetch to ignore its cache instead
of running a separate `makecache` first.

## Commands and flags

Every command below also accepts every global flag listed under **Global
flags** — they can appear before or after the subcommand, exactly like dnf5.

### Global flags

These apply to `rum` itself, not to a specific subcommand, and work no
matter where on the command line they're placed:

| Flag | Meaning |
| --- | --- |
| `--config <path>` | Path to `rum.conf` (default `/etc/rum/rum.conf`). |
| `--repo-dir <path>` | Directory of `.repo` files (default `/etc/yum.repos.d`). |
| `--cache-dir <path>` | Where downloaded `.rpm`s and repo metadata cache live (default `/var/cache/rum`). |
| `-y`, `--assumeyes` | Assume "yes" to any prompt (rum doesn't currently prompt for anything; accepted for script compatibility). |
| `--assumeno` | Assume "no" to any prompt — same compatibility note as `-y`. |
| `-q`, `--quiet` | Suppress non-essential output. |
| `-x`, `--exclude <pattern>` | Exclude a package (name, glob-capable) from resolution — repeatable. |
| `-C`, `--cacheonly` | Run entirely from cached repo metadata; error instead of touching the network. |
| `--refresh` | Force a repo metadata refresh, ignoring `metadata_expire=`. |
| `--repo <id>` (alias `--repoid`) | Restrict operations to only these repo ids (glob-capable, repeatable). |
| `--enablerepo <id>` (alias `--enable-repo`) | Enable a repo for this run only, even if `enabled=0` — repeatable. |
| `--disablerepo <id>` (alias `--disable-repo`) | Disable a repo for this run only, even if `enabled=1` — repeatable. |
| `--no-gpgchecks` (alias `--nogpgcheck`) | Skip GPG signature verification entirely for this run. |
| `--setopt KEY=VALUE` | dnf-style config override, repeatable. rum only acts on `tsflags=noscripts` (skip rpm scriptlets); any other key is accepted and ignored. |
| `--allowerasing` | Permit replacing/erasing conflicting or obsoleted packages to complete a transaction instead of refusing. |
| `--best` | Accepted for dnf compatibility — rum's resolver always picks the newest candidate already, so there's no separate mode to opt into. |
| `--skip-unavailable` | Drop package names that don't match anything in the enabled repos instead of failing the whole transaction (only affects `install`). |
| `--repofrompath <name,baseurl>` | Add an ad-hoc repo for this run only — repeatable. |
| `--no-autoremove` (alias `--noautoremove`) | Don't sweep up now-unneeded dependency-only packages after `remove`. |
| `--installroot <path>` | Install/operate against an alternate root instead of `/`, same semantics as dnf/rpm. Writes a fresh, disposable overlay rpmdb under `<installroot>/var/lib/rakuos/rum-rpmdb`. |
| `--no-recommends` | Don't pull in weak dependencies (`Recommends`) for this run. |

### Package transaction commands

| Command | Aliases | Flags | Notes |
| --- | --- | --- | --- |
| `install <pkg>...` | `in`, `pour` | `--dry-run` | Resolve + install, skipping deps already satisfied by the base image. Accepts `@group-id`/`@^environment-id` entries. |
| `remove <pkg>...` | `rm`, `erase` | `--dry-run` | Removes packages, then runs an implicit `autoremove` sweep unless `--no-autoremove`/`clean_requirements_on_remove=false`. |
| `upgrade [<pkg>...]` | `up`, `update` | `--dry-run` | Upgrades overlay-installed packages only, even if a base-image name is given explicitly (reports it instead of layering a duplicate). |
| `system-upgrade` | | `--dry-run` | Upgrades every overlay-installed package; never touches the read-only base image. |
| `reinstall <pkg>...` | `rei` | `--dry-run` | Reinstalls at the exact currently-installed version. |
| `downgrade <pkg>...` | `dg` | `--dry-run` | Downgrades to the newest available version older than what's installed. |
| `distro-sync [<pkg>...]` | `dsync`, `distrosync` | `--dry-run` | Makes installed packages exactly match the best available repo version (up or down). No names = every overlay-installed package. |
| `autoremove` | | `--dry-run` | Removes overlay packages pulled in only as a dependency that nothing installed still needs. |
| `swap <remove-spec> <install-spec>` | | | Remove then install as two back-to-back transactions (an approximation of dnf's atomic swap). |
| `mark install <pkg>...` / `mark dependency <pkg>...` | | | Changes a package's recorded install reason, controlling `autoremove` eligibility. |
| `check` | | | Checks every overlay-installed package's `Requires` against what's currently installed (base + overlay). |
| `download <pkg>...` | | `--destdir <path>` | Resolves and downloads packages (and deps) without installing. |
| `debuginfo-install <pkg>...` | | `--dry-run` | Installs `<name>-debuginfo` for named packages (thin wrapper over `install`). |

### Query commands

| Command | Aliases | Flags | Notes |
| --- | --- | --- | --- |
| `list [filter]` | `ls` | `--base` | Lists installed packages, optionally name-filtered. `--base` lists the read-only base image's set instead of the overlay's. |
| `search <query>` | `se` | | Searches enabled repos' name + summary for a substring. |
| `provides <capability>` | `whatprovides` | | Shows what's installed or available that provides a name/versioned dep/file path. |
| `check-upgrade [<pkg>...]` | `check-update` | `--json` | Lists installed packages with a newer version available. `--json` emits `{"updates": [...]}`. |
| `repoquery [pattern]` | `rq` | `--installed`, `--available`, `--whatprovides <cap>`, `--whatrequires <cap>`, `--requires`, `--provides` | Queries repo package metadata. No `--list` (filelists.xml isn't parsed). |
| `info <pkg>` | `if`, `more` | | NEVRA, repo, and summary for a package. |
| `leaves` | | | Lists overlay-installed packages nothing else installed depends on. |
| `origin <pkg>` | | | `Base` / `Overlay` / `Installed` / `NotInstalled`. |
| `paths` | | | Prints the overlay paths and mode (`Split`/`Standalone`) rum is using. |
| `changelog <pkg>...` | | | Prints the RPM changelog for installed packages. |
| `needs-restarting` | | | Scans running processes for ones with a deleted/replaced mapped file. Reports PIDs, not systemd units. |

### Repo and metadata commands

| Command | Aliases | Flags | Notes |
| --- | --- | --- | --- |
| `makecache` | `mc` | | Unconditionally refreshes cached repo metadata for every enabled repo. |
| `repo list` | `repolist` (top-level) | `--all` | Lists configured repos and whether each is enabled (`--all` also lists disabled). |
| `repo info <id>` | `repoinfo` (top-level) | | Shows one repo's full configuration. |
| `copr enable <owner>/<project> [chroot]` | | | Enables a Copr repo; auto-detects chroot from `/etc/os-release` unless one is given. |
| `copr disable <owner>/<project>` | | | Removes a previously-enabled Copr repo's `.repo` file. |
| `config-manager` | | `--set-enabled <id>`, `--set-disabled <id>`, `--add-repo <id=baseurl>` | Minimal `.repo` editing: enable/disable existing repos or add a new one from a bare baseurl. All repeatable. |
| `reposync` | | `-p`/`--download-path <path>`, `--newest-only`, `--norepopath` | Mirrors a repo's packages to local disk. Respects the global `--repo` filter for which repo(s). |
| `repomanage <path>` | | `--new`, `--keep <n>` (default 1) | Scans a local directory of `.rpm` files and reports old (or, with `--new`, newest) versions per name+arch. |
| `repoclosure` | | `--pkg <pattern>` (repeatable) | Checks every candidate's `Requires` against the full candidate pool of enabled repos; reports unresolved ones. |
| `clean [all\|packages\|metadata]` | | | Clears cached data under `--cache-dir` (default `all`). |

### History and version pinning

| Command | Flags | Notes |
| --- | --- | --- |
| `history list` | | Lists every recorded transaction, oldest first. |
| `history info <id>` | | Shows one transaction's full package list by id. |
| `versionlock add <pkg>` | | Excludes a package from `upgrade`/`distro-sync` candidate selection. |
| `versionlock delete <pkg>` | | Removes a versionlock entry. |
| `versionlock list` | | Lists current versionlock entries. |
| `versionlock clear` | | Clears all versionlock entries. |

### Comps groups and environments

| Command | Notes |
| --- | --- |
| `group list` / `group info <id>` / `group install <id>` / `group remove <id>` | Comps group management, parsed from each repo's `group`/`group_gz` metadata. `install`/`remove` act on a group's mandatory + default members only (matching dnf's own default). Also usable inline as `install @<group-id>`. |
| `environment list` / `environment info <id>` / `environment install <id>` / `environment remove <id>` | Comps environment management, from the same comps data as `group`. `install`/`remove` act on every group in the environment's `<grouplist>` (not `<optionlist>`). Also usable inline as `install @^<environment-id>`. |

### Not yet implemented

These parse but print a message and exit rather than doing something wrong
silently: `module list`/`info`/`enable`/`disable`/`reset`, `advisory`
(alias `updateinfo`), `offline`, `offline-upgrade`, `offline-distrosync`,
`builddep` (alias `build-dep`), `replay`, `do`.

`rum paths` reports which mode is active: `Split` (a real RakuOS overlay
system, `/var/lib/rakuos/base-rpmdb` plus rum's own
`/var/lib/rakuos/rum-rpmdb`) or `Standalone` (no overlay present,
distrobox/podman or an image-build environment, a single rpmdb using rpm's
own default, auto-detected by checking for the base-image snapshot).

Repo metadata is cached under `/var/cache/rum/repos/<repoid>-<hash>/`
(hash derived from the repo's `baseurl`/`mirrorlist`/`metalink`, so a URL
change gets a fresh cache slot automatically, mirroring dnf5's
`/var/cache/libdnf5` naming). Each repo's `metadata_expire=` (seconds, or
suffixed `6h`/`7d`/`14d`, or `-1`/`never` to disable expiry, defaulting to
172800s/48h if unset, matching `dnf5.conf`) controls how long a cache hit
is trusted before rum re-fetches. Package downloads for install/upgrade
use a separate location directly under `--cache-dir` (default
`/var/cache/rum`), unaffected by this.

`rum copr enable owner/project` guesses the right Copr chroot from
`/etc/os-release`'s `ID_LIKE` (falling back to `ID`) and `VERSION_ID`.
RakuOS sets `ID=rakuos`, `ID_LIKE="fedora"`, so it resolves to
`fedora-<VERSION_ID>-<arch>` since Copr has no `rakuos-*` chroots. Other
families (`mageia`, `opensuse`/`suse`, `rhel`/`centos`/`almalinux`/`rocky`
mapping to `epel-*`) are recognized the same way any RHEL/openSUSE/Mageia
derivative's `ID_LIKE` would name them, in `ID_LIKE`'s listed order
(closest upstream first) so a distro that lists multiple families doesn't
resolve to the wrong one. Pass an explicit chroot (e.g.
`rum copr enable owner/project fedora-44-x86_64`) to override the guess.
The fetched `.repo` file is written to
`<repo-dir>/_copr:copr.fedorainfracloud.org:<owner>:<project>.repo`,
matching dnf's own copr plugin's naming convention.
