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

# RakuOS runtime infrastructure. RUM is installed last because rum-dnf-shim
# intentionally supersedes the dnf/dnf5 command-line package manager.
RUN dnf5 -y --setopt=install_weak_deps=False install \
        rakuos-core rakuos-rum rum-dnf-shim

COPY config/rum.conf /etc/rum/rum.conf

# Full RakuOS seeds a factory application set. RakuKrisOS deliberately does
# not: a daily-driver boot must never depend on network availability, repo
# health, or a successful RUM transaction before the display manager starts.
# An empty packages.list makes upstream rakuos-overlay-sync exit immediately,
# while the persistent /usr overlay remains available for explicit RUM installs
# after login.
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
    test -d /usr/share/icons/breeze; \
    rpm -q glibc-langpack-en glibc-langpack-it langpacks-core-en langpacks-core-it; \
    ! rpm -q glibc-all-langpacks >/dev/null 2>&1
