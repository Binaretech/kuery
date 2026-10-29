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
mod crash_report;
mod data_file_tabs;
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
mod instance;
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
mod session;
mod settings;
mod shell_command;
mod table_tab;
mod theme;
mod tunnel;
mod ui;
mod updates;
mod welcome;
mod workspace;
mod write_mode;
mod write_review;

use std::sync::Arc;

use cli::Command;
use instance::{HandOff, Listener};

fn main() {
    attach_console();
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
    let address = instance::address();
    match instance::hand_off(&address, &targets) {
        Ok(HandOff::Delivered) => return,
        Ok(HandOff::NotRunning) => {}
        // Hung, or from another version: start a Kuery of our own.
        Err(error) => eprintln!("kuery: the running Kuery did not answer: {error}"),
    }
    if detach() {
        return;
    }
    let listener = Listener::bind(&address)
        .map_err(|error| eprintln!("kuery: other kuery commands will start their own app: {error}"))
        .ok();
    paths::migrate_legacy_config_dir();
    // What was open before restarting to update; read once.
    let restored = session::take();
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
            shell_command::init(cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            if let Some(listener) = listener {
                let (sent, received) = async_channel::unbounded();
                listener.serve(move |targets| {
                    let _ = sent.try_send(targets);
                });
                cx.spawn(async move |cx| {
                    while let Ok(targets) = received.recv().await {
                        cx.update(|cx| open(&targets, cx));
                    }
                })
                .detach();
            }
            if let Some(restored) = restored {
                app::restore_session(restored, cx);
            }
            open(&targets, cx);
            crash_report::offer(cx);
        });
}

/// Opens what a `kuery` command names, or the welcome window when nothing
/// opened and no window is left, shows why any path failed, and brings
/// Kuery to the front.
fn open(targets: &[cli::Target], cx: &mut gpui_kit::App) {
    let opened = app::open_targets(targets, cx);
    if !opened.any && cx.windows().is_empty() {
        app::open_welcome(cx);
    }
    app::report_open_errors(&opened.errors, cx);
    cx.activate(true);
}

/// A Windows release build has no console of its own (it would open one
/// next to the app), so it prints into the terminal that started it, if any.
fn attach_console() {
    #[cfg(all(windows, not(debug_assertions)))]
    // SAFETY: no arguments to uphold; it fails harmlessly without a parent
    // console.
    unsafe {
        use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Started from a terminal, a release build runs the app in a process of
/// its own and returns the prompt, like the command a running Kuery answers.
/// Returns whether that process took over.
fn detach() -> bool {
    use std::io::IsTerminal as _;

    const FOREGROUND: &str = "KUERY_FOREGROUND";
    if cfg!(debug_assertions)
        || std::env::var_os(FOREGROUND).is_some()
        || !std::io::stdout().is_terminal()
    {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut command = std::process::Command::new(exe);
    command
        .args(std::env::args_os().skip(1))
        .env(FOREGROUND, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Out of the terminal's process group, so closing it leaves Kuery open.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().is_ok()
}

/// Appends every panic, with its backtrace, to `panic.log` in the config
/// folder: a panic on a gpui thread ends the app, and an app opened from the
/// Finder or the Start menu has nowhere else to say why. It also leaves a
/// crash report that the next start offers to send.
fn log_panics() {
    let log = paths::config_dir().join("panic.log");
    let pending = crash_report::pending_path();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let thread = std::thread::current();
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let report = crash_report::Report::new(
            info.payload_as_str().unwrap_or("Box<dyn Any>"),
            info.location().map(ToString::to_string),
            thread.name().unwrap_or("unnamed"),
            &backtrace,
        );
        if let Err(error) = crash_report::record(&pending, &report) {
            eprintln!("kuery: could not write {}: {error}", pending.display());
        }
        let entry = format!(
            "--- Kuery {}, Unix time {}, thread '{}'\n{info}\n{}\n",
            env!("CARGO_PKG_VERSION"),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
            thread.name().unwrap_or("unnamed"),
            backtrace,
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
