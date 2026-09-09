// RakuOS overlay initrd mount.
// Runs as a systemd unit inside the initrd after ostree-prepare-root.service
// has mapped /sysroot/usr to the deployment, before initrd-root-fs.target.

use std::fs;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};

// ── Path constants ────────────────────────────────────────────────────────────

const SYSROOT: &str = "/sysroot";

fn var() -> PathBuf {
    PathBuf::from(SYSROOT).join("ostree/deploy/default/var")
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn log(msg: &str) {
    println!("RakuOS overlay: {msg}");
}

fn read_file(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_default().trim().to_string()
}

fn write_file(p: &Path, s: &str) -> Result<()> {
    if let Some(parent) = p.parent() { fs::create_dir_all(parent)?; }
    fs::write(p, s).with_context(|| format!("write {}", p.display()))
}

fn touch(p: &Path) -> Result<()> {
    write_file(p, "")
}

fn copy_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let dst_path = dst.join(&name);
        // Use symlink_metadata so we don't follow symlinks when checking type
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            let target = fs::read_link(entry.path())?;
            let _ = fs::remove_file(&dst_path);
            std::os::unix::fs::symlink(&target, &dst_path)?;
        } else if file_type.is_dir() {
            copy_dir_contents(&entry.path(), &dst_path)?;
        } else {
            fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

fn remove_rpmdb_locks(dir: &Path) {
    for name in [".rpm.lock", ".keyring.lock", "__db.001", "__db.002", "__db.003"] {
        let _ = fs::remove_file(dir.join(name));
    }
    // .keyring.lock (and any other *.lock) can live under a nested keyring/
    // subdirectory rather than the rpmdb root — walk one level of subdirs too.
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Ok(sub) = fs::read_dir(&p) {
                    for sub_entry in sub.flatten() {
                        let name = sub_entry.file_name();
                        if name.to_string_lossy().ends_with(".lock") {
                            let _ = fs::remove_file(sub_entry.path());
                        }
                    }
                }
            }
        }
    }
}

fn is_overlay_mounted(target: &Path) -> bool {
    let Ok(content) = fs::read_to_string("/proc/mounts") else { return false };
    content.lines().any(|line| {
        let mut parts = line.splitn(4, ' ');
        let fstype_field = parts.next(); // device
        let mount_point  = parts.next();
        let fstype       = parts.next();
        let _ = fstype_field;
        mount_point.map(|m| m == target.to_string_lossy().as_ref()).unwrap_or(false)
            && fstype.map(|t| t == "overlay").unwrap_or(false)
    })
}

fn dir_is_empty(dir: &Path) -> bool {
    fs::read_dir(dir).map(|mut d| d.next().is_none()).unwrap_or(true)
}

// ── Migration from old bash overlay → soft reset ─────────────────────────────

fn check_legacy_migration(var: &Path) -> Result<()> {
    let rakuos_dir = var.join("lib/rakuos");
    let current_deploy = rakuos_dir.join("current-deploy");

    // Only trigger if the rakuos state dir exists (existing install) but
    // current-deploy is absent (old bash overlay never wrote it).
    if !rakuos_dir.exists() || current_deploy.exists() {
        return Ok(());
    }

    let soft_reset_marker = rakuos_dir.join("soft-reset.marker");
    if soft_reset_marker.exists() {
        return Ok(());
    }

    log("current-deploy not found on existing install — legacy bash overlay detected, scheduling soft reset for migration.");
    touch(&soft_reset_marker)?;
    Ok(())
}

// ── Migration to rum's split-mode overlay rpmdb → soft reset ────────────────

fn check_rum_migration(var: &Path) -> Result<()> {
    let rum_rpmdb = var.join("lib/rakuos/rum-rpmdb");
    let packages_list = var.join("lib/rakuos/packages.list");

    // Only trigger for an existing install (packages.list present, so it's
    // not a fresh first boot — sync's own first-install path already seeds
    // rum-rpmdb from scratch) that predates rum's split-mode rpmdb.
    if rum_rpmdb.exists() || !packages_list.exists() {
        return Ok(());
    }

    let soft_reset_marker = var.join("lib/rakuos/soft-reset.marker");
    if soft_reset_marker.exists() {
        return Ok(());
    }

    log("no rum-rpmdb found but packages.list exists — pre-rum install detected, scheduling soft reset to migrate.");
    touch(&soft_reset_marker)?;
    Ok(())
}

// ── OS version change → soft reset ───────────────────────────────────────────

fn check_os_version_change(var: &Path) -> Result<()> {
    let os_version_file = var.join("lib/rakuos/os-version");
    let os_release = PathBuf::from(SYSROOT).join("etc/os-release");

    if !os_version_file.exists() || !os_release.exists() {
        return Ok(());
    }

    let saved = read_file(&os_version_file);
    let release = read_file(&os_release);
    let current = release.lines()
        .find(|l| l.starts_with("VERSION_ID="))
        .and_then(|l| l.splitn(2, '=').nth(1))
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_default();

    if saved == "unknown" || current == "unknown" || saved.is_empty() || current.is_empty() {
        return Ok(());
    }

    if saved != current {
        log(&format!("version change detected ({saved} → {current}) — scheduling soft reset."));
        let dnf_cache = var.join("cache/libdnf5");
        if dnf_cache.exists() { let _ = fs::remove_dir_all(&dnf_cache); }
        touch(&var.join("lib/rakuos/soft-reset.marker"))?;
    }

    Ok(())
}

// ── DE change → soft reset ────────────────────────────────────────────────────

fn check_de_change(var: &Path) -> Result<()> {
    let image_de_file = PathBuf::from(SYSROOT).join("usr/share/rakuos/de-name");
    let saved_de_file = var.join("lib/rakuos/de-name");

    if !image_de_file.exists() { return Ok(()); }

    let current = read_file(&image_de_file);
    if current.is_empty() { return Ok(()); }

    if saved_de_file.exists() {
        let saved = read_file(&saved_de_file);
        if !saved.is_empty() && saved != current {
            log(&format!("DE change detected ({saved} → {current}) — scheduling soft reset to rebuild overlay from packages.list."));
            touch(&var.join("lib/rakuos/soft-reset.marker"))?;
        }
    }

    // Always update the saved DE name so the next boot has a baseline.
    write_file(&saved_de_file, &current)?;

    Ok(())
}

// ── Catastrophic whiteout check ───────────────────────────────────────────────

fn check_catastrophic_whiteouts(upper_dir: &Path) -> bool {
    // Opaque whiteout on the root hides the entire /usr
    if upper_dir.join(".wh..wh..opq").exists() {
        log("CRITICAL — opaque whiteout found in upper dir (entire /usr hidden).");
        return true;
    }
    for dir in ["bin", "lib", "lib64", "libexec", "sbin", "share"] {
        // Character device == whiteout
        let p = upper_dir.join(dir);
        if p.exists() {
            if let Ok(meta) = p.metadata() {
                use std::os::unix::fs::FileTypeExt;
                if meta.file_type().is_char_device() {
                    log(&format!("CRITICAL — whiteout (char dev) found for /usr/{dir}."));
                    return true;
                }
            }
        }
        let wh = upper_dir.join(format!(".wh.{dir}"));
        if wh.exists() {
            log(&format!("CRITICAL — whiteout (.wh.) found for /usr/{dir}."));
            return true;
        }
        if upper_dir.join(dir).join(".wh..wh..opq").exists() {
            log(&format!("CRITICAL — opaque whiteout in /usr/{dir}."));
            return true;
        }
    }
    false
}

// ── Preserved markers for resets ─────────────────────────────────────────────

const PRESERVE: &[&str] = &[
    "current-deploy", "os-version", "de-name", "user-sudo", "user-uutils",
];

fn save_markers(rakuos_dir: &Path) -> Vec<(String, String)> {
    PRESERVE.iter().filter_map(|name| {
        let p = rakuos_dir.join(name);
        fs::read_to_string(&p).ok().map(|v| (name.to_string(), v.trim().to_string()))
    }).collect()
}

fn restore_markers(rakuos_dir: &Path, saved: &[(String, String)]) {
    for (name, val) in saved {
        let p = rakuos_dir.join(name);
        let _ = write_file(&p, val);
    }
}

// ── Reset handling ────────────────────────────────────────────────────────────

fn handle_resets(
    var:           &Path,
    packages_list: &Path,
    dirty_file:    &Path,
) -> Result<bool> {
    let reset_marker      = var.join("lib/rakuos/overlay.reset");
    let soft_reset_marker = var.join("lib/rakuos/soft-reset.marker");
    let mut reset_queued  = false;

    if reset_marker.exists() {
        reset_queued = true;
        log("reset marker found — wiping all rakuos state...");
        let rakuos_dir = var.join("lib/rakuos");
        let saved = save_markers(&rakuos_dir);
        if rakuos_dir.exists() { fs::remove_dir_all(&rakuos_dir)?; }
        fs::create_dir_all(&rakuos_dir)?;
        let dnf_cache = var.join("cache/libdnf5");
        if dnf_cache.exists() { fs::remove_dir_all(&dnf_cache)?; }
        restore_markers(&rakuos_dir, &saved);
        let _ = fs::remove_file(&reset_marker);
        log("reset complete — seeding from factory to trigger sync finalization...");
        // Always restore from factory after a hard reset so overlay.state and
        // dirty_file are set correctly for sync to run the prebaked finalization.
        // Without this, sync has no markers to trigger the reinstall path.
        factory_restore(var, dirty_file)?;
    }

    if soft_reset_marker.exists() {
        reset_queued = true;
        log("soft reset marker found — wiping all rakuos state (preserving packages.list, local RPMs, and user markers)...");
        let rakuos_dir = var.join("lib/rakuos");
        let saved = save_markers(&rakuos_dir);
        // Also preserve packages.list and local-rpms dir contents
        let pkgs_list = fs::read_to_string(packages_list).unwrap_or_default();
        let local_rpm_list = fs::read_to_string(&rakuos_dir.join("packages-rpm.list")).unwrap_or_default();
        let local_rpms: Vec<(String, Vec<u8>)> = {
            let dir = rakuos_dir.join("local-rpms");
            if dir.is_dir() {
                fs::read_dir(&dir).into_iter().flatten()
                    .flatten()
                    .filter_map(|e| {
                        let data = fs::read(e.path()).ok()?;
                        Some((e.file_name().to_string_lossy().to_string(), data))
                    })
                    .collect()
            } else { vec![] }
        };
        if rakuos_dir.exists() { fs::remove_dir_all(&rakuos_dir)?; }
        fs::create_dir_all(&rakuos_dir)?;
        let dnf_cache = var.join("cache/libdnf5");
        if dnf_cache.exists() { fs::remove_dir_all(&dnf_cache)?; }
        restore_markers(&rakuos_dir, &saved);
        if !pkgs_list.is_empty() { write_file(packages_list, &pkgs_list)?; }
        if !local_rpm_list.is_empty() { write_file(&rakuos_dir.join("packages-rpm.list"), &local_rpm_list)?; }
        if !local_rpms.is_empty() {
            let local_rpms_dir = rakuos_dir.join("local-rpms");
            fs::create_dir_all(&local_rpms_dir)?;
            for (name, data) in local_rpms {
                let _ = fs::write(local_rpms_dir.join(name), data);
            }
        }
        let _ = fs::remove_file(&soft_reset_marker);

        // Explicitly arm the dirty-reinstall trigger, same as hard reset's
        // factory_restore does — don't rely solely on STATE_FILE happening to
        // be absent after the wipe above to make sync.rs reinstall packages.list.
        let _ = touch(dirty_file);

        // Write current-deploy so check_legacy_migration won't re-trigger next boot.
        let cmdline = read_file(Path::new("/proc/cmdline"));
        let current_deploy = cmdline.split_whitespace()
            .find(|t| t.starts_with("ostree="))
            .and_then(|t| t.strip_prefix("ostree="))
            .unwrap_or("")
            .to_string();
        if !current_deploy.is_empty() {
            let _ = write_file(&rakuos_dir.join("current-deploy"), &current_deploy);
        }

        log("soft reset complete — sync will reinstall packages.list on next boot.");
    }

    Ok(reset_queued)
}

// ── Image update: reseed the RPM db view from the new base ───────────────────
//
// rakuos-overlay-update is deprecated: rum's split-mode overlay rpmdb
// (`/var/lib/rakuos/rum-rpmdb`) tracks overlay packages independently of the
// base image's own `/usr/share/rpm`, so there's nothing left to merge or
// reconcile in userspace after a bootc update. This function no longer arms
// `image-update.marker` (nothing consumes it anymore) — its only remaining
// job is to stop the *overlay-mounted* `/usr/share/rpm` (and dnf5's
// packages.toml, still used by rakuos-install/rakuos-remove) from
// permanently shadowing the new base's copy with a stale one left over from
// before the deployment changed.

fn handle_image_update(
    var:               &Path,
    overlay_rpm_db:    &Path,
    overlay_pkgs_toml: &Path,
    base_rpm_db:       &Path,
    base_pkgs_toml:    &Path,
) -> Result<()> {
    // The caller already gates this on `!reset_queued` — a queued soft reset
    // always wins, it wipes and reinstalls everything from packages.list,
    // which already leaves both dbs fresh.
    if var.join("lib/rakuos/soft-reset.marker").exists() {
        return Ok(());
    }

    // Detect deployment change via /proc/cmdline (no bootc/jq needed in initrd)
    let cmdline = read_file(Path::new("/proc/cmdline"));
    let current_deploy = cmdline.split_whitespace()
        .find(|t| t.starts_with("ostree="))
        .and_then(|t| t.strip_prefix("ostree="))
        .unwrap_or("")
        .to_string();

    let deploy_state = var.join("lib/rakuos/current-deploy");
    let saved_deploy = read_file(&deploy_state);

    let deploy_changed = !current_deploy.is_empty()
        && !saved_deploy.is_empty()
        && current_deploy != saved_deploy;

    if !current_deploy.is_empty() {
        write_file(&deploy_state, &current_deploy)?;
    }

    if deploy_changed {
        log(&format!("deployment changed ({saved_deploy} → {current_deploy}) — reseeding /usr/share/rpm view from new base..."));
        fs::create_dir_all(overlay_rpm_db)?;
        copy_dir_contents(base_rpm_db, overlay_rpm_db)?;
        remove_rpmdb_locks(overlay_rpm_db);
        if let Some(parent) = overlay_pkgs_toml.parent() { fs::create_dir_all(parent)?; }
        if base_pkgs_toml.exists() {
            fs::copy(base_pkgs_toml, overlay_pkgs_toml)?;
        } else {
            fs::write(overlay_pkgs_toml, "")?;
        }
        log("reseed complete.");
    }

    Ok(())
}

// ── Factory restore ───────────────────────────────────────────────────────────

fn factory_restore(var: &Path, dirty_file: &Path) -> Result<()> {
    let deploy_root = {
        let cmdline = read_file(Path::new("/proc/cmdline"));
        cmdline.split_whitespace()
            .find(|t| t.starts_with("ostree="))
            .and_then(|t| t.strip_prefix("ostree="))
            .map(|p| PathBuf::from(SYSROOT).join(p.trim_start_matches('/')))
            .unwrap_or_else(|| PathBuf::from(SYSROOT).join("ostree/deploy/default/deploy"))
    };
    let factory_dir = deploy_root.join("usr/share/factory/var/lib/rakuos");
    if !factory_dir.exists() { return Ok(()); }

    log("restoring prebaked overlay state from factory...");
    let rakuos_dir = var.join("lib/rakuos");
    let saved = save_markers(&rakuos_dir);

    if rakuos_dir.exists() { fs::remove_dir_all(&rakuos_dir)?; }
    let var_cache = var.join("cache/libdnf5");
    if var_cache.exists() { fs::remove_dir_all(&var_cache)?; }

    fs::create_dir_all(var.join("lib"))?;
    copy_dir_contents(&factory_dir, &rakuos_dir)?;
    restore_markers(&rakuos_dir, &saved);

    let factory_cache = PathBuf::from(SYSROOT).join("usr/share/factory/var/cache/libdnf5");
    if factory_cache.exists() {
        copy_dir_contents(&factory_cache, &var_cache)?;
    }

    touch(dirty_file)?;
    log("prebaked state restored.");
    Ok(())
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn run() -> Result<()> {

    let var = var();
    if !var.is_dir() {
        log("var directory not found — not an installed system, skipping.");
        return Ok(());
    }

    // /var/lib/rpm should not exist — rpm's canonical db lives under
    // /usr/share/rpm (merged via the overlay), reached from /var/lib/rpm only
    // via a symlink some packaging leaves behind. A stray symlink (or worse, a
    // real leftover directory) here can shadow the overlay path and get read
    // instead of it. Remove it every boot; if nothing recreates it, this is a
    // no-op after the first boot.
    let var_lib_rpm = var.join("lib/rpm");
    if let Ok(meta) = fs::symlink_metadata(&var_lib_rpm) {
        if meta.file_type().is_symlink() {
            let _ = fs::remove_file(&var_lib_rpm);
            log("removed stray /var/lib/rpm symlink.");
        }
    }

    let upper_dir           = var.join("lib/rakuos/overlay/upper");
    let work_dir            = var.join("lib/rakuos/overlay/work");
    let packages_list       = var.join("lib/rakuos/packages.list");
    let dirty_file          = var.join("lib/rakuos/overlay.dirty");
    let overlay_rpm_db      = upper_dir.join("share/rpm");
    let overlay_pkgs_toml   = upper_dir.join("lib/sysimage/libdnf5/packages.toml");
    let base_rpm_db         = PathBuf::from(SYSROOT).join("usr/share/rpm");
    let base_pkgs_toml      = PathBuf::from(SYSROOT).join("usr/lib/sysimage/libdnf5/packages.toml");

    log("pre-pivot hook starting...");
    fs::create_dir_all(&upper_dir).context("create upper dir")?;
    fs::create_dir_all(&work_dir).context("create work dir")?;

    check_legacy_migration(&var)?;
    check_rum_migration(&var)?;
    check_os_version_change(&var)?;
    check_de_change(&var)?;

    if check_catastrophic_whiteouts(&upper_dir) {
        log("auto-recovering — scheduling soft reset to rebuild overlay from packages.list.");
        touch(&var.join("lib/rakuos/soft-reset.marker"))?;
    }

    let reset_queued = handle_resets(&var, &packages_list, &dirty_file)?;

    if !reset_queued {
        handle_image_update(
            &var,
            &overlay_rpm_db, &overlay_pkgs_toml,
            &base_rpm_db, &base_pkgs_toml,
        )?;
    }

    // Seed packages.list from factory if missing (first boot only —
    // post-reset already called factory_restore directly above).
    fs::create_dir_all(packages_list.parent().unwrap_or(Path::new("/")))?;
    if !packages_list.exists() {
        factory_restore(&var, &dirty_file)?;
    }

    // Already mounted?
    let usr_target = PathBuf::from(SYSROOT).join("usr");
    if is_overlay_mounted(&usr_target) {
        log("already mounted.");
        return Ok(());
    }

    // Upper dir empty — seed RPM db and exit for sync to install
    if dir_is_empty(&upper_dir) {
        log("seeding RPM db into upper dir before first install...");
        let seed_rpmdb = upper_dir.join("share/rpm");
        fs::create_dir_all(&seed_rpmdb)?;
        copy_dir_contents(&base_rpm_db, &seed_rpmdb)?;
        remove_rpmdb_locks(&seed_rpmdb);
        log("RPM db seeded — sync will handle install.");
        return Ok(());
    }

    // Mount
    log("mounting persistent overlay over /sysroot/usr...");
    let opts = format!(
        "lowerdir={SYSROOT}/usr,upperdir={},workdir={}",
        upper_dir.display(), work_dir.display()
    );
    nix::mount::mount(
        Some("overlay"),
        usr_target.as_path(),
        Some("overlay"),
        nix::mount::MsFlags::empty(),
        Some(opts.as_str()),
    ).context("mount overlay over /sysroot/usr")?;

    log("mounted.");
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("RakuOS overlay: FATAL: {e:#}");
        std::process::exit(1);
    }
}
