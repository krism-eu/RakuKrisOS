#!/usr/bin/bash
set -Eeuo pipefail

root_was_symlink=0
root_target=""
initramfs_listing=""

cleanup() {
    if [[ -n "${initramfs_listing}" ]]; then
        rm -f -- "${initramfs_listing}"
    fi
    if [[ "${root_was_symlink}" -eq 1 ]]; then
        rm -rf -- /root
        ln -s -- "${root_target}" /root
    fi
}
trap cleanup EXIT

if [[ -L /root ]]; then
    root_was_symlink=1
    root_target="$(readlink /root)"
    rm -f -- /root
    install -d -m 0700 /root
elif [[ ! -d /root ]]; then
    echo "Unsupported /root type in bootc build" >&2
    exit 1
fi

found_kernel=0
for moddir in /usr/lib/modules/*; do
    [[ -d "${moddir}" ]] || continue
    kver="${moddir##*/}"
    [[ -e "${moddir}/vmlinuz" ]] || continue
    found_kernel=1

    dracut --force --no-hostonly "${moddir}/initramfs.img" "${kver}"

    # Avoid lsinitrd | grep -q under pipefail: grep intentionally closes the
    # pipe after a match, which makes lsinitrd exit with SIGPIPE (141).
    initramfs_listing="$(mktemp)"
    lsinitrd "${moddir}/initramfs.img" > "${initramfs_listing}"
    grep -Fq 'usr/lib/rakuos/rakuos-overlay-mount' "${initramfs_listing}"
    grep -Fq 'usr/lib/systemd/system/rakuos-overlay-mount.service' "${initramfs_listing}"
    grep -Fq 'initrd-root-fs.target.wants/rakuos-overlay-mount.service' "${initramfs_listing}"
    rm -f -- "${initramfs_listing}"
    initramfs_listing=""
done
[[ "${found_kernel}" -eq 1 ]]

rm -f -- /boot/initramfs-*.img
[[ -z "$(find /boot -mindepth 1 -maxdepth 1 -type f -print -quit 2>/dev/null)" ]]
