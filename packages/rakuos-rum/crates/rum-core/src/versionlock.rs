use serde::{Deserialize, Serialize};

/// One `rum versionlock` entry — dnf5-parity: a lock pins a name to the
/// exact `epoch:version-release` (and, if the locked package is arch-
/// specific, the exact arch) it had *at the moment it was locked*, not just
/// "never touch this name again." `evr`/`arch` are `None` when the name
/// wasn't installed yet at lock time (nothing to snapshot), in which case
/// the lock currently blocks nothing — matching dnf5, where a versionlock
/// entry for a not-yet-installed package can't meaningfully restrict a
/// version that doesn't exist yet either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionLock {
    pub name: String,
    pub evr: Option<String>,
    pub arch: Option<String>,
}

impl VersionLock {
    /// `true` if `evr`/`arch` (a candidate package's own fields) are exactly
    /// what this lock pins the name to — a mismatch means the candidate
    /// must never be selected while the lock exists, whether picked
    /// directly (`install`/`upgrade`) or pulled in transitively as someone
    /// else's dependency.
    pub fn allows(&self, evr: &str, arch: &str) -> bool {
        self.evr.as_deref().is_none_or(|locked| locked == evr) && self.arch.as_deref().is_none_or(|locked| locked == arch)
    }
}

/// Serializes as `name\tevr\tarch`, `*` standing in for an unconstrained
/// field — kept newline-delimited (one entry per line) so the on-disk file
/// stays diffable/greppable like the old plain name list was.
impl std::fmt::Display for VersionLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\t{}\t{}", self.name, self.evr.as_deref().unwrap_or("*"), self.arch.as_deref().unwrap_or("*"))
    }
}

impl std::str::FromStr for VersionLock {
    type Err = ();

    fn from_str(line: &str) -> Result<Self, ()> {
        let mut parts = line.splitn(3, '\t');
        let name = parts.next().ok_or(())?.to_string();
        let evr = parts.next().filter(|s| *s != "*").map(str::to_string);
        let arch = parts.next().filter(|s| *s != "*").map(str::to_string);
        Ok(Self { name, evr, arch })
    }
}
