FROM registry.fedoraproject.org/fedora:44

# RakuKrisOS: Fedora 44 Minimal base.
# This first Containerfile is intentionally conservative: overlay/RUM source
# packages are brought in after their dependency and build path is validated.

ARG RAKUKRISOS_RELEASE=0.1.0

LABEL org.opencontainers.image.title="RakuKrisOS"
LABEL org.opencontainers.image.version="${RAKUKRISOS_RELEASE}"

ENV container=oci

RUN dnf -y install \
        bootc \
        bootupd \
        composefs \
        dracut \
        ostree \
        systemd \
        flatpak \
    && dnf clean all \
    && rm -rf /var/cache/dnf

# KDE/Plasma and the initial native applications will be added once the
# RakuOS overlay core has been reduced and its Fedora 44 build is verified.

# bootc images use the normal Fedora filesystem layout and are finalized by
# bootc rather than by a separate OSTree compose step.
CMD ["/sbin/init"]
