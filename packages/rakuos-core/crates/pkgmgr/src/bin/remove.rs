use anyhow::{Result, bail};
use rakuos_pkgmgr::*;

/// Legacy entrypoint kept for scripts/sudoers rules still calling it directly
/// during the transition to `rum`. packages.list / packages-rpm.list
/// bookkeeping now lives in `rum remove` itself.
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        bail!("Usage: rakuos-remove [rum-flags...] <package> [package2...]");
    }
    require_root()?;
    let arg_strs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut rum_args = vec!["remove", "-y"];
    rum_args.extend_from_slice(&arg_strs);
    overlay::run("rum", &rum_args)
}
