# RakuKrisOS

RakuKrisOS is a personal Fedora 44 bootc desktop experiment: a small immutable base, KDE Plasma and a persistent RakuOS/RUM overlay for explicitly selected native packages.

## Current architecture

- Fedora 44 Minimal bootc base, pinned by OCI digest.
- Fedora stock kernel, bootc, OSTree/composefs, dracut and systemd.
- Plasma 6 shell and Plasma Login Manager in the immutable base.
- RakuOS persistent `/usr` overlay and RUM infrastructure.
- Flatpak support.
- Fedora and RakuOS repositories enabled.
- No gaming stack, NVIDIA-specific configuration, Ollama, Nix or unrelated RakuOS hardware extras.

SELinux userspace is still present in the current image. AppArmor-only remains the intended direction, but that transition will happen only after kernel support and the boot/update/rollback path have passed real-machine or VM runtime tests. The README must not claim that work is already complete.

## Package profiles

`build_files/base-packages.txt` is the source of truth for packages installed in the immutable image.

`build_files/desktop-packages.txt` is an optional native-application profile. It is shipped inside the image as `/usr/share/rakukrisos/profiles/desktop-packages.txt`, but is not copied into RUM's `packages.list` and is not installed automatically. This avoids making first login depend on the network and keeps application selection explicit.

## Supply-chain policy

The two build-stage images are pinned by OCI index digest. Updating Fedora is therefore an explicit reviewed commit, not an implicit effect of rebuilding `:latest`.

The RakuOS package-signing key is stored as `build_files/RPM-GPG-KEY-rakuos`. Its expected values are:

- Primary fingerprint: `DF9A06B0DF051609859D3FC518447E77BDBF0EDE`
- SHA-256: `db4c3b5e4c5e662bdb98078cd289d07206142c7c4466655232c50ccb3028eada`

A key rotation must update the file, checksum and documented fingerprint in the same reviewed commit.

## CI and publication

Pull requests and manual branch runs build and validate the image without publishing it. A push to `main` publishes both the immutable commit-SHA tag and `latest`. A manual publication is accepted only from `main` with the `publish` input enabled. Main-branch builds are never cancelled by a newer run.

Static image checks currently pass. They prove package presence, service enablement, initramfs contents and bootc container invariants; they do not yet prove that the overlay mounts and survives reboot on a real booted system.

## Reliability gates before installation

1. Harden and test `rakuos-base-protect` against pre-existing upperdir entries, moved directories and symbolic-link shadows.
2. Produce a bootable disk image from the OCI image.
3. Boot it twice in QEMU and verify the `/usr` overlay, persistence, RUM state and graphical-login ordering.
4. Test bootc update and rollback.
5. Only then decide the SELinux-to-AppArmor transition and the initial optional desktop applications.

## Branches

- `main`: release-ready RakuKrisOS build and configuration.
- `codex/stabilize-rakukrisos`: current stabilization work.
- `rakuos-upstream`: intact imported RakuOS components retained for provenance and comparison.
