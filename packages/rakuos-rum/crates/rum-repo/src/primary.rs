//! Streaming parser for `primary.xml`, the createrepo_c-generated file that
//! carries per-package NEVRA + summary + provides/requires/conflicts/
//! obsoletes. Hand-written over `quick_xml`'s pull-event API rather than
//! serde-derive: the file mixes an unprefixed default namespace with the
//! `rpm:` namespace, and matching only on each tag's *local* name (ignoring
//! the namespace prefix) sidesteps having to model that properly while
//! still being unambiguous — `primary.xml` never reuses a local name across
//! the two namespaces in a way that would collide.

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use rum_core::{Comparator, Dependency, Nevra, Package};

#[derive(Default)]
struct Building {
    name: String,
    arch: String,
    epoch: u32,
    version: String,
    release: String,
    summary: String,
    location: String,
    install_size: u64,
    download_size: u64,
    provides: Vec<Dependency>,
    requires: Vec<Dependency>,
    conflicts: Vec<Dependency>,
    obsoletes: Vec<Dependency>,
    recommends: Vec<Dependency>,
    suggests: Vec<Dependency>,
    enhances: Vec<Dependency>,
    supplements: Vec<Dependency>,
    vendor: String,
    checksum_type: String,
    checksum: String,
}

impl Building {
    fn finish(self) -> Package {
        Package {
            nevra: Nevra { name: self.name, epoch: self.epoch, version: self.version, release: self.release, arch: self.arch },
            summary: self.summary,
            provides: self.provides,
            requires: self.requires,
            conflicts: self.conflicts,
            obsoletes: self.obsoletes,
            recommends: self.recommends,
            suggests: self.suggests,
            enhances: self.enhances,
            supplements: self.supplements,
            location: self.location,
            repo_id: String::new(),
            repo_priority: rum_core::default_repo_priority(),
            repo_cost: rum_core::default_repo_cost(),
            install_size: self.install_size,
            download_size: self.download_size,
            vendor: self.vendor,
            checksum_type: self.checksum_type,
            checksum: self.checksum,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Provides,
    Requires,
    Conflicts,
    Obsoletes,
    Recommends,
    Suggests,
    Enhances,
    Supplements,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextTarget {
    None,
    Name,
    Arch,
    Summary,
    File,
    Vendor,
    Checksum,
}

pub fn parse_primary_xml(xml: &str) -> Result<Vec<Package>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut packages = Vec::new();
    let mut current: Option<Building> = None;
    let mut section = Section::None;
    let mut text_target = TextTarget::None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf).context("parsing primary.xml")? {
            Event::Start(e) | Event::Empty(e) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match local.as_str() {
                    "package" => current = Some(Building::default()),
                    "name" => text_target = TextTarget::Name,
                    "arch" => text_target = TextTarget::Arch,
                    "summary" => text_target = TextTarget::Summary,
                    // `createrepo_c` includes a curated subset of file
                    // ownership in primary.xml itself (executables under
                    // bin/sbin/usr/{bin,sbin}, plus anything under /etc)
                    // precisely so file-path Requires like `/usr/bin/xprop`
                    // resolve without needing the (much larger)
                    // filelists.xml — real dnf relies on the same mechanism.
                    // Treat every `<file>` entry as a synthetic unversioned
                    // Provides on that exact path.
                    "file" => text_target = TextTarget::File,
                    "vendor" => text_target = TextTarget::Vendor,
                    // `pkgid="YES"` marks the checksum that also doubles as
                    // this package's cache-file id (there's only ever one
                    // `<checksum>` per `<package>` in practice, so no need
                    // to check that attribute — just capture the algorithm
                    // and let the following `Event::Text` fill in the hex
                    // digest).
                    "checksum" => {
                        text_target = TextTarget::Checksum;
                        if let Some(pkg) = current.as_mut() {
                            if let Some(ty) = e.attributes().flatten().find(|a| a.key.local_name().as_ref() == b"type") {
                                pkg.checksum_type = ty.decode_and_unescape_value(reader.decoder())?.to_string();
                            }
                        }
                    }
                    "version" => {
                        if let Some(pkg) = current.as_mut() {
                            for attr in e.attributes().flatten() {
                                let val = attr.decode_and_unescape_value(reader.decoder())?.to_string();
                                match attr.key.local_name().as_ref() {
                                    b"epoch" => pkg.epoch = val.parse().unwrap_or(0),
                                    b"ver" => pkg.version = val,
                                    b"rel" => pkg.release = val,
                                    _ => {}
                                }
                            }
                        }
                    }
                    "location" => {
                        if let Some(pkg) = current.as_mut() {
                            if let Some(href) = e.attributes().flatten().find(|a| a.key.local_name().as_ref() == b"href") {
                                pkg.location = href.decode_and_unescape_value(reader.decoder())?.to_string();
                            }
                        }
                    }
                    "size" => {
                        if let Some(pkg) = current.as_mut() {
                            for attr in e.attributes().flatten() {
                                let val = attr.decode_and_unescape_value(reader.decoder())?.to_string();
                                match attr.key.local_name().as_ref() {
                                    b"package" => pkg.download_size = val.parse().unwrap_or(0),
                                    b"installed" => pkg.install_size = val.parse().unwrap_or(0),
                                    _ => {}
                                }
                            }
                        }
                    }
                    "provides" => section = Section::Provides,
                    "requires" => section = Section::Requires,
                    "conflicts" => section = Section::Conflicts,
                    "obsoletes" => section = Section::Obsoletes,
                    "recommends" => section = Section::Recommends,
                    "suggests" => section = Section::Suggests,
                    "enhances" => section = Section::Enhances,
                    "supplements" => section = Section::Supplements,
                    "entry" if section != Section::None => {
                        if let Some(pkg) = current.as_mut() {
                            let dep = parse_entry(&e, &reader)?;
                            match section {
                                Section::Provides => pkg.provides.push(dep),
                                Section::Requires => pkg.requires.push(dep),
                                Section::Conflicts => pkg.conflicts.push(dep),
                                Section::Obsoletes => pkg.obsoletes.push(dep),
                                Section::Recommends => pkg.recommends.push(dep),
                                Section::Suggests => pkg.suggests.push(dep),
                                Section::Enhances => pkg.enhances.push(dep),
                                Section::Supplements => pkg.supplements.push(dep),
                                Section::None => unreachable!(),
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(t) => {
                if let Some(pkg) = current.as_mut() {
                    let text = t.unescape().unwrap_or_default().to_string();
                    match text_target {
                        TextTarget::Name => pkg.name = text,
                        TextTarget::Arch => pkg.arch = text,
                        TextTarget::Summary => pkg.summary = text,
                        TextTarget::File => pkg.provides.push(Dependency { name: text, constraint: None }),
                        TextTarget::Vendor => pkg.vendor = text,
                        TextTarget::Checksum => pkg.checksum = text,
                        TextTarget::None => {}
                    }
                }
            }
            Event::End(e) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match local.as_str() {
                    "package" => {
                        if let Some(pkg) = current.take() {
                            packages.push(pkg.finish());
                        }
                    }
                    "name" | "arch" | "summary" | "file" | "vendor" | "checksum" => text_target = TextTarget::None,
                    "provides" | "requires" | "conflicts" | "obsoletes" | "recommends" | "suggests" | "enhances" | "supplements" => section = Section::None,
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(packages)
}

fn parse_entry(e: &quick_xml::events::BytesStart, reader: &Reader<&[u8]>) -> Result<Dependency> {
    let mut name = String::new();
    let mut flags: Option<String> = None;
    let mut epoch = "0".to_string();
    let mut version = String::new();
    let mut release: Option<String> = None;

    for attr in e.attributes().flatten() {
        let val = attr.decode_and_unescape_value(reader.decoder())?.to_string();
        match attr.key.local_name().as_ref() {
            b"name" => name = val,
            b"flags" => flags = Some(val),
            b"epoch" => epoch = val,
            b"ver" => version = val,
            b"rel" => release = Some(val),
            _ => {}
        }
    }

    let constraint = match flags.as_deref() {
        Some("EQ") => Some(Comparator::Eq),
        Some("LE") => Some(Comparator::Le),
        Some("LT") => Some(Comparator::Lt),
        Some("GE") => Some(Comparator::Ge),
        Some("GT") => Some(Comparator::Gt),
        _ => None,
    }
    .filter(|_| !version.is_empty())
    .map(|cmp| {
        let evr = match &release {
            Some(rel) => format!("{epoch}:{version}-{rel}"),
            None => format!("{epoch}:{version}"),
        };
        (cmp, evr)
    });

    Ok(Dependency { name, constraint })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_checksum() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata xmlns="http://linux.duke.edu/metadata/common" xmlns:rpm="http://linux.duke.edu/metadata/rpm" packages="1">
  <package type="rpm">
    <name>foo</name>
    <arch>x86_64</arch>
    <version epoch="0" ver="1.0" rel="1.fc40"/>
    <checksum type="sha256" pkgid="YES">deadbeef00112233445566778899aabbccddeeff00112233445566778899aa</checksum>
    <summary>Foo</summary>
    <location href="foo-1.0-1.fc40.x86_64.rpm"/>
    <size package="100" installed="200"/>
  </package>
</metadata>"#;
        let pkgs = parse_primary_xml(xml).unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].checksum_type, "sha256");
        assert_eq!(pkgs[0].checksum, "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa");
    }
}
