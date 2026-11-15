#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]

use std::path::Path;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(__do_not_set_zed_release_channel)");
    println!("cargo::rerun-if-env-changed=ZED_RELEASE_CHANNEL");

    // Enabling this `cfg` will cause a runtime panic if the
    // `ZED_RELEASE_CHANNEL` env var is not also set. TLDR: don't set the `cfg`
    // directly, just set the env var (hence the name).
    if std::env::var("ZED_RELEASE_CHANNEL").is_ok() {
        println!("cargo::rustc-cfg=__do_not_set_zed_release_channel");
    }

    println!("cargo::rerun-if-env-changed=ICARUS_VERSION");
    let icarus_version = std::env::var("ICARUS_VERSION").ok().unwrap_or_else(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../Cargo.toml");
        println!("cargo::rerun-if-changed={}", manifest.display());
        std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|text| workspace_package_version(&text))
            .unwrap_or_else(|| "unknown".to_string())
    });
    println!("cargo::rustc-env=ICARUS_VERSION={icarus_version}");
}

fn workspace_package_version(manifest: &str) -> Option<String> {
    let mut in_workspace_package = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_workspace_package = line == "[workspace.package]";
        } else if in_workspace_package
            && let Some(value) = line.strip_prefix("version")
            && let Some(value) = value.trim_start().strip_prefix('=')
        {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}
