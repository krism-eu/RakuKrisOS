pub use rakuos_overlay as overlay;

use anyhow::{Result, bail};

// ── Path constants ────────────────────────────────────────────────────────────

pub const PACKAGES_LIST: &str = "/var/lib/rakuos/packages.list";
pub const LOCAL_RPM_LIST: &str = "/var/lib/rakuos/packages-rpm.list";
pub const LOCAL_RPM_CACHE: &str = "/var/lib/rakuos/local-rpms";
pub const PKG_CACHE_DIR: &str = "/var/cache/libdnf5";
pub const STATE_FILE: &str = "/var/lib/rakuos/overlay.state";
pub const DIRTY_FILE: &str = "/var/lib/rakuos/overlay.dirty";
pub const UPPER_DIR: &str = "/var/lib/rakuos/overlay/upper";
pub const WORK_DIR: &str = "/var/lib/rakuos/overlay/work";
// Merged path, not the raw upper dir — see the comment on the same constant
// in crates/overlay/src/lib.rs for why.
pub const OVERLAY_RPM_DB: &str = "/usr/share/rpm";
pub const USER_READY_FILE: &str = "/tmp/rakuos-overlay-update.ready";

// ── Root check ────────────────────────────────────────────────────────────────

pub fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("this command must be run as root");
    }
    Ok(())
}

// ── Package list management ───────────────────────────────────────────────────

pub fn read_packages_list(path: &str) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else { return vec![] };
    content.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

pub fn write_packages_list(path: &str, pkgs: &[String]) -> Result<()> {
    let content = pkgs.join("\n") + if pkgs.is_empty() { "" } else { "\n" };
    std::fs::write(path, content)?;
    Ok(())
}

pub fn package_in_list(path: &str, name: &str) -> bool {
    read_packages_list(path).iter().any(|p| p == name)
}

pub fn add_to_list(path: &str, name: &str) -> Result<()> {
    if package_in_list(path, name) { return Ok(()); }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", name)?;
    Ok(())
}

pub fn remove_from_list(path: &str, name: &str) -> Result<()> {
    let pkgs: Vec<String> = read_packages_list(path)
        .into_iter()
        .filter(|p| p != name)
        .collect();
    write_packages_list(path, &pkgs)
}

// ── Overlay mount ─────────────────────────────────────────────────────────────

pub fn ensure_overlay_mounted() -> Result<()> {
    if rakuos_overlay::usr_is_overlay_mounted() { return Ok(()); }
    if rakuos_overlay::is_live_env() { return Ok(()); }
    rakuos_overlay::run("mount", &[
        "-t", "overlay", "overlay",
        "-o", &format!("lowerdir=/usr,upperdir={UPPER_DIR},workdir={WORK_DIR}"),
        "/usr",
    ])
}

// ── Image info ────────────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub struct ImageInfo {
    pub full_image: String,
    pub repo_url: String,
    pub channel_tag: String,
    pub digest: String,
    pub timestamp: String,
}

pub fn get_booted_image_info() -> Result<ImageInfo> {
    // `bootc status` requires root. Sudoers grants %wheel passwordless access
    // to this exact command, so this works non-interactively from the daemon.
    let json = rakuos_overlay::run_capture("sudo-rs", &["bootc", "status", "--json"])?;
    let v: serde_json::Value = serde_json::from_str(&json)?;
    let image = v["status"]["booted"]["image"]["image"]["image"]
        .as_str().unwrap_or("").trim().to_string();
    let digest = v["status"]["booted"]["image"]["imageDigest"]
        .as_str().unwrap_or("").trim().to_string();
    let timestamp = v["status"]["booted"]["image"]["timestamp"]
        .as_str().unwrap_or("").trim().to_string();
    if image.is_empty() {
        bail!("could not detect booted image from bootc status");
    }
    let (repo_url, raw_tag) = split_image_ref(&image);
    let channel_tag = parse_channel_tag(&raw_tag);
    Ok(ImageInfo { full_image: image, repo_url, channel_tag, digest, timestamp })
}

/// Builds a friendly display label for an image repo path, e.g.
/// "RakuOS KDE (v3)" or "RakuOS COSMIC (v4, Nvidia)" or just "RakuOS" for
/// the base image. Used so update UIs never show the raw quay.io repo
/// path/tag (which reads as if the update is switching DE/ISA variant)
/// or a bare "RakuOS" that hides which variant is actually installed.
pub fn friendly_image_label(repo_url: &str) -> String {
    let path = repo_url.rsplit_once('@').map(|(p, _)| p).unwrap_or(repo_url);
    let path = path.rsplit_once(':').map(|(p, _)| p).unwrap_or(path);
    let last_segment = path.rsplit('/').next().unwrap_or("");

    let (without_isa, isa_suffix) = ["-v4", "-v3"]
        .iter()
        .find_map(|&sfx| last_segment.strip_suffix(sfx).map(|rest| (rest, sfx)))
        .unwrap_or((last_segment, ""));
    let is_nvidia = without_isa.ends_with("-nvidia");
    let base = without_isa.trim_end_matches("-nvidia");

    let de_label = match base {
        "rakuos-kde" => "KDE Plasma",
        "rakuos-gnome" => "GNOME",
        "rakuos-cosmic" => "COSMIC",
        "rakuos-niri" => "Niri",
        _ => "",
    };
    let mut label = if de_label.is_empty() {
        "RakuOS".to_string()
    } else {
        format!("RakuOS {de_label}")
    };

    let mut tags = Vec::new();
    if !isa_suffix.is_empty() {
        tags.push(isa_suffix.trim_start_matches('-').to_string());
    }
    if is_nvidia {
        tags.push("Nvidia".to_string());
    }
    if !tags.is_empty() {
        label = format!("{} ({})", label, tags.join(", "));
    }
    label
}

/// Splits a bootc/OCI image reference into (repo_path, tag). Handles both
/// tag-pinned refs ("quay.io/rakuos/rakuos-cosmic-v3:latest.20260601") and
/// digest-pinned refs ("quay.io/rakuos/rakuos-cosmic-v3@sha256:abcd..."),
/// which is how bootc reports the booted image once it's been pulled.
/// Naively splitting on the last ':' breaks on the digest form, since the
/// colon inside "sha256:..." gets mistaken for the tag separator, truncating
/// the repo path (and silently dropping the -v3/-v4 ISA suffix with it).
pub fn split_image_ref(image: &str) -> (String, String) {
    if let Some((repo, _digest)) = image.rsplit_once('@') {
        return (repo.to_string(), String::new());
    }
    let repo = image.rsplitn(2, ':').nth(1).unwrap_or(image).to_string();
    let tag = image.rsplitn(2, ':').next().unwrap_or("latest").to_string();
    (repo, tag)
}

pub fn parse_channel_tag(tag: &str) -> String {
    // "staging.20260531" → "staging", "20260531" → "latest", "staging" → "staging"
    if tag.len() == 8 && tag.chars().all(|c| c.is_ascii_digit()) {
        return "latest".to_string();
    }
    if let Some((channel, date)) = tag.rsplit_once('.') {
        if date.len() == 8 && date.chars().all(|c| c.is_ascii_digit()) {
            return channel.to_ascii_lowercase();
        }
    }
    tag.to_ascii_lowercase()
}

// ── Quay API ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct QuayTagInfo {
    pub last_modified: String,
    pub manifest_digest: String,
}

pub fn query_quay_tag(repo_path: &str, tag: &str) -> Result<QuayTagInfo> {
    let url = format!("https://quay.io/api/v1/repository/{repo_path}/tag/?specificTag={tag}&limit=1");
    let resp = reqwest::blocking::get(&url)?.error_for_status()?;
    let v: serde_json::Value = resp.json()?;
    let last_modified = v["tags"][0]["last_modified"].as_str().unwrap_or("").to_string();
    let manifest_digest = v["tags"][0]["manifest_digest"].as_str().unwrap_or("").to_string();
    if last_modified.is_empty() {
        bail!("tag {tag} not found on Quay for {repo_path}");
    }
    Ok(QuayTagInfo { last_modified, manifest_digest })
}

// ── Flatpak helpers ───────────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize)]
pub struct FlatpakUpdate {
    pub name: String,
    pub app_id: String,
    pub branch: String,
    pub current_version: String,
    pub is_runtime: bool,
    pub installation: String,
}

pub fn get_flatpak_updates(installation: &str) -> Result<Vec<FlatpakUpdate>> {
    let flag = if installation == "user" { "--user" } else { "--system" };
    let out = rakuos_overlay::run_capture_ok("flatpak", &[
        "remote-ls", flag, "--updates", "--columns=application,branch,version,options",
    ]);
    let mut updates = vec![];
    for line in out.lines() {
        let cols: Vec<&str> = line.splitn(4, '\t').collect();
        if cols.len() < 3 { continue; }
        let app_id = cols[0].trim().to_string();
        let branch = cols[1].trim().to_string();
        let current_version = cols[2].trim().to_string();
        let options = cols.get(3).unwrap_or(&"").to_lowercase();
        let is_runtime = options.contains("runtime");
        let name = app_id.split('.').last().unwrap_or(&app_id).to_string();
        updates.push(FlatpakUpdate {
            name, app_id, branch, current_version, is_runtime,
            installation: installation.to_string(),
        });
    }
    Ok(updates)
}

// ── Timestamp comparison ──────────────────────────────────────────────────────

pub fn parse_rfc2822_epoch(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(s)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(s))
        .map(|dt| dt.timestamp())
        .ok()
}
