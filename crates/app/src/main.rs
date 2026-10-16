//! Kuery: a desktop database manager built with gpui.
//!
//! `kuery [folder]` opens the project in `folder` (creating its `.kuery/`
//! if needed); without an argument the welcome window lists recent projects.

// Release builds on Windows open no console window next to the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod app_icon;
mod assets;
mod audit;
mod chrome;
mod commands;
mod completion;
mod connection;
mod connection_editor;
mod context_menus;
mod definition;
mod diagram_tab;
mod drivers;
mod explorer;
mod grid;
mod history;
mod keymap;
mod menus;
mod palette;
mod paths;
mod plan_view;
mod profile_charts;
mod profile_view;
mod project;
mod query_tab;
mod recent;
mod redis_console;
mod redis_key_tab;
mod redis_write;
mod results;
mod secrets;
mod table_tab;
mod theme;
mod tunnel;
mod ui;
mod welcome;
mod workspace;
mod write_mode;

use std::path::PathBuf;
use std::sync::Arc;

use kuery_pg::Engine;

fn main() {
    paths::migrate_legacy_config_dir();
    let engine = Arc::new(Engine::new().expect("start the Postgres runtime"));
    let redis = Arc::new(kuery_redis::Engine::new().expect("start the Redis runtime"));
    let folder = std::env::args_os().nth(1).map(PathBuf::from);

    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            app_icon::init();
            assets::load_fonts(cx).expect("register the embedded fonts");
            theme::apply(cx);
            app::init(
                engine.clone(),
                redis.clone(),
                Arc::new(secrets::Keyring),
                cx,
            );
            menus::init(cx);
            audit::init(cx);
            // After gpui_kit::init, so the keymap can keep its bindings.
            keymap::init(cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let opened = match &folder {
                Some(folder) => app::open_project(folder, cx)
                    .map_err(|error| eprintln!("kuery: {error}"))
                    .is_ok(),
                None => false,
            };
            if !opened {
                app::open_welcome(cx);
            }
            cx.activate(true);
        });
}
