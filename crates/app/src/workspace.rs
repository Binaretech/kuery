//! Ventana principal: barra de título propia, lista de tablas, editor SQL y
//! resultados en streaming.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dbadmin_pg::{Engine, Event, QueryError, RunOptions, Session};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, TitleBar, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyBinding, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, actions, div, px,
};

use crate::assets::AppIcon;
use crate::results::ResultsDelegate;
use crate::theme::{self, color};

actions!(dbadmin, [RunQuery, CancelQuery]);

const DEFAULT_SQL: &str = "-- Solo lectura: se ejecuta dentro de BEGIN TRANSACTION READ ONLY.\n\
-- ⌘↵ / Ctrl+↵ para ejecutar.\n\
select o.id, o.status, o.total, o.currency, o.created_at, c.email, o.shipping\n\
from orders o\n\
join customers c on c.id = o.customer_id\n\
order by o.created_at desc;\n";

const CATALOG_SQL: &str = "select table_schema || '.' || table_name \
from information_schema.tables \
where table_schema not in ('pg_catalog', 'information_schema') \
order by 1";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-enter", RunQuery, None),
        KeyBinding::new("secondary-.", CancelQuery, None),
    ]);
}

enum Connection {
    Connecting,
    Ready(Session),
    Failed(String),
}

enum RunState {
    Idle,
    Running {
        started: Instant,
    },
    Done {
        rows: usize,
        elapsed: Duration,
        first_batch_after: Option<Duration>,
    },
    Failed(String),
}

pub struct DbAdminView {
    /// El runtime de las conexiones vive lo que vive la vista: si se suelta,
    /// tokio cancela las tareas de la sesión.
    _engine: Arc<Engine>,
    connection: Connection,
    target: SharedString,
    editor: Entity<EditorState>,
    table: Entity<TableState<ResultsDelegate>>,
    run: RunState,
    run_task: Option<Task<()>>,
    tables: Vec<SharedString>,
    focus_handle: FocusHandle,
}

impl DbAdminView {
    pub fn new(
        engine: Arc<Engine>,
        url: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(DEFAULT_SQL)
        });
        let table = cx.new(|cx| TableState::new(ResultsDelegate::default(), window, cx));

        let connect = engine.connect(url.clone());
        cx.spawn(async move |this, cx| {
            let result = connect.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(session) => {
                        this.connection = Connection::Ready(session);
                        this.load_catalog(cx);
                    }
                    Err(error) => this.connection = Connection::Failed(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();

        Self {
            _engine: engine,
            connection: Connection::Connecting,
            target: describe_target(&url).into(),
            editor,
            table,
            run: RunState::Idle,
            run_task: None,
            tables: Vec::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    fn session(&self) -> Option<&Session> {
        match &self.connection {
            Connection::Ready(session) => Some(session),
            _ => None,
        }
    }

    fn load_catalog(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session() else {
            return;
        };
        let rx = session.run_read_only(CATALOG_SQL.into(), RunOptions::default());
        cx.spawn(async move |this, cx| {
            let mut set = dbadmin_core::result_set::ResultSet::default();
            while let Ok(event) = rx.recv().await {
                match event {
                    Event::Columns(names) => set = dbadmin_core::result_set::ResultSet::new(names),
                    Event::Batch(batch) => set.push(batch),
                    _ => {}
                }
            }
            this.update(cx, |this, cx| {
                this.tables = (0..set.rows())
                    .filter_map(|row| {
                        set.cell(row, 0)
                            .flatten()
                            .map(|t| SharedString::from(t.to_owned()))
                    })
                    .collect();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn run_query(&mut self, _: &RunQuery, _: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let sql = {
            let editor = self.editor.read(cx);
            let selected = editor.selected_text().to_string();
            if selected.trim().is_empty() {
                editor.text().to_string()
            } else {
                selected
            }
        };

        // Soltar la tarea anterior suelta su receptor y cancela esa query.
        self.run_task = None;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().start();
            table.refresh(cx);
        });
        self.run = RunState::Running {
            started: Instant::now(),
        };
        cx.notify();

        let rx = session.run_read_only(sql, RunOptions::default());
        self.run_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                if this
                    .update(cx, |this, cx| this.on_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Columns(names) => self.table.update(cx, |table, cx| {
                table.delegate_mut().set_columns(names);
                table.refresh(cx);
            }),
            Event::Batch(batch) => self.table.update(cx, |table, cx| {
                table.delegate_mut().push(batch);
                cx.notify();
            }),
            Event::Finished(summary) => {
                self.finish_table(cx);
                self.run = RunState::Done {
                    rows: summary.rows,
                    elapsed: summary.elapsed,
                    first_batch_after: summary.first_batch_after,
                };
            }
            Event::Failed(error) => {
                self.finish_table(cx);
                self.run = RunState::Failed(match error {
                    QueryError::Database { code, message } => format!("{message} [{code}]"),
                    other => other.to_string(),
                });
            }
        }
        cx.notify();
    }

    fn finish_table(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            table.delegate_mut().finish();
            cx.notify();
        });
    }

    fn cancel_query(&mut self, _: &CancelQuery, _: &mut Window, cx: &mut Context<Self>) {
        if let (Some(session), RunState::Running { .. }) = (self.session(), &self.run) {
            session.cancel();
            cx.notify();
        }
    }

    fn open_table(&mut self, name: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let sql = format!("select * from {name};");
        self.editor
            .update(cx, |editor, cx| editor.set_value(sql, window, cx));
        self.run_query(&RunQuery, window, cx);
    }

    fn is_running(&self) -> bool {
        matches!(self.run, RunState::Running { .. })
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (dot, label) = match &self.connection {
            Connection::Connecting => (theme::AMBER, "Conectando…".to_string()),
            Connection::Ready(session) => (
                theme::GREEN,
                format!("{} · PostgreSQL {}", self.target, session.server_version()),
            ),
            Connection::Failed(_) => (theme::RED, format!("{} · sin conexión", self.target)),
        };
        let search_shortcut = if cfg!(target_os = "macos") {
            "⌘K"
        } else {
            "Ctrl+K"
        };

        TitleBar::new().child(
            h_flex()
                .w_full()
                .h_full()
                .gap_3()
                .pr_3()
                .text_size(px(12.5))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .size(px(18.))
                                .rounded(px(5.))
                                .bg(color(0x1fa971))
                                .text_color(color(0x04140c))
                                .text_size(px(8.5))
                                .font_weight(gpui_kit::FontWeight::BOLD)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child("TO"),
                        )
                        .child(
                            div()
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .child("Checkout v2"),
                        )
                        .child(div().text_color(color(theme::TEXT_TERTIARY)).child("/"))
                        .child(
                            div()
                                .text_color(color(theme::TEXT_SECONDARY))
                                .child("Tienda Online"),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .px_2()
                        .h(px(24.))
                        .rounded(px(6.))
                        .bg(color(0x2a2a2d))
                        .child(div().size(px(7.)).rounded_full().bg(color(dot)))
                        .child(label),
                )
                .child(div().flex_1())
                .child(
                    h_flex()
                        .p(px(2.))
                        .gap(px(2.))
                        .rounded(px(7.))
                        .bg(color(0x2a2a2d))
                        .child(segment("SQL", true))
                        .child(segment("Datos", false))
                        .child(segment("Estructura", false)),
                )
                .child(div().flex_1())
                .child(
                    h_flex()
                        .w(px(240.))
                        .h(px(26.))
                        .px_2()
                        .gap_2()
                        .rounded(px(7.))
                        .bg(color(0x2a2a2d))
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child(div().flex_1().child("Buscar o ejecutar comando"))
                        .child(
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(px(10.5))
                                .child(search_shortcut),
                        ),
                ),
        )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex()
            .id("tables")
            .flex_1()
            .overflow_y_scroll()
            .px_2()
            .gap(px(1.));
        for (index, name) in self.tables.iter().enumerate() {
            let name = name.clone();
            list = list.child(
                h_flex()
                    .id(("table", index))
                    .h(px(26.))
                    .px_2()
                    .gap_2()
                    .rounded(px(6.))
                    .hover(|style| style.bg(color(0x2e2e32)))
                    .child(
                        Icon::new(AppIcon::Table)
                            .size(px(14.))
                            .text_color(color(theme::ACCENT)),
                    )
                    .child(name.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_table(name.clone(), window, cx)
                    })),
            );
        }
        if let Connection::Failed(error) = &self.connection {
            list = list.child(
                div()
                    .p_2()
                    .text_color(color(theme::RED))
                    .child(error.clone()),
            );
        }

        v_flex()
            .w(px(248.))
            .h_full()
            .flex_shrink_0()
            .bg(color(theme::SIDEBAR))
            .border_r_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_1()
                    .text_size(px(11.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child("Tablas"),
            )
            .child(list)
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.is_running();
        let run_shortcut = if cfg!(target_os = "macos") {
            "⌘↵"
        } else {
            "Ctrl+↵"
        };
        h_flex()
            .h(px(44.))
            .px_4()
            .gap_2()
            .flex_shrink_0()
            .bg(color(theme::TOOLBAR))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                Icon::new(AppIcon::Database)
                    .size(px(16.))
                    .text_color(color(theme::TEXT_SECONDARY)),
            )
            .child(
                v_flex()
                    .child(
                        div()
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .child("Query 1"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(color(theme::TEXT_TERTIARY))
                            .child(format!("{} — Solo lectura", self.target)),
                    ),
            )
            .child(div().flex_1())
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py(px(3.))
                    .rounded(px(6.))
                    .bg(color(theme::ACCENT).opacity(0.16))
                    .text_color(color(0x4da3ff))
                    .text_size(px(12.))
                    .child(Icon::new(AppIcon::Lock).size(px(12.)))
                    .child("Solo lectura"),
            )
            .when(running, |row| {
                row.child(Button::new("cancel").small().label("Cancelar").on_click(
                    cx.listener(|this, _, window, cx| this.cancel_query(&CancelQuery, window, cx)),
                ))
            })
            .child(
                Button::new("run")
                    .small()
                    .primary()
                    .label(format!("Ejecutar  {run_shortcut}"))
                    .loading(running)
                    .disabled(self.session().is_none())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.run_query(&RunQuery, window, cx)),
                    ),
            )
    }

    fn render_read_only_banner(&self) -> impl IntoElement {
        h_flex()
            .h(px(28.))
            .px_4()
            .gap_2()
            .flex_shrink_0()
            .bg(color(theme::READ_ONLY_BG))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .text_size(px(12.))
            .text_color(color(theme::READ_ONLY_FG))
            .child(Icon::new(AppIcon::Lock).size(px(13.)))
            .child(
                "Query de solo lectura: se ejecuta en una transacción READ ONLY. INSERT, UPDATE, DELETE y DDL están bloqueados.",
            )
    }

    fn render_results_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let loaded = self.table.read(cx).delegate().rows();
        let memory = self.table.read(cx).delegate().memory_size() as f64 / 1_048_576.0;
        let (text, tone) = match &self.run {
            RunState::Idle => ("Listo".to_string(), theme::TEXT_SECONDARY),
            RunState::Running { started } => (
                format!(
                    "{} filas · {:.1} s · en streaming…",
                    thousands(loaded),
                    started.elapsed().as_secs_f32()
                ),
                theme::TEXT_SECONDARY,
            ),
            RunState::Done {
                rows,
                elapsed,
                first_batch_after,
            } => (
                format!(
                    "{} filas · {} · primeras filas en {} · {:.0} MB",
                    thousands(*rows),
                    format_duration(*elapsed),
                    first_batch_after
                        .map(format_duration)
                        .unwrap_or_else(|| "—".into()),
                    memory
                ),
                theme::TEXT_SECONDARY,
            ),
            RunState::Failed(message) => (message.clone(), theme::RED),
        };

        h_flex()
            .h(px(36.))
            .px_4()
            .gap_3()
            .flex_shrink_0()
            .bg(color(0x1c1c1e))
            .border_t_1()
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                div()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child("Resultado"),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(color(tone))
                    .truncate()
                    .child(text),
            )
    }
}

fn segment(label: &'static str, selected: bool) -> impl IntoElement {
    let base = div()
        .px_3()
        .h(px(22.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .child(label);
    if selected {
        base.bg(color(0x636366)).text_color(color(0xffffff))
    } else {
        base.text_color(color(0xc7c7cc))
    }
}

fn describe_target(url: &str) -> String {
    // postgres://usuario:clave@host:puerto/base → host:puerto/base, sin credenciales.
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let without_user = without_scheme
        .rsplit_once('@')
        .map(|(_, rest)| rest)
        .unwrap_or(without_scheme);
    without_user
        .split('?')
        .next()
        .unwrap_or(without_user)
        .to_string()
}

fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(digit);
    }
    out
}

fn format_duration(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{} ms", duration.as_millis())
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

impl Focusable for DbAdminView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DbAdminView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.is_running() {
            // Refresca el contador de tiempo mientras llega el streaming.
            window.request_animation_frame();
        }

        v_flex()
            .size_full()
            .key_context("DbAdmin")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::cancel_query))
            .bg(color(theme::GROUND))
            .text_color(color(theme::TEXT))
            .text_size(px(13.))
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.render_sidebar(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .child(self.render_toolbar(cx))
                            .child(self.render_read_only_banner())
                            .child(
                                div()
                                    .h(px(220.))
                                    .flex_shrink_0()
                                    .child(Editor::new(&self.editor).h_full().bordered(false)),
                            )
                            .child(self.render_results_header(cx))
                            .child(
                                div().flex_1().min_h_0().child(
                                    DataTable::new(&self.table).stripe(true).bordered(false),
                                ),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_credentials_in_the_target() {
        assert_eq!(
            describe_target("postgres://dbadmin:secret@localhost:54329/dbadmin?sslmode=disable"),
            "localhost:54329/dbadmin"
        );
    }

    #[test]
    fn groups_thousands_with_spaces() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000_000), "1 000 000");
    }
}
