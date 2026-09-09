/// rakuos-user: per-user session setup. Run from the rakuos-user.service systemd user unit.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use rakuos_overlay as overlay;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".to_string()))
}

fn username() -> String {
    std::env::var("USER").unwrap_or_else(|_| {
        overlay::run_capture_ok("id", &["-un"]).trim().to_string()
    })
}

fn gsettings_get(schema: &str, key: &str) -> String {
    overlay::run_capture_ok("gsettings", &["get", schema, key])
        .trim()
        .trim_matches('\'')
        .to_string()
}

fn gsettings_set(schema: &str, key: &str, value: &str) {
    overlay::run_best_effort("gsettings", &["set", schema, key, value]);
}

fn main() {
    let home = home();
    let user = username();
    let rakuos_cfg = home.join(".config/rakuos");
    fs::create_dir_all(&rakuos_cfg).ok();

    // ── First-run setup ───────────────────────────────────────────────────────
    let marker = rakuos_cfg.join(".firstrun-done");
    if !marker.exists() {
        first_run_setup(&home, &user);
        fs::write(&marker, "").ok();
    }

    // ── Sync system themes for Flatpak access ─────────────────────────────────
    sync_themes(&home, &user);

    // ── GTK theme management ──────────────────────────────────────────────────
    manage_gtk_theme(&home, &rakuos_cfg);

    // ── Fish shell setup ──────────────────────────────────────────────────────
    setup_fish_shell(&user, &rakuos_cfg);

    // ── Live environment tweaks ───────────────────────────────────────────────
    setup_live_env(&home);

    // ── Signal overlay update ready ───────────────────────────────────────────
    signal_overlay_update();

    // ── Queued notifications ──────────────────────────────────────────────────
    dispatch_queued_notification();

    println!("RakuOS user setup complete.");
}

fn first_run_setup(home: &Path, _user: &str) {
    let icons_dir = home.join(".local/share/icons");
    fs::create_dir_all(&icons_dir).ok();
    let icons_link = home.join(".icons");
    if !icons_link.exists() {
        std::os::unix::fs::symlink(&icons_dir, &icons_link).ok();
    }

    // Flatpak global override — grant access to theme dirs
    let override_path = home.join(".local/share/flatpak/overrides/global");
    fs::create_dir_all(override_path.parent().unwrap()).ok();
    fs::write(
        &override_path,
        "[Context]\nfilesystems=xdg-config/gtk-3.0:ro;xdg-config/gtk-4.0:ro;~/.themes;~/.icons;xdg-data/icons\n",
    ).ok();
}

fn sync_themes(home: &Path, user: &str) {
    let system_themes = Path::new("/usr/share/themes");
    let user_themes = home.join(".themes");
    fs::create_dir_all(&user_themes).ok();

    let exclude = ["Clearlooks", "Crux", "HighContrast", "Industrial", "Mist", "Raleigh", "ThinIce"];

    if let Ok(entries) = fs::read_dir(system_themes) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if exclude.iter().any(|e| *e == name_str.as_ref()) {
                continue;
            }
            let dest = user_themes.join(&name);
            overlay::run_best_effort("rsync", &[
                "-a", "--copy-links",
                entry.path().to_str().unwrap_or(""),
                dest.to_str().unwrap_or(""),
            ]);
        }
    }

    // Fix ownership
    overlay::run_best_effort("chown", &[
        "-R", &format!("{user}:{user}"),
        user_themes.to_str().unwrap_or(""),
    ]);
}

fn manage_gtk_theme(home: &Path, rakuos_cfg: &Path) {
    let gtk4_css = home.join(".config/gtk-4.0/gtk.css");
    let gtk3_cfg = home.join(".config/gtk-3.0/settings.ini");
    fs::create_dir_all(home.join(".config/gtk-4.0")).ok();
    fs::create_dir_all(home.join(".config/gtk-3.0")).ok();

    // Kill stale watcher
    let watcher_pid_file = rakuos_cfg.join("gtk-theme-watcher.pid");
    if let Ok(pid_str) = fs::read_to_string(&watcher_pid_file) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            unsafe { libc::kill(pid, libc::SIGTERM); }
        }
        fs::remove_file(&watcher_pid_file).ok();
    }

    // Determine active theme
    let mut active_theme = gsettings_get("org.gnome.desktop.interface", "gtk-theme");
    if active_theme.is_empty() {
        // KDE fallback
        active_theme = overlay::run_capture_ok("sh", &[
            "-c",
            "grep -m1 'gtk-theme-name' ~/.config/kdeglobals 2>/dev/null | cut -d= -f2 | tr -d ' '"
        ]).trim().to_string();
    }

    // Re-derive dark/light from color-scheme if we own the theme
    if active_theme.starts_with("OrigamiPaper") {
        active_theme = origami_for_scheme();
        gsettings_set("org.gnome.desktop.interface", "gtk-theme", &active_theme);
    }

    apply_theme(&active_theme, &gtk4_css, &gtk3_cfg, home);

    // Spawn background watcher for GNOME/COSMIC theme changes
    let gtk4_css_str = gtk4_css.to_str().unwrap_or("").to_string();
    let home_themes = home.join(".themes").to_str().unwrap_or("").to_string();

    if let Ok(child) = Command::new("sh")
        .arg("-c")
        .arg(format!(
            r#"gsettings monitor org.gnome.desktop.interface 2>/dev/null | \
            while IFS= read -r line; do
                if [[ "$line" == gtk-theme* ]]; then
                    theme=$(printf '%s' "$line" | sed "s/gtk-theme: '//;s/'.*//")
                    # apply gtk4 symlink
                    src="{home_themes}/${{theme}}/gtk-4.0/gtk.css"
                    if [[ "$theme" == OrigamiPaper* ]] && [[ -f "$src" ]]; then
                        ln -sf "$src" "{gtk4_css}"
                    elif [[ -L "{gtk4_css}" ]] && [[ "$(readlink "{gtk4_css}")" == *OrigamiPaper* ]]; then
                        rm -f "{gtk4_css}"
                    fi
                elif [[ "$line" == color-scheme* ]]; then
                    cur=$(gsettings get org.gnome.desktop.interface gtk-theme 2>/dev/null | tr -d "'")
                    if [[ "$cur" == OrigamiPaper* ]]; then
                        scheme=$(printf '%s' "$line" | sed "s/color-scheme: '//;s/'.*//")
                        new=$([[ "$scheme" == "prefer-dark" ]] && echo "OrigamiPaper" || echo "OrigamiPaperLight")
                        gsettings set org.gnome.desktop.interface gtk-theme "$new" 2>/dev/null || true
                    fi
                fi
            done"#,
            home_themes = home_themes,
            gtk4_css = gtk4_css_str,
        ))
        .spawn()
    {
        fs::write(&watcher_pid_file, child.id().to_string()).ok();
    }

    // KDE inotify watcher for gtk-4.0/settings.ini
    let gtk4_settings = home.join(".config/gtk-4.0/settings.ini");
    if gtk4_settings.exists() {
        Command::new("sh")
            .arg("-c")
            .arg(format!(
                r#"command -v inotifywait &>/dev/null && \
                inotifywait -m -e close_write "{path}" 2>/dev/null | \
                while IFS= read -r _; do
                    theme=$(grep -m1 'gtk-theme-name' "{path}" 2>/dev/null | cut -d= -f2 | tr -d ' ')
                    src="{home_themes}/${{theme}}/gtk-4.0/gtk.css"
                    if [[ "$theme" == OrigamiPaper* ]] && [[ -f "$src" ]]; then
                        ln -sf "$src" "{gtk4_css}"
                    fi
                done"#,
                path = gtk4_settings.to_str().unwrap_or(""),
                home_themes = home_themes,
                gtk4_css = gtk4_css_str,
            ))
            .spawn().ok();
    }
}

fn apply_theme(theme: &str, gtk4_css: &Path, gtk3_cfg: &Path, home: &Path) {
    if theme.is_empty() { return; }

    // GTK4 symlink
    let gtk4_src = home.join(format!(".themes/{}/gtk-4.0/gtk.css", theme));
    if theme.starts_with("OrigamiPaper") && gtk4_src.exists() {
        fs::remove_file(gtk4_css).ok();
        std::os::unix::fs::symlink(&gtk4_src, gtk4_css).ok();
    } else if gtk4_css.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
        if fs::read_link(gtk4_css).map(|t| t.to_string_lossy().contains("OrigamiPaper")).unwrap_or(false) {
            fs::remove_file(gtk4_css).ok();
        }
    }

    // GTK3 settings.ini
    if theme.starts_with("OrigamiPaper") {
        let entry = format!("gtk-theme-name={}", theme);
        if let Ok(content) = fs::read_to_string(gtk3_cfg) {
            let new = if content.contains("gtk-theme-name=") {
                content.lines()
                    .map(|l| if l.starts_with("gtk-theme-name=") { entry.as_str() } else { l })
                    .collect::<Vec<_>>().join("\n") + "\n"
            } else if content.contains("[Settings]") {
                content.replacen("[Settings]", &format!("[Settings]\n{}", entry), 1)
            } else {
                format!("{}\n{}\n", content, entry)
            };
            fs::write(gtk3_cfg, new).ok();
        } else {
            fs::write(gtk3_cfg, format!("[Settings]\n{}\n", entry)).ok();
        }
    }

    // Icon theme
    if theme.starts_with("OrigamiPaper") {
        let icon = if theme.contains("Light") { "WhiteSur-light" } else { "WhiteSur-dark" };
        gsettings_set("org.gnome.desktop.interface", "icon-theme", icon);
    }
}

fn origami_for_scheme() -> String {
    let scheme = gsettings_get("org.gnome.desktop.interface", "color-scheme");
    if scheme == "prefer-dark" { "OrigamiPaper".to_string() } else { "OrigamiPaperLight".to_string() }
}

fn setup_fish_shell(user: &str, rakuos_cfg: &Path) {
    if !Path::new("/etc/fish/conf.d/rakuos-aliases.fish").exists() { return; }
    if !Path::new("/usr/bin/fish").exists() { return; }
    if rakuos_cfg.join("keep-shell").exists() { return; }

    let current_shell = overlay::run_capture_ok("getent", &["passwd", user])
        .split(':').nth(6).unwrap_or("").trim().to_string();
    if current_shell != "/usr/bin/fish" {
        overlay::run_best_effort("chsh", &["-s", "/usr/bin/fish"]);
    }
}

fn setup_live_env(home: &Path) {
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    if !cmdline.contains("rd.live") { return; }

    // niri has no wayland-sessions .desktop entry for livesys to key off of,
    // so dotfiles-setup.service (a systemd --user unit like this one) never
    // gets a chance to run under greetd's autologin. Run it here too.
    if Path::new("/usr/libexec/dotfiles-setup.sh").exists() {
        overlay::run_best_effort("/usr/libexec/dotfiles-setup.sh", &[]);
    }

    // COSMIC: add installer to favorites
    let cosmic_favorites = home.join(".config/cosmic/com.system76.CosmicAppList/v1/favorites");
    if Path::new("/usr/share/cosmic/com.system76.CosmicAppList/v1/favorites").exists() {
        fs::create_dir_all(cosmic_favorites.parent().unwrap()).ok();
        fs::write(&cosmic_favorites, r#"[
  "org.rakuos.Installer.Cosmic",
  "org.mozilla.firefox",
  "com.system76.CosmicFiles",
  "com.system76.CosmicTerm",
  "com.system76.CosmicSettings",
  "org.rakuos.Software"
]
"#).ok();
    }

    // KDE: disable KWallet so installer doesn't prompt
    let is_kde = Path::new("/usr/share/plasma/plasmoids/org.kde.plasma.taskmanager/metadata.json").exists()
        || overlay::run_capture_ok("sh", &["-c", "command -v plasmashell"]).contains("plasmashell");
    if is_kde {
        let kwalletrc = home.join(".config/kwalletrc");
        fs::create_dir_all(kwalletrc.parent().unwrap()).ok();
        fs::write(&kwalletrc, "[Wallet]\nEnabled=false\n").ok();
    }
}

fn signal_overlay_update() {
    let uid = unsafe { libc::getuid() };
    if uid < 1000 { return; }
    if Path::new("/var/lib/rakuos/image-update.marker").exists() {
        fs::write(overlay::USER_READY_FILE, "").ok();
    }
}

fn dispatch_queued_notification() {
    let notify_file = Path::new(overlay::NOTIFY_FILE);
    if !notify_file.exists() { return; }
    let content = fs::read_to_string(notify_file).unwrap_or_default();
    let mut lines = content.lines();
    let title = lines.next().unwrap_or("RakuOS");
    let body = lines.collect::<Vec<_>>().join("\n");
    fs::remove_file(notify_file).ok();
    // Small delay so the desktop session is fully ready
    std::thread::sleep(std::time::Duration::from_secs(3));
    overlay::run_best_effort("notify-send",
        &["--app-name=RakuOS", "--urgency=normal", title, &body]);
}
