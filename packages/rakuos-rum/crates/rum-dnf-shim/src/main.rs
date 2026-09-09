//! `dnf`/`dnf5` compatibility shim: rewrites dnf-only grammar (positional
//! `list <keyword>`, dnf4 single-word group commands, `mark remove`) into
//! `rum`'s own CLI, then process-replaces into `rum` so exit codes, stdio,
//! and signal handling all behave exactly as if `rum` had been invoked
//! directly. Everything `rum` already aliases 1:1 with dnf/dnf5 (most verbs
//! and flags) passes through untouched.

use std::os::unix::process::CommandExt;
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let translated = translate(&args);

    let err = Command::new("rum").args(&translated).exec();
    eprintln!("dnf-shim: failed to exec rum: {err}");
    std::process::exit(1);
}

/// Rewrites dnf-grammar argv into rum-grammar argv. Global flags (anything
/// starting with `-`) are passed through untouched and left wherever they
/// appeared relative to the verb, since rum's clap parser (like dnf's)
/// accepts them before or after the subcommand.
fn translate(args: &[String]) -> Vec<String> {
    let Some(verb_idx) = args.iter().position(|a| !a.starts_with('-')) else {
        return args.to_vec();
    };
    let verb = args[verb_idx].as_str();
    let before = &args[..verb_idx];
    let after = &args[verb_idx + 1..];

    match verb {
        "list" => translate_list(before, after),
        "groupinstall" => rebuild(before, &["group", "install"], after),
        "groupremove" | "groupremove-with-deps" => rebuild(before, &["group", "remove"], after),
        "grouplist" => rebuild(before, &["group", "list"], after),
        "groupinfo" => rebuild(before, &["group", "info"], after),
        "mark" if after.first().map(String::as_str) == Some("remove") => {
            eprintln!("dnf-shim: `mark remove` has no rum equivalent (rum only tracks install/dependency reasons) — ignoring");
            std::process::exit(1);
        }
        _ => args.to_vec(),
    }
}

fn rebuild(before: &[String], verb_words: &[&str], after: &[String]) -> Vec<String> {
    let mut out = before.to_vec();
    out.extend(verb_words.iter().map(|s| s.to_string()));
    out.extend(after.iter().cloned());
    out
}

/// dnf's `list` takes a positional keyword (`installed`, `available`,
/// `updates`, `upgrades`, `extras`, `obsoletes`, `recent`) plus an optional
/// glob/name pattern. rum's `list` only knows the installed set (optionally
/// `--base` for the read-only base image), since it has no separate
/// "available in repos but not installed" concept the way dnf does.
fn translate_list(before: &[String], after: &[String]) -> Vec<String> {
    let mut out = before.to_vec();
    out.push("list".to_string());

    let Some(keyword) = after.first() else {
        return out;
    };

    match keyword.as_str() {
        "installed" => {
            out.extend(after[1..].iter().cloned());
        }
        "updates" | "upgrades" => {
            eprintln!("dnf-shim: `list {keyword}` has no direct rum equivalent — use `rum check-upgrade`; listing installed packages instead");
            out.extend(after[1..].iter().cloned());
        }
        "available" | "extras" | "obsoletes" | "recent" => {
            eprintln!("dnf-shim: `list {keyword}` has no rum equivalent (rum's overlay model has no separate available-but-not-installed set) — listing installed packages instead");
            out.extend(after[1..].iter().cloned());
        }
        _ => {
            // Not a recognized keyword — treat as a plain name/glob filter,
            // same as `dnf list <pattern>`.
            out.extend(after.iter().cloned());
        }
    }

    out
}
