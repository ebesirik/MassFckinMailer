//! Embed the Windows application icon into the executable. No-op elsewhere.

fn main() {
    // rust-i18n reads these at compile time via a proc macro, which can't emit
    // rerun hints — so watch them here to pick up translation edits.
    println!("cargo:rerun-if-changed=locales");

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
