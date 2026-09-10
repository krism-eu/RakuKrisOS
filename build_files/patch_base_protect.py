#!/usr/bin/python3
"""Apply reviewed RakuKrisOS safety fixes to pinned RakuOS base_protect.rs."""

from pathlib import Path

SOURCE = Path("/src/base-protect/src/main.rs")


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one upstream match, found {count}")
    return text.replace(old, new, 1)


source = SOURCE.read_text()

source = replace_once(
    source,
    """    // ── Shadow file handling ──────────────────────────────────────────────────
    if !meta.is_file() { return; }

    // Allow alternatives symlinks
    if meta.file_type().is_symlink() {
        if let Ok(target) = fs::read_link(overlay_path) {
            if target.starts_with("/etc/alternatives/") {
                return;
            }
        }
    }

    let rel = match overlay_path.strip_prefix(UPPER_DIR) {
        Ok(r) => r.to_string_lossy().to_string(),
        Err(_) => return,
    };
    let base_path = format!("/usr/{}", rel.trim_start_matches('/'));
""",
    """    // ── Shadow file handling ──────────────────────────────────────────────────
    // symlink_metadata() does not follow links. The old regular-file-only
    // guard made the alternatives exception unreachable and let every other
    // symlink shadow a protected base file indefinitely.
    if !meta.is_file() && !meta.file_type().is_symlink() { return; }

    // Allow only the alternatives symlinks expected from package scripts.
    if meta.file_type().is_symlink() {
        if let Ok(target) = fs::read_link(overlay_path) {
            if target.starts_with("/etc/alternatives/") {
                return;
            }
        }
    }

    let rel = match overlay_path.strip_prefix(UPPER_DIR) {
        Ok(r) => r.to_string_lossy().to_string(),
        Err(_) => return,
    };
    let base_path = format!("/usr/{}", rel.trim_start_matches('/'));
""",
    "symlink protection",
)

source = replace_once(
    source,
    """    fn add_watch(
        inotify: &mut Inotify,
        path: &Path,
        mask: WatchMask,
        watched: &mut HashMap<WatchDescriptor, PathBuf>,
    ) {
        if let Ok(wd) = inotify.watches().add(path, mask) {
            watched.insert(wd, path.to_path_buf());
        }
    }

    fn add_watches_recursive(
        inotify: &mut Inotify,
        root: &Path,
        mask: WatchMask,
        watched: &mut HashMap<WatchDescriptor, PathBuf>,
    ) {
        for entry in walkdir::WalkDir::new(root).min_depth(1).into_iter().flatten() {
            if entry.file_type().is_dir() && !watched.values().any(|p| p == entry.path()) {
                add_watch(inotify, entry.path(), mask, watched);
            }
        }
    }

    add_watch(&mut inotify, Path::new(UPPER_DIR), mask, &mut watched);
    add_watches_recursive(&mut inotify, Path::new(UPPER_DIR), mask, &mut watched);
""",
    """    fn add_watch(
        inotify: &mut Inotify,
        path: &Path,
        mask: WatchMask,
        watched: &mut HashMap<WatchDescriptor, PathBuf>,
    ) -> Result<()> {
        let wd = inotify.watches().add(path, mask)?;
        watched.insert(wd, path.to_path_buf());
        Ok(())
    }

    fn add_watches_recursive(
        inotify: &mut Inotify,
        root: &Path,
        mask: WatchMask,
        watched: &mut HashMap<WatchDescriptor, PathBuf>,
    ) -> Result<()> {
        for entry in walkdir::WalkDir::new(root).min_depth(1) {
            let entry = entry?;
            if entry.file_type().is_dir() && !watched.values().any(|p| p == entry.path()) {
                add_watch(inotify, entry.path(), mask, watched)?;
            }
        }
        Ok(())
    }

    fn scan_existing(
        root: &Path,
        manifest: &HashSet<String>,
        debouncer: &Debouncer,
    ) -> Result<()> {
        for entry in walkdir::WalkDir::new(root).min_depth(1) {
            let entry = entry?;
            if !entry.file_type().is_dir() {
                handle_event(entry.path(), manifest, debouncer);
            }
        }
        Ok(())
    }

    // Watches are installed first: changes racing with the startup scan stay
    // queued for the event loop. Then pre-existing protected shadows are removed.
    add_watch(&mut inotify, Path::new(UPPER_DIR), mask, &mut watched)?;
    add_watches_recursive(&mut inotify, Path::new(UPPER_DIR), mask, &mut watched)?;
    scan_existing(Path::new(UPPER_DIR), &manifest, &debouncer)?;
""",
    "startup scan and fallible watches",
)

source = replace_once(
    source,
    """            // If a new directory was created, add a watch for it too
            if event.mask.contains(EventMask::CREATE) && full_path.is_dir() {
                if !watched.values().any(|p| p == &full_path) {
                    add_watch(&mut inotify, &full_path, mask, &mut watched);
                    add_watches_recursive(&mut inotify, &full_path, mask, &mut watched);
                }
                continue;
            }
""",
    """            // A populated directory may arrive in one MOVED_TO event. Watch
            // and scan it immediately; CREATE alone misses that entire tree.
            let is_new_directory =
                (event.mask.contains(EventMask::CREATE)
                    || event.mask.contains(EventMask::MOVED_TO))
                && full_path
                    .symlink_metadata()
                    .map(|m| m.file_type().is_dir())
                    .unwrap_or(false);
            if is_new_directory {
                if !watched.values().any(|p| p == &full_path) {
                    add_watch(&mut inotify, &full_path, mask, &mut watched)?;
                    add_watches_recursive(&mut inotify, &full_path, mask, &mut watched)?;
                }
                scan_existing(&full_path, &manifest, &debouncer)?;
                continue;
            }
""",
    "moved directory scan",
)

source = replace_once(
    source,
    'eprintln!("rakuos-base-protect: loaded manifest: {} files", manifest.len());',
    'eprintln!("rakuos-base-protect: RakuKrisOS hardened build; loaded manifest: {} files", manifest.len());',
    "build marker",
)

SOURCE.write_text(source)
