FROM quay.io/bootc-devel/fedora-bootc-44-minimal:latest

ARG RAKUKRISOS_RELEASE=0.1.0

LABEL org.opencontainers.image.title="RakuKrisOS"
LABEL org.opencontainers.image.version="${RAKUKRISOS_RELEASE}"
LABEL org.opencontainers.image.description="Fedora 44 bootc Minimal desktop with the RakuOS persistent overlay and RUM"
LABEL containers.bootc="1"
LABEL ostree.bootable="1"

# RakuOS uses its own signing key. Keep the repository definition explicit so
# image builds and later RUM transactions use the same trust configuration.
COPY build_files/rakuos.repo /etc/yum.repos.d/rakuos.repo
RUN curl --fail --silent --show-error --location \
        https://repo.rakuos.org/pubkey.gpg \
        --output /etc/pki/rpm-gpg/RPM-GPG-KEY-rakuos \
    && rpm --import /etc/pki/rpm-gpg/RPM-GPG-KEY-rakuos

# Install the immutable/base portion only. Weak dependencies are intentionally
# disabled; optional KDE applications belong to the persistent RUM overlay.
RUN dnf5 -y --setopt=install_weak_deps=False install \
        bootc bootupd composefs composefs-libs dracut ostree systemd flatpak \
        plasma-workspace plasma-workspace-common plasma-workspace-libs \
        kwin kwin-common kwin-libs kwayland \
        plasma-login-manager kcm-plasmalogin \
        plasma-nm plasma-pa plasma-integration \
        kde-cli-tools polkit-kde polkit-qt6-1 powerdevil kglobalacceld ksystemstats \
        xdg-desktop-portal xdg-desktop-portal-kde xdg-user-dirs \
        pipewire pipewire-alsa pipewire-jack-audio-connection-kit \
        wireplumber phonon-qt6 \
    && dnf5 clean all

# RakuOS runtime infrastructure. RUM is installed last because rum-dnf-shim
# intentionally supersedes the dnf/dnf5 command-line package manager.
RUN dnf5 -y --setopt=install_weak_deps=False install \
        rakuos-core rakuos-rum rum-dnf-shim

COPY config/rum.conf /etc/rum/rum.conf

# The first CI image deliberately keeps Fedora's stock kernel and SELinux
# userspace until the custom no-SELinux OSTree + AppArmor path is proven in CI.
# Removing SELinux before that point can make ostree-finalize-staged fail and
# produce exactly the kind of deployment rollback RakuKrisOS must avoid.

# Plasma Login Manager owns the graphical login path used by the previous
# working RakuKrisOS build.
RUN systemctl enable --force plasmalogin.service \
    && systemctl set-default graphical.target

# Basic image invariants: fail the build here rather than publishing an image
# missing the components needed for a bootc deployment.
RUN test -x /usr/bin/bootc \
    && test -x /usr/bin/ostree \
    && test -x /usr/bin/rum \
    && test -e /usr/lib/systemd/system/ostree-finalize-staged.service \
    && test -e /usr/lib/systemd/system/plasmalogin.service
