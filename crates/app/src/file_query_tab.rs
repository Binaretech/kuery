//! SQL over a named set of local files, using the shared SQL editor and grid.
use crate::{
    assets::AppIcon,
    chrome,
    completion::{SharedCatalog, SqlCompletions},
    file_jobs,
    profile_view::ProfilePanel,
    query_tab::{CancelQuery, FormatSql, RunQuery},
    results::ResultsDelegate,
    results_pane::ResultsPane,
    theme::{self, color},
    ui::{format_duration, thousands},
};
use gpui_kit::component::{
    Icon, IconName, h_flex,
    input::{Editor, EditorState},
    table::TableState,
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, px,
};
use kuery_core::{
    catalog::{TableKind, TableRef},
    completion::{Catalog, Column, Table},
    format, script,
};
use kuery_files::{ColumnSchema, PreviewRequest, PreviewScope, SqlFile};
use std::{rc::Rc, time::Instant};

#[derive(Clone)]
pub struct Source {
    pub file: SqlFile,
    pub columns: Vec<ColumnSchema>,
}

/// What Query files queries: every file with a preview, the one at `active`
/// first.
pub fn query_sources<'a>(
    files: impl IntoIterator<Item = &'a crate::file_tab::FileTab>,
    active: Option<&std::path::Path>,
) -> Vec<Source> {
    let mut sources = files
        .into_iter()
        .filter_map(|file| file.sql_source())
        .collect::<Vec<_>>();
    if let Some(index) = active.and_then(|active| {
        sources
            .iter()
            .position(|(source, _, _)| source.path == active)
    }) {
        sources.rotate_left(index);
    }
    name_sources(sources)
}

/// Names are independent of SQL syntax/filenames, and collisions are explicit.
pub fn name_sources(
    sources: Vec<(
        kuery_files::FileSource,
        kuery_files::FileFingerprint,
        Vec<ColumnSchema>,
    )>,
) -> Vec<Source> {
    let mut used = std::collections::HashSet::new();
    sources
        .into_iter()
        .map(|(source, fingerprint, columns)| {
            let stem = source
                .path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy();
            let mut base: String = stem
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        c.to_ascii_lowercase()
                    } else {
                        '_'
                    }
                })
                .collect();
            if base.is_empty() {
                base = "file".into();
            }
            if base.as_bytes()[0].is_ascii_digit() {
                base = format!("file_{base}");
            }
            let mut name = base.clone();
            let mut suffix = 2;
            while !used.insert(name.clone()) {
                name = format!("{base}_{suffix}");
                suffix += 1;
            }
            Source {
                file: SqlFile {
                    name,
                    source,
                    fingerprint,
                },
                columns,
            }
        })
        .collect()
}

pub struct FileQueryTab {
    sources: Vec<Source>,
    editor: Entity<EditorState>,
    table: Entity<TableState<ResultsDelegate>>,
    results: Entity<ResultsPane>,
    profile: Entity<ProfilePanel>,
    show_profile: bool,
    status: String,
    error: Option<String>,
    running: bool,
    generation: u64,
    task: Option<Task<()>>,
    focus: FocusHandle,
}
impl FileQueryTab {
    pub fn new(sources: Vec<Source>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sql = format!(
            "-- Queries read entire files; the grid shows up to 10,000 result rows.\nSELECT * FROM \"{}\" LIMIT 100;\n",
            sources[0].file.name
        );
        let catalog: SharedCatalog = Rc::new(std::cell::RefCell::new(Catalog {
            search_path: vec![String::new()],
            tables: sources
                .iter()
                .map(|s| Table {
                    table: TableRef {
                        schema: String::new(),
                        name: s.file.name.clone(),
                    },
                    kind: TableKind::Table,
                    columns: s
                        .columns
                        .iter()
                        .map(|c| Column {
                            name: c.name.clone(),
                            data_type: c.native_type.clone(),
                        })
                        .collect(),
                })
                .collect(),
            ..Default::default()
        }));
        let editor = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(sql);
            editor.lsp_mut().completion_provider = Some(Rc::new(SqlCompletions::new(catalog)));
            editor
        });
        let table = cx.new(|cx| {
            TableState::new(ResultsDelegate::default(), window, cx).cell_selectable(true)
        });
        Self {
            sources,
            editor,
            results: cx.new(|cx| ResultsPane::new(table.clone(), cx)),
            table,
            profile: cx.new(|_| ProfilePanel::default()),
            show_profile: false,
            status: "Ready · full file inputs · read only".into(),
            error: None,
            running: false,
            generation: 0,
            task: None,
            focus: cx.focus_handle(),
        }
    }
    pub fn title(&self) -> SharedString {
        "File SQL".into()
    }
    pub fn status(&self) -> &str {
        &self.status
    }
    fn run(&mut self, _: &RunQuery, _: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let editor = self.editor.read(cx);
        let selected = editor.selected_text().to_string();
        let text = editor.text().to_string();
        let sql = if !selected.trim().is_empty() {
            selected
        } else {
            script::statement_at(&text, editor.cursor())
                .map(|r| text[r].to_owned())
                .unwrap_or(text)
        };
        if sql.trim().is_empty() {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.running = true;
        self.error = None;
        self.status = "Running · full file inputs…".into();
        self.table.update(cx, |table, cx| {
            table.delegate_mut().start();
            table.clear_selection(cx);
            table.refresh(cx);
        });
        self.profile.update(cx, |p, cx| p.clear(cx));
        let files = self
            .sources
            .iter()
            .map(|s| s.file.clone())
            .collect::<Vec<_>>();
        self.task = Some(cx.spawn(async move |this, cx| {
            let permit = file_jobs::limiter().acquire().await;
            if !this
                .update(cx, |this, _| this.generation == generation && this.running)
                .unwrap_or(false)
            {
                return;
            }
            let started = Instant::now();
            let result = cx
                .background_spawn(async move {
                    let _permit = permit;
                    kuery_files::query_files(&files, &sql, PreviewRequest::default())
                })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.running = false;
                this.task = None;
                match result {
                    Ok(result) => {
                        let prefix = !matches!(result.scope, PreviewScope::Complete);
                        this.status = format!(
                            "{}{} result rows · {} · full file inputs",
                            if prefix { "First " } else { "" },
                            thousands(result.results.rows() as u64),
                            format_duration(started.elapsed())
                        );
                        this.profile.update(cx, |p, cx| {
                            p.load_scoped(result.results.clone(), this.status.clone(), vec![], cx)
                        });
                        this.table.update(cx, |table, cx| {
                            let has_columns = !result.results.columns().is_empty();
                            table.delegate_mut().load(result.results);
                            table.refresh(cx);
                            if table.delegate().rows() > 0 {
                                table.scroll_to_row(0, cx);
                            }
                            if has_columns {
                                table.scroll_to_col(0, cx);
                            }
                        });
                    }
                    Err(error) => {
                        this.table.update(cx, |t, cx| {
                            t.delegate_mut().finish();
                            cx.notify();
                        });
                        this.status = "Query failed".into();
                        this.error = Some(error.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
    fn cancel(&mut self, _: &CancelQuery, _: &mut Window, cx: &mut Context<Self>) {
        if !self.running {
            return;
        }
        self.table.update(cx, |t, cx| {
            t.delegate_mut().finish();
            cx.notify();
        });
        self.generation += 1;
        // Keep the task alive until its worker ends. Its permit remains held.
        self.running = false;
        self.status = "Cancelled · a running Polars worker may finish in the background".into();
        cx.notify();
    }
    fn format(&mut self, _: &FormatSql, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let selected = editor.selected_text().to_string();
            if selected.trim().is_empty() {
                editor.replace_all(format::format_sql(&editor.text().to_string()), window, cx);
            } else {
                editor.replace(format::format_sql(&selected), window, cx);
            }
        });
    }
}
impl FileQueryTab {
    /// Run, Cancel and Format as in a database query tab, then the tables
    /// this tab can read. The status bar says it is Polars SQL and read only.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mac = cfg!(target_os = "macos");
        h_flex()
            .h(px(46.))
            .px_3()
            .gap(px(6.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                chrome::primary_button(
                    "file-sql-run",
                    IconName::Play,
                    if self.running { "Running" } else { "Run" },
                    Some(if mac { "⌘↵" } else { "Ctrl+↵" }),
                )
                .on_click(cx.listener(|this, _, window, cx| this.run(&RunQuery, window, cx))),
            )
            .child(
                chrome::icon_button(
                    "file-sql-cancel",
                    IconName::Square,
                    if mac {
                        "Cancel  ⌘."
                    } else {
                        "Cancel  Ctrl+."
                    },
                    self.running,
                )
                .when(self.running, |b| {
                    b.on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&CancelQuery, window, cx)),
                    )
                }),
            )
            .child(
                chrome::icon_button(
                    "file-sql-format",
                    AppIcon::Format,
                    if mac {
                        "Format SQL  ⌘⇧F"
                    } else {
                        "Format SQL  Ctrl+Shift+F"
                    },
                    true,
                )
                .on_click(cx.listener(|this, _, window, cx| this.format(&FormatSql, window, cx))),
            )
            .child(chrome::toolbar_divider())
            .child(h_flex().min_w_0().gap(px(6.)).overflow_hidden().children(
                self.sources.iter().enumerate().map(|(i, source)| {
                    // The path and options are fixed when the tab opens.
                    let path: SharedString = format!(
                        "{} · fixed when this tab opened",
                        source.file.source.path.display()
                    )
                    .into();
                    h_flex()
                        .id(("file-sql-source", i))
                        .h(px(24.))
                        .flex_shrink_0()
                        .gap(px(5.))
                        .px(px(8.))
                        .rounded(px(6.))
                        .bg(color(theme::CONTROL))
                        .text_size(px(12.))
                        .text_color(color(theme::TEXT_SECONDARY))
                        .child(Icon::new(AppIcon::Table).size(px(12.)))
                        .child(source.file.name.clone())
                        .tooltip(move |window, cx| Tooltip::new(path.clone()).build(window, cx))
                }),
            ))
    }

    /// Results and Profile, as tabs; the run's figures are in the status bar.
    fn render_panel_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self.table.read(cx).delegate().results();
        let count = (!results.columns().is_empty()).then(|| thousands(results.rows() as u64));
        let tab = |id: &'static str,
                   icon: AppIcon,
                   label: &'static str,
                   count: Option<String>,
                   profile: bool,
                   cx: &mut Context<Self>| {
            let active = self.show_profile == profile;
            h_flex()
                .id(id)
                .h(px(26.))
                .gap(px(6.))
                .px(px(10.))
                .rounded(px(6.))
                .font_weight(FontWeight::MEDIUM)
                .when(active, |t| {
                    t.bg(color(theme::ACTIVE)).text_color(color(theme::TEXT))
                })
                .when(!active, |t| {
                    t.text_color(color(theme::TEXT_TERTIARY))
                        .hover(|s| s.text_color(color(theme::TEXT)))
                })
                .child(Icon::new(icon).size(px(13.)))
                .child(label)
                .children(count.map(|count| {
                    div()
                        .text_size(px(11.))
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child(count)
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.show_profile = profile;
                    cx.notify();
                }))
        };
        h_flex()
            .h(px(36.))
            .px_2()
            .gap(px(2.))
            .flex_shrink_0()
            .border_t_1()
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .text_size(px(12.5))
            .child(tab(
                "file-sql-results",
                AppIcon::Table,
                "Results",
                count,
                false,
                cx,
            ))
            .child(tab(
                "file-sql-profile",
                AppIcon::Chart,
                "Profile",
                None,
                true,
                cx,
            ))
    }
}
impl Focusable for FileQueryTab {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for FileQueryTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .key_context("FileQueryTab")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::run))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::format))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .h(px(240.))
                    .flex_shrink_0()
                    .child(Editor::new(&self.editor).h_full().bordered(false)),
            )
            .child(self.render_panel_header(cx))
            .children(
                self.error
                    .clone()
                    .map(|e| div().p_3().text_color(color(theme::AMBER)).child(e)),
            )
            .child(div().flex_1().min_h_0().child(if self.show_profile {
                self.profile.clone().into_any_element()
            } else {
                self.results.clone().into_any_element()
            }))
    }
}
