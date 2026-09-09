#!/usr/bin/env python3
"""Apply the RakuKrisOS first-boot overlay fix to pinned RakuOS source."""

from pathlib import Path


SOURCE = Path("/src/crates/initrd/src/bin/overlay_mount.rs")

OLD = """    // Upper dir empty — seed RPM db and exit for sync to install
    if dir_is_empty(&upper_dir) {
        log("seeding RPM db into upper dir before first install...");
        let seed_rpmdb = upper_dir.join("share/rpm");
        fs::create_dir_all(&seed_rpmdb)?;
        copy_dir_contents(&base_rpm_db, &seed_rpmdb)?;
        remove_rpmdb_locks(&seed_rpmdb);
        log("RPM db seeded — sync will handle install.");
        return Ok(());
    }
"""

NEW = """    // An empty upperdir is valid on a fresh Fedora Minimal deployment.
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
"""


def main() -> None:
    source = SOURCE.read_text(encoding="utf-8")
    occurrences = source.count(OLD)
    if occurrences != 1:
        raise SystemExit(
            f"expected exactly one upstream empty-upper block, found {occurrences}; "
            "refusing to patch changed source"
        )
    SOURCE.write_text(source.replace(OLD, NEW, 1), encoding="utf-8")


if __name__ == "__main__":
    main()
