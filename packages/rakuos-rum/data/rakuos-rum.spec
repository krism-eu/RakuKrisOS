%global debug_package %{nil}

%if 0%{?fedora}
%global os_version %{fedora}
%elif 0%{?rhel}
%global os_version %{rhel}
%else
%global os_version rawhide
%endif

Name:           rakuos-rum
Version:        %{os_version}
Release:        1%{?dist}
Summary:        rum — RakuOS's overlay-aware RPM package manager
License:        GPL-3.0-or-later
URL:            https://gitlab.com/rakuos/packages/rakuos/rakuos-rum
Source0:        https://gitlab.com/rakuos/packages/rakuos/rakuos-rum/-/archive/main/rakuos-rum-main.zip

ExclusiveArch:  x86_64 aarch64

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  systemd-rpm-macros
BuildRequires:  libsolv-devel
BuildRequires:  pkgconfig(libsolv)
BuildRequires:  pkgconfig(libsolvext)
BuildRequires:  pkgconfig(openssl)
BuildRequires:  pkgconfig(librepo)
BuildRequires:  pkgconfig(glib-2.0)
BuildRequires:  pkgconfig(rpm)

Requires:       rpm
Requires:       systemd
Requires:       libsolv
Requires:       librepo

%{?systemd_requires}

%description
rum ("RakuOS + yum") is a native, overlay-aware RPM package manager for
RakuOS. It consumes standard yum/dnf-compatible repo metadata but resolves
and applies transactions itself, so it can tell packages already provided
by the read-only base image apart from packages layered into the overlay
— something dnf, working from a single merged view of "installed", cannot
do. rum is intended as a full dnf replacement on RakuOS, not a wrapper
around it.

%package -n rum-dnf-shim
Summary:        dnf/dnf5 compatible command shims that route to rum
License:        GPL-3.0-or-later
Requires:       rakuos-rum = %{version}-%{release}

# Supersede the dnf5 CLI itself and its command-plugin subpackage — every
# dnf5-plugins verb (builddep, changelog, config-manager, copr,
# needs-restarting, repoclosure, repomanage, reposync) is implemented
# natively by rum, so nothing needs dnf5-plugins to remain installed.
Provides:       dnf = %{version}-%{release}
Provides:       dnf5 = %{version}-%{release}
Provides:       dnf5-plugins = %{version}-%{release}
Provides:       /usr/bin/dnf
Provides:       /usr/bin/dnf5
Obsoletes:      dnf5 < %{version}-%{release}
Obsoletes:      dnf5-plugins < %{version}-%{release}
Conflicts:      dnf5
Conflicts:      dnf5-plugins

# Supersede the legacy dnf4/yum stack too, in case it's present instead of
# (or alongside) dnf5 on a given base — same reasoning: rum is the only
# package manager left, nothing may depend on the real thing surviving.
Provides:       dnf-data = %{version}-%{release}
Provides:       dnf-plugins-core = %{version}-%{release}
Provides:       dnf-utils = %{version}-%{release}
Provides:       dnf-automatic = %{version}-%{release}
Provides:       dnf-yum = %{version}-%{release}
Provides:       python3-dnf = %{version}-%{release}
Provides:       python3-dnf-plugins-core = %{version}-%{release}
Provides:       microdnf = %{version}-%{release}
Provides:       yum = %{version}-%{release}
Provides:       yum-utils = %{version}-%{release}
Obsoletes:      dnf < %{version}-%{release}
Obsoletes:      dnf-data < %{version}-%{release}
Obsoletes:      dnf-plugins-core < %{version}-%{release}
Obsoletes:      dnf-utils < %{version}-%{release}
Obsoletes:      dnf-automatic < %{version}-%{release}
Obsoletes:      dnf-yum < %{version}-%{release}
Obsoletes:      python3-dnf < %{version}-%{release}
Obsoletes:      python3-dnf-plugins-core < %{version}-%{release}
Obsoletes:      microdnf < %{version}-%{release}
Obsoletes:      yum < %{version}-%{release}
Obsoletes:      yum-utils < %{version}-%{release}
Conflicts:      dnf
Conflicts:      dnf-data
Conflicts:      dnf-plugins-core
Conflicts:      dnf-utils
Conflicts:      dnf-automatic
Conflicts:      dnf-yum
Conflicts:      python3-dnf
Conflicts:      python3-dnf-plugins-core
Conflicts:      microdnf
Conflicts:      yum
Conflicts:      yum-utils

# Every command rum implements, advertised as an RPM capability the same
# way dnf5/dnf5-plugins advertise "dnf5-command(<verb>)" — lets any package
# that Requires a specific dnf5 subcommand be satisfied by this shim
# instead of needing the real dnf5-plugins package installed.
Provides:       dnf5-command(advisory)
Provides:       dnf5-command(autoremove)
Provides:       dnf5-command(builddep)
Provides:       dnf5-command(changelog)
Provides:       dnf5-command(check)
Provides:       dnf5-command(check-upgrade)
Provides:       dnf5-command(clean)
Provides:       dnf5-command(config-manager)
Provides:       dnf5-command(copr)
Provides:       dnf5-command(debuginfo-install)
Provides:       dnf5-command(distro-sync)
Provides:       dnf5-command(downgrade)
Provides:       dnf5-command(download)
Provides:       dnf5-command(environment)
Provides:       dnf5-command(group)
Provides:       dnf5-command(history)
Provides:       dnf5-command(info)
Provides:       dnf5-command(install)
Provides:       dnf5-command(leaves)
Provides:       dnf5-command(list)
Provides:       dnf5-command(makecache)
Provides:       dnf5-command(mark)
Provides:       dnf5-command(module)
Provides:       dnf5-command(needs-restarting)
Provides:       dnf5-command(offline)
Provides:       dnf5-command(provides)
Provides:       dnf5-command(reinstall)
Provides:       dnf5-command(remove)
Provides:       dnf5-command(repoclosure)
Provides:       dnf5-command(repomanage)
Provides:       dnf5-command(repoquery)
Provides:       dnf5-command(reposync)
Provides:       dnf5-command(repo)
Provides:       dnf5-command(replay)
Provides:       dnf5-command(search)
Provides:       dnf5-command(swap)
Provides:       dnf5-command(system-upgrade)
Provides:       dnf5-command(upgrade)
Provides:       dnf5-command(versionlock)

# dnf4's plugin system advertises the same idea under a different capability
# name — `dnf-command(<verb>)`, not `dnf5-command(<verb>)` — and it's what
# packages built against the dnf4/dnf-plugins-core world actually Require
# (e.g. fedora-review's `Requires: dnf-command(repoquery)`). Obsoleting/
# conflicting dnf-plugins-core away above removes the real provider of that
# capability, so every dnf4-style verb needs the same shim coverage as its
# dnf5-command(*) counterpart above or those Requires go unsatisfiable.
Provides:       dnf-command(builddep)
Provides:       dnf-command(changelog)
Provides:       dnf-command(config-manager)
Provides:       dnf-command(copr)
Provides:       dnf-command(debuginfo-install)
Provides:       dnf-command(download)
Provides:       dnf-command(needs-restarting)
Provides:       dnf-command(repoclosure)
Provides:       dnf-command(repomanage)
Provides:       dnf-command(repoquery)
Provides:       dnf-command(reposync)
Provides:       dnf-command(system-upgrade)
Provides:       dnf-command(versionlock)

%description -n rum-dnf-shim
Drop-in `dnf`/`dnf5` binaries that translate dnf's CLI grammar (including
dnf4-only forms like `groupinstall` and positional `list installed`) into
equivalent `rum` invocations and exec into `rum` directly. Installing this
package supersedes and removes the dnf5/dnf5-plugins CLI (and the legacy
dnf4/yum stack, if present) — rum is the only package manager left on the
system, not a wrapper layered on top of dnf.

Note: this deliberately does NOT Obsolete/Conflict libdnf5, libdnf5-cli, or
libdnf5-plugin-expired-pgp-keys — those are shared libraries other packages
(e.g. PackageKit backends) may link against independently of the dnf5 CLI,
so they're left for the base image/dependency resolution to manage.

%prep
%autosetup -n rakuos-rum-main

%build
# Strip -flto=auto/-ffat-lto-objects out of the RPM-injected CFLAGS before
# building: cc-rs (used by rum-solv's bindgen build and by zstd-sys/
# lzma-sys's vendored C sources) picks up $CFLAGS automatically, but
# rustc's own final link step doesn't invoke LTO-aware linking. Static
# archives built from fat-LTO objects then fail to link under ld.bfd with
# "undefined reference to FSE_readNCount"/"HUF_readStats" (zstd-sys) even
# though the real (non-IR) symbols are present in the object — reproduced
# and confirmed locally via a from-scratch `rpmbuild` (not just `cargo
# build`) in a matching Fedora 44 container; disabling just the LTO flags
# fixes it while keeping every other hardening flag RPM sets.
export CFLAGS="$(echo "${CFLAGS:-%{optflags}}" | sed -E 's/-flto=[^ ]*//g; s/-ffat-lto-objects//g')"
cargo build --release --locked

%install
install -d %{buildroot}%{_bindir}
install -m 0755 target/release/rum %{buildroot}%{_bindir}/rum
install -m 0755 target/release/dnf %{buildroot}%{_bindir}/dnf
install -m 0755 target/release/dnf5 %{buildroot}%{_bindir}/dnf5
install -d %{buildroot}%{_sysconfdir}/rum
install -m 0644 data/rum.conf %{buildroot}%{_sysconfdir}/rum/rum.conf
install -d %{buildroot}%{_unitdir}
install -m 0644 data/rum-makecache.service %{buildroot}%{_unitdir}/rum-makecache.service
install -m 0644 data/rum-makecache.timer %{buildroot}%{_unitdir}/rum-makecache.timer

%post
%systemd_post rum-makecache.timer

%preun
%systemd_preun rum-makecache.timer

%postun
%systemd_postun_with_restart rum-makecache.timer

%files
%license LICENSE
%{_bindir}/rum
%config(noreplace) %{_sysconfdir}/rum/rum.conf
%{_unitdir}/rum-makecache.service
%{_unitdir}/rum-makecache.timer

%files -n rum-dnf-shim
%license LICENSE
%{_bindir}/dnf
%{_bindir}/dnf5

%changelog
* Sat Aug 15 2026 Joshua Webb <joshwebb84@outlook.com> - %{os_version}-1
- Drop the manual "Requires: libsolvext" — no package is actually named
  libsolvext on Fedora or EL10 (libsolvext.so.1 ships inside the libsolv
  package on both), so the literal name was unsatisfiable; rpm's automatic
  soname scanner already adds the correct libsolvext.so.1()(64bit)
  requirement from the linked binary
* Sat Aug 15 2026 Joshua Webb <joshwebb84@outlook.com> - %{os_version}-1
- rum-solv: generated/static_fns.c is now hand-maintained, wrapping only
  the two static-inline libsolv functions we actually call
  (pool_id2solvable, queue_push2) instead of every static inline function
  bindgen's wrap_static_fns happened to find. The full vendored dump broke
  on EL10 (libsolv 0.7.33) with "implicit declaration of function
  'allochashtable'" — that helper isn't part of libsolv's stable API and
  simply isn't declared in EL10's older headers, even though we never
  called it. Verified end-to-end with a real `rpmbuild -bb` inside a
  CentOS Stream 10 container.
* Sat Aug 15 2026 Joshua Webb <joshwebb84@outlook.com> - %{os_version}-1
- rum-solv: use committed, pre-generated libsolv FFI bindings by default
  instead of running bindgen at build time (bindgen is now an opt-in
  `regen-bindings` cargo feature for maintainers only); drop clang-devel
  BuildRequires — on rakuos-el (EL10) it required a newer llvm-libs than
  our own `rust` package is built against, making the two mutually
  uninstallable via `dnf builddep`
* Sat Aug 15 2026 Joshua Webb <joshwebb84@outlook.com> - %{os_version}-1
- %build: strip -flto=auto/-ffat-lto-objects from RPM-injected CFLAGS
  before `cargo build` — fixes "undefined reference to
  FSE_readNCount"/"HUF_readStats" link errors: cc-rs (used by zstd-sys's
  vendored C sources) picks up $CFLAGS automatically, but rustc's final
  link step isn't LTO-aware, so static archives built from fat-LTO objects
  fail to link under ld.bfd even though the real symbols are present
* Sat Aug 15 2026 Joshua Webb <joshwebb84@outlook.com> - %{os_version}-1
- Resolver now backed by libsolv (rum-solv crate) instead of a hand-rolled
  SAT solver; add libsolv-devel BuildRequires and libsolv/libsolvext
  runtime Requires (pool_parserpmrichdep, used for rpm rich/boolean
  dependency parsing, lives in libsolvext)
* Sat Aug 15 2026 Joshua Webb <joshwebb84@outlook.com> - %{os_version}-1
- Add rum-makecache.service/.timer (mirrors dnf-makecache.timer): refreshes
  repo metadata hourly in the background so `rum check-upgrade`/software
  center update checks hit a warm cache instead of a cold/expired one
* Wed Aug 12 2026 Joshua Webb <joshwebb84@outlook.com> - %{os_version}-1
- Package downloads now stream straight to disk instead of buffering the
  whole body in memory, verifying the transfer against the server-reported
  Content-Length (the same safety net rakurpm gets from wget, without
  losing rum's pooled-connection reuse across hundreds of packages)
- Client no longer applies a single whole-request timeout to package/
  metadata fetches (was killing large downloads that legitimately took
  longer than `timeout=`); replaced with a per-chunk stall timeout so only
  a connection gone genuinely silent gets aborted
- rum-dnf-shim: add dnf-command(<verb>) provides (dnf4 plugin capability
  naming) alongside the existing dnf5-command(<verb>) set — fixes
  `dnf-command(repoquery)` (needed by fedora-review) becoming unsatisfiable
  once dnf-plugins-core, its real provider, is obsoleted/conflicted away
* Sun Aug 09 2026 Joshua Webb <joshwebb84@outlook.com> - %{os_version}-1
- Add rum-dnf-shim subpackage: Rust dnf/dnf5 CLI shims that exec into rum,
  Provides/Obsoletes/Conflicts real dnf5 so it can be removed outright
- Extend Provides/Obsoletes/Conflicts to cover dnf5-plugins, the legacy
  dnf4/yum/microdnf stack, and every dnf5-command(<verb>) capability rum
  implements, so nothing pulls the real dnf/dnf5 packages back in
* Fri Aug 07 2026 Joshua Webb <joshwebb84@outlook.com> - %{os_version}-1
- Initial package: install/remove/upgrade/system-upgrade/list/search/origin/paths
- Overlay-aware dependency resolution with Conflicts/Obsoletes handling
- GPG key import and signature verification on downloaded packages
- gzip/xz/zstd repo metadata support, $releasever/$basearch expansion
