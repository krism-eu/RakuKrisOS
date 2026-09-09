use rakuos_overlay::*;
use std::fs;
use std::path::Path;
use std::process;

fn main() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("ERROR: rakuos reset-overlay must be run as root");
        process::exit(1);
    }

    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");

    match cmd {
        "--confirm" => hard_reset(),
        "--soft" => soft_reset(),
        _ => {
            eprintln!("WARNING: This will remove packages installed via rakuos install.");
            eprintln!("Your base system and /home will not be affected.");
            eprintln!();
            eprintln!("Options:");
            eprintln!("  --confirm   Hard reset: wipes overlay and packages.list completely.");
            eprintln!("  --soft      Soft reset: wipes overlay but preserves packages.list —");
            eprintln!("              all packages are reinstalled cleanly on next boot.");
            eprintln!();
            eprintln!("Usage: sudo rakuos reset-overlay [--confirm|--soft]");
            process::exit(1);
        }
    }
}

fn hard_reset() {
    let _ = fs::create_dir_all("/var/lib/rakuos");
    let _ = touch(Path::new(RESET_MARKER));
    wipe_dnf_cache();
    wipe_local_rpm_cache();
    println!();
    println!("RakuOS: overlay reset scheduled.");
    println!("Please reboot — the overlay will be wiped cleanly on next boot.");
}

fn soft_reset() {
    let _ = fs::create_dir_all("/var/lib/rakuos");
    let _ = touch(Path::new(SOFT_RESET_MARKER));
    // Pre-clean everything the initrd mount script will also wipe — belt and suspenders
    // so stale state can't cause DM lockups even if the initrd path has issues.
    let _ = fs::remove_file(STATE_FILE);
    let _ = fs::remove_file(DIRTY_FILE);
    let _ = fs::remove_file(IMAGE_UPDATE_MARKER);
    let _ = fs::remove_file("/var/lib/rakuos/pkgcache.migrated");
    wipe_dnf_cache();
    cache_overlay_packages_for_reinstall();
    wipe_rum_state();
    println!();
    println!("RakuOS: soft reset scheduled.");
    println!("Please reboot — the overlay will be wiped and packages reinstalled on next boot.");
}

/// Pre-populates rum's repo package cache with every package currently
/// tracked in the overlay rpmdb, before `wipe_rum_state` below deletes that
/// rpmdb. `rum`'s downloader already skips re-fetching a package whose exact
/// NEVRA is already sitting at its expected cache path (see
/// `rum-transaction::download_all_ex_with_metadata_root`), so as long as
/// this cache survives the reset (`wipe_rum_state` only clears rum's repo
/// *metadata* cache, not downloaded packages — see below), next boot's
/// overlay-sync `rum install` run transparently reuses these RPMs instead of
/// re-downloading everything from the network. Anything whose upstream
/// version has since moved on simply won't match the cached filename, so
/// `rum install` falls back to a normal repo download for that one package —
/// this needs no special-casing on the install side, it's just how the
/// existing cache-skip check behaves.
fn cache_overlay_packages_for_reinstall() {
    let names = run_capture_ok("rpm", &["--dbpath", RUM_RPMDB, "-qa", "--qf", "%{NAME}\n"]);
    let names: Vec<&str> = names.lines().map(str::trim).filter(|n| !n.is_empty()).collect();
    if names.is_empty() {
        return;
    }
    println!("RakuOS: caching {} overlay package(s) for offline reinstall...", names.len());
    let mut args: Vec<&str> = vec!["download", "--refresh"];
    args.extend(names.iter().copied());
    run_best_effort("rum", &args);
}

fn wipe_dnf_cache() {
    let cache = Path::new("/var/cache/libdnf5");
    if cache.exists() {
        for entry in fs::read_dir(cache).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                let _ = fs::remove_dir_all(&p);
            } else {
                let _ = fs::remove_file(&p);
            }
        }
    }
}

/// Resets rum's own tracked state — its split-mode overlay rpmdb and
/// install-reason/history log — so a soft reset starts rum from a
/// completely clean slate rather than replaying stale reasons.json/history
/// against packages that no longer exist after the wipe above. Deliberately
/// does NOT touch rum's repo cache (metadata or downloaded packages) —
/// `cache_overlay_packages_for_reinstall` just staged/refreshed both above
/// specifically so next boot's overlay-sync can do a fully offline
/// `--cacheonly` reinstall with no repo refresh needed.
fn wipe_rum_state() {
    let _ = fs::remove_dir_all(RUM_RPMDB);
    let _ = fs::remove_dir_all("/var/lib/rakuos/rum-state");
}

fn wipe_local_rpm_cache() {
    let cache = Path::new(LOCAL_RPM_CACHE);
    if cache.exists() {
        for entry in fs::read_dir(cache).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                let _ = fs::remove_dir_all(&p);
            } else {
                let _ = fs::remove_file(&p);
            }
        }
    }
    let _ = fs::remove_file(LOCAL_RPM_LIST);
}
