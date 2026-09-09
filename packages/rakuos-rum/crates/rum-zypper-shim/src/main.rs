//! `zypper` compatibility shim: rewrites zypper-only grammar (its own verb
//! names/aliases, `addrepo`/`modifyrepo`'s positional argument order,
//! `-n`/`--non-interactive`) into `rum`'s own CLI, then process-replaces
//! into `rum` so exit codes, stdio, and signal handling all behave exactly
//! as if `rum` had been invoked directly. Verbs/flags zypper and rum happen
//! to already spell the same way (`install`/`in`, `remove`/`rm`, `update`,
//! `search`/`se`, `info`/`if`, `clean`) pass through untouched.

use std::os::unix::process::CommandExt;
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let translated = translate(&args);

    let err = Command::new("rum").args(&translated).exec();
    eprintln!("zypper-shim: failed to exec rum: {err}");
    std::process::exit(1);
}

/// Rewrites zypper-grammar argv into rum-grammar argv. Global flags
/// (anything starting with `-`) are passed through untouched and left
/// wherever they appeared relative to the verb, since rum's clap parser
/// (like zypper's) accepts global flags before or after the subcommand —
/// except the handful translated in place below (`-n`/`--non-interactive`,
/// `--no-gpg-checks`, `--root`).
fn translate(args: &[String]) -> Vec<String> {
    let Some(verb_idx) = args.iter().position(|a| !a.starts_with('-')) else {
        return translate_flags(args);
    };
    let verb = args[verb_idx].as_str();
    let before = translate_flags(&args[..verb_idx]);
    let after = &args[verb_idx + 1..];

    match verb {
        "dist-upgrade" | "dup" => rebuild(&before, &["distro-sync"], after),
        "list-updates" | "lu" => rebuild(&before, &["check-upgrade"], after),
        "what-provides" | "wp" => rebuild(&before, &["provides"], after),
        "verify" | "ve" => rebuild(&before, &["check"], after),
        "packages" | "pa" => rebuild(&before, &["list"], after),
        "repos" | "lr" => rebuild(&before, &["repo", "list"], after),
        "refresh" | "ref" => rebuild(&before, &["makecache"], after),
        "addrepo" | "ar" => translate_addrepo(&before, after),
        "removerepo" | "rr" => {
            eprintln!("zypper-shim: `removerepo` has no rum equivalent (rum's config-manager only enables/disables/adds) — remove the .repo file directly");
            std::process::exit(1);
        }
        "modifyrepo" | "mr" => translate_modifyrepo(&before, after),
        "source-install" | "si" => {
            eprintln!("zypper-shim: `source-install` has no rum equivalent (no source-rpm awareness) — ignoring");
            std::process::exit(1);
        }
        "shell" | "sh" => {
            eprintln!("zypper-shim: interactive `shell` mode has no rum equivalent — run individual commands instead");
            std::process::exit(1);
        }
        "patch" => {
            eprintln!("zypper-shim: `patch` has no rum equivalent yet (rum doesn't fetch/parse updateinfo) — falling back to `rum advisory`");
            rebuild(&before, &["advisory"], after)
        }
        _ => {
            let mut out = before;
            out.push(verb.to_string());
            out.extend(after.iter().cloned());
            out
        }
    }
}

/// Translates global flags that appear before the verb. `-n`/`--non-interactive`
/// is zypper's `-y` equivalent; `--no-gpg-checks` maps to rum's
/// `--no-gpgchecks`; `--root <dir>` maps to rum's `--installroot`.
/// Everything else passes through untouched.
fn translate_flags(flags: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut iter = flags.iter().peekable();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "-n" | "--non-interactive" => out.push("-y".to_string()),
            "--no-gpg-checks" => out.push("--no-gpgchecks".to_string()),
            "--gpg-auto-import-keys" => {} // rum always imports on install; nothing to map
            "--root" | "-R" => {
                out.push("--installroot".to_string());
                if let Some(v) = iter.next() {
                    out.push(v.clone());
                }
            }
            "--pkg-cache-dir" => {
                out.push("--cache-dir".to_string());
                if let Some(v) = iter.next() {
                    out.push(v.clone());
                }
            }
            _ => out.push(flag.clone()),
        }
    }
    out
}

fn rebuild(before: &[String], verb_words: &[&str], after: &[String]) -> Vec<String> {
    let mut out = before.to_vec();
    out.extend(verb_words.iter().map(|s| s.to_string()));
    out.extend(after.iter().cloned());
    out
}

/// zypper's `addrepo <url> <alias>` (or `-r <path>.repo`) takes the baseurl
/// before the alias — the opposite order of rum's `--add-repo id=baseurl`.
/// Only the plain `<url> <alias>` form is translated; `-r`/`--repo <file>`
/// (pointing at an existing `.repo` file to import) has no rum equivalent
/// since rum doesn't fetch and store an arbitrary `.repo` file verbatim.
fn translate_addrepo(before: &[String], after: &[String]) -> Vec<String> {
    let positional: Vec<&String> = after.iter().filter(|a| !a.starts_with('-')).collect();
    if positional.len() != 2 {
        eprintln!("zypper-shim: `addrepo` needs exactly `<url> <alias>` to translate to rum's `config-manager --add-repo` (got {} positional args) — pass `--add-repo id=baseurl` to rum directly instead", positional.len());
        std::process::exit(1);
    }
    let url = positional[0];
    let alias = positional[1];
    let mut out = before.to_vec();
    out.push("config-manager".to_string());
    out.push("--add-repo".to_string());
    out.push(format!("{alias}={url}"));
    out
}

/// zypper's `modifyrepo -e|--enable <alias>` / `-d|--disable <alias>` maps
/// to rum's `config-manager --set-enabled`/`--set-disabled`.
fn translate_modifyrepo(before: &[String], after: &[String]) -> Vec<String> {
    let enable = after.iter().any(|a| a == "-e" || a == "--enable");
    let disable = after.iter().any(|a| a == "-d" || a == "--disable");
    let alias = after.iter().find(|a| !a.starts_with('-'));
    let Some(alias) = alias else {
        eprintln!("zypper-shim: `modifyrepo` needs a repo alias to translate to rum's `config-manager`");
        std::process::exit(1);
    };
    let mut out = before.to_vec();
    out.push("config-manager".to_string());
    if enable {
        out.push("--set-enabled".to_string());
        out.push(alias.clone());
    }
    if disable {
        out.push("--set-disabled".to_string());
        out.push(alias.clone());
    }
    if !enable && !disable {
        eprintln!("zypper-shim: `modifyrepo` needs `-e`/`--enable` or `-d`/`--disable` to translate to rum's `config-manager`");
        std::process::exit(1);
    }
    out
}
