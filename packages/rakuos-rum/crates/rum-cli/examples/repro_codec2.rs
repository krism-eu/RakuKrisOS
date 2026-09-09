use rum_core::Package;
use rum_overlay::OverlayContext;
use rum_repo::main_config::DEFAULT_INSTALLONLYPKGS;
use rum_resolver::{resolve_ex, ResolveOptions};

fn resolve_and_extend(step: &str, names: &[&str], candidates: &[Package], overlay_pkgs: &mut Vec<Package>) {
    let overlay = OverlayContext::for_test(vec![], overlay_pkgs.clone());
    let opts = ResolveOptions {
        install_weak_deps: false,
        installonly_pkgs: DEFAULT_INSTALLONLYPKGS.iter().map(|s| s.to_string()).collect(),
        allow_erasing: true,
        force_names: Default::default(),
        protected_names: Vec::new(),
        ..Default::default()
    };
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    match resolve_ex(&names, candidates, &overlay, "x86_64", &opts) {
        Ok(plan) => {
            eprintln!("[{step}] OK: {} to install", plan.to_install.len());
            for pkg in &plan.to_install {
                overlay_pkgs.push(pkg.clone());
            }
        }
        Err(e) => {
            eprintln!("[{step}] FAILED: {e:#}");
            std::process::exit(1);
        }
    }
}

fn main() {
    let dirs = [
        "/tmp/rum-debug-cache/rakuos-8be3a0ea573d3de7",
        "/tmp/rum-debug-cache/fedora-e08b18de31fedb3b",
        "/tmp/rum-debug-cache/fedora-cisco-openh264-f010256ac5b3137e",
        "/tmp/rum-debug-cache/rpmfusion-free-39860f5959b7d038",
        "/tmp/rum-debug-cache/rpmfusion-free-updates-0c1c856e51f4cb91",
        "/tmp/rum-debug-cache/rpmfusion-nonfree-089b8aa91d2d9b06",
        "/tmp/rum-debug-cache/rpmfusion-nonfree-updates-517de50441060c24",
        "/tmp/rum-debug-cache/terra-a0bbee2e515a1f19",
        "/tmp/rum-debug-cache/terra-mesa-d6d953ffadf1910b",
        "/tmp/rum-debug-cache/updates-archive-c5693a4ce63cff64",
        "/tmp/rum-debug-cache/updates-dca6061ccf88c300",
    ];

    let mut candidates: Vec<Package> = Vec::new();
    for dir in dirs {
        let path = std::path::Path::new(dir).join("primary.xml");
        let repo_id = dir.rsplit('/').next().unwrap();
        if let Ok(mut pkgs) = rum_repo::load_primary_file(&path, repo_id) {
            candidates.append(&mut pkgs);
        }
    }
    eprintln!("total candidates: {}", candidates.len());

    // Base image ships Fedora's default *-free multimedia stack already
    // installed (that's the entire point of `rum swap X-free X`) — seed the
    // overlay with that pre-existing state before simulating the rest of
    // build.sh, instead of starting from nothing.
    let base_free_names = ["ffmpeg-free", "libavcodec-free", "libavformat-free", "libavutil-free", "libavfilter-free", "libswscale-free", "libpostproc-free", "libavdevice-free", "libavresample-free", "libswresample-free"];
    let mut overlay_pkgs: Vec<Package> = Vec::new();
    for name in base_free_names {
        if let Some(p) = candidates.iter().find(|p| p.nevra.name == name && p.nevra.arch == "x86_64") {
            overlay_pkgs.push(p.clone());
        } else {
            eprintln!("warning: no candidate found for base package {name}");
        }
    }
    eprintln!("seeded base overlay: {:?}", overlay_pkgs.iter().map(|p| p.nevra.to_string()).collect::<Vec<_>>());

    // Mirrors build.sh's sequence up through the failing swap, using the
    // resolver itself at each step (rather than a guessed package list) so
    // the overlay accumulates real transitive deps, including whatever
    // multilib/i686 packages the kernel-devel install actually pulls in.
    resolve_and_extend("kernel-p03-v2", &["kernel-p03-v2", "kernel-p03-v2-devel", "kernel-p03-v2-devel-matched"], &candidates, &mut overlay_pkgs);
    resolve_and_extend("extras", &["intel-vaapi-driver", "libopenjph", "pipewire-libs-extra", "heif-pixbuf-loader"], &candidates, &mut overlay_pkgs);

    // rum swap -y ffmpeg-free ffmpeg --allowerasing: remove half (--nodeps,
    // no cascade) then install half.
    overlay_pkgs.retain(|p| p.nevra.name != "ffmpeg-free");
    resolve_and_extend("ffmpeg (install half of swap)", &["ffmpeg"], &candidates, &mut overlay_pkgs);

    resolve_and_extend("libfdk-aac", &["libfdk-aac"], &candidates, &mut overlay_pkgs);

    // rum swap -y libavcodec-free libavcodec --allowerasing: remove half
    // then install half — this is the one that fails in CI.
    overlay_pkgs.retain(|p| p.nevra.name != "libavcodec-free");
    eprintln!("overlay size before libavcodec install half: {}", overlay_pkgs.len());
    resolve_and_extend("libavcodec (install half of swap)", &["libavcodec"], &candidates, &mut overlay_pkgs);
}
