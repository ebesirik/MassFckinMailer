// Launch as a Windows GUI app in release builds so double-clicking the exe (or
// the installer's shortcut) doesn't flash/keep a console window. Debug builds
// keep the console so panics and logs stay visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod assets;
mod main_window;
mod theme;

use gpui::*;
use gpui_component::{Root, TitleBar};
use main_window::MainWindow;
use mmm_core::settings::AppSettings;

// Load all translations from `crates/app/locales/` at compile time; English is
// the fallback for any missing key.
rust_i18n::i18n!("locales", fallback = "en");

fn main() {
    let app = Application::new().with_assets(assets::Assets);

    app.run(move |cx| {
        // Apply the saved UI language before the first render.
        rust_i18n::set_locale(&AppSettings::load().language);

        // Must be called before using any GPUI Component features.
        gpui_component::init(cx);

        // Register the bundled Geist / Geist Mono faces so the theme can use them.
        assets::load_fonts(cx);

        cx.spawn(async move |cx| {
            cx.open_window(
                WindowOptions {
                    // Transparent OS title bar so we can render our own with
                    // in-app window controls (see `MainWindow::render_title_bar`).
                    titlebar: Some(TitleBar::title_bar_options()),
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(120.), px(80.)),
                        size: size(px(1160.), px(800.)),
                    })),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| MainWindow::new(window, cx));
                    // The first level on the window must be a Root.
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
