// Embed the application manifest that declares PER_MONITOR_AWARE_V2.
// It has to be the manifest, not a runtime call: a runtime call can lose a race
// with library init, and a DPI-unaware process is silently handed logical pixels
// (the v0 bug, see docs/DESIGN.md §3). main.rs asserts it took effect.
use embed_manifest::{embed_manifest, manifest::DpiAwareness, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(new_manifest("winshot").dpi_awareness(DpiAwareness::PerMonitorV2Only))
            .expect("unable to embed manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
