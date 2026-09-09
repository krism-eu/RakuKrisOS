use anyhow::{Result, bail};
use rakuos_pkgmgr::*;

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "upgrade".to_string());
    match cmd.as_str() {
        "check"            => cmd_check()?,
        "upgrade"          => cmd_upgrade()?,
        "upgrade-package"  => {
            let pkg = std::env::args().nth(2)
                .ok_or_else(|| anyhow::anyhow!("Usage: rakuos-update upgrade-package <name>"))?;
            cmd_upgrade_package(&pkg)?;
        }
        "check-flatpak"    => cmd_check_flatpak()?,
        "upgrade-flatpak"  => cmd_upgrade_flatpak()?,
        "check-image"      => cmd_check_image()?,
        "upgrade-image"    => cmd_upgrade_image()?,
        other => bail!("unknown subcommand: {other}"),
    }
    Ok(())
}

/// Delegates to `rum check-upgrade --json`, which already emits the
/// `{"updates": [...]}` shape this command's callers (rakuos-software,
/// rakuos-updater) expect.
fn cmd_check() -> Result<()> {
    let out = overlay::run_capture_ok("rum", &["check-upgrade", "--json"]);
    let data: serde_json::Value = serde_json::from_str(&out).unwrap_or(serde_json::json!({"updates": []}));
    let updates = data["updates"].as_array().cloned().unwrap_or_default();
    let has_updates = !updates.is_empty();
    let out = serde_json::json!({"updates": updates, "count": updates.len()});
    println!("{}", serde_json::to_string(&out)?);
    if !has_updates { std::process::exit(1); }
    Ok(())
}

fn cmd_upgrade() -> Result<()> {
    require_root()?;
    let _lock = overlay::LockGuard::acquire(overlay::LOCK_FILE)?;
    ensure_overlay_mounted()?;
    overlay::run("rum", &["upgrade"])?;
    println!("RakuOS: package upgrade complete.");
    Ok(())
}

fn cmd_upgrade_package(pkg: &str) -> Result<()> {
    require_root()?;
    let _lock = overlay::LockGuard::acquire(overlay::LOCK_FILE)?;
    ensure_overlay_mounted()?;
    overlay::run("rum", &["upgrade", pkg])?;
    println!("RakuOS: {pkg} upgraded.");
    Ok(())
}

fn cmd_check_flatpak() -> Result<()> {
    let mut all: Vec<FlatpakUpdate> = vec![];
    all.extend(get_flatpak_updates("system")?);

    // Check user flatpaks for each logged-in user
    for uid in overlay::logged_in_uids() {
        let out = std::process::Command::new("runuser")
            .args(["-l", &uid_to_username(uid), "-c",
                "flatpak remote-ls --user --updates --columns=application,branch,version,options 2>/dev/null"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        for line in out.lines() {
            let cols: Vec<&str> = line.splitn(4, '\t').collect();
            if cols.len() < 3 { continue; }
            let app_id = cols[0].trim().to_string();
            let branch = cols[1].trim().to_string();
            let current_version = cols[2].trim().to_string();
            let options = cols.get(3).unwrap_or(&"").to_lowercase();
            let is_runtime = options.contains("runtime");
            let name = app_id.split('.').last().unwrap_or(&app_id).to_string();
            all.push(FlatpakUpdate { name, app_id, branch, current_version, is_runtime, installation: "user".to_string() });
        }
    }

    let has = !all.is_empty();
    println!("{}", serde_json::to_string(&all)?);
    if !has { std::process::exit(1); }
    Ok(())
}

fn cmd_upgrade_flatpak() -> Result<()> {
    overlay::run_best_effort("flatpak", &["update", "--system", "-y", "--noninteractive"]);
    for uid in overlay::logged_in_uids() {
        let user = uid_to_username(uid);
        overlay::run_best_effort("runuser", &[
            "-l", &user, "-c",
            "flatpak update --user -y --noninteractive 2>/dev/null || true",
        ]);
    }
    println!("RakuOS: flatpak upgrade complete.");
    Ok(())
}

fn cmd_check_image() -> Result<()> {
    let info = get_booted_image_info()?;
    // Images are exclusively on quay.io. Strip the "quay.io/" prefix to get the repo path.
    let repo_path = info.repo_url.trim_start_matches("quay.io/");

    match query_quay_tag(repo_path, &info.channel_tag) {
        Ok(tag) => {
            let available_date = parse_rfc2822_epoch(&tag.last_modified)
                .map(|e| chrono::DateTime::from_timestamp(e, 0)
                    .map(|d| d.format("%Y%m%d").to_string())
                    .unwrap_or_default())
                .unwrap_or_default();
            let has_update = tag.manifest_digest != info.digest;

            // Determine hotfix vs update:
            // Extract the date from the current booted tag (e.g. staging.20260531 → 20260531)
            // If same date but different digest → hotfix (same-day rebuild)
            // If different date → update (new build)
            let (_, current_tag) = rakuos_pkgmgr::split_image_ref(&info.full_image);
            let current_date = current_tag
                .rsplit_once('.').map(|(_, d)| d).unwrap_or("");
            let update_type = if !available_date.is_empty()
                && !current_date.is_empty()
                && available_date == current_date
            {
                "hotfix"
            } else {
                "update"
            };

            let available_version = format!("{}.{}", info.channel_tag, available_date);
            // Human-friendly tag (e.g. "staging.20260729"), not the raw
            // quay.io/... image reference — showing that in the UI is what
            // confused users into thinking they were on/updating to the
            // wrong image variant.
            let current_version = if current_tag.is_empty() {
                info.channel_tag.clone()
            } else {
                current_tag.clone()
            };
            let out = serde_json::json!({
                "update":            has_update,
                "type":              update_type,
                "current_version":   &current_version,
                "channel":           &info.channel_tag,
                "available_version": &available_version,
                "available":         &available_version,
                "available_digest":  tag.manifest_digest,
                "image_label":       rakuos_pkgmgr::friendly_image_label(&info.repo_url),
                "repo":              info.repo_url,
            });
            println!("{}", serde_json::to_string(&out)?);
            if !has_update { std::process::exit(1); }
        }
        Err(e) => {
            eprintln!("RakuOS: could not check image update: {e}");
            let out = serde_json::json!({"update": false, "error": e.to_string()});
            println!("{}", serde_json::to_string(&out)?);
            std::process::exit(1);
        }
    }
    Ok(())
}

fn cmd_upgrade_image() -> Result<()> {
    require_root()?;
    overlay::run("bootc", &["upgrade"])?;
    overlay::run_best_effort("fc-cache", &["-f"]);
    println!("RakuOS: system image update staged. Reboot to apply.");
    Ok(())
}

fn uid_to_username(uid: u32) -> String {
    let out = overlay::run_capture_ok("id", &["-un", &uid.to_string()]);
    out.trim().to_string()
}
