/// rakupkg — user-facing dispatcher for RakuOS package management subcommands.
use std::process::{self, Command};

const LIBEXEC: &str = "/usr/libexec/rakuos";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("");
    let rest = &args[args.len().min(2)..];

    let ok = match command {
        "install" => run(&format!("{LIBEXEC}/rakuos-install"), rest),
        "update" => run(&format!("{LIBEXEC}/rakuos-update"), rest),
        "remove" => run(&format!("{LIBEXEC}/rakuos-remove"), rest),
        "upgrade" => {
            run(&format!("{LIBEXEC}/rakuos-update"), &[])
                && run(&format!("{LIBEXEC}/rakuos-update"), &["upgrade-flatpak".to_string()])
                && run(&format!("{LIBEXEC}/rakuos-update"), &["upgrade-image".to_string()])
        }
        "system-upgrade" => run(&format!("{LIBEXEC}/rakuos-update"), &["upgrade-image".to_string()]),
        "list" => run(&format!("{LIBEXEC}/rakuos-list"), &[]),
        _ => {
            print_usage();
            process::exit(1);
        }
    };

    if !ok {
        process::exit(1);
    }
}

fn run(bin: &str, args: &[String]) -> bool {
    Command::new(bin)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn print_usage() {
    println!("RakuOS Package management");
    println!();
    println!("Usage:");
    println!("  rakupkg install <package> [pkg2...]   Install packages via overlay");
    println!("  rakupkg update                        Update packages via overlay");
    println!("  rakupkg remove <package> [pkg2...]    Remove packages from overlay");
    println!("  rakupkg list                          List overlay installed packages");
    println!("  rakupkg upgrade                       Upgrade packages, flatpaks, and system image");
    println!("  rakupkg system-upgrade                Check for and Apply system image upgrades");
}
