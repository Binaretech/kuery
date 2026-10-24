//! Kuery: a desktop database manager built with gpui.
//!
//! `kuery [PATH...]` opens each folder as a project (creating its `.kuery/`
//! if needed) and each data file in a tab; without a path the welcome window
//! lists recent projects. `kuery --help` lists the options.

// Release builds on Windows open no console window next to the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod activity_tab;
mod app;
mod app_icon;
mod assets;
mod audit;
mod chrome;
mod cli;
mod commands;
mod completion;
mod connection;
mod connection_editor;
mod context_menus;
mod definition;
mod diagram_tab;
mod drivers;
mod explorer;
mod file_jobs;
mod file_query_tab;
mod file_tab;
mod file_window;
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
mod results_pane;
mod secrets;
mod settings;
mod table_tab;
mod theme;
mod tunnel;
mod ui;
mod updates;
mod welcome;
mod workspace;
mod write_mode;

use std::sync::Arc;

use cli::Command;

fn main() {
    let cwd = std::env::current_dir().unwrap_or_default();
    let targets = match cli::parse(std::env::args_os().skip(1), &cwd) {
        Ok(Command::Open(targets)) => targets,
        Ok(Command::Help) => {
            print!("{}", cli::help());
            return;
        }
        Ok(Command::Version) => {
            println!("{}", cli::version());
            return;
        }
        Err(error) => {
            eprintln!("kuery: {error}");
            std::process::exit(1);
        }
    };
    paths::migrate_legacy_config_dir();
    log_panics();
    drivers::install_tls_provider();
    let redis = Arc::new(kuery_redis::Engine::new().expect("start the Redis runtime"));

    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            app_icon::init();
            assets::load_fonts(cx).expect("register the embedded fonts");
            theme::apply(cx);
            app::init(redis.clone(), Arc::new(secrets::Keyring), cx);
            menus::init(cx);
            audit::init(cx);
            // After gpui_kit::init, so the keymap can keep its bindings.
            keymap::init(cx);
            updates::init(cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            if !app::open_targets(&targets, cx) {
                app::open_welcome(cx);
            }
            cx.activate(true);
        });
}

/// Appends every panic, with its backtrace, to `panic.log` in the config
/// folder: a panic on a gpui thread ends the app, and an app opened from the
/// Finder or the Start menu has nowhere else to say why.
fn log_panics() {
    let log = paths::config_dir().join("panic.log");
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let thread = std::thread::current();
        let entry = format!(
            "--- Kuery {}, Unix time {}, thread '{}'\n{info}\n{}\n",
            env!("CARGO_PKG_VERSION"),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
            thread.name().unwrap_or("unnamed"),
            std::backtrace::Backtrace::force_capture(),
        );
        let written = log
            .parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .and_then(|_| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log)
            })
            .and_then(|mut file| std::io::Write::write_all(&mut file, entry.as_bytes()));
        if let Err(error) = written {
            eprintln!("kuery: could not write {}: {error}", log.display());
        }
    }));
}
