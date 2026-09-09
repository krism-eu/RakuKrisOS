use anyhow::Result;
use rakuos_pkgmgr::*;
use std::collections::HashMap;
use walkdir::WalkDir;

fn main() -> Result<()> {
    require_root()?;

    let db = OVERLAY_RPM_DB;
    if !std::path::Path::new(db).exists() {
        println!("RakuOS cache clean: overlay RPM database not found — nothing to check against.");
        return Ok(());
    }

    clean_dnf_cache(db)?;
    clean_local_cache(db)?;
    println!("RakuOS cache clean: done.");
    Ok(())
}

fn clean_dnf_cache(db: &str) -> Result<()> {
    let cache = std::path::Path::new(PKG_CACHE_DIR);
    if !cache.exists() { return Ok(()); }
    println!("RakuOS cache clean: scanning DNF cache...");

    // Collect all RPMs grouped by name.arch
    let mut pkg_map: HashMap<String, Vec<(String, std::path::PathBuf)>> = HashMap::new();

    for entry in WalkDir::new(cache).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().map(|e| e == "rpm").unwrap_or(false) {
            let info = overlay::run_capture_ok("rpm", &[
                "-qp", "--qf", "%{NAME}\t%{ARCH}\t%{EPOCH}:%{VERSION}-%{RELEASE}",
                path.to_str().unwrap_or(""),
            ]);
            let parts: Vec<&str> = info.trim().splitn(3, '\t').collect();
            if parts.len() < 3 { continue; }
            let key = format!("{}.{}", parts[0], parts[1]);
            pkg_map.entry(key).or_default().push((parts[2].to_string(), path.to_path_buf()));
        }
    }

    let mut removed_old = 0usize;
    let mut removed_uninst = 0usize;

    for (pkg_key, mut entries) in pkg_map {
        entries.sort_by(|a, b| {
            if overlay::version_ge(&a.0, &b.0) { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater }
        });

        let (_newest_evr, newest_path) = &entries[0];
        for (_, old_path) in entries.iter().skip(1) {
            println!("  Removing old version: {}", old_path.file_name().unwrap_or_default().to_string_lossy());
            std::fs::remove_file(old_path).ok();
            removed_old += 1;
        }

        let pkg_name = pkg_key.split('.').next().unwrap_or("");
        let pkg_arch = pkg_key.split('.').last().unwrap_or("");
        let query = format!("{pkg_name}.{pkg_arch}");
        let installed = overlay::run_capture_ok("rpm", &["--dbpath", db, "-q", &query]);
        if installed.trim().is_empty() || installed.contains("not installed") {
            println!("  Removing uninstalled: {}", newest_path.file_name().unwrap_or_default().to_string_lossy());
            std::fs::remove_file(newest_path).ok();
            removed_uninst += 1;
        }
    }

    // Remove empty dirs
    for entry in WalkDir::new(cache).min_depth(1).into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
    {
        std::fs::remove_dir(entry.path()).ok();
    }

    println!("RakuOS cache clean: DNF — removed {removed_old} old, {removed_uninst} uninstalled.");
    Ok(())
}

fn clean_local_cache(db: &str) -> Result<()> {
    let cache = std::path::Path::new(LOCAL_RPM_CACHE);
    if !cache.exists() { return Ok(()); }
    println!("RakuOS cache clean: scanning local-RPM cache...");

    let mut removed_uninst = 0usize;
    let mut removed_orphaned = 0usize;

    let tracked: Vec<String> = read_packages_list(LOCAL_RPM_LIST);
    let mut new_tracked: Vec<String> = vec![];

    for pkg in &tracked {
        let installed = overlay::run_capture_ok("rpm", &["--dbpath", db, "-q", pkg]);
        if !installed.trim().is_empty() && !installed.contains("not installed") {
            new_tracked.push(pkg.clone());
        } else {
            println!("  Removing uninstalled local RPM: {pkg}");
            std::fs::remove_file(format!("{LOCAL_RPM_CACHE}/{pkg}.rpm")).ok();
            removed_uninst += 1;
        }
    }
    write_packages_list(LOCAL_RPM_LIST, &new_tracked)?;

    // Remove orphaned RPMs not in list
    for entry in WalkDir::new(cache).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.extension().map(|e| e == "rpm").unwrap_or(false) { continue; }
        let name = overlay::run_capture_ok("rpm", &["-qp", "--qf", "%{NAME}", path.to_str().unwrap_or("")]);
        let name = name.trim();
        if new_tracked.iter().any(|p| p == name) { continue; }
        println!("  Removing orphaned RPM: {}", path.file_name().unwrap_or_default().to_string_lossy());
        std::fs::remove_file(path).ok();
        removed_orphaned += 1;
    }

    println!("RakuOS cache clean: local — removed {removed_uninst} uninstalled, {removed_orphaned} orphaned.");
    Ok(())
}
