%global debug_package %{nil}

%if 0%{?fedora}
%global os_version %{fedora}
%elif 0%{?rhel}
%global os_version %{rhel}
%else
%global os_version rawhide
%endif

Name:           rakuos-core
Version:        %{os_version}
Release:        1%{?dist}
Summary:        RakuOS core scripts, services, and overlay management
License:        GPL-3.0-or-later
URL:            https://gitlab.com/rakuos/apps/rakuos-core
Source0:        https://gitlab.com/rakuos/apps/rakuos-core/-/archive/main/rakuos-core-main.zip

ExclusiveArch:  x86_64 aarch64

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  systemd-rpm-macros

Requires:       bash
Requires:       rpm
Requires:       dnf5
Requires:       dracut
Requires:       systemd
Requires:       flatpak
Requires:       bootc
Requires:       jq

%description
RakuOS core package providing overlay filesystem management, system scripts,
and systemd services for RakuOS Linux.

%prep
%autosetup -n rakuos-core-main

%build
cargo build --release --locked

%install
# ── Rust overlay binaries → /usr/libexec/rakuos/ ─────────────────────────────
# rakuos-overlay-update is deliberately NOT installed here — deprecated now
# that rum's split-mode overlay rpmdb needs no post-image-update merge/
# reconcile pass. The crate/binary source is kept (not deleted) in case it's
# needed for reference, but nothing packages or triggers it anymore.
install -d %{buildroot}%{_libexecdir}/rakuos
install -m 0755 target/release/rakuos-overlay-sync     %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 target/release/rakuos-reset-overlay    %{buildroot}%{_libexecdir}/rakuos/

# ── Rust pkgmgr binaries → /usr/libexec/rakuos/ ──────────────────────────────
install -m 0755 target/release/rakuos-install          %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 target/release/rakuos-remove           %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 target/release/rakuos-list             %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 target/release/rakuos-update           %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 target/release/rakuos-updater          %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 target/release/rakuos-cache-clean      %{buildroot}%{_libexecdir}/rakuos/

# ── Rust systems binaries → /usr/libexec/rakuos/ ─────────────────────────────
install -m 0755 target/release/rakuos-system-setup       %{buildroot}%{_libexecdir}/rakuos/system-setup
install -m 0755 target/release/rakuos-flatpak-wrapper-gen %{buildroot}%{_libexecdir}/rakuos/flatpak-wrapper-gen
install -m 0755 target/release/rakuos-flatpak-event-watcher %{buildroot}%{_libexecdir}/rakuos/flatpak-event-watcher
install -m 0755 target/release/rakuos-base-protect     %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 target/release/rakuos-user             %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 target/release/rakuos-generate-base-manifest %{buildroot}%{_libexecdir}/rakuos/generate-base-manifest

# ── Other scripts → /usr/libexec/rakuos/ ─────────────────────────────────────
install -d %{buildroot}%{_libexecdir}/rakuos/flatpak-fixes
install -m 0755 data/scripts/enroll-secureboot-key        %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 data/scripts/reenroll-tpm2                %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 data/scripts/setup-g733zm-audio           %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 data/scripts/setup-gaming                 %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 data/scripts/setup-nvidia                 %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 data/scripts/setup-nvidia-legacy           %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 data/scripts/setup-lsfg-vk               %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 data/scripts/setup-ollama                 %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 data/scripts/setup-sudo                   %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 data/scripts/setup-uutils                 %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 data/scripts/setup-virtualization         %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 data/scripts/rakuos-sign-modules          %{buildroot}%{_libexecdir}/rakuos/
install -m 0755 data/flatpak-fixes/com.core447.StreamController.sh \
    %{buildroot}%{_libexecdir}/rakuos/flatpak-fixes/
install -m 0755 data/flatpak-fixes/org.mozilla.firefox.sh \
    %{buildroot}%{_libexecdir}/rakuos/flatpak-fixes/

# ── Main CLI → /usr/bin/rakuos ────────────────────────────────────────────────
install -d %{buildroot}%{_bindir}
install -m 0755 target/release/rakuos   %{buildroot}%{_bindir}/rakuos
install -m 0755 target/release/rakupkg  %{buildroot}%{_bindir}/rakupkg

# ── Dracut module ─────────────────────────────────────────────────────────────
install -d %{buildroot}/usr/lib/dracut/modules.d/90rakuos-overlay
install -m 0755 data/dracut/modules.d/90rakuos-overlay/module-setup.sh \
    %{buildroot}/usr/lib/dracut/modules.d/90rakuos-overlay/
install -m 0644 data/dracut/modules.d/90rakuos-overlay/rakuos-overlay-mount.service \
    %{buildroot}/usr/lib/dracut/modules.d/90rakuos-overlay/

# ── Initrd Rust binary → /usr/lib/rakuos/initrd/ ─────────────────────────────
install -d %{buildroot}/usr/lib/rakuos/initrd
install -m 0755 target/release/rakuos-overlay-mount        %{buildroot}/usr/lib/rakuos/initrd/

# ── Systemd system units ──────────────────────────────────────────────────────
install -d %{buildroot}/usr/lib/systemd/system
install -d %{buildroot}/usr/lib/systemd/system/display-manager.service.d
install -m 0644 data/systemd/system/dmemcg-booster.service              %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/flatpak-cleanup.service             %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/flatpak-cleanup.timer               %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/flatpak-repair.service              %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/flatpak-repair.timer                %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/nix.mount                           %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/podman-prune.service                %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/podman-prune.timer                  %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rakuos-base-protect.service         %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rakuos-bootc-switch.service         %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rakuos-bootc-upgrade.service        %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rakuos-cache-clean.service          %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rakuos-cache-clean.timer            %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rakuos-flatpak-watcher.service      %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rakuos-overlay-sync.service         %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rakuos-setup.service                %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rakuos-updater.service              %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rakuos-updater.timer                %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rpm-ostree-clean-deployments.service %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rpm-ostree-clean-deployments.timer  %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rpm-ostree-clean-metadata.service   %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/rpm-ostree-clean-metadata.timer     %{buildroot}/usr/lib/systemd/system/
install -m 0644 data/systemd/system/display-manager.service.d/rakuos-overlay-sync.conf \
    %{buildroot}/usr/lib/systemd/system/display-manager.service.d/

# ── Systemd user units ────────────────────────────────────────────────────────
install -d %{buildroot}/usr/lib/systemd/user
install -m 0644 data/systemd/user/dmemcg-booster-user.service  %{buildroot}/usr/lib/systemd/user/
install -m 0644 data/systemd/user/rakuos-user.service          %{buildroot}/usr/lib/systemd/user/

%post
dracut --force --regenerate-all 2>/dev/null || true

%files
%license LICENSE
%{_bindir}/rakuos
%{_bindir}/rakupkg
%{_libexecdir}/rakuos/
/usr/lib/dracut/modules.d/90rakuos-overlay/
/usr/lib/rakuos/initrd/
/usr/lib/systemd/system/dmemcg-booster.service
/usr/lib/systemd/system/display-manager.service.d/
/usr/lib/systemd/system/flatpak-cleanup.service
/usr/lib/systemd/system/flatpak-cleanup.timer
/usr/lib/systemd/system/flatpak-repair.service
/usr/lib/systemd/system/flatpak-repair.timer
/usr/lib/systemd/system/nix.mount
/usr/lib/systemd/system/podman-prune.service
/usr/lib/systemd/system/podman-prune.timer
/usr/lib/systemd/system/rakuos-base-protect.service
/usr/lib/systemd/system/rakuos-bootc-switch.service
/usr/lib/systemd/system/rakuos-bootc-upgrade.service
/usr/lib/systemd/system/rakuos-cache-clean.service
/usr/lib/systemd/system/rakuos-cache-clean.timer
/usr/lib/systemd/system/rakuos-flatpak-watcher.service
/usr/lib/systemd/system/rakuos-overlay-sync.service
/usr/lib/systemd/system/rakuos-setup.service
/usr/lib/systemd/system/rakuos-updater.service
/usr/lib/systemd/system/rakuos-updater.timer
/usr/lib/systemd/system/rpm-ostree-clean-deployments.service
/usr/lib/systemd/system/rpm-ostree-clean-deployments.timer
/usr/lib/systemd/system/rpm-ostree-clean-metadata.service
/usr/lib/systemd/system/rpm-ostree-clean-metadata.timer
/usr/lib/systemd/user/dmemcg-booster-user.service
/usr/lib/systemd/user/rakuos-user.service

%changelog
* Sun Jul 19 2026 Joshua Webb <joshwebb84@outlook.com> - %{os_version}-1
- Rewrite system-setup, flatpak-wrapper-gen, and flatpak-event-watcher from bash to Rust (crates/systems)
- Drop inotify-tools requirement; event watching now uses native inotify via Rust
- Rewrite rakupkg from bash to Rust (crates/pkgmgr), still installed to /usr/bin/rakupkg
- Rewrite rakuos from bash to Rust (crates/systems), still installed to /usr/bin/rakuos
- Move rakuos-base-protect and rakuos-user from crates/pkgmgr to crates/systems
- Rewrite generate-base-manifest from bash to Rust (crates/systems)

* Thu Jun 25 2026 Joshua Webb <joshwebb84@outlook.com> - %{os_version}-1
- Rewrite overlay management (mount/sync/update/services/reset) from bash to Rust
- Add dracut module 90rakuos-overlay: mounts overlayfs in pre-pivot, eliminating rakuos-overlay-mount.service
- Rewrite rakuos-install/remove/list/update/updater/cache-clean/migrate/user from bash to Rust (crates/pkgmgr)
