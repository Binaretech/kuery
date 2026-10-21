//! What one frame of the sidebar costs with a large catalog: 3000 tables and
//! 3000 functions in one schema, both groups open, in a headless window.
//! Not run with the suite; run it on its own:
//!
//! ```sh
//! cargo test -p kuery explorer_frame_cost -- --ignored --nocapture
//! ```
//!
//! The headless platform lays out and paints but does not rasterize, and its
//! text system is a stand-in, so the numbers are lower than on screen; they
//! compare builds of the sidebar, not frames.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::executor::block_on;
use gpui::TestAppContext;
use gpui_kit::{AppContext as _, Bounds, Entity, Point, WindowBounds, WindowOptions, px, size};
use kuery_core::catalog::{
    CatalogOverview, FunctionInfo, RoutineKind, TableInfo, TableKind, TableRef,
};
use kuery_core::session::{Access, Opener, Session};

use super::{CatalogState, ConnectionNode, ConnectionState, Explorer};
use crate::connection::{Db, Environment};
use crate::project::{ConnectionSpec, Project, ProjectModel};

const OBJECTS: usize = 3000;
const FRAMES: u32 = 30;

fn overview() -> CatalogOverview {
    CatalogOverview {
        schemas: vec!["public".into()],
        tables: (0..OBJECTS)
            .map(|n| TableInfo {
                table: TableRef {
                    schema: "public".into(),
                    name: format!("table_{n:04}"),
                },
                kind: TableKind::Table,
                estimated_rows: Some(n as u64 * 37),
                comment: None,
            })
            .collect(),
        functions: (0..OBJECTS)
            .map(|n| FunctionInfo {
                schema: "public".into(),
                name: format!("function_{n:04}"),
                arguments: "p_id integer, p_day date".into(),
                result: "void".into(),
                kind: RoutineKind::Function,
                language: "plpgsql".into(),
                oid: n as u32,
            })
            .collect(),
        enums: Vec::new(),
        triggers: Vec::new(),
    }
}

/// A real session on an empty SQLite file: the rows only need a [`Db`] to
/// exist, never to answer.
fn db(dir: &std::path::Path) -> Db {
    let path = dir.join("bench.sqlite");
    std::fs::write(&path, b"").unwrap();
    let opener: Opener = Arc::new(move |access| kuery_sqlite::open(&path, access));
    let session = block_on(Session::open(opener, Access::ReadWrite)).unwrap();
    Db::new(
        "bench".into(),
        Environment::Development,
        session,
        "bench.sqlite".into(),
        None,
    )
}

fn spec() -> ConnectionSpec {
    ConnectionSpec {
        id: "bench".into(),
        name: "bench".into(),
        driver: "sqlite".into(),
        host: String::new(),
        port: 0,
        database: "bench.sqlite".into(),
        user: String::new(),
        sslmode: None,
        environment: Some("development".into()),
        ssh: None,
    }
}

#[gpui::test]
#[ignore = "a bench: run it with --ignored --nocapture"]
fn explorer_frame_cost(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("kuery-explorer-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = db(&dir);

    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::theme::apply(cx);
    });
    let model: Entity<ProjectModel> = cx.new(|_| ProjectModel {
        project: Project {
            root: dir.clone(),
            name: "bench".into(),
            workspace: None,
            connections: Vec::new(),
        },
    });
    let (window, explorer) = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(256.), px(900.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| Explorer::new(model, window, cx)),
        )
        .unwrap()
    });
    cx.update(|cx| {
        explorer.update(cx, |explorer, cx| {
            explorer.connections.push(ConnectionNode {
                spec: spec(),
                state: ConnectionState::Ready {
                    db,
                    catalog: CatalogState::Loaded(overview()),
                },
            });
            for key in ["0", "0/public", "0/public/tables", "0/public/functions"] {
                explorer.expanded.insert(key.into());
            }
            cx.notify();
        })
    });

    // Every frame renders the explorer again, as hovering a row does; after
    // a change (an expand, a catalog arriving) it also lists the rows again.
    let (mut redraw, mut rebuild) = (Duration::ZERO, Duration::ZERO);
    cx.update_window(window, |_, window, cx| {
        window.draw(cx).clear(cx);
        for _ in 0..FRAMES {
            let start = Instant::now();
            window.refresh();
            window.draw(cx).clear(cx);
            redraw += start.elapsed();
        }
        for _ in 0..FRAMES {
            explorer.update(cx, |explorer, _| explorer.rows_stale = true);
            let start = Instant::now();
            window.refresh();
            window.draw(cx).clear(cx);
            rebuild += start.elapsed();
        }
    })
    .unwrap();
    // The connection, the schema, two groups and their objects.
    let rows = explorer.read_with(cx, |explorer, _| explorer.rows.len());
    assert_eq!(rows, 4 + 2 * OBJECTS);
    // The list fits the window and scrolls, rather than growing to its rows.
    let sizes = explorer.read_with(cx, |explorer, _| explorer.scroll.0.borrow().last_item_size);
    let sizes = sizes.expect("the list was laid out");
    assert!(sizes.item.height > px(500.) && sizes.item.height < px(900.));
    assert_eq!(sizes.contents.height, px(29.) * rows as f32);
    let mean = |total: Duration| total.as_secs_f64() * 1000. / f64::from(FRAMES);
    println!(
        "explorer frame, {OBJECTS} tables + {OBJECTS} functions open: {:.2} ms, \
         {:.2} ms listing the rows again (mean of {FRAMES})",
        mean(redraw),
        mean(rebuild),
    );
    let _ = std::fs::remove_dir_all(&dir);
}
