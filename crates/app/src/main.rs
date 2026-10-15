//! dbadmin: a technical prototype of the user interface built with gpui.
//!
//! Connections come from `dbadmin.toml` (or `DBADMIN_CONFIG`); without a file,
//! `DBADMIN_DATABASE_URL` picks a single database (by default, the Postgres
//! from `dev/compose.yaml`).

mod assets;
mod config;
mod connection;
mod definition;
mod explorer;
mod grid;
mod menus;
mod query_tab;
mod results;
mod table_tab;
mod theme;
mod ui;
mod workspace;

use std::sync::Arc;

use dbadmin_pg::Engine;
use gpui_kit::component::TitleBar;
use gpui_kit::{AppContext as _, Bounds, WindowBounds, WindowOptions, px, size};

use crate::config::Config;
use crate::workspace::DbAdminView;

fn main() {
    let engine = Arc::new(Engine::new().expect("start the Postgres runtime"));
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("dbadmin: {error}");
            std::process::exit(1);
        }
    };

    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            assets::load_fonts(cx).expect("register the embedded fonts");
            theme::apply(cx);
            query_tab::bind_keys(cx);
            menus::init(cx);
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
                cx.new(|cx| DbAdminView::new(engine.clone(), config.clone(), window, cx))
            })
            .expect("open the main window");
            cx.activate(true);
        });
}
