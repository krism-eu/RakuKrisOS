use anyhow::Result;
use rakuos_pkgmgr::*;

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "check".to_string());
    match cmd.as_str() {
        "check"   => cmd_check()?,
        "upgrade" => cmd_upgrade()?,
        other => anyhow::bail!("unknown subcommand: {other}"),
    }
    Ok(())
}

fn cmd_check() -> Result<()> {
    let info = match get_booted_image_info() {
        Ok(i) => i,
        Err(e) => {
            let out = serde_json::json!({"update": false, "error": e.to_string()});
            println!("{}", serde_json::to_string(&out)?);
            std::process::exit(1);
        }
    };

    let registry_host = info.repo_url.split('/').next().unwrap_or("");
    let repo_path = info.repo_url.trim_start_matches(&format!("{registry_host}/"));

    match query_quay_tag(repo_path, &info.channel_tag) {
        Ok(tag) => {
            let has_update = tag.manifest_digest != info.digest;
            let available_date = parse_rfc2822_epoch(&tag.last_modified)
                .map(|e| chrono::DateTime::from_timestamp(e, 0)
                    .map(|d| d.format("%Y%m%d").to_string())
                    .unwrap_or_default())
                .unwrap_or_default();
            let out = serde_json::json!({
                "update": has_update,
                "current_image": info.full_image,
                "channel": info.channel_tag,
                "available_version": format!("{}.{}", info.channel_tag, available_date),
                "available_digest": tag.manifest_digest,
                "repo": info.repo_url,
            });
            println!("{}", serde_json::to_string(&out)?);
            if !has_update { std::process::exit(1); }
        }
        Err(e) => {
            eprintln!("RakuOS: could not check image: {e}");
            let out = serde_json::json!({"update": false, "error": e.to_string()});
            println!("{}", serde_json::to_string(&out)?);
            std::process::exit(1);
        }
    }
    Ok(())
}

fn cmd_upgrade() -> Result<()> {
    require_root()?;
    overlay::run("bootc", &["upgrade"])?;
    overlay::run_best_effort("fc-cache", &["-f"]);
    println!("RakuOS: image upgrade staged. Reboot to apply.");
    Ok(())
}
