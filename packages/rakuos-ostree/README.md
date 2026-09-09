# rakuos-ostree

A custom `ostree` build for RakuOS with SELinux support compiled out entirely.

## Why

RakuOS fully removes `selinux-policy` (AppArmor is the sole MAC on desktop images), but stock Fedora `ostree` is built with `HAVE_SELINUX` defined. During `ostree-finalize-staged.service`, `sysroot_finalize_selinux_policy()` (`src/libostree/ostree-sysroot-deploy.c`) checks only whether `/etc/selinux/config` exists in the new deployment — if it does, it unconditionally shells out to `semodule -N --refresh`, regardless of whether a real policy is loaded. With `selinux-policy` gone, that call fails and aborts deployment finalization (`error: Finalizing deployment: Finalizing SELinux policy: ...`).

`ostree`'s own build system exposes exactly the knob needed to remove this: `configure.ac` has `AC_ARG_WITH(selinux, ...)` / `--without-selinux`, which skips `AC_DEFINE(HAVE_SELINUX)` and compiles the SELinux-aware code paths out of the binary completely — no source patch, no runtime workaround.

## How it works

The CI pipeline does the following on every push:

1. Downloads the Fedora rawhide SRPM — full Fedora patch stack (security fixes, distro integration) without us maintaining it.
2. Flips the spec's selinux bcond off (and/or the `--with-selinux`/`--without-selinux` configure flag directly) so the build runs with `--without-selinux`.
3. Installs BuildRequires and builds.
4. Publishes to the RakuOS RPM repo for `rakuos-44`.

## Why not just delete `/etc/selinux/config` at image-build time instead?

That does work as a stopgap (ostree's check is purely file-existence-based), but it's a workaround, not a fix — it depends on every image build script remembering to strip that file, and any future flow that re-creates it (or a base image update that restores it) silently reopens the bug. Building `ostree` itself without SELinux support removes the capability at the source, matching how `rpm-plugin-selinux` and `rpm` were already handled ([[project_rpm_selinux_plugin_removal]]).

## Credits

- Fedora ostree packaging: https://src.fedoraproject.org/rpms/ostree
- Upstream: https://github.com/ostreedev/ostree
