//! App asset source: serves the app's bundled SVG icons and Geist fonts
//! (`assets/`), falling back to gpui-component's built-in icon set for the
//! `IconName` glyphs.

use std::borrow::Cow;

use gpui::{App, AssetSource, Result, SharedString};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../assets"]
#[include = "icons/*.svg"]
#[include = "fonts/*.ttf"]
struct Embedded;

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }
        if let Some(f) = Embedded::get(path) {
            return Ok(Some(f.data));
        }
        // Falls through to gpui-component's embedded icons (icons/<name>.svg).
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut items = gpui_component_assets::Assets.list(path)?;
        items.extend(Embedded::iter().filter_map(|p| p.starts_with(path).then(|| p.into())));
        Ok(items)
    }
}

/// Load the bundled Geist / Geist Mono faces into the text system. Call once at
/// startup, before opening the first window.
pub fn load_fonts(cx: &App) {
    const FACES: &[&str] = &[
        "fonts/Geist-Regular.ttf",
        "fonts/Geist-Medium.ttf",
        "fonts/Geist-SemiBold.ttf",
        "fonts/Geist-Bold.ttf",
        "fonts/GeistMono-Regular.ttf",
        "fonts/GeistMono-Medium.ttf",
    ];
    let fonts: Vec<Cow<'static, [u8]>> = FACES
        .iter()
        .filter_map(|p| Embedded::get(p).map(|f| f.data))
        .collect();
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        eprintln!("failed to load bundled fonts: {e}");
    }
}
