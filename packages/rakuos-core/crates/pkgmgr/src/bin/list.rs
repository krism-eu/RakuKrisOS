use rakuos_pkgmgr::*;

fn main() {
    let pkgs = read_packages_list(PACKAGES_LIST);
    let rpm_pkgs = read_packages_list(LOCAL_RPM_LIST);
    if pkgs.is_empty() && rpm_pkgs.is_empty() {
        println!("No packages installed via RakuOS overlay.");
        return;
    }
    println!("RakuOS overlay packages:\n");
    for pkg in &pkgs {
        println!("  \u{2022} {pkg}");
    }
    for pkg in &rpm_pkgs {
        println!("  \u{2022} {pkg}  (local RPM)");
    }
}
