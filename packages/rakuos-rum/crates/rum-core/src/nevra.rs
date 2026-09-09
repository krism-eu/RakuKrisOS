use serde::{Deserialize, Serialize};
use std::fmt;

/// Name-Epoch-Version-Release-Arch: RPM's full package identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Nevra {
    pub name: String,
    /// RPM epoch defaults to 0 when absent from metadata/filename.
    pub epoch: u32,
    pub version: String,
    pub release: String,
    pub arch: String,
}

impl Nevra {
    /// `epoch:version-release`, the form used in dependency version
    /// comparisons and `rpm -q` output.
    pub fn evr(&self) -> String {
        format!("{}:{}-{}", self.epoch, self.version, self.release)
    }

    /// Parses the standard `name-version-release.arch` form rpm/dnf print
    /// (e.g. from `rpm -qa --qf '%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}\n'`).
    /// Epoch isn't representable in this form, so it's left at 0 — callers
    /// that need epoch should read it from a separate `%{EPOCH}` field.
    pub fn parse_nvra(s: &str) -> Option<Self> {
        let (rest, arch) = s.rsplit_once('.')?;
        let (name_version, release) = rest.rsplit_once('-')?;
        let (name, version) = name_version.rsplit_once('-')?;
        Some(Self { name: name.to_string(), epoch: 0, version: version.to_string(), release: release.to_string(), arch: arch.to_string() })
    }

    /// Parses the full `name-epoch:version-release.arch` form modulemd
    /// artifact lists use (always has an explicit epoch, unlike
    /// [`Self::parse_nvra`]'s plain `rpm -qa` form).
    pub fn parse_nevra(s: &str) -> Option<Self> {
        let (rest, arch) = s.rsplit_once('.')?;
        let (name_evr, release) = rest.rsplit_once('-')?;
        let (name, evr) = name_evr.rsplit_once('-')?;
        let (epoch, version) = evr.split_once(':')?;
        Some(Self { name: name.to_string(), epoch: epoch.parse().ok()?, version: version.to_string(), release: release.to_string(), arch: arch.to_string() })
    }
}

impl fmt::Display for Nevra {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.epoch != 0 {
            write!(f, "{}-{}:{}-{}.{}", self.name, self.epoch, self.version, self.release, self.arch)
        } else {
            write!(f, "{}-{}-{}.{}", self.name, self.version, self.release, self.arch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_nvra() {
        let n = Nevra::parse_nvra("kernel-p03-6.15.4-1.fc44.x86_64").unwrap();
        assert_eq!(n.name, "kernel-p03");
        assert_eq!(n.version, "6.15.4");
        assert_eq!(n.release, "1.fc44");
        assert_eq!(n.arch, "x86_64");
    }

    #[test]
    fn parses_modulemd_artifact_nevra() {
        let n = Nevra::parse_nevra("nodejs-1:18.18.2-1.module_f39+19733+abcdef12.x86_64").unwrap();
        assert_eq!(n.name, "nodejs");
        assert_eq!(n.epoch, 1);
        assert_eq!(n.version, "18.18.2");
        assert_eq!(n.release, "1.module_f39+19733+abcdef12");
        assert_eq!(n.arch, "x86_64");
    }
}
