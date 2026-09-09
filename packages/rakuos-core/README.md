# rakuos-core

The core package for [RakuOS Linux](https://rakuos.org) — providing overlay filesystem management, package management, and system services that make RakuOS's immutable-but-flexible approach work.

## What is rakuos-core?

RakuOS uses an overlayfs mounted over `/usr` to allow users to install packages on top of an immutable base image without modifying the base. `rakuos-core` owns everything that makes that system run:

- **Early overlay mount** — dracut module mounts `/usr` overlayfs before systemd starts, at kernel handoff
- **Overlay management** — sync, update, protect, and reset the `/usr` overlay
- **Package management** — `rakupkg install`, `rakupkg remove`, `rakupkg list`, `rakupkg update`
- **System services** — systemd units for overlay lifecycle, flatpak maintenance, updater, and more
- **Base protection** — inotify daemon that prevents accidental modification of base image files
- **Setup scripts** — sudo/uutils configuration, virtualization, gaming, secure boot enrollment, and more

## Architecture

### Early overlay mount (dracut)

The overlay is now mounted in the initramfs **before systemd ever starts**, via a custom dracut module (`90rakuos-overlay`). This means:

- `/usr` is writable with user packages before the display manager, udev, or any service sees it
- DKMS kernel modules installed by the user are visible to the kernel before udev runs — no initramfs regeneration needed for driver installs
- modprobe configs from the overlay are applied in the initrd, so things like nouveau blacklists work immediately on the next boot
- Extra kernel modules in `/usr/lib/modules/<kver>/extra/` are loaded via `depmod` + `modprobe` run against the overlay sysroot

The dracut module replaces what previously required `rakuos-overlay-mount.service` and `rakuos-overlay-services.service`.

### Image update merge

On image updates, packages installed in the overlay are automatically:

- **Handed back** to the base image if the new image ships the same or newer version (overlay files removed, kernel dentry cache invalidated)
- **Re-registered** in the overlay RPM database if they need to stay in the overlay
- **Downloaded to cache** before decisions are made, so packages survive across image updates

The sync happens in two stages:
1. **Boot-time** (`rakuos-overlay-sync`) — fast, offline: merges `packages.toml`, rebuilds the RPM db from cache, runs with Plymouth splash visible
2. **Post-login** (`rakuos-overlay-update`) — background: waits for a user session, upgrades overlay packages from repos, sends desktop notifications with progress

## Rust workspace

All core logic is written in Rust. The workspace has four crates:

### `crates/overlay` — overlay lifecycle

| Binary | Function |
|--------|----------|
| `rakuos-overlay-sync` | Boot-time overlay sync — merges RPM db, reinstalls packages from cache |
| `rakuos-overlay-update` | Background overlay update — upgrades overlay packages from repos |
| `rakuos-reset-overlay` | Wipe the overlay (hard or soft reset) |

Shared library (`lib.rs`) provides RPM db helpers, GPG key save/restore, packages.toml merge, cache download, version comparison, Plymouth messaging, and desktop notifications used across the workspace (overlay, pkgmgr, and systems crates all depend on it).

### `crates/pkgmgr` — package management

| Binary | Function |
|--------|----------|
| `rakupkg` | User-facing CLI dispatcher (`install`/`update`/`remove`/`list`/`upgrade`/`system-upgrade`) — installed to `/usr/bin/rakupkg` |
| `rakuos-install` | Install packages into the overlay via dnf5 |
| `rakuos-remove` | Remove packages from the overlay |
| `rakuos-list` | List overlay-installed packages |
| `rakuos-update` | Check/apply package, flatpak, and image updates |
| `rakuos-updater` | Background image update checker (Quay API) |
| `rakuos-cache-clean` | Clean the DNF and local-RPM cache |

`rakuos-install` and `rakuos-remove` forward any `-`/`--` flags verbatim to dnf5 (`--nogpgcheck`, `--enablerepo=`, `-y`, etc.).

### `crates/systems` — system helpers, setup, and per-user/per-app fixups

Home for smaller system-facing binaries — anything that isn't overlay lifecycle or package management lands here.

| Binary | Function |
|--------|----------|
| `rakuos` | User-facing CLI dispatcher for setup/helper subcommands (`setup-nvidia`, `setup-gaming`, `reset-overlay`, `shell`, etc.) — installed to `/usr/bin/rakuos` |
| `rakuos-system-setup` | First-boot / watcher-triggered fixups: `/etc` and sudoers.d permissions, fish shell migration, install-user home seeding, MOK dir, uutils setup, Flathub remote repair. Run at boot (`rakuos-setup.service`) and per-app (via the event watcher) |
| `rakuos-flatpak-event-watcher` | Watches `/var/lib/flatpak/exports/bin` for installs/uninstalls, triggers `system-setup` per-app fixups and wrapper regeneration |
| `rakuos-flatpak-wrapper-gen` | Generates `/usr/local/bin/flatpak/*` CLI wrapper scripts for exported Flatpak apps |
| `rakuos-base-protect` | inotify daemon protecting base image files from being shadowed/deleted in the overlay |
| `rakuos-user` | Per-user session setup: themes, GTK settings, fish shell, live-environment tweaks, queued notifications. Run from the `rakuos-user.service` systemd user unit |
| `rakuos-generate-base-manifest` | Build-time: generates the base file manifest and `dnf.conf` excludepkgs list from protected packages. Run at the end of the Containerfile |

### `crates/initrd` — dracut initrd binaries

These binaries run inside the initramfs (compiled for static linking) and replace the previous shell scripts in the dracut module:

| Binary | Installed to | Function |
|--------|-------------|----------|
| `rakuos-overlay-mount` | `/usr/lib/rakuos/initrd/` | Mounts the overlayfs over `/sysroot/usr` — handles whiteout recovery, hard/soft reset, image update RPM db seeding, factory restore, and base RPM db snapshot |
| `rakuos-load-modprobe-configs` | `/usr/lib/rakuos/initrd/` | Copies modprobe drop-ins from the overlay sysroot into the live initrd `/etc/modprobe.d/` |
| `rakuos-load-extra-modules` | `/usr/lib/rakuos/initrd/` | Runs `depmod` + `modprobe` against the overlay sysroot to load DKMS/extra kernel modules |

The dracut `module-setup.sh` uses `inst_binary` for these, so dracut handles shared lib deps automatically. With a musl target they are fully static with no deps to pull.

## Package contents

| Path | Contents |
|------|----------|
| `/usr/bin/rakuos` | Main CLI dispatcher (`crates/systems`) |
| `/usr/bin/rakupkg` | Package management CLI dispatcher (`crates/pkgmgr`) |
| `/usr/libexec/rakuos/` | Runtime management binaries and scripts |
| `/usr/lib/rakuos/initrd/` | Initrd Rust binaries (overlay mount, modprobe configs, extra modules) |
| `/usr/lib/dracut/modules.d/90rakuos-overlay/` | Dracut module — `module-setup.sh` + systemd units |
| `/usr/lib/systemd/system/` | System service units |
| `/usr/lib/systemd/user/` | User service units |

## Building

CI builds RPMs automatically on push to `main` and publishes to the RakuOS package repository. The pipeline builds for:

- RakuOS Linux 44 (Fedora 44 base)
- RakuOS Rawhide
- RakuOS Enterprise (CentOS Stream 10 base)

To build locally:

```bash
cargo build --release --locked
```

Or build the full RPM:

```bash
dnf install rpm-build rpmdevtools rust cargo
rpmbuild -bb data/rakuos-core.spec
```

## License

GPL-3.0-or-later — see [LICENSE](LICENSE)
