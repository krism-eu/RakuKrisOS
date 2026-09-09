use rum_core::Package;
use rum_overlay::OverlayContext;
use rum_repo::main_config::DEFAULT_INSTALLONLYPKGS;
use rum_resolver::{resolve_ex, ResolveOptions};

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
    let matches: Vec<&Package> = candidates.iter().filter(|p| p.nevra.name == "libavcodec").collect();
    eprintln!("libavcodec candidates: {}", matches.len());
    for m in &matches {
        eprintln!("  {} repo={}", m.nevra, m.repo_id);
    }

    // Simulate the real overlay state right after `rum swap -y ffmpeg-free
    // ffmpeg --allowerasing` then `rum install -y libfdk-aac`, then the
    // *remove* half of `rum swap -y libavcodec-free libavcodec
    // --allowerasing` (nodeps erase of libavcodec-free), just before its
    // install half runs.
    let ffmpeg = candidates.iter().find(|p| p.nevra.name == "ffmpeg" && p.nevra.arch == "x86_64").cloned().unwrap();
    let libfdk = candidates.iter().find(|p| p.nevra.name == "libfdk-aac" && p.nevra.arch == "x86_64").cloned().unwrap();
    let kernel_core = candidates.iter().find(|p| p.nevra.name == "kernel-p03-v2-core" && p.nevra.arch == "x86_64").cloned();
    let mut overlay_pkgs = vec![ffmpeg, libfdk];
    if let Some(k) = kernel_core {
        overlay_pkgs.push(k);
    }
    eprintln!("overlay pkgs: {:?}", overlay_pkgs.iter().map(|p| p.nevra.to_string()).collect::<Vec<_>>());

    let overlay = OverlayContext::for_test(vec![], overlay_pkgs);
    let opts = ResolveOptions {
        install_weak_deps: false,
        installonly_pkgs: DEFAULT_INSTALLONLYPKGS.iter().map(|s| s.to_string()).collect(),
        allow_erasing: true,
        force_names: Default::default(),
        protected_names: Vec::new(),
        ..Default::default()
    };

    match resolve_ex(&["libavcodec".to_string()], &candidates, &overlay, "x86_64", &opts) {
        Ok(plan) => println!("OK: {} to install", plan.to_install.len()),
        Err(e) => println!("FAILED: {e:#}"),
    }
}
