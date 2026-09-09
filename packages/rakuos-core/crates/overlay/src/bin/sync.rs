use anyhow::Result;
use rakuos_overlay::*;
use std::fs;
use std::path::Path;
use std::process;

fn main() {
    if let Err(e) = run_sync() {
        eprintln!("RakuOS overlay sync: FATAL — {e:#}");
        process::exit(1);
    }
}

fn run_sync() -> Result<()> {
    // ── Live environment ───────────────────────────────────────────────────────
    if is_live_env() {
        println!("RakuOS overlay sync: live environment — skipping overlay sync.");
        plymsg("Live environment ready.");
        plymouth_quit();
        return Ok(());
    }

    // ── Load packages list ─────────────────────────────────────────────────────
    let packages_list = Path::new(PACKAGES_LIST);
    let packages = load_packages(packages_list);
    if packages.is_empty() {
        println!("RakuOS overlay sync: no packages in list — skipping.");
        return Ok(());
    }

    // ── Acquire lock ───────────────────────────────────────────────────────────
    let _lock = LockGuard::acquire(LOCK_FILE)?;

    // ── Ensure dirs exist ──────────────────────────────────────────────────────
    let upper = Path::new(UPPER_DIR);
    let work = Path::new(WORK_DIR);
    fs::create_dir_all(upper)?;
    fs::create_dir_all(work)?;
    fs::create_dir_all(RAKUOS_PKG_CACHE)?;

    // ── Get image digest ───────────────────────────────────────────────────────
    let current_digest = bootc_image_digest();
    if current_digest == "unknown" {
        println!("WARNING: Could not detect image digest — using unknown.");
    }

    let saved_digest = read_file_trimmed(Path::new(STATE_FILE));

    // ── Mount overlay if not already mounted ───────────────────────────────────
    if !usr_is_overlay_mounted() {
        println!("RakuOS overlay sync: mounting overlay...");
        let opts = format!(
            "lowerdir=/usr,upperdir={UPPER_DIR},workdir={WORK_DIR}"
        );
        run("mount", &["-t", "overlay", "overlay", "-o", &opts, "/usr"])?;
    }

    // ── Determine what needs doing ─────────────────────────────────────────────
    let overlay_has_content = !dir_is_empty(upper);
    let state_missing = saved_digest.is_empty();
    let dirty_file = Path::new(DIRTY_FILE);

    // ── Prebaked overlay path ──────────────────────────────────────────────────
    if saved_digest == "prebaked-installroot" && dirty_file.exists() {
        println!("RakuOS overlay sync: prebaked overlay detected — finalizing...");
        plymsg("Finalizing initial package overlay...");

        // factory_restore() already copied both the baked `/usr` payload
        // (cp -a from the --installroot build) and rum's own pre-baked
        // rum-rpmdb into place — the overlay's file contents and package
        // database are already fully populated, so there's nothing left
        // for `rum install` to do. Calling it here used to be needed
        // before the rpmdb was part of the factory bake, but now it just
        // sees these packages as already installed and routes them
        // through rum's reinstall-guard path instead of a plain install,
        // which isn't built for a local --cacheonly "package is already
        // there" case and fails/no-ops.

        install_local_rpms("sync");
        write_file(Path::new(STATE_FILE), &current_digest)?;
        fs::remove_file(dirty_file).ok();
        restore_sudo_pref();
        restore_uutils_pref();
        plymsg("Initial package overlay finalized.");
        println!("RakuOS overlay sync: prebaked overlay finalization complete.");

    } else if state_missing || !overlay_has_content {
        // ── Full install path (first boot / post-reset) ────────────────────────
        println!("RakuOS overlay sync: overlay empty or state missing — performing full install...");
        plymsg("Installing user applications...");

        if !internet_available() {
            println!("RakuOS overlay sync: no internet — deferring install.");
            plymsg("Connect to the internet, then reboot to finish installing default applications.");
            plymouth_quit();
            return Ok(());
        }

        plymsg("Installing user applications...");
        rum_install(&packages);
        wipe_rum_package_cache();
        install_local_rpms("sync");
        maybe_setup_ollama();
        restore_sudo_pref();
        restore_uutils_pref();
        write_file(Path::new(STATE_FILE), &current_digest)?;
        fs::remove_file(dirty_file).ok();
        fs::remove_file(Path::new("/var/lib/rakuos/soft-reset.marker")).ok();
        plymsg("User applications installed.");
        println!("RakuOS overlay sync: full install complete.");

    } else if dirty_file.exists() {
        // ── Dirty overlay reinstall ────────────────────────────────────────────
        println!("RakuOS overlay sync: package changes detected — reinstalling user applications...");
        plymsg("Reinstalling user applications...");

        if !internet_available() {
            println!("RakuOS overlay sync: no internet — deferring reinstall.");
            plymsg("Connect to the internet, then reboot to finish reinstalling applications.");
            plymouth_quit();
            return Ok(());
        }

        rum_install(&packages);
        wipe_rum_package_cache();
        install_local_rpms("sync");
        fs::remove_file(dirty_file).ok();
        plymsg("User applications reinstalled.");

        println!("RakuOS overlay sync: sync complete — rebooting.");
        plymouth_quit();
        run_best_effort("systemctl", &["reboot"]);
        return Ok(());

    } else {
        println!("RakuOS overlay sync: nothing to do.");
    }

    // ── Save digest and version stamp ──────────────────────────────────────────
    write_file(Path::new(STATE_FILE), &current_digest)?;

    if let Ok(content) = fs::read_to_string("/etc/os-release") {
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("VERSION_ID=") {
                let ver = val.trim_matches('"');
                write_file(Path::new(OS_VERSION_FILE), ver)?;
                break;
            }
        }
    }

    println!("RakuOS overlay sync: done.");
    plymouth_quit();
    Ok(())
}

fn load_packages(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            // Strip inline comments
            let l = if let Some(pos) = l.find('#') { l[..pos].trim() } else { l };
            if l.is_empty() { None } else { Some(l.to_string()) }
        })
        .collect()
}

/// Overlay-sync runs at boot, before the network is up — a soft/hard reset's
/// "reinstall everything" pass can't assume it'll ever get one, and even a
/// normal boot might race a slow link. `rum -C/--cacheonly install` resolves
/// against whatever repo metadata is already cached and only ever touches
/// RPMs already sitting in rum's package cache (see `rum-cli`'s `download`/
/// `clean` handling and `rakuos reset-overlay --soft`'s pre-download step,
/// which exists specifically to make this succeed offline) — so try that
/// first, and only fall back to a real networked `--refresh` install if
/// something genuinely isn't cached (a package whose version moved on
/// upstream, or a first boot with no cache at all).
fn rum_install(packages: &[String]) {
    let pkg_refs: Vec<&str> = packages.iter().map(String::as_str).collect();

    let mut cacheonly_args = vec!["install", "-y", "--cacheonly"];
    cacheonly_args.extend_from_slice(&pkg_refs);
    if run("rum", &cacheonly_args).is_ok() {
        return;
    }

    if !has_network_connection() {
        println!("RakuOS overlay sync: offline cache-only install incomplete and no network connection — skipping networked fallback.");
        return;
    }

    println!("RakuOS overlay sync: offline cache-only install incomplete — falling back to a networked install...");
    let mut refresh_args = vec!["install", "-y", "--refresh"];
    refresh_args.extend_from_slice(&pkg_refs);
    run_best_effort("rum", &refresh_args);
}

/// The downloaded-RPM cache under `/var/cache/rum` only exists to serve a
/// single offline reinstall (staged by `rakuos reset-overlay --soft`, see
/// `rum_install` above) — once that reinstall has actually run, whether it
/// used the cache or fell back to the network, there's nothing left that
/// needs it, so free the space rather than letting it sit around until the
/// next reset.
fn wipe_rum_package_cache() {
    run_best_effort("rum", &["clean", "packages"]);
}

fn maybe_setup_ollama() {
    if Path::new("/usr/local/bin/ollama").exists() {
        println!("RakuOS overlay sync: ollama detected — running setup-ollama...");
        run_best_effort("/usr/libexec/rakuos/setup-ollama", &[]);
        run_best_effort("systemctl", &["daemon-reload"]);
        run_best_effort("systemctl", &["restart", "ollama.service"]);
    }
}

fn restore_sudo_pref() {
    let f = Path::new("/var/lib/rakuos/user-sudo");
    if f.exists() {
        let choice = read_file_trimmed(f);
        println!("RakuOS overlay sync: restoring sudo preference: {choice}");
        run_best_effort("/usr/libexec/rakuos/setup-sudo", &[&choice]);
    }
}

fn restore_uutils_pref() {
    let f = Path::new("/var/lib/rakuos/user-uutils");
    if f.exists() {
        let choice = read_file_trimmed(f);
        println!("RakuOS overlay sync: restoring uutils preference: {choice}");
        run_best_effort("/usr/libexec/rakuos/setup-uutils", &[&choice]);
    }
}
