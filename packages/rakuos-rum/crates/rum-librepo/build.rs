fn main() {
    let librepo = pkg_config::Config::new().probe("librepo").expect("librepo not found via pkg-config (dnf install librepo-devel / equivalent)");
    let glib = pkg_config::Config::new().probe("glib-2.0").expect("glib-2.0 not found via pkg-config (dnf install glib2-devel / equivalent)");

    for path in librepo.link_paths.iter().chain(&glib.link_paths) {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for name in librepo.libs.iter().chain(&glib.libs) {
        println!("cargo:rustc-link-lib={name}");
    }
}
