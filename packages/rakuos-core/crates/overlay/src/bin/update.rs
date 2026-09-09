// RakuOS overlay update — DEPRECATED.
//
// Historically reconciled the overlay RPM database against a fresh base
// image after a bootc update (merge packages.toml, re-register
// user-installed packages, hand back base-satisfied ones). rum's own
// split-mode overlay rpmdb (`/var/lib/rakuos/rum-rpmdb`) tracks overlay
// packages independently of the base snapshot, so there is nothing left to
// reconcile after an image update — no merge, no diff, no background job.
//
// This binary is intentionally kept in the source tree (see the spec file's
// notes on this crate) but is no longer built into a systemd-triggered
// service; it's a safe no-op if ever invoked directly.

fn main() {
    println!("RakuOS overlay update: deprecated — rum's split-mode overlay rpmdb needs no post-update reconciliation. Nothing to do.");
}
