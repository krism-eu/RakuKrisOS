# Build only the initrd overlay binary we need, from a pinned RakuOS source
# revision. RakuKrisOS patches one important first-boot behavior: an empty
# upperdir is seeded with the base rpmdb and then mounted immediately instead
# of returning and leaving /usr unoverlaid.
FROM fedora:44 AS overlay-builder
ARG RAKUOS_SOURCE_REV=7d6a4e9ed535eb00f425ef7261a254a5d0043bab
RUN dnf5 -y --setopt=install_weak_deps=False install cargo rust curl python3 \
    && dnf5 clean all
RUN set -eux; \
    mkdir -p /src/crates/initrd/src/bin; \
    base="https://raw.githubusercontent.com/krism-eu/RakuKrisOS/${RAKUOS_SOURCE_REV}/packages/rakuos-core"; \
    curl -fL "$base/Cargo.toml" -o /src/Cargo.toml; \
    curl -fL "$base/Cargo.lock" -o /src/Cargo.lock; \
    curl -fL "$base/crates/initrd/Cargo.toml" -o /src/crates/initrd/Cargo.toml; \
    curl -fL "$base/crates/initrd/src/bin/overlay_mount.rs" -o /src/crates/initrd/src/bin/overlay_mount.rs; \
    python3 - <<'PY'
from pathlib import Path
p = Path('/src/crates/initrd/src/bin/overlay_mount.rs')
s = p.read_text()
old = '''    // Upper dir empty — seed RPM db and exit for sync to install
    if dir_is_empty(&upper_dir) {
        log("seeding RPM db into upper dir before first install...");
        let seed_rpmdb = upper_dir.join("share/rpm");
        fs::create_dir_all(&seed_rpmdb)?;
        copy_dir_contents(&base_rpm_db, &seed_rpmdb)?;
        remove_rpmdb_locks(&seed_rpmdb);
        log("RPM db seeded — sync will handle install.");
        return Ok(());
    }
'''
new = '''    // An empty upperdir is valid on a fresh Fedora Minimal deployment.
    // Seed the rpmdb view, but DO NOT return: /usr must already be an overlay
    // before switch-root so RUM can safely persist native packages later.
    if dir_is_empty(&upper_dir) {
        log("seeding RPM db into empty upper dir...");
        let seed_rpmdb = upper_dir.join("share/rpm");
        fs::create_dir_all(&seed_rpmdb)?;
        copy_dir_contents(&base_rpm_db, &seed_rpmdb)?;
        remove_rpmdb_locks(&seed_rpmdb);
        log("RPM db seeded — continuing to mount persistent overlay.");
    }
'''
if old not in s:
    raise SystemExit('expected upstream empty-upper block not found; refusing to build against changed source')
p.write_text(s.replace(old, new, 1))
PY
RUN set -eux; \
    cd /src; \
    cargo build --locked --release -p rakuos-initrd --target-dir /out; \
    test -x /out/release/rakuos-overlay-mount; \
    install -D -m 0755 /out/release/rakuos-overlay-mount /usr/local/bin/rakuos-overlay-mount; \
    test -x /usr/local/bin/rakuos-overlay-mount

FROM quay.io/bootc-devel/fedora-bootc-44-minimal:latest

ARG RAKUKRISOS_RELEASE=0.1.0

LABEL org.opencontainers.image.title="RakuKrisOS"
LABEL org.opencontainers.image.version="${RAKUKRISOS_RELEASE}"
LABEL org.opencontainers.image.description="Fedora 44 bootc Minimal desktop with the RakuOS persistent overlay and RUM"
LABEL containers.bootc="1"
LABEL ostree.bootable="1"

# RakuOS repository and signing key. Fedora repositories remain enabled.
COPY build_files/rakuos.repo /etc/yum.repos.d/rakuos.repo
RUN curl --fail --silent --show-error --location \
        https://repo.rakuos.org/pubkey.gpg \
        --output /etc/pki/rpm-gpg/RPM-GPG-KEY-rakuos \
    && rpm --import /etc/pki/rpm-gpg/RPM-GPG-KEY-rakuos

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
RUN chmod 0755 /usr/lib/rakuos/initrd/rakuos-overlay-mount

COPY config/rum.conf /etc/rum/rum.conf

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

# rakuos-core's RPM %post may generate an initramfs under /boot. A bootc image
# instead carries initramfs next to the kernel under /usr/lib/modules/$kver.
# Rebuild after replacing the overlay binary and fail if the module/binary are
# not really present in the initramfs. Fedora bootc intentionally has
# /root -> /var/roothome; that target is absent at image-build time and dracut
# otherwise fails trying to copy /root. Temporarily materialize /root only for
# dracut, then restore the bootc symlink exactly as it was.
RUN set -eux; \
    root_was_symlink=0; \
    root_target=''; \
    if [ -L /root ]; then \
      root_was_symlink=1; \
      root_target="$(readlink /root)"; \
      rm -f /root; \
      install -d -m 0700 /root; \
    fi; \
    found_kernel=0; \
    for moddir in /usr/lib/modules/*; do \
      [ -d "$moddir" ] || continue; \
      kver="${moddir##*/}"; \
      [ -e "$moddir/vmlinuz" ] || continue; \
      found_kernel=1; \
      dracut --force --no-hostonly "$moddir/initramfs.img" "$kver"; \
      lsinitrd "$moddir/initramfs.img" | grep -q 'rakuos-overlay-mount'; \
      lsinitrd "$moddir/initramfs.img" | grep -q '90rakuos-overlay'; \
    done; \
    [ "$found_kernel" -eq 1 ]; \
    if [ "$root_was_symlink" -eq 1 ]; then \
      rm -rf /root; \
      ln -s "$root_target" /root; \
    fi; \
    rm -f /boot/initramfs-*.img; \
    test -z "$(find /boot -mindepth 1 -maxdepth 1 -type f -print -quit 2>/dev/null)"

# Keep Fedora stock kernel + SELinux userspace for the first deployment tests.
# Security-stack changes come only after boot/update/rollback behavior is proven.

# Fedora 44 Plasma Login Manager uses plasmalogin.service.
RUN systemctl enable --force plasmalogin.service \
    && systemctl enable rakuos-base-protect.service \
    && systemctl set-default graphical.target

# Image invariants. Fail before publication when a Minimal-specific assumption
# or a RakuOS path has been missed.
RUN set -eux; \
    test -x /usr/bin/bootc; \
    test -x /usr/bin/ostree; \
    test -x /usr/bin/rum; \
    test -x /usr/lib/rakuos/initrd/rakuos-overlay-mount; \
    test -e /usr/lib/dracut/modules.d/90rakuos-overlay/module-setup.sh; \
    test -e /usr/lib/systemd/system/ostree-finalize-staged.service; \
    test -e /usr/lib/systemd/system/plasmalogin.service; \
    test -e /usr/lib/systemd/system/rakuos-overlay-sync.service; \
    test -e /usr/lib/systemd/system/rakuos-base-protect.service; \
    test -s /usr/share/rakuos/protected-packages.txt; \
    test -s /usr/share/rakuos/base-manifest.txt; \
    test -e /usr/share/factory/var/lib/rakuos/packages.list; \
    test ! -s /usr/share/factory/var/lib/rakuos/packages.list; \
    test ! -e /usr/share/factory/var/lib/rakuos/overlay/upper/share/rakukrisos/.overlay-bootstrap; \
    test -L /root; \
    rpm -q glibc-langpack-en glibc-langpack-it langpacks-core-en langpacks-core-it; \
    ! rpm -q glibc-all-langpacks >/dev/null 2>&1; \
    bootc container lint
