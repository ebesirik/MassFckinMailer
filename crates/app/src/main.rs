mod main_window;

use gpui::*;
use gpui_component::Root;
use gpui_component_assets::Assets;
use main_window::MainWindow;

fn main() {
    let app = Application::new().with_assets(Assets);

    app.run(move |cx| {
        // Must be called before using any GPUI Component features.
        gpui_component::init(cx);

        cx.spawn(async move |cx| {
            cx.open_window(
                WindowOptions {
                    titlebar: Some(TitlebarOptions {
                        title: Some("MassFckinMailer".into()),
                        ..Default::default()
                    }),
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
