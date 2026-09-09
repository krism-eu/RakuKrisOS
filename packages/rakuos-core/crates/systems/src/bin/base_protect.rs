/// rakuos-base-protect — Monitor overlay upper dir for shadowing of base /usr files.
/// Runs as a daemon, removes shadowed/whiteout files immediately and notifies users.
use anyhow::{Result, bail};
use inotify::{Inotify, WatchDescriptor, WatchMask, EventMask};
use rakuos_overlay as overlay;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const UPPER_DIR: &str = "/var/lib/rakuos/overlay/upper";
const MANIFEST: &str = "/usr/share/rakuos/base-manifest.txt";

const EXCLUSION_PREFIXES: &[&str] = &[
    "/usr/share/doc",
    "/usr/share/licenses",
    "/usr/lib/.build-id",
    "/usr/lib64/.build-id",
    "/usr/lib/python",
    "/usr/lib64/python",
];

const EXCLUSIONS: &[&str] = &[
    "/usr/share/applications/mimeinfo.cache",
    "/usr/share/glib-2.0/schemas/gschemas.compiled",
    "/usr/bin/sudo",
    "/usr/bin/su",
    "/usr/bin/visudo",
];

fn is_excluded(base_path: &str) -> bool {
    if EXCLUSIONS.contains(&base_path) {
        return true;
    }
    for prefix in EXCLUSION_PREFIXES {
        if base_path.starts_with(prefix) {
            return true;
        }
    }
    // Python __pycache__ glob equivalents
    if (base_path.contains("/lib/python") || base_path.contains("/lib64/python"))
        && (base_path.contains("/__pycache__/") || base_path.contains("/site-packages/"))
    {
        return true;
    }
    // "/usr/share/icons/*/icon-theme.cache" glob equivalent — every icon
    // theme's own cache, not just hicolor's.
    if base_path.starts_with("/usr/share/icons/") && base_path.ends_with("/icon-theme.cache") {
        return true;
    }
    false
}

fn is_overlay_operation() -> bool {
    Path::new(overlay::LOCK_FILE).exists()
}

fn load_manifest(path: &str) -> Result<HashSet<String>> {
    let content = fs::read_to_string(path)?;
    Ok(content.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

fn notify_user(title: &str, body: &str) {
    let uids = overlay::logged_in_uids();
    overlay::notify_users(&uids, title, body);
}

// Debounced notifier — collects blocked paths for 3s then sends one notification
struct Debouncer {
    pending: Arc<Mutex<Vec<String>>>,
    last_event: Arc<Mutex<Instant>>,
}

impl Debouncer {
    fn new() -> Self {
        let pending = Arc::new(Mutex::new(Vec::<String>::new()));
        let last_event = Arc::new(Mutex::new(Instant::now()));
        let p2 = Arc::clone(&pending);
        let l2 = Arc::clone(&last_event);

        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let elapsed = l2.lock().unwrap().elapsed();
                if elapsed < Duration::from_secs(3) {
                    continue;
                }
                let mut items = p2.lock().unwrap();
                if items.is_empty() {
                    continue;
                }
                items.sort();
                items.dedup();
                let count = items.len();
                let preview: Vec<String> = items.iter().take(5).cloned().collect();
                items.clear();
                drop(items);

                let list = preview.join("\n");
                let (title, body) = if count == 1 {
                    (
                        "Base System Protection".to_string(),
                        format!("A modification to a base system file was blocked:\n\n{list}\n\nBase files are managed by bootc update."),
                    )
                } else {
                    (
                        "Base System Protection".to_string(),
                        format!("{count} modifications to base system files were blocked:\n\n{list}\n\nBase files are managed by bootc update."),
                    )
                };
                notify_user(&title, &body);
            }
        });

        Self { pending, last_event }
    }

    fn push(&self, path: String) {
        self.pending.lock().unwrap().push(path);
        *self.last_event.lock().unwrap() = Instant::now();
    }
}

fn handle_event(
    overlay_path: &Path,
    manifest: &HashSet<String>,
    debouncer: &Debouncer,
) {
    // Only act on regular files
    let meta = match overlay_path.symlink_metadata() {
        Ok(m) => m,
        Err(_) => return,
    };

    let filename = overlay_path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // ── Whiteout handling ─────────────────────────────────────────────────────
    if filename.starts_with(".wh.") {
        let real_name = &filename[4..];
        let parent_rel = overlay_path.parent()
            .and_then(|p| p.strip_prefix(UPPER_DIR).ok())
            .map(|r| r.to_string_lossy().to_string())
            .unwrap_or_default();

        // Opaque whiteout — directory deletion
        if real_name == ".wh..opq" {
            let base_path = format!("/usr/{}", parent_rel.trim_start_matches('/'));
            if manifest.iter().any(|m| m.starts_with(&base_path)) && !is_overlay_operation() {
                eprintln!("rakuos-base-protect: BLOCKED opaque whiteout at {}", overlay_path.display());
                fs::remove_file(overlay_path).ok();
                touch_parent(overlay_path);
                debouncer.push(base_path);
            }
            return;
        }

        // Regular whiteout
        let base_path = format!("/usr/{}/{}", parent_rel.trim_start_matches('/'), real_name);
        let base_path = base_path.replace("//", "/");
        if !manifest.contains(&base_path) { return; }
        if is_excluded(&base_path) { return; }
        if is_overlay_operation() { return; }

        eprintln!("rakuos-base-protect: BLOCKED deletion of base file: {base_path}");
        fs::remove_file(overlay_path).ok();
        touch_parent(overlay_path);
        debouncer.push(base_path);
        return;
    }

    // ── Shadow file handling ──────────────────────────────────────────────────
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

    if !manifest.contains(&base_path) { return; }
    if is_excluded(&base_path) { return; }
    if is_overlay_operation() {
        eprintln!("rakuos-base-protect: overlay operation in progress — allowing write to {base_path}");
        return;
    }

    eprintln!("rakuos-base-protect: BLOCKED shadow of base file: {}", overlay_path.display());
    fs::remove_file(overlay_path).ok();
    touch_parent(overlay_path);
    debouncer.push(base_path);
}

fn touch_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        overlay::run_best_effort("touch", &[parent.to_str().unwrap_or("")]);
    }
}

fn main() -> Result<()> {
    if !Path::new(MANIFEST).exists() {
        bail!("base manifest not found at {MANIFEST} — run generate-base-manifest first");
    }

    let manifest = load_manifest(MANIFEST)?;
    eprintln!("rakuos-base-protect: loaded manifest: {} files", manifest.len());

    // Wait for overlay upper to exist
    while !Path::new(UPPER_DIR).exists() {
        eprintln!("rakuos-base-protect: waiting for overlay upper dir...");
        std::thread::sleep(Duration::from_secs(2));
    }

    eprintln!("rakuos-base-protect: watching {UPPER_DIR}");

    let debouncer = Debouncer::new();

    let mut inotify = Inotify::init()?;
    let mask = WatchMask::CREATE | WatchMask::MOVED_TO | WatchMask::CLOSE_WRITE;

    // wd -> absolute path, so a full path can be reconstructed from an event's
    // watch descriptor + name in O(1). Previously this scanned the entire upper
    // dir tree with walkdir on every single inotify event, which pegged a CPU
    // core under any real write load (package installs, browser caches, etc).
    let mut watched: HashMap<WatchDescriptor, PathBuf> = HashMap::new();

    fn add_watch(
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

    let mut buffer = [0u8; 4096];
    loop {
        let events = inotify.read_events_blocking(&mut buffer)?;
        for event in events {
            let name = match event.name {
                Some(n) => n.to_string_lossy().to_string(),
                None => continue,
            };

            let Some(dir) = watched.get(&event.wd) else { continue };
            let full_path = dir.join(&name);

            // If a new directory was created, add a watch for it too
            if event.mask.contains(EventMask::CREATE) && full_path.is_dir() {
                if !watched.values().any(|p| p == &full_path) {
                    add_watch(&mut inotify, &full_path, mask, &mut watched);
                    add_watches_recursive(&mut inotify, &full_path, mask, &mut watched);
                }
                continue;
            }

            handle_event(&full_path, &manifest, &debouncer);
        }
    }
}
