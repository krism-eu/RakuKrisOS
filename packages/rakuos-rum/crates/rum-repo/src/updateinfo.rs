//! Streaming parser for `updateinfo.xml[.gz]` (the repomd `type="updateinfo"`
//! data entry) — security/bugfix/enhancement advisories, enough of it for
//! `rum advisory list/info/summary` and `upgrade --advisory=`/`--security`/
//! etc. filtering. Only the fields those need are kept: `<references>` (CVE/
//! bugzilla links) aren't parsed, since nothing in rum surfaces them yet.

use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};

/// `type="…"` on an `<update>` element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdvisoryKind {
    Security,
    Bugfix,
    Enhancement,
    Newpackage,
    /// Anything else a repo's updateinfo.xml might use (rare, but dnf5
    /// itself falls back to "unknown" rather than rejecting the advisory).
    Unknown,
}

impl AdvisoryKind {
    fn parse(s: &str) -> Self {
        match s {
            "security" => Self::Security,
            "bugfix" => Self::Bugfix,
            "enhancement" => Self::Enhancement,
            "newpackage" => Self::Newpackage,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Security => "security",
            Self::Bugfix => "bugfix",
            Self::Enhancement => "enhancement",
            Self::Newpackage => "newpackage",
            Self::Unknown => "unknown",
        }
    }
}

/// `<severity>…</severity>`, present on security advisories — dnf's own
/// four-level scale plus "None" for advisories that don't set one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    None,
    Low,
    Moderate,
    Important,
    Critical,
}

impl Severity {
    fn parse(s: &str) -> Self {
        Self::parse_cli(s)
    }

    /// Same mapping as [`Severity::parse`], exposed for `--severity=`
    /// CLI-argument parsing (case-insensitive, unrecognized text maps to
    /// `None` rather than erroring — matches how an unset `<severity>`
    /// element is treated).
    pub fn parse_cli(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "critical" => Self::Critical,
            "important" => Self::Important,
            "moderate" => Self::Moderate,
            "low" => Self::Low,
            _ => Self::None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Critical => "Critical",
            Self::Important => "Important",
            Self::Moderate => "Moderate",
            Self::Low => "Low",
            Self::None => "None",
        }
    }
}

/// One `<update>` entry: an advisory id (e.g. `FEDORA-2024-abc123`) plus the
/// exact package NEVRAs it covers, parsed from `<pkglist>/<collection>/
/// <package>` (epoch/version/release/arch reassembled into a full NEVRA
/// string, since that's the identity rum's resolver/rpmdb code already
/// keys on everywhere else).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Advisory {
    pub id: String,
    pub kind: AdvisoryKind,
    pub severity: Severity,
    pub title: String,
    pub description: String,
    /// `<issued date="…"/>` — kept as the raw string dnf itself prints
    /// (`YYYY-MM-DD HH:MM:SS`), no parsing needed for display purposes.
    pub issued: String,
    /// Every package NEVRA (`name-epoch:version-release.arch`) this advisory
    /// covers, across all `<collection>`s.
    pub packages: Vec<String>,
    /// Repo id this advisory was loaded from — stamped by the caller after
    /// parsing, same pattern as `Package::repo_id`.
    #[serde(default)]
    pub repo_id: String,
}

pub fn parse_updateinfo_xml(xml: &str) -> Vec<Advisory> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    let mut advisories = Vec::new();
    let mut cur: Option<Advisory> = None;
    let mut in_id = false;
    let mut in_title = false;
    let mut in_description = false;
    let mut in_severity = false;
    let mut in_pkglist = false;
    let mut pkg_epoch = String::new();
    let mut pkg_version = String::new();
    let mut pkg_release = String::new();
    let mut pkg_arch = String::new();
    let mut pkg_name = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let local = e.local_name();
                let name = String::from_utf8_lossy(local.as_ref()).to_string();
                match name.as_str() {
                    "update" => {
                        let kind = e
                            .attributes()
                            .flatten()
                            .find(|a| a.key.local_name().as_ref() == b"type")
                            .map(|a| AdvisoryKind::parse(&String::from_utf8_lossy(&a.value)))
                            .unwrap_or(AdvisoryKind::Unknown);
                        cur = Some(Advisory { id: String::new(), kind, severity: Severity::None, title: String::new(), description: String::new(), issued: String::new(), packages: Vec::new(), repo_id: String::new() });
                    }
                    "id" => in_id = true,
                    "title" => in_title = true,
                    "description" => in_description = true,
                    "severity" => in_severity = true,
                    "pkglist" => in_pkglist = true,
                    "issued" => {
                        if let Some(advisory) = cur.as_mut() {
                            if let Some(date) = e.attributes().flatten().find(|a| a.key.local_name().as_ref() == b"date") {
                                advisory.issued = String::from_utf8_lossy(&date.value).into_owned();
                            }
                        }
                    }
                    "package" if in_pkglist => {
                        pkg_epoch.clear();
                        pkg_version.clear();
                        pkg_release.clear();
                        pkg_arch.clear();
                        pkg_name.clear();
                        for attr in e.attributes().flatten() {
                            let value = String::from_utf8_lossy(&attr.value).into_owned();
                            match attr.key.local_name().as_ref() {
                                b"name" => pkg_name = value,
                                b"epoch" => pkg_epoch = value,
                                b"version" => pkg_version = value,
                                b"release" => pkg_release = value,
                                b"arch" => pkg_arch = value,
                                _ => {}
                            }
                        }
                        if !pkg_name.is_empty() {
                            let epoch = if pkg_epoch.is_empty() || pkg_epoch == "0" { String::new() } else { format!("{pkg_epoch}:") };
                            if let Some(advisory) = cur.as_mut() {
                                advisory.packages.push(format!("{pkg_name}-{epoch}{pkg_version}-{pkg_release}.{pkg_arch}"));
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                let Ok(text) = t.unescape() else { continue };
                if let Some(advisory) = cur.as_mut() {
                    if in_id {
                        advisory.id.push_str(&text);
                    } else if in_title {
                        advisory.title.push_str(&text);
                    } else if in_description {
                        advisory.description.push_str(&text);
                    } else if in_severity {
                        advisory.severity = Severity::parse(&text);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"update" => {
                        if let Some(advisory) = cur.take() {
                            if !advisory.id.is_empty() {
                                advisories.push(advisory);
                            }
                        }
                    }
                    b"id" => in_id = false,
                    b"title" => in_title = false,
                    b"description" => in_description = false,
                    b"severity" => in_severity = false,
                    b"pkglist" => in_pkglist = false,
                    b"package" => {}
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    advisories
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_updates_and_packages() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<updates>
  <update from="updates@fedoraproject.org" status="stable" type="security" version="2">
    <id>FEDORA-2024-abc123</id>
    <title>Security update for foo</title>
    <issued date="2024-01-02 03:04:05"/>
    <severity>Important</severity>
    <description>Fixes a heap overflow in foo.</description>
    <pkglist>
      <collection short="fedora">
        <name>fedora</name>
        <package name="foo" version="1.2" release="3.fc44" epoch="0" arch="x86_64" src="foo-1.2-3.fc44.src.rpm">
          <filename>foo-1.2-3.fc44.x86_64.rpm</filename>
        </package>
        <package name="foo" version="1.2" release="3.fc44" epoch="1" arch="noarch" src="foo-1.2-3.fc44.src.rpm">
          <filename>foo-1.2-3.fc44.noarch.rpm</filename>
        </package>
      </collection>
    </pkglist>
  </update>
  <update type="bugfix" version="2">
    <id>FEDORA-2024-def456</id>
    <title>Bugfix update for bar</title>
    <pkglist>
      <collection short="fedora">
        <package name="bar" version="4.5" release="1.fc44" epoch="0" arch="x86_64" src="bar.src.rpm"><filename>bar.rpm</filename></package>
      </collection>
    </pkglist>
  </update>
</updates>
"#;
        let advisories = parse_updateinfo_xml(xml);
        assert_eq!(advisories.len(), 2);
        let sec = &advisories[0];
        assert_eq!(sec.id, "FEDORA-2024-abc123");
        assert_eq!(sec.kind, AdvisoryKind::Security);
        assert_eq!(sec.severity, Severity::Important);
        assert_eq!(sec.issued, "2024-01-02 03:04:05");
        assert_eq!(sec.title, "Security update for foo");
        assert_eq!(sec.packages, vec!["foo-1.2-3.fc44.x86_64".to_string(), "foo-1:1.2-3.fc44.noarch".to_string()]);

        let bug = &advisories[1];
        assert_eq!(bug.kind, AdvisoryKind::Bugfix);
        assert_eq!(bug.severity, Severity::None);
        assert_eq!(bug.packages, vec!["bar-4.5-1.fc44.x86_64".to_string()]);
    }
}
