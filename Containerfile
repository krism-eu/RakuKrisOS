# Build the two RakuOS binaries whose safety properties RakuKrisOS changes,
# from one pinned upstream revision. The initrd patch fixes first boot: an empty
# upperdir is seeded with the base rpmdb and then mounted immediately instead
# of returning and leaving /usr unoverlaid.
FROM registry.fedoraproject.org/fedora@sha256:c1e938afd5dfd7f172fac9168ba8e148fdf41c0517c04849072a15e4bd34eac6 AS overlay-builder
ARG RAKUOS_SOURCE_REV=7d6a4e9ed535eb00f425ef7261a254a5d0043bab
RUN dnf5 -y --setopt=install_weak_deps=False install cargo rust curl python3 \
    && rustc --version \
    && cargo --version \
    && dnf5 clean all
COPY build_files/patch_overlay_mount.py /usr/local/libexec/patch_overlay_mount.py
COPY build_files/initrd-Cargo.lock /src/crates/initrd/Cargo.lock
RUN set -eux; \
    mkdir -p /src/crates/initrd/src/bin; \
    base="https://raw.githubusercontent.com/krism-eu/RakuKrisOS/${RAKUOS_SOURCE_REV}/packages/rakuos-core"; \
    curl -fL "$base/crates/initrd/Cargo.toml" -o /src/crates/initrd/Cargo.toml; \
    printf '\n[workspace]\n' >> /src/crates/initrd/Cargo.toml; \
    curl -fL "$base/crates/initrd/src/bin/overlay_mount.rs" -o /src/crates/initrd/src/bin/overlay_mount.rs; \
    python3 /usr/local/libexec/patch_overlay_mount.py; \
    cargo build --locked --release \
      --manifest-path /src/crates/initrd/Cargo.toml \
      --target-dir /out; \
    test -x /out/release/rakuos-overlay-mount; \
    install -D -m 0755 /out/release/rakuos-overlay-mount /usr/local/bin/rakuos-overlay-mount; \
    test -x /usr/local/bin/rakuos-overlay-mount

# Build our hardened base-protect daemon from the same pinned RakuOS revision.
# Recreate the exact upstream Cargo workspace so its lockfile remains binding;
# only the selected daemon and its library dependency are actually compiled.
COPY build_files/patch_base_protect.py /usr/local/libexec/patch_base_protect.py
RUN set -eux; \
    base="https://raw.githubusercontent.com/krism-eu/RakuKrisOS/${RAKUOS_SOURCE_REV}/packages/rakuos-core"; \
    mkdir -p \
      /src/base-protect/crates/systems/src/bin \
      /src/base-protect/crates/overlay/src \
      /src/base-protect/crates/pkgmgr \
      /src/base-protect/crates/initrd; \
    curl -fL "$base/Cargo.toml" -o /src/base-protect/Cargo.toml; \
    curl -fL "$base/Cargo.lock" -o /src/base-protect/Cargo.lock; \
    for crate in systems overlay pkgmgr initrd; do \
      curl -fL "$base/crates/$crate/Cargo.toml" \
        -o "/src/base-protect/crates/$crate/Cargo.toml"; \
    done; \
    curl -fL "$base/crates/systems/src/bin/base_protect.rs" \
      -o /src/base-protect/crates/systems/src/bin/base_protect.rs; \
    curl -fL "$base/crates/overlay/src/lib.rs" \
      -o /src/base-protect/crates/overlay/src/lib.rs; \
    python3 /usr/local/libexec/patch_base_protect.py; \
    cargo build --locked --release \
      --manifest-path /src/base-protect/Cargo.toml \
      --package rakuos-systems \
      --bin rakuos-base-protect \
      --target-dir /out-base-protect; \
    test -x /out-base-protect/release/rakuos-base-protect; \
    grep -aFq 'RakuKrisOS hardened build' /out-base-protect/release/rakuos-base-protect; \
    install -D -m 0755 /out-base-protect/release/rakuos-base-protect \
      /usr/local/bin/rakuos-base-protect

FROM quay.io/bootc-devel/fedora-bootc-44-minimal@sha256:03d9e53e46040b1d91441f7776a987dfc136ceb39500daa605237eb0cd211207

ARG RAKUKRISOS_RELEASE=0.1.0

LABEL org.opencontainers.image.title="RakuKrisOS"
LABEL org.opencontainers.image.version="${RAKUKRISOS_RELEASE}"
LABEL org.opencontainers.image.description="Fedora 44 bootc Minimal desktop with the RakuOS persistent overlay and RUM"
LABEL containers.bootc="1"
LABEL ostree.bootable="1"

# RakuOS repository and signing key. The reviewed key is part of this
# repository so a compromised build-time network endpoint cannot replace it.
# Primary fingerprint: DF9A06B0DF051609859D3FC518447E77BDBF0EDE
COPY build_files/rakuos.repo /etc/yum.repos.d/rakuos.repo
COPY build_files/RPM-GPG-KEY-rakuos /etc/pki/rpm-gpg/RPM-GPG-KEY-rakuos
RUN set -eux; \
    echo 'db4c3b5e4c5e662bdb98078cd289d07206142c7c4466655232c50ccb3028eada  /etc/pki/rpm-gpg/RPM-GPG-KEY-rakuos' \
      | sha256sum --check --strict; \
    chown root:root /etc/pki/rpm-gpg/RPM-GPG-KEY-rakuos /etc/yum.repos.d/rakuos.repo; \
    chmod 0644 /etc/pki/rpm-gpg/RPM-GPG-KEY-rakuos /etc/yum.repos.d/rakuos.repo; \
    rpm --import /etc/pki/rpm-gpg/RPM-GPG-KEY-rakuos; \
    test "$(stat -c '%U:%G %a' /etc/pki/rpm-gpg/RPM-GPG-KEY-rakuos)" = "root:root 644"; \
    test "$(stat -c '%U:%G %a' /etc/yum.repos.d/rakuos.repo)" = "root:root 644"

# One source of truth for the immutable base. Weak dependencies stay disabled:
# optional native applications belong to the persistent RUM overlay.
COPY build_files/base-packages.txt /tmp/base-packages.txt
RUN set -eux; \
    grep -vE '^[[:space:]]*(#|$)' /tmp/base-packages.txt \
      | xargs dnf5 -y --setopt=install_weak_deps=False install; \
    if rpm -q glibc-all-langpacks >/dev/null 2>&1; then \
      dnf5 -y remove glibc-all-langpacks; \
    fi; \
    dnf5 clean all; \
    rm -f /tmp/base-packages.txt; \
    ! rpm -q glibc-all-langpacks >/dev/null 2>&1

# RakuOS runtime infrastructure. This is temporary until rakuos-core-slim is
# packaged; the initrd overlay binary is replaced below with our pinned,
# Minimal-safe build. RUM is installed last because rum-dnf-shim supersedes
# the dnf/dnf5 command-line package manager.
RUN dnf5 -y --setopt=install_weak_deps=False install \
        rakuos-core rakuos-rum rum-dnf-shim
COPY --from=overlay-builder /usr/local/bin/rakuos-overlay-mount \
    /usr/lib/rakuos/initrd/rakuos-overlay-mount
COPY --from=overlay-builder /usr/local/bin/rakuos-base-protect \
    /usr/libexec/rakuos/rakuos-base-protect
RUN chmod 0755 \
    /usr/lib/rakuos/initrd/rakuos-overlay-mount \
    /usr/libexec/rakuos/rakuos-base-protect

COPY config/rum.conf /etc/rum/rum.conf
COPY build_files/systemd/ /etc/systemd/system/
# This is a shipped, user-selectable profile. It is deliberately not copied
# to packages.list and therefore causes no automatic native app installation.
COPY build_files/desktop-packages.txt /usr/share/rakukrisos/profiles/desktop-packages.txt

# Starting from Fedora Minimal means all RakuOS state contracts must be seeded
# explicitly. No default native applications are installed at boot: an empty
# packages.list makes overlay-sync a no-op, so login never depends on network
# or repository availability.
RUN set -eux; \
    install -d -m 0755 /usr/share/rakuos /usr/share/factory/var/lib/rakuos; \
    printf '%s\n' plasma > /usr/share/rakuos/de-name; \
    printf '%s\n' \
      bootc ostree rakuos-core rakuos-rum rum-dnf-shim \
      plasma-workspace plasma-desktop kwin plasma-login-manager \
      > /usr/share/rakuos/protected-packages.txt; \
    : > /usr/share/factory/var/lib/rakuos/packages.list; \
    /usr/libexec/rakuos/generate-base-manifest; \
    test -s /usr/share/rakuos/base-manifest.txt

# Rebuild the bootc initramfs only after replacing the overlay binary. The
# helper owns the temporary /root materialization and restores its bootc
# symlink from an EXIT trap, including when dracut or validation fails.
COPY build_files/rebuild-initramfs.sh /usr/local/libexec/rebuild-rakukrisos-initramfs
RUN /usr/local/libexec/rebuild-rakukrisos-initramfs

# Keep Fedora stock kernel + SELinux userspace for the first deployment tests.
# Security-stack changes come only after boot/update/rollback behavior is proven.

# Fedora 44 Plasma Login Manager uses plasmalogin.service.
RUN systemctl enable --force plasmalogin.service \
    && systemctl enable rakuos-base-protect.service \
    && systemctl set-default graphical.target

# Image invariants. Fail before publication when a Minimal-specific assumption
# or a RakuOS path has been missed.
RUN set -eux; \
    echo 'db4c3b5e4c5e662bdb98078cd289d07206142c7c4466655232c50ccb3028eada  /etc/pki/rpm-gpg/RPM-GPG-KEY-rakuos' | sha256sum --check --strict; \
    test -x /usr/bin/bootc; \
    test -x /usr/bin/ostree; \
    test -x /usr/bin/rum; \
    test -x /usr/lib/rakuos/initrd/rakuos-overlay-mount; \
    test -x /usr/libexec/rakuos/rakuos-base-protect; \
    grep -aFq 'RakuKrisOS hardened build' /usr/libexec/rakuos/rakuos-base-protect; \
    test "$(stat -c '%U:%G %a' /etc/pki/rpm-gpg/RPM-GPG-KEY-rakuos)" = "root:root 644"; \
    test "$(stat -c '%U:%G %a' /etc/yum.repos.d/rakuos.repo)" = "root:root 644"; \
    test -e /usr/lib/dracut/modules.d/90rakuos-overlay/module-setup.sh; \
    test -e /usr/lib/systemd/system/ostree-finalize-staged.service; \
    test -e /usr/lib/systemd/system/plasmalogin.service; \
    test -e /usr/lib/systemd/system/rakuos-overlay-sync.service; \
    test -e /usr/lib/systemd/system/rakuos-base-protect.service; \
    test -e /etc/systemd/system/rakuos-base-protect.service.d/rakukrisos.conf; \
    systemd-analyze verify rakuos-base-protect.service; \
    systemctl --root=/ is-enabled rakuos-base-protect.service; \
    systemctl --root=/ is-enabled plasmalogin.service; \
    test -s /usr/share/rakuos/protected-packages.txt; \
    test -s /usr/share/rakuos/base-manifest.txt; \
    test -s /usr/share/rakukrisos/profiles/desktop-packages.txt; \
    test -e /usr/share/factory/var/lib/rakuos/packages.list; \
    test ! -s /usr/share/factory/var/lib/rakuos/packages.list; \
    test ! -e /usr/share/factory/var/lib/rakuos/overlay/upper/share/rakukrisos/.overlay-bootstrap; \
    test -L /root; \
    rpm -q glibc-langpack-en glibc-langpack-it langpacks-core-en langpacks-core-it; \
    ! rpm -q glibc-all-langpacks >/dev/null 2>&1; \
    bootc container lint
