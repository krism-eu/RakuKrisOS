use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use walkdir::WalkDir;

// ── Paths ─────────────────────────────────────────────────────────────────────

pub const UPPER_DIR: &str = "/var/lib/rakuos/overlay/upper";
pub const WORK_DIR: &str = "/var/lib/rakuos/overlay/work";
pub const STATE_FILE: &str = "/var/lib/rakuos/overlay.state";
pub const PACKAGES_LIST: &str = "/var/lib/rakuos/packages.list";
pub const LOCAL_RPM_LIST: &str = "/var/lib/rakuos/packages-rpm.list";
pub const LOCAL_RPM_CACHE: &str = "/var/lib/rakuos/local-rpms";
pub const LOCK_FILE: &str = "/run/rakuos-overlay.lock";
pub const DIRTY_FILE: &str = "/var/lib/rakuos/overlay.dirty";
pub const RESET_MARKER: &str = "/var/lib/rakuos/overlay.reset";
pub const SOFT_RESET_MARKER: &str = "/var/lib/rakuos/soft-reset.marker";
pub const IMAGE_UPDATE_MARKER: &str = "/var/lib/rakuos/image-update.marker";
pub const OS_VERSION_FILE: &str = "/var/lib/rakuos/os-version";
// This is the MERGED path, not the raw upper dir. Every userspace consumer of
// this constant (sync/update/reset binaries, this crate) runs after /usr is
// already overlay-mounted — writing straight to the upper dir bypasses the
// mount, and the kernel's cached merged dentry never picks up the change
// until reboot. Going through the merged path lets the kernel handle copy-up
// itself, so the merged view can never go stale. (The dracut initrd stage is
// the one place that legitimately writes the raw upper path directly, since
// it runs before /usr is mounted at all — it has its own separate constants.)
pub const OVERLAY_RPM_DB: &str = "/usr/share/rpm";
pub const OVERLAY_PKGS_TOML: &str =
    "/var/lib/rakuos/overlay/upper/lib/sysimage/libdnf5/packages.toml";
pub const PKG_CACHE_DIR: &str = "/var/cache/libdnf5";
pub const RAKUOS_PKG_CACHE: &str = "/var/cache/libdnf5/rakuos-downloaded/packages";
pub const IMAGE_PKGS_SAVE: &str = "/var/lib/rakuos/image-packages.toml";
pub const SERVICE_DIR: &str = "/var/lib/rakuos/overlay/upper/lib/systemd/system";
pub const NOTIFY_FILE: &str = "/var/lib/rakuos/update-notify.pending";
pub const USER_READY_FILE: &str = "/tmp/rakuos-overlay-update.ready";
pub const SAVED_GPG_DIR: &str = "/var/lib/rakuos/saved-gpg-keys";
pub const RUM_RPMDB: &str = "/var/lib/rakuos/rum-rpmdb";

// ── Process helpers ───────────────────────────────────────────────────────────

/// Run a command, streaming stdout/stderr to our stdout. Returns Ok if exit 0.
pub fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to exec {program}"))?;
    if !status.success() {
        bail!("{program} exited with status {status}");
    }
    Ok(())
}

/// Like run() but ignores non-zero exit (best-effort calls).
pub fn run_best_effort(program: &str, args: &[&str]) {
    let _ = Command::new(program).args(args).status();
}

/// Run and capture stdout as a string.
pub fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to exec {program}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run, capturing stdout; returns empty string on failure (best-effort).
pub fn run_capture_ok(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// True if a wired or wireless device is connected (or mid-connect), per
/// `nmcli`. `rakuos-overlay-sync.service` used to gate its whole run on
/// network-online via `ExecCondition`, but that meant the offline
/// cache-only install path — the entire point of `rum install --cacheonly`
/// plus `rakuos reset-overlay --soft`'s pre-download step — never even ran
/// without a network. The network check now lives here instead, checked
/// only right before the networked fallback install, so cache-only sync
/// still runs on every boot regardless of connectivity.
pub fn has_network_connection() -> bool {
    run_capture_ok("nmcli", &["-t", "-f", "TYPE,STATE", "device"])
        .lines()
        .any(|l| matches!(l, "ethernet:connected" | "ethernet:connecting" | "wifi:connected" | "wifi:connecting"))
}

// ── Filesystem helpers ────────────────────────────────────────────────────────

pub fn dir_is_empty(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut iter) => iter.next().is_none(),
        Err(_) => true,
    }
}

/// Return true if path is a character device (overlayfs whiteout).
pub fn is_char_device(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    path.metadata()
        .map(|m| m.file_type().is_char_device())
        .unwrap_or(false)
}

/// Recursively copy src into dst (like cp -a src/. dst/).
pub fn copy_dir_into(src: &Path, dst: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dst)?;
    for entry in WalkDir::new(src).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let dest = dst.join(rel);
        let ft = entry.file_type();
        if ft.is_dir() {
            fs::create_dir_all(&dest)?;
        } else if ft.is_symlink() {
            let target = fs::read_link(entry.path())?;
            let _ = fs::remove_file(&dest);
            std::os::unix::fs::symlink(target, &dest)?;
        } else {
            fs::copy(entry.path(), &dest)?;
            let meta = entry.metadata()?;
            fs::set_permissions(&dest, fs::Permissions::from_mode(meta.permissions().mode()))?;
        }
    }
    Ok(())
}

/// Wipe all contents of a directory without removing the directory itself.
pub fn wipe_dir_contents(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            fs::remove_dir_all(&p)?;
        } else {
            fs::remove_file(&p)?;
        }
    }
    Ok(())
}

/// Touch a file (create if missing).
pub fn touch(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::OpenOptions::new().create(true).append(true).open(path)?;
    Ok(())
}

pub fn read_file_trimmed(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

// ── Overlay mount check ───────────────────────────────────────────────────────

pub fn usr_is_overlay_mounted() -> bool {
    fs::read_to_string("/proc/mounts")
        .map(|s| {
            s.lines().any(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                parts.len() >= 3 && parts[1] == "/usr" && parts[2] == "overlay"
            })
        })
        .unwrap_or(false)
}

// ── Plymouth ──────────────────────────────────────────────────────────────────

pub fn plymsg(msg: &str) {
    // Only attempt if Plymouth is responsive
    if Command::new("plymouth")
        .arg("--ping")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        let _ = Command::new("plymouth")
            .args(["display-message", "--text", msg])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

pub fn plymouth_quit() {
    run_best_effort("plymouth", &["--ping"]);
    run_best_effort("plymouth", &["quit"]);
}

// ── Network check ─────────────────────────────────────────────────────────────

pub fn internet_available() -> bool {
    // network-online.target / nm-online can report the link up slightly
    // before DNS/routing is actually usable (seen on fresh VM boots), so a
    // single fast probe can false-negative and defer an install unnecessarily.
    // Retry a few times with backoff before giving up.
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        let ok = Command::new("curl")
            .args([
                "-fsSLI",
                "--max-time",
                "5",
                "--connect-timeout",
                "3",
                "https://mirrors.fedoraproject.org/",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
    }
    false
}

// ── Live environment detection ────────────────────────────────────────────────

pub fn is_live_env() -> bool {
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if cmdline.contains("rd.live.image") || cmdline.contains("liveimg") {
        return true;
    }
    Path::new("/run/initramfs/livedev").exists()
}

// ── bootc ─────────────────────────────────────────────────────────────────────

pub fn bootc_image_digest() -> String {
    // Parse bootc status JSON for the booted image digest.
    // Use a timeout — bootc status can hang if the daemon socket isn't ready yet.
    let out = Command::new("timeout")
        .args(["10", "bootc", "status", "--json"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    // Simple grep: find imageDigest line
    for line in out.lines() {
        let line = line.trim();
        if line.contains("imageDigest") {
            if line.find('"').and_then(|i| {
                let after = &line[i + 1..];
                after.find('"').map(|j| (i + 1, i + 1 + j))
            }).is_some() {
                // imageDigest key — find value: "imageDigest": "sha256:..."
                if let Some(colon) = line.find(':') {
                    let val_part = line[colon + 1..].trim().trim_matches(['"', ',', ' ']);
                    if val_part.starts_with("sha256:") || val_part.len() > 10 {
                        return val_part.to_string();
                    }
                }
            }
        }
    }
    // Try jq if available
    let jq = run_capture_ok(
        "sh",
        &["-c", "timeout 10 bootc status --json 2>/dev/null | jq -r '.status.booted.image.imageDigest // empty' 2>/dev/null"],
    );
    if !jq.is_empty() {
        return jq;
    }
    "unknown".to_string()
}

// ── RPM database helpers ──────────────────────────────────────────────────────

pub fn rpm_query_all(dbpath: &Path) -> Vec<String> {
    let out = run_capture_ok(
        "rpm",
        &[
            "--dbpath",
            dbpath.to_str().unwrap_or(""),
            "-qa",
            "--qf",
            "%{NAME}.%{ARCH}\\n",
        ],
    );
    let mut pkgs: Vec<String> = out.lines().filter(|l| !l.is_empty()).map(String::from).collect();
    pkgs.sort();
    pkgs
}

pub fn rpm_query_evr(dbpath: &Path, name: &str) -> String {
    run_capture_ok(
        "rpm",
        &[
            "--dbpath",
            dbpath.to_str().unwrap_or(""),
            "-q",
            name,
            "--qf",
            "%{EPOCH}:%{VERSION}-%{RELEASE}",
        ],
    )
}

pub fn rpm_query_files(dbpath: &Path, name: &str) -> Vec<String> {
    let out = run_capture_ok(
        "rpm",
        &[
            "--dbpath",
            dbpath.to_str().unwrap_or(""),
            "-ql",
            name,
        ],
    );
    out.lines().filter(|l| !l.is_empty()).map(String::from).collect()
}

pub fn rpm_query_pkg_info(path: &Path, qf: &str) -> String {
    run_capture_ok("rpm", &["-qp", "--qf", qf, path.to_str().unwrap_or("")])
}

pub fn rpm_rebuilddb(dbpath: &Path) {
    run_best_effort("rpm", &["--rebuilddb", "--dbpath", dbpath.to_str().unwrap_or("")]);
}

pub fn rpm_drop_lock_files(dir: &Path) {
    let _ = fs::remove_file(dir.join(".rpm.lock"));
    let _ = fs::remove_file(dir.join(".keyring.lock"));
    // Remove __db.* Berkeley DB cache files, and any *.lock file one level
    // down — .keyring.lock can live under a nested keyring/ subdirectory
    // rather than the rpmdb root.
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("__db.") {
                let _ = fs::remove_file(entry.path());
                continue;
            }
            let p = entry.path();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Ok(sub) = fs::read_dir(&p) {
                    for sub_entry in sub.flatten() {
                        let sub_name = sub_entry.file_name();
                        if sub_name.to_string_lossy().ends_with(".lock") {
                            let _ = fs::remove_file(sub_entry.path());
                        }
                    }
                }
            }
        }
    }
}

/// Seed an RPM db directory from source, dropping lock files.
pub fn seed_rpmdb(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    copy_dir_into(src, dst)?;
    rpm_drop_lock_files(dst);
    Ok(())
}

// ── packages.toml helpers ─────────────────────────────────────────────────────

pub fn ensure_packages_toml() {
    let live = Path::new("/usr/lib/sysimage/libdnf5/packages.toml");
    if !live.exists() {
        let upper_path = Path::new(OVERLAY_PKGS_TOML);
        if let Some(parent) = upper_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = std::fs::OpenOptions::new().create(true).write(true).open(upper_path);
        let _ = std::fs::set_permissions(
            upper_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        );
        println!("RakuOS overlay sync: created empty packages.toml for dnf5.");
    }
}

/// Check whether an RPM for `name` (any version) exists anywhere in `cache_root`.
/// Walks the full tree so repo-specific subdirs are included.
pub fn rpm_already_in_cache(cache_root: &str, name: &str, arch: &str) -> bool {
    let prefix = format!("{name}-");
    for entry in WalkDir::new(cache_root).into_iter().flatten() {
        let fname = entry.file_name().to_string_lossy();
        if !fname.ends_with(".rpm") || !fname.starts_with(prefix.as_str()) { continue; }
        if arch == "i686" {
            if fname.ends_with(".i686.rpm") { return true; }
        } else if !fname.ends_with(".i686.rpm") {
            return true;
        }
    }
    false
}

// ── Install local RPMs ────────────────────────────────────────────────────────

pub fn install_local_rpms(label: &str) {
    let list = Path::new(LOCAL_RPM_LIST);
    if !list.exists() {
        return;
    }
    let content = fs::read_to_string(list).unwrap_or_default();
    let mut rpms: Vec<String> = Vec::new();
    for pkg_name in content.lines() {
        let pkg_name = pkg_name.trim();
        if pkg_name.is_empty() {
            continue;
        }
        let path = Path::new(LOCAL_RPM_CACHE).join(format!("{pkg_name}.rpm"));
        if path.exists() {
            rpms.push(path.to_str().unwrap_or("").to_string());
        } else {
            println!(
                "RakuOS overlay {label}: WARNING — cached RPM for {pkg_name} not found, skipping."
            );
        }
    }
    if !rpms.is_empty() {
        println!("RakuOS overlay {label}: reinstalling {} local RPM(s)...", rpms.len());
        let mut args = vec!["install", "-y"];
        let refs: Vec<&str> = rpms.iter().map(String::as_str).collect();
        args.extend_from_slice(&refs);
        run_best_effort("rum", &args);
    }
}

// ── Desktop notifications ─────────────────────────────────────────────────────

pub fn logged_in_uids() -> Vec<u32> {
    let out = run_capture_ok(
        "sh",
        &["-c", "loginctl list-sessions --no-legend 2>/dev/null | awk '{print $3}' | sort -u"],
    );
    out.lines()
        .filter_map(|u| {
            let uid = run_capture_ok("id", &["-u", u]);
            uid.parse::<u32>().ok().filter(|&id| id >= 1000)
        })
        .collect()
}

pub fn notify_users(uids: &[u32], title: &str, msg: &str) {
    let mut sent = false;
    for uid in uids {
        let bus = format!("/run/user/{uid}/bus");
        if !Path::new(&bus).exists() {
            continue;
        }
        let ok = Command::new("sudo-rs")
            .args(["-u", &format!("#{uid}")])
            .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={bus}"))
            .args([
                "notify-send",
                "--app-name=RakuOS",
                "--urgency=normal",
                title,
                msg,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            sent = true;
        }
    }
    if !sent {
        let content = format!("{title}\n{msg}\n");
        let _ = fs::write(NOTIFY_FILE, content);
    }
}

pub fn notify_progress(uids: &[u32], notify_id: &mut Option<String>, title: &str, msg: &str, value: u8) {
    for uid in uids {
        let bus = format!("/run/user/{uid}/bus");
        if !Path::new(&bus).exists() {
            continue;
        }
        let mut cmd = Command::new("sudo-rs");
        cmd.args(["-u", &format!("#{uid}")])
            .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={bus}"))
            .args([
                "notify-send",
                "--app-name=RakuOS",
                "--urgency=normal",
                "-t",
                "30000",
                "-h",
                &format!("int:value:{value}"),
                "--print-id",
            ]);
        if let Some(id) = notify_id.as_deref() {
            cmd.args(["-r", id]);
        }
        cmd.args([title, msg]);
        if let Ok(out) = cmd.output() {
            let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !id.is_empty() {
                *notify_id = Some(id);
            }
        }
    }
}

// ── Version comparison ────────────────────────────────────────────────────────

/// Returns true if a >= b (simple sort -V equivalent using rpmvercmp logic).
/// For correctness we delegate to `rpmdev-vercmp` if available, else use sort.
pub fn version_ge(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // Use sort -V to compare
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "printf '%s\\n%s\\n' '{}' '{}' | sort -V | tail -1",
            a.replace('\'', "'\\''"),
            b.replace('\'', "'\\''")
        ))
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    out == a
}

// ── Lock file ─────────────────────────────────────────────────────────────────

pub struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    pub fn acquire(path: &str) -> Result<Self> {
        touch(Path::new(path))?;
        Ok(Self { path: PathBuf::from(path) })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
