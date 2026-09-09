//! Copr repo support (`rum copr enable/disable`), modeled on dnf's
//! `dnf-plugins-core` copr plugin: given `owner/project`, work out which
//! Copr chroot matches this machine and fetch the ready-made `.repo` file
//! Copr generates for it, rather than making the user guess a chroot name
//! themselves.
//!
//! Chroot naming is `<distname>-<version>-<arch>` (e.g. `fedora-44-x86_64`).
//! `<distname>`/`<version>` come from `/etc/os-release`: `ID_LIKE` (falling
//! back to `ID`) picks the Copr chroot family, `VERSION_ID` picks the
//! version. RakuOS's own `/etc/os-release` sets `ID=rakuos`,
//! `ID_LIKE="fedora"`, `VERSION_ID=44` — since Copr has no `rakuos-*`
//! chroots, `ID_LIKE` pointing at `fedora` is exactly what lets a RakuOS
//! machine resolve to the right (`fedora-44-<arch>`) chroot instead of a
//! nonexistent `rakuos-44-<arch>` one.

use crate::get_with_retry;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const COPR_HOSTNAME: &str = "copr.fedorainfracloud.org";
const COPR_URL: &str = "https://copr.fedorainfracloud.org";

/// The handful of `/etc/os-release` fields chroot-guessing needs.
struct OsRelease {
    id: String,
    id_like: String,
    version_id: String,
}

impl OsRelease {
    fn load() -> Result<Self> {
        let text = fs::read_to_string("/etc/os-release")
            .or_else(|_| fs::read_to_string("/usr/lib/os-release"))
            .context("reading /etc/os-release")?;
        Ok(Self::parse(&text))
    }

    fn parse(text: &str) -> Self {
        let mut fields: HashMap<String, String> = HashMap::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            fields.insert(key.trim().to_string(), value.trim().trim_matches('"').to_string());
        }
        Self {
            id: fields.get("ID").cloned().unwrap_or_default(),
            id_like: fields.get("ID_LIKE").cloned().unwrap_or_default(),
            version_id: fields.get("VERSION_ID").cloned().unwrap_or_default(),
        }
    }

    /// `ID` followed by `ID_LIKE`'s words, in order. `os-release` lists
    /// `ID_LIKE` from closest to furthest upstream (e.g. AlmaLinux ships
    /// `ID_LIKE="rhel centos fedora"` — RHEL-compatible first, Fedora only
    /// as a very loose last resort), so scanning in this order and taking
    /// the first word [`guess_chroot_from`] recognizes gives the most
    /// specific correct family instead of whichever `contains()` check
    /// happens to run first.
    fn family_words(&self) -> Vec<String> {
        std::iter::once(self.id.as_str()).chain(self.id_like.split_whitespace()).map(|w| w.to_lowercase()).collect()
    }
}

/// This machine's architecture, in Copr's naming (matches `uname -m`,
/// which is also what dnf's `$basearch` substitution resolves to on every
/// arch Copr supports).
fn basearch() -> &'static str {
    std::env::consts::ARCH
}

/// Guesses the Copr chroot for this machine, e.g. `fedora-44-x86_64`.
/// Mirrors dnf's `copr` plugin's `_guess_chroot`, generalized to read
/// `ID_LIKE` (dnf's plugin only ever sees `NAME`/`VERSION_ID` via
/// `platform.freedesktop_os_release`, so it can't do this — RakuOS's
/// `ID` is never going to be a Copr chroot family by itself).
pub fn guess_chroot() -> Result<String> {
    let os_release = OsRelease::load()?;
    guess_chroot_from(&os_release)
}

fn guess_chroot_from(os_release: &OsRelease) -> Result<String> {
    let arch = basearch();
    let version = os_release.version_id.as_str();

    // The first recognized family word wins — see `family_words`' doc
    // comment for why order (not "does this substring appear anywhere")
    // is what makes a RHEL derivative that also lists `fedora` in
    // `ID_LIKE` resolve to `epel-*`, not `fedora-*`.
    for word in os_release.family_words() {
        let chroot = match word.as_str() {
            "fedora" => {
                if version.is_empty() || version.eq_ignore_ascii_case("rawhide") {
                    format!("fedora-rawhide-{arch}")
                } else {
                    format!("fedora-{version}-{arch}")
                }
            }
            "mageia" => {
                if version.eq_ignore_ascii_case("cauldron") {
                    format!("mageia-cauldron-{arch}")
                } else {
                    format!("mageia-{version}-{arch}")
                }
            }
            "opensuse" | "suse" => {
                if version.to_lowercase().contains("tumbleweed") {
                    format!("opensuse-tumbleweed-{arch}")
                } else {
                    format!("opensuse-leap-{version}-{arch}")
                }
            }
            "rhel" | "centos" | "almalinux" | "rocky" => {
                format!("epel-{}-{arch}", version.split('.').next().unwrap_or(version))
            }
            _ => continue,
        };
        return Ok(chroot);
    }

    // Nothing recognized at all (unknown ID, no ID_LIKE) — EPEL's chroot
    // naming is the closest thing Copr has to a generic fallback.
    Ok(format!("epel-{}-{arch}", version.split('.').next().unwrap_or(version)))
}

/// Splits `owner/project` (dnf's copr plugin also accepts a 3-part
/// `hostname/owner/project` form for non-default Copr instances — not
/// supported here, rum only talks to the main Fedora Copr instance).
fn split_project(owner_project: &str) -> Result<(&str, &str)> {
    let mut parts = owner_project.splitn(2, '/');
    let owner = parts.next().filter(|s| !s.is_empty());
    let project = parts.next().filter(|s| !s.is_empty());
    match (owner, project) {
        (Some(o), Some(p)) => Ok((o, p)),
        _ => bail!("expected `owner/project` (e.g. `atim/starship`), got '{owner_project}'"),
    }
}

/// The `.repo` filename rum writes for an enabled Copr project, matching
/// dnf's copr plugin's `_copr:<hostname>:<owner>:<project>.repo` naming so
/// the two tools don't collide or double-add the same repo under different
/// names on a system where both happen to be present.
fn repo_filename(repo_dir: &Path, owner: &str, project: &str) -> PathBuf {
    repo_dir.join(format!("_copr:{COPR_HOSTNAME}:{owner}:{project}.repo"))
}

/// Enables a Copr project: guesses (or uses the given) chroot, downloads
/// the `.repo` file Copr generates for that owner/project/chroot, and
/// writes it into `repo_dir`. Returns the path written.
pub async fn enable(client: &reqwest::Client, owner_project: &str, chroot: Option<&str>, repo_dir: &Path) -> Result<PathBuf> {
    let (owner, project) = split_project(owner_project)?;
    let chroot = match chroot {
        Some(c) => c.to_string(),
        None => guess_chroot().context("guessing Copr chroot from /etc/os-release")?,
    };
    let parts: Vec<&str> = chroot.rsplitn(2, '-').collect();
    let (arch, short_chroot) = match parts.as_slice() {
        [arch, short] => (*arch, *short),
        _ => bail!("bad chroot '{chroot}', expected '<distname>-<version>-<arch>'"),
    };

    let url = format!("{COPR_URL}/coprs/{owner}/{project}/repo/{short_chroot}/dnf.repo?arch={arch}");
    let resp = get_with_retry(client, &url).await?;
    if resp.status >= 400 {
        bail!("Copr has no build of '{owner}/{project}' for chroot '{chroot}' (HTTP {})", resp.status);
    }
    let body = resp.text().await.with_context(|| format!("reading response body from {url}"))?;

    fs::create_dir_all(repo_dir).with_context(|| format!("creating {}", repo_dir.display()))?;
    let path = repo_filename(repo_dir, owner, project);
    fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Disables (removes) a previously-enabled Copr project's `.repo` file.
/// Not an error if it was never enabled — same idempotent behavior as
/// `rum remove` on an uninstalled package.
pub fn disable(owner_project: &str, repo_dir: &Path) -> Result<()> {
    let (owner, project) = split_project(owner_project)?;
    let path = repo_filename(repo_dir, owner, project);
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_fedora_chroot_from_id_like() {
        let os_release = OsRelease { id: "rakuos".into(), id_like: "fedora".into(), version_id: "44".into() };
        let chroot = guess_chroot_from(&os_release).unwrap();
        assert_eq!(chroot, format!("fedora-44-{}", basearch()));
    }

    #[test]
    fn guesses_fedora_rawhide() {
        let os_release = OsRelease { id: "fedora".into(), id_like: String::new(), version_id: "rawhide".into() };
        let chroot = guess_chroot_from(&os_release).unwrap();
        assert_eq!(chroot, format!("fedora-rawhide-{}", basearch()));
    }

    #[test]
    fn guesses_epel_chroot_for_rhel_like() {
        let os_release = OsRelease { id: "almalinux".into(), id_like: "rhel centos fedora".into(), version_id: "9.3".into() };
        let chroot = guess_chroot_from(&os_release).unwrap();
        // "fedora" also appears in id_like here (common on RHEL-family
        // os-release files) but rhel/centos should win — EPEL, not Fedora,
        // is the right chroot family for a RHEL derivative.
        assert_eq!(chroot, format!("epel-9-{}", basearch()));
    }

    #[test]
    fn splits_owner_project() {
        assert_eq!(split_project("atim/starship").unwrap(), ("atim", "starship"));
        assert!(split_project("no-slash").is_err());
    }

    #[test]
    fn repo_filename_matches_dnf_copr_plugin_convention() {
        let path = repo_filename(Path::new("/etc/yum.repos.d"), "atim", "starship");
        assert_eq!(path, PathBuf::from("/etc/yum.repos.d/_copr:copr.fedorainfracloud.org:atim:starship.repo"));
    }
}
