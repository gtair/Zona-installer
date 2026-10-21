//! Embed the application icon and the `requireAdministrator` UAC manifest into
//! the release binary, and only for MSVC targets.

fn main() {
    // Declared unconditionally so a resource change rebuilds the binary.
    println!("cargo:rerun-if-changed=manifest/require-administrator.manifest");
    println!("cargo:rerun-if-changed=dependencies/icon.ico");

    // `CARGO_CFG_TARGET_ENV` describes the target triple's environment ("msvc"
    // or "gnu"), not the host's; `PROFILE` is "debug" or "release".
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_default();

    // Only the shipped (MSVC release) binary carries the icon and elevation
    // manifest; debug builds keep their console and skip this.
    if target_env == "msvc" && profile == "release" {
        let mut res = winres::WindowsResource::new();
        res.set_icon("dependencies/icon.ico");
        res.set_manifest_file("manifest/require-administrator.manifest");
        res.compile().expect("failed to compile Windows resources");
    }
}
