//! Embed the Windows application icon into the executable. No-op elsewhere.

fn main() {
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
