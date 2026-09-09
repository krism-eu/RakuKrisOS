//! dnf5-style transaction summary table: the `Package Arch Version
//! Repository Size` block, `Installing:`/`Removing:`/etc. section labels,
//! `Transaction Summary:` counts, and the `"After this operation, ..."`
//! size-delta line printed just before the confirmation prompt.

use rum_core::{format_size, Package};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Installing,
    Upgrading,
    Downgrading,
    Reinstalling,
    Removing,
}

impl Action {
    /// The `Installing:`/`Upgrading:`/... section header this entry sorts
    /// under in the table, and the label reused for its
    /// `Transaction Summary:` count line.
    fn label(self) -> &'static str {
        match self {
            Action::Installing => "Installing:",
            Action::Upgrading => "Upgrading:",
            Action::Downgrading => "Downgrading:",
            Action::Reinstalling => "Reinstalling:",
            Action::Removing => "Removing:",
        }
    }
}

/// One row of the table: the action being taken and the package it applies
/// to. For `Upgrading`/`Downgrading`, `replacing` is the currently-installed
/// package being replaced, rendered as an indented `replacing X` sub-row the
/// way dnf5 shows an obsoleted/upgraded-away package right under its
/// replacement.
pub struct Entry<'a> {
    pub action: Action,
    pub pkg: &'a Package,
    pub replacing: Option<&'a Package>,
}

impl<'a> Entry<'a> {
    pub fn new(action: Action, pkg: &'a Package) -> Self {
        Self { action, pkg, replacing: None }
    }

    pub fn replacing(mut self, old: &'a Package) -> Self {
        self.replacing = Some(old);
        self
    }
}

/// Prints the full dnf5-style transaction summary: column table grouped by
/// action, `Transaction Summary:` counts, and the size-delta line. `entries`
/// need not be pre-grouped by action — this function does that itself.
pub fn print_transaction_summary(entries: &[Entry]) {
    if entries.is_empty() {
        return;
    }

    let mut name_w = "Package".len();
    let mut arch_w = "Arch".len();
    let mut ver_w = "Version".len();
    let mut repo_w = "Repository".len();
    let mut size_w = "Size".len();
    for e in entries {
        for pkg in [Some(e.pkg), e.replacing].into_iter().flatten() {
            name_w = name_w.max(pkg.nevra.name.len());
            arch_w = arch_w.max(pkg.nevra.arch.len());
            ver_w = ver_w.max(evr(pkg).len());
            repo_w = repo_w.max(repo_of(pkg).len());
            size_w = size_w.max(size_of(pkg).len());
        }
    }

    println!("{:<name_w$} {:<arch_w$} {:<ver_w$} {:<repo_w$} {:>size_w$}", "Package", "Arch", "Version", "Repository", "Size");

    let mut counts: Vec<(Action, u32)> = Vec::new();
    for action in [Action::Installing, Action::Upgrading, Action::Downgrading, Action::Reinstalling, Action::Removing] {
        let rows: Vec<&Entry> = entries.iter().filter(|e| e.action == action).collect();
        if rows.is_empty() {
            continue;
        }
        println!("{}", action.label());
        for e in &rows {
            println!(" {:<name_w$} {:<arch_w$} {:<ver_w$} {:<repo_w$} {:>size_w$}", e.pkg.nevra.name, e.pkg.nevra.arch, evr(e.pkg), repo_of(e.pkg), size_of(e.pkg));
            if let Some(old) = e.replacing {
                println!("   replacing {:<name_w$} {:<arch_w$} {:<ver_w$} {:<repo_w$} {:>size_w$}", old.nevra.name, old.nevra.arch, evr(old), repo_of(old), size_of(old));
            }
        }
        counts.push((action, rows.len() as u32));
    }

    println!("Transaction Summary:");
    for (action, n) in &counts {
        println!(" {:<20} {n} package{}", action.label(), if *n == 1 { "" } else { "s" });
    }
    println!();

    print_size_delta(entries);
}

fn evr(pkg: &Package) -> String {
    format!("{}:{}-{}", pkg.nevra.epoch, pkg.nevra.version, pkg.nevra.release)
}

fn repo_of(pkg: &Package) -> String {
    if pkg.repo_id.is_empty() {
        "<unknown>".to_string()
    } else {
        pkg.repo_id.clone()
    }
}

fn size_of(pkg: &Package) -> String {
    format_size(if pkg.download_size > 0 { pkg.download_size } else { pkg.install_size })
}

fn print_size_delta(entries: &[Entry]) {
    let install_bytes: u64 = entries
        .iter()
        .filter(|e| matches!(e.action, Action::Installing | Action::Upgrading | Action::Downgrading | Action::Reinstalling))
        .map(|e| e.pkg.install_size)
        .sum();
    let remove_bytes: u64 = entries
        .iter()
        .map(|e| match e.action {
            Action::Removing => e.pkg.install_size,
            _ => e.replacing.map(|r| r.install_size).unwrap_or(0),
        })
        .sum();

    if install_bytes > 0 && remove_bytes > 0 {
        let word = if install_bytes >= remove_bytes { "extra" } else { "less" };
        let net_abs = install_bytes.abs_diff(remove_bytes);
        println!("After this operation, {} {word} will be used (install {}, remove {}).", format_size(net_abs), format_size(install_bytes), format_size(remove_bytes));
    } else if install_bytes > 0 {
        println!("After this operation, {} will be used.", format_size(install_bytes));
    } else if remove_bytes > 0 {
        println!("After this operation, {} will be freed.", format_size(remove_bytes));
    }
}
