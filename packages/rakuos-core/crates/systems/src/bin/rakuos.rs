/// rakuos — user-facing dispatcher for RakuOS system helper subcommands.
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{self, Command};

const LIBEXEC: &str = "/usr/libexec/rakuos";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("");
    let rest = &args[args.len().min(2)..];

    let ok = match command {
        // Undocumented rakupkg aliases for a transition period — keep in sync
        // with crates/pkgmgr/src/bin/rakupkg.rs, but don't list in print_usage.
        // install/remove/upgrade now go straight to `rum`, which owns
        // packages.list/packages-rpm.list bookkeeping itself.
        "install" => run("rum", &rum_args("install", rest)),
        "update" => run(&format!("{LIBEXEC}/rakuos-update"), rest),
        "remove" => run("rum", &rum_args("remove", rest)),
        "upgrade" => {
            run("rum", &["upgrade".to_string()])
                && run(&format!("{LIBEXEC}/rakuos-update"), &["upgrade-flatpak".to_string()])
                && run(&format!("{LIBEXEC}/rakuos-update"), &["upgrade-image".to_string()])
        }
        "system-upgrade" => run(&format!("{LIBEXEC}/rakuos-update"), &["upgrade-image".to_string()]),
        "list" => run(&format!("{LIBEXEC}/rakuos-list"), &[]),

        "reset-overlay" => run(&format!("{LIBEXEC}/rakuos-reset-overlay"), rest),
        "setup-nvidia" => run(&format!("{LIBEXEC}/setup-nvidia"), &[]),
        "setup-nvidia-legacy" => run(&format!("{LIBEXEC}/setup-nvidia-legacy"), &[]),
        "setup-gaming" => run(&format!("{LIBEXEC}/setup-gaming"), &[]),
        "setup-virtualization" => run(&format!("{LIBEXEC}/setup-virtualization"), &[]),
        "setup-lsfg-vk" => run(&format!("{LIBEXEC}/setup-lsfg-vk"), &[]),
        "setup-ollama" => run(&format!("{LIBEXEC}/setup-ollama"), &[]),
        "setup-sudo" => run(&format!("{LIBEXEC}/setup-sudo"), rest),
        "setup-uutils" => run(&format!("{LIBEXEC}/setup-uutils"), rest),
        "enroll-secureboot-key" => run(&format!("{LIBEXEC}/enroll-secureboot-key"), &[]),
        "reenroll-tpm2" => exec_replace(&format!("{LIBEXEC}/reenroll-tpm2")),
        "setup-g733zm-audio" => run(&format!("{LIBEXEC}/setup-g733zm-audio"), &[]),
        "shell" => set_shell(rest),
        _ => {
            print_usage();
            process::exit(1);
        }
    };

    if !ok {
        process::exit(1);
    }
}

fn rum_args(subcmd: &str, rest: &[String]) -> Vec<String> {
    let mut args = vec![subcmd.to_string(), "-y".to_string()];
    args.extend_from_slice(rest);
    args
}

fn run(bin: &str, args: &[String]) -> bool {
    Command::new(bin)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn exec_replace(bin: &str) -> bool {
    let err = Command::new(bin).exec();
    eprintln!("rakuos: failed to exec {bin}: {err}");
    false
}

fn set_shell(args: &[String]) -> bool {
    let shell_name = args.first().map(String::as_str).unwrap_or("");
    let shell_bin = match shell_name {
        "fish" => "/usr/bin/fish",
        "bash" => "/usr/bin/bash",
        "zsh" => "/usr/bin/zsh",
        _ => {
            let current_shell = Command::new("getent")
                .args(["passwd", &std::env::var("USER").unwrap_or_default()])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| s.trim().split(':').nth(6).map(str::to_string))
                .unwrap_or_default();

            println!("Usage: rakuos shell <fish|bash|zsh>");
            println!();
            println!("Current shell: {current_shell}");
            println!();
            println!("Available:");
            for s in ["fish", "bash", "zsh"] {
                let bin = format!("/usr/bin/{s}");
                if is_executable(Path::new(&bin)) {
                    println!("  {s}");
                } else {
                    println!("  {s}  (not installed)");
                }
            }
            return false;
        }
    };

    if !is_executable(Path::new(shell_bin)) {
        println!("Error: {shell_bin} is not installed.");
        return false;
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let rakuos_cfg = format!("{home}/.config/rakuos");
    let _ = fs::create_dir_all(&rakuos_cfg);
    let _ = fs::write(format!("{rakuos_cfg}/keep-shell"), "");

    let ok = Command::new("chsh")
        .args(["-s", shell_bin])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if ok {
        println!("Login shell set to {shell_name}. Takes effect on next login.");
    }
    ok
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn print_usage() {
    println!("RakuOS Helper");
    println!();
    println!("Usage:");
    println!("  rakuos reset-overlay --confirm       Wipe overlay and all installed packages");
    println!("  rakuos reset-overlay --soft          Soft reset: rebuild overlay, preserve packages.list");
    println!("  rakuos setup-nvidia                  Install and configure NVIDIA drivers via DKMS");
    println!("  rakuos setup-nvidia-legacy           Install and configure NVIDIA legacy drivers via DKMS");
    println!("  rakuos setup-gaming                  Set up native gaming packages along with some apps from flathub");
    println!("  rakuos setup-virtualization          Set up KVM/QEMU virtualization");
    println!("  rakuos setup-lsfg-vk                 Set up Lossless Scaling Frame Gen");
    println!("  rakuos setup-stoat                   Set up Stoat");
    println!("  rakuos setup-ollama                  Set up Ollama local AI");
    println!("  rakuos setup-uutils <uutils|gnu>           Switch between uutils (Rust) and GNU coreutils");
    println!("  rakuos setup-sudo <sudo-rs|sudo|doas|run0> Select the privilege escalation tool");
    println!("  rakuos enroll-secureboot-key         Enroll the RakuOS Secure Boot key (MOK)");
    println!("  rakuos reenroll-tpm2                 Re-enroll TPM2 auto-unlock after a firmware update");
    println!("  rakuos setup-g733zm-audio            Apply ASUS G733ZM audio fix (requires reboot)");
    println!("  rakuos shell <fish|bash|zsh>         Set your login shell");
}
