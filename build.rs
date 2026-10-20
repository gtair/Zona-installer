//! Embed a `requireAdministrator` UAC manifest into the release binary only,
//! and only for MSVC targets.

fn main() {
    // Declared unconditionally so a manifest change rebuilds the binary.
    println!("cargo:rerun-if-changed=manifest/require-administrator.manifest");

    // `CARGO_CFG_TARGET_ENV` describes the target triple's environment ("msvc"
    // or "gnu"), not the host's; `PROFILE` is "debug" or "release".
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_default();

    if target_env == "msvc" && profile == "release" {
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        // Suppress the linker's default `asInvoker` UAC fragment: without it,
        // `mt.exe` fails to merge two `requestedExecutionLevel` values
        // (link error LNK1327).
        println!("cargo:rustc-link-arg=/MANIFESTUAC:NO");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:manifest/require-administrator.manifest");
    }
}
