//! Embed the Windows application icon into the executable. No-op elsewhere.

fn main() {
    // rust-i18n reads these at compile time via a proc macro, which can't emit
    // rerun hints — so watch them here to pick up translation edits.
    println!("cargo:rerun-if-changed=locales");

    // Displayed version: CI sets MFM_VERSION (base version from Cargo.toml plus a
    // channel/build suffix); local builds fall back to the crate version. Exposed
    // to the app as env!("MFM_VERSION").
    let version = std::env::var("MFM_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("CARGO_PKG_VERSION").ok())
        .unwrap_or_else(|| "0.0.0".to_string());
    println!("cargo:rustc-env=MFM_VERSION={version}");
    println!("cargo:rerun-if-env-changed=MFM_VERSION");

    #[cfg(windows)]
    {
        let icon = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/icon.ico");
        println!("cargo:rerun-if-changed={icon}");
        let mut res = winresource::WindowsResource::new();
        res.set_icon(icon);
        if let Err(e) = res.compile() {
            // Missing resource compiler shouldn't fail the whole build.
            println!("cargo:warning=could not embed app icon: {e}");
        }
    }
}
