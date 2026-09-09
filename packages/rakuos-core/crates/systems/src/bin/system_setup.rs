/// rakuos-system-setup — first-boot / watcher-triggered system fixups.
///
/// Modes:
///   boot            — run once at boot via rakuos-setup.service
///   watcher <id> <action> — run per-app fixups triggered by flatpak-event-watcher
use anyhow::Result;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

const FIX_DIR: &str = "/usr/libexec/rakuos/flatpak-fixes";

const HOME_SEED_MARKER: &str = "/var/lib/rakuos/install-user-home-fixed";
const UUTILS_MARKER: &str = "/var/lib/rakuos/user-uutils";

const FISH_BIN: &str = "/usr/bin/fish";
const FISH_CONFIG: &str = "/etc/fish/conf.d/rakuos-aliases.fish";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode_arg = args.get(1).map(String::as_str).unwrap_or("boot");
    let appid = args.get(2).cloned().unwrap_or_default();
    let action = args.get(3).cloned().unwrap_or_else(|| "unknown".to_string());

    let mode = match mode_arg {
        "boot" | "watcher" => mode_arg,
        other => {
            println!("Unknown mode '{other}', defaulting to boot");
            "boot"
        }
    };

    fs::create_dir_all("/var/lib/rakuos")?;

    if mode == "boot" {
        boot_fixes();
    }

    if mode == "watcher" && !appid.is_empty() {
        watcher_fix(&appid, &action);
    }

    Ok(())
}

fn boot_fixes() {
    fix_etc_permissions();
    fix_sudoers_permissions();
    fix_var_lib_rpm_symlink();
    clean_stale_kernel_module_trees();
    clean_stale_dkms_symlinks();
    migrate_users_to_fish();
    seed_install_user_home();
    ensure_mok_dir();
    setup_uutils();
}

// ── Remove a stray /var/lib/rpm symlink ─────────────────────────────────────
// rpm's canonical db lives under /usr/share/rpm (merged via the overlay);
// /var/lib/rpm should not exist. A stray symlink there (left behind by some
// packaging, or recreated by systemd-tmpfiles after the dracut mount stage
// already removed it once this boot) can shadow the overlay path and get read
// instead of it. Belt-and-suspenders alongside the removal in
// crates/initrd/src/bin/overlay_mount.rs, since this runs later in boot.
fn fix_var_lib_rpm_symlink() {
    let path = Path::new("/var/lib/rpm");
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            let _ = fs::remove_file(path);
            println!("Removed stray /var/lib/rpm symlink.");
        }
    }
}

// ── Clean stale kernel module trees from the overlay upper dir ─────────────
// User-triggered akmod/DKMS builds write into the overlay's upper layer at
// /var/lib/rakuos/overlay/upper/lib/modules/<kver>/. Old kernel trees pile up
// there across upgrades since nothing else prunes them. Never touch the
// currently running kernel's tree.
fn clean_stale_kernel_module_trees() {
    let modules_overlay_dir = Path::new("/var/lib/rakuos/overlay/upper/lib/modules");
    if !modules_overlay_dir.is_dir() {
        return;
    }
    let running_kver = kernel_version();
    if running_kver.is_empty() {
        return;
    }

    for entry in fs::read_dir(modules_overlay_dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let kver = entry.file_name();
        if kver.to_string_lossy() == running_kver {
            continue;
        }
        println!("Removing stale kernel module tree: {}", kver.to_string_lossy());
        let _ = fs::remove_dir_all(&path);
    }
}

// ── Remove DKMS symlinks and build trees left over from a previous kernel ──
// Real on-disk layout (verified against a live /var/lib/dkms):
//   /var/lib/dkms/<module>/kernel-<kver>-<arch>            — symlink, directly
//                                                             under the module dir
//   /var/lib/dkms/<module>/<version>/<kver>/<arch>/        — the actual build
//                                                             tree (log/, module/)
// The symlink and the build tree are two separate levels — a version older
// implementation assumed the symlink lived *inside* the version dir, so it
// never matched either: `read_dir(version_dir)` only ever yields the real
// per-kernel directories (never symlinks, so the is_symlink() filter always
// skipped them), and the actual space-consuming module trees were never
// touched at all. Handle both here: prune stale top-level symlinks, and
// prune stale per-kernel directories under each version dir.
// A DKMS symlink is named "kernel-<kver>-<arch>". Extract the <kver> part by
// stripping the "kernel-" prefix and the trailing "-<arch>" segment, instead
// of reconstructing the expected symlink name from the running kernel string
// and comparing prefixes — that reconstruction is only as good as our
// assumption about how the arch suffix is appended, and a single wrong
// assumption there makes every real symlink look stale.
fn dkms_symlink_kver(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("kernel-")?;
    let (kver, _arch) = rest.rsplit_once('-')?;
    Some(kver)
}

fn clean_stale_dkms_symlinks() {
    let dkms_dir = Path::new("/var/lib/dkms");
    if !dkms_dir.is_dir() {
        return;
    }
    let current_kernel = kernel_version();
    if current_kernel.is_empty() {
        return;
    }

    for module in fs::read_dir(dkms_dir).into_iter().flatten().flatten() {
        if !module.path().is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(module.path()) else { continue };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else { continue };
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if file_type.is_symlink() {
                let is_current = dkms_symlink_kver(&name_str) == Some(current_kernel.as_str());
                if !is_current {
                    let path = entry.path();
                    let _ = fs::remove_file(&path);
                    println!("Removed stale DKMS symlink for old kernel: {}", path.display());
                }
                continue;
            }

            if !file_type.is_dir() {
                continue;
            }

            // Version dir (e.g. <module>/0.10.4/) — descend to the per-kernel trees.
            let Ok(kver_entries) = fs::read_dir(entry.path()) else { continue };
            for kver_entry in kver_entries.flatten() {
                let Ok(kver_type) = kver_entry.file_type() else { continue };
                if !kver_type.is_dir() {
                    continue;
                }
                if kver_entry.file_name().to_string_lossy() == current_kernel {
                    continue;
                }
                let path = kver_entry.path();
                println!("Removing stale DKMS module tree for old kernel: {}", path.display());
                let _ = fs::remove_dir_all(&path);
            }
        }
    }
}

fn kernel_version() -> String {
    Command::new("uname")
        .arg("-r")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

// ── Ensure /etc has correct ownership and permissions ──────────────────────
// bootc switches and rebases can leave /etc world-writable, allowing any
// user to modify config files without root.
fn fix_etc_permissions() {
    run_best_effort("chown", &["root:root", "/etc"]);
    run_best_effort("chmod", &["0755", "/etc"]);

    // Strip world-write from regular files directly in /etc
    if let Ok(entries) = fs::read_dir("/etc") {
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            if meta.mode() & 0o002 != 0 {
                let mut perms = meta.permissions();
                perms.set_mode(meta.mode() & !0o002);
                let _ = fs::set_permissions(&path, perms);
            }
        }
    }
}

// ── Ensure sudoers.d has correct permissions so sudo works ─────────────────
//
// /etc/sudoers.d is stateful config (ostree/bootc 3-way-merges it, it isn't
// replaced wholesale on image updates), so a file that gets corrupted on the
// live system — e.g. truncated/missing its trailing newline, as happened in
// the field — stays broken across reboots and image updates, and breaks sudo
// entirely since a single malformed file fails the whole sudoers parse.
// /usr/etc holds the pristine copy shipped in the currently booted image
// (ostree's merge source). For each file we ship there, if the live /etc
// copy doesn't byte-for-byte match, restore it from the image — this is
// the only way to recover from live corruption (e.g. a truncated/missing
// trailing newline, as happened in the field), since a stateful file that's
// already drifted won't be fixed by a future image update on its own.
fn fix_sudoers_permissions() {
    let sudoers_d = Path::new("/etc/sudoers.d");
    if !sudoers_d.is_dir() {
        return;
    }

    // /etc/sudoers.d/rakuos-software was renamed to /etc/sudoers.d/rakuos (it
    // now covers more than just the software center). Since sudoers.d is
    // stateful, systems that booted an older image before the rename still
    // carry the old file — remove it so it doesn't linger as a stale,
    // unmanaged duplicate alongside the new one.
    let stale_rakuos_software = sudoers_d.join("rakuos-software");
    if stale_rakuos_software.is_file() {
        println!("RakuOS: removing stale /etc/sudoers.d/rakuos-software (renamed to rakuos)");
        if let Err(e) = fs::remove_file(&stale_rakuos_software) {
            eprintln!("RakuOS: WARNING: failed to remove stale rakuos-software sudoers file: {e}");
        }
    }

    run_best_effort("chown", &["root:root", "/etc/sudoers.d"]);
    run_best_effort("chmod", &["0750", "/etc/sudoers.d"]);

    let image_sudoers_d = Path::new("/usr/etc/sudoers.d");

    if let Ok(entries) = fs::read_dir(image_sudoers_d) {
        for entry in entries.flatten() {
            let image_copy = entry.path();
            if !image_copy.is_file() {
                continue;
            }
            let Some(name) = image_copy.file_name() else { continue };
            let live_path = sudoers_d.join(name);

            let matches = fs::read(&live_path).ok()
                .is_some_and(|live| fs::read(&image_copy).map(|img| img == live).unwrap_or(false));
            if !matches {
                let Some(live_path_str) = live_path.to_str() else { continue };
                println!("RakuOS: {live_path_str} differs from the shipped image copy, restoring");
                if let Err(e) = fs::copy(&image_copy, &live_path) {
                    eprintln!("RakuOS: WARNING: failed to restore {live_path_str}: {e}");
                }
            }
        }
    }

    if let Ok(entries) = fs::read_dir(sudoers_d) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(path_str) = path.to_str() else { continue };
            run_best_effort("chown", &["root:root", path_str]);
            run_best_effort("chmod", &["0440", path_str]);
        }
    }
}

// ── Migrate real users to fish shell if rakuos fish config is present ──────
fn migrate_users_to_fish() {
    if !Path::new(FISH_CONFIG).is_file() || !Path::new(FISH_BIN).exists() {
        return;
    }

    let Ok(passwd) = fs::read_to_string("/etc/passwd") else { return };
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 7 {
            continue;
        }
        let uname = fields[0];
        let uid: i64 = fields[2].parse().unwrap_or(-1);
        let uhome = fields[5];
        let ushell = fields[6];

        if !(1000..=60000).contains(&uid) {
            continue;
        }
        if ushell == FISH_BIN {
            continue;
        }
        if Path::new(&format!("{uhome}/.config/rakuos/keep-shell")).is_file() {
            continue;
        }

        println!("Migrating {uname} shell to fish...");
        run_best_effort("usermod", &["-s", FISH_BIN, uname]);
    }
}

fn seed_install_user_home() {
    if Path::new(HOME_SEED_MARKER).exists() {
        return;
    }

    let output = Command::new("getent").args(["passwd", "1000"]).output();
    let entry = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    };

    if entry.is_empty() {
        println!("No UID 1000 install user found yet; deferring home directory check.");
        return;
    }

    let fields: Vec<&str> = entry.split(':').collect();
    if fields.len() < 6 {
        return;
    }
    let install_user_name = fields[0];
    let install_user_uid = fields[2];
    let install_user_gid = fields[3];
    let install_user_home = fields[5];

    if !install_user_home.is_empty() && !Path::new(install_user_home).is_dir() {
        println!("Creating missing home directory for {install_user_name} at {install_user_home}");
        let _ = fs::create_dir_all(install_user_home);
        run_best_effort(
            "chown",
            &[&format!("{install_user_uid}:{install_user_gid}"), install_user_home],
        );
        run_best_effort("chmod", &["700", install_user_home]);
    }

    let _ = fs::write(HOME_SEED_MARKER, "");
}

// ── Ensure per-machine MOK dir exists ───────────────────────────────────────
// Keys are generated by the installer; this just ensures the dir is present
// so akmods/DKMS don't fail if they check the path before the installer runs.
fn ensure_mok_dir() {
    let dir = "/var/lib/rakuos-user-MOK";
    let _ = fs::create_dir_all(dir);
    run_best_effort("chmod", &["755", dir]);
}

// ── Set up uutils as default coreutils (first boot only) ───────────────────
fn setup_uutils() {
    if Path::new(UUTILS_MARKER).exists() {
        return;
    }
    println!("Setting up uutils-coreutils as default...");
    if which("rakuos").is_some() {
        run_best_effort("rakuos", &["setup-uutils", "uutils"]);
    } else {
        println!("WARNING: rakuos command not found; skipping uutils setup");
    }
}

fn watcher_fix(appid: &str, action: &str) {
    let fix_script = Path::new(FIX_DIR).join(format!("{appid}.sh"));
    if is_executable(&fix_script) {
        println!("Running fix module for {appid}");
        let _ = Command::new(&fix_script).arg(action).status();
    } else {
        println!("No fix module for {appid}");
    }
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn which(bin: &str) -> Option<()> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(bin))
        .find(|p| p.is_file())
        .map(|_| ())
}

fn run_best_effort(cmd: &str, args: &[&str]) {
    let _ = Command::new(cmd).args(args).status();
}
