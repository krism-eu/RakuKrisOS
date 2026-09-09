#!/bin/bash
# Dracut module: 90rakuos-overlay
# Installs the RakuOS initrd Rust binary that mounts the overlayfs over /usr
# before switch-root.
#
# rakuos-load-extra-modules is intentionally not installed here anymore.
# It used to parse modprobe.d/modules-load.d straight out of the overlay
# upper at early boot so overlay-installed modules would autoload. rum's
# split-mode install/remove now copies those same package-shipped
# modprobe.d/modules-load.d files into the real /etc (see
# rum-transaction's sync_modprobe_configs), where the normal, non-initrd
# systemd-modules-load.service already picks them up on every boot with no
# initrd involvement at all — so this dracut module no longer has anything
# left to do.

check() {
    return 0
}

depends() {
    echo "ostree"
    return 0
}

install() {
    # Rust binary installed to /usr/lib/rakuos/initrd/ by the RPM.
    # inst_binary pulls in any shared lib deps; with the musl target they are
    # fully static so nothing extra is dragged in.
    inst_binary /usr/lib/rakuos/initrd/rakuos-overlay-mount        /usr/lib/rakuos/rakuos-overlay-mount

    inst_simple "$moddir/rakuos-overlay-mount.service" \
        "${systemdsystemunitdir}/rakuos-overlay-mount.service"

    mkdir -p "${initdir}${systemdsystemunitdir}/initrd-root-fs.target.wants"
    ln_r "${systemdsystemunitdir}/rakuos-overlay-mount.service" \
        "${systemdsystemunitdir}/initrd-root-fs.target.wants/rakuos-overlay-mount.service"

    # Runtime tools still needed by the Rust binary (mount) and for any
    # other initrd work.
    inst_multiple findmnt mount mkdir cp rm stat grep sort tail touch head dirname cat ls chmod || true
    inst_simple "${systemdsystemunitdir}/sysroot-ostree-deploy-default-var.mount" || true
}
