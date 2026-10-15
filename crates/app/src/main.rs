//! dbadmin: prueba técnica de la interfaz con gpui.
//!
//! `DBADMIN_DATABASE_URL` elige la base de datos (por defecto, el Postgres de
//! `dev/compose.yaml`).

mod assets;
mod results;
mod theme;
mod workspace;

use std::sync::Arc;

use dbadmin_pg::Engine;
use gpui_kit::component::TitleBar;
use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};

use crate::workspace::DbAdminView;

const DEFAULT_URL: &str = "postgres://dbadmin:dbadmin@localhost:54329/dbadmin";

fn main() {
    let engine = Arc::new(Engine::new().expect("arranca el runtime de Postgres"));
    let url = std::env::var("DBADMIN_DATABASE_URL").unwrap_or_else(|_| DEFAULT_URL.into());

    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            assets::load_fonts(cx).expect("registra las fuentes embebidas");
            theme::apply(cx);
            workspace::bind_keys(cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..TitleBar::window_options()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| DbAdminView::new(engine.clone(), url.clone(), window, cx))
            })
            .expect("abre la ventana principal");
            cx.activate(true);
        });
}
