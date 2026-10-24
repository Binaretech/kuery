//! Local files reuse the SQL results grid and Profile, without a database.

use gpui_kit::component::Disableable as _;
use std::path::Path;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::table::TableState;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Task, Window, div, px,
};
use kuery_files::{
    FileDataset, FileFormat, FileSource, LimitReason, PreviewRequest, PreviewResult, PreviewScope,
};

use crate::assets::AppIcon;
use crate::chrome;
use crate::commands::{QueryDataFiles, ReloadDataFile};
use crate::context_menus::item;
use crate::file_jobs::{self, LoadState};
use crate::profile_view::ProfilePanel;
use crate::results::ResultsDelegate;
use crate::results_pane::ResultsPane;
use crate::theme::{self, color};
use crate::ui::{format_bytes, thousands};

/// Describes the data file at `path`, by its canonical path. It touches the
/// disk, so callers run it in the background.
pub fn load_source(path: std::path::PathBuf) -> Result<FileSource, String> {
    let mut source = FileSource::new(path).map_err(|e| e.to_string())?;
    source.path = source.path.canonicalize().map_err(|e| e.to_string())?;
    Ok(source)
}

/// Query files with no previewed file to query.
pub fn no_files_to_query(window: &mut Window, cx: &mut App) {
    drop(window.prompt(
        gpui_kit::PromptLevel::Info,
        "Open a data file first",
        Some("Wait for a file preview to finish, then choose Query files."),
        &["OK"],
        cx,
    ));
}

pub fn data_file_error(error: &str, window: &mut Window, cx: &mut App) {
    drop(window.prompt(
        gpui_kit::PromptLevel::Critical,
        "Could not open data file",
        Some(error),
        &["OK"],
        cx,
    ));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Data,
    Profile,
    Schema,
}

pub struct FileTab {
    source: FileSource,
    draft: FileSource,
    request: PreviewRequest,
    draft_limit: usize,
    null_literal: Entity<InputState>,
    table: Entity<TableState<ResultsDelegate>>,
    results: Entity<ResultsPane>,
    profile: Entity<ProfilePanel>,
    preview: Option<PreviewResult>,
    error: Option<String>,
    loading: bool,
    jobs: LoadState,
    task: Option<Task<()>>,
    view: View,
    options: bool,
    focus_handle: FocusHandle,
}

impl FileTab {
    pub fn new(source: FileSource, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table = cx.new(|cx| {
            TableState::new(ResultsDelegate::default(), window, cx).cell_selectable(true)
        });
        let results = cx.new(|cx| ResultsPane::new(table.clone(), cx));
        let profile = cx.new(|_| ProfilePanel::default());
        let null_literal = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Custom NULL value (optional)")
                .default_value(source.csv.null_value.clone().unwrap_or_default())
        });
        let mut tab = Self {
            draft: source.clone(),
            source,
            request: PreviewRequest::default(),
            draft_limit: PreviewRequest::default().row_limit,
            null_literal,
            table,
            results,
            profile,
            preview: None,
            error: None,
            loading: false,
            jobs: LoadState::default(),
            task: None,
            view: View::Data,
            options: false,
            focus_handle: cx.focus_handle(),
        };
        tab.reload(cx);
        tab
    }

    pub fn sql_source(
        &self,
    ) -> Option<(
        FileSource,
        kuery_files::FileFingerprint,
        Vec<kuery_files::ColumnSchema>,
    )> {
        self.preview
            .as_ref()
            .map(|p| (self.source.clone(), p.fingerprint.clone(), p.schema.clone()))
    }

    pub fn matches(&self, source: &FileSource) -> bool {
        &self.source == source
    }
    pub fn path(&self) -> &Path {
        &self.source.path
    }
    pub fn title(&self) -> SharedString {
        self.path()
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
            .into()
    }
    pub fn status(&self) -> String {
        if self.loading {
            return "Loading file…".into();
        }
        if self.error.is_some() {
            return "File could not be read".into();
        }
        self.preview
            .as_ref()
            .map(scope_label)
            .unwrap_or_else(|| "No preview".into())
    }

    fn reload_action(&mut self, _: &ReloadDataFile, _: &mut Window, cx: &mut Context<Self>) {
        self.reload(cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.request.row_limit = self.draft_limit;
        self.draft.csv.null_value = match self.null_literal.read(cx).value().to_string() {
            s if s.is_empty() => None,
            s => Some(s),
        };
        self.source = self.draft.clone();
        self.preview = None;
        self.error = None;
        self.loading = true;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().start();
            table.clear_selection(cx);
            table.refresh(cx);
        });
        self.profile.update(cx, |profile, cx| profile.clear(cx));
        if self.jobs.request() {
            self.launch(cx);
        }
        cx.notify();
    }

    fn launch(&mut self, cx: &mut Context<Self>) {
        let ticket = self.jobs.start();
        let source = self.source.clone();
        let request = self.request;
        self.task = Some(cx.spawn(async move |this, cx| {
            let permit = file_jobs::limiter().acquire().await;
            // A queued read may already have been replaced or its tab closed.
            let current = this.update(cx, |this, _| this.jobs.current(ticket));
            let result = match current {
                Ok(true) => Some(
                    cx.background_spawn(async move {
                        let _permit = permit;
                        FileDataset::open(source).and_then(|dataset| dataset.preview(request))
                    })
                    .await,
                ),
                _ => {
                    drop(permit);
                    None
                }
            };
            this.update(cx, |this, cx| {
                if !this.jobs.finish(ticket) {
                    this.launch(cx);
                    return;
                }
                this.loading = false;
                this.task = None;
                if let Some(result) = result {
                    match result {
                        Ok(preview) => {
                            let label = scope_label(&preview);
                            let inferred = preview.schema.iter().map(|c| c.inferred).collect();
                            this.table.update(cx, |table, cx| {
                                table.delegate_mut().load(preview.results.clone());
                                table.refresh(cx);
                                if preview.results.rows() > 0 {
                                    table.scroll_to_row(0, cx);
                                }
                                if !preview.schema.is_empty() {
                                    table.scroll_to_col(0, cx);
                                }
                            });
                            this.profile.update(cx, |profile, cx| {
                                profile.load_scoped(preview.results.clone(), label, inferred, cx);
                            });
                            this.preview = Some(preview);
                        }
                        Err(error) => {
                            this.error = Some(error.to_string());
                            this.table.update(cx, |table, cx| {
                                table.delegate_mut().finish();
                                cx.notify();
                            });
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                chrome::segmented().children(
                    [
                        ("file-data", "Data", AppIcon::Table, View::Data),
                        ("file-profile", "Profile", AppIcon::Chart, View::Profile),
                        ("file-schema", "Columns", AppIcon::Plan, View::Schema),
                    ]
                    .into_iter()
                    .map(|(id, label, icon, view)| {
                        chrome::segment(id, icon, label, self.view == view).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.view = view;
                                cx.notify();
                            },
                        ))
                    }),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .child(format!(
                        "{} · {}",
                        format_name(self.source.format),
                        self.path().display()
                    )),
            )
            .child(
                Button::new("file-query")
                    .ghost()
                    .label("Query files")
                    .disabled(self.preview.is_none())
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(QueryDataFiles), cx)),
            )
            .child(
                Button::new("file-options")
                    .ghost()
                    .label("Read options")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.options = !this.options;
                        cx.notify();
                    })),
            )
            .child(
                chrome::icon_button(
                    "file-reload",
                    AppIcon::Reload,
                    "Reload file / apply read options",
                    true,
                )
                .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
    }

    fn options(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let mut bar = h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .text_size(px(12.))
            .border_b_1()
            .border_color(color(theme::HAIRLINE));
        if self.source.format == FileFormat::Csv {
            let selected = self.draft.csv.separator;
            let header = self.draft.csv.has_header;
            let empty = self.draft.csv.empty_is_null;
            let options_weak = weak.clone();
            bar = bar
                .child(
                    Button::new("csv-options")
                        .ghost()
                        .label("CSV settings")
                        .dropdown_menu(move |mut menu, _, _| {
                            for (label, separator) in
                                [("Comma", b','), ("Semicolon", b';'), ("Tab", b'\t')]
                            {
                                menu = menu.item(
                                    item(label, &options_weak, move |this, _, cx| {
                                        this.draft.csv.separator = separator;
                                        cx.notify();
                                    })
                                    .checked(selected == separator),
                                );
                            }
                            menu.separator()
                                .item(
                                    item("First row is a header", &options_weak, |this, _, cx| {
                                        this.draft.csv.has_header = !this.draft.csv.has_header;
                                        cx.notify();
                                    })
                                    .checked(header),
                                )
                                .item(
                                    item("Empty fields are NULL", &options_weak, |this, _, cx| {
                                        this.draft.csv.empty_is_null =
                                            !this.draft.csv.empty_is_null;
                                        cx.notify();
                                    })
                                    .checked(empty),
                                )
                        }),
                )
                .child(div().w(px(240.)).child(Input::new(&self.null_literal)));
        }
        let selected = self.draft_limit;
        bar.child(
            Button::new("file-limit")
                .ghost()
                .label(format!("First {} rows", thousands(selected as u64)))
                .dropdown_menu(move |mut menu, _, _| {
                    for rows in [1_000, 10_000, 50_000, 100_000] {
                        menu = menu.item(
                            item(
                                format!("{} rows", thousands(rows as u64)),
                                &weak,
                                move |this, _, cx| {
                                    this.draft_limit = rows;
                                    cx.notify();
                                },
                            )
                            .checked(selected == rows),
                        );
                    }
                    menu
                }),
        )
        .child(div().flex_1())
        .child(
            Button::new("file-apply")
                .label("Apply & reload")
                .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
        )
    }
}

impl Focusable for FileTab {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FileTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = v_flex()
            .size_full()
            .key_context("FileTab")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::reload_action))
            .child(self.toolbar(cx));
        if self.options {
            content = content.child(self.options(cx));
        }
        content = content.child(
            div()
                .px_3()
                .py_1()
                .text_size(px(12.))
                .text_color(color(theme::TEXT_SECONDARY))
                .child(self.status()),
        );
        if let Some(error) = &self.error {
            return content
                .child(
                    div()
                        .p_4()
                        .text_color(color(theme::AMBER))
                        .child(error.clone()),
                )
                .into_any_element();
        }
        if self.loading {
            return content
                .child(
                    div()
                        .p_4()
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child("Reading preview… You can switch tabs or close this file."),
                )
                .into_any_element();
        }
        content = content.child(div().flex_1().min_h_0().child(match self.view {
            View::Data => self.results.clone().into_any_element(),
            View::Profile => self.profile.clone().into_any_element(),
            View::Schema => {
                let mut schema = v_flex()
                    .id("file-schema-list")
                    .overflow_y_scroll()
                    .p_4()
                    .gap_2();
                if let Some(preview) = &self.preview {
                    for column in &preview.schema {
                        schema = schema.child(h_flex().gap_4().child(column.name.clone()).child(
                            format!(
                                "{}{}",
                                column.native_type,
                                if column.inferred { " · inferred" } else { "" }
                            ),
                        ));
                    }
                    for warning in &preview.warnings {
                        schema = schema.child(
                            div()
                                .text_size(px(12.))
                                .text_color(color(theme::TEXT_TERTIARY))
                                .child(warning.clone()),
                        );
                    }
                }
                schema.into_any_element()
            }
        }));
        content.into_any_element()
    }
}

fn scope_label(preview: &PreviewResult) -> String {
    let count = preview.results.rows();
    let rows = format!(
        "{} {}",
        thousands(count as u64),
        if count == 1 { "row" } else { "rows" }
    );
    let bytes = format_bytes(preview.results.memory_size() as u64);
    match &preview.scope {
        PreviewScope::Complete => format!("Complete file · {rows} · {bytes}"),
        PreviewScope::PrefixLimited { reason, .. } => format!(
            "First {rows} · {} limit reached · {bytes} · Profile uses this prefix",
            match reason {
                LimitReason::Rows => "row",
                LimitReason::Bytes => "preview memory",
            }
        ),
    }
}

fn format_name(format: FileFormat) -> &'static str {
    match format {
        FileFormat::Csv => "CSV",
        FileFormat::Json => "JSON",
        FileFormat::NdJson => "JSONL",
        FileFormat::Parquet => "Parquet",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuery_core::result_set::{BatchBuilder, ResultSet};

    #[test]
    fn scope_distinguishes_loaded_prefix_from_whole_file_and_requested_limit() {
        let mut results = ResultSet::new(vec!["value".into()]);
        let mut batch = BatchBuilder::new(1, 1);
        batch.push_row(|_| Some("x"));
        results.push(batch.finish());
        let mut preview = PreviewResult {
            results,
            schema: vec![],
            warnings: vec![],
            fingerprint: kuery_files::FileFingerprint {
                bytes: 1_000_000,
                modified: std::time::SystemTime::UNIX_EPOCH,
            },
            scope: PreviewScope::PrefixLimited {
                loaded_rows: 1,
                row_limit: 10_000,
                reason: LimitReason::Bytes,
            },
        };
        let prefix = scope_label(&preview);
        assert!(prefix.starts_with("First 1 row"));
        assert!(prefix.contains("Profile uses this prefix"));
        assert!(!prefix.contains("10,000"));
        assert!(!prefix.contains("Complete file"));
        preview.scope = PreviewScope::Complete;
        assert!(scope_label(&preview).starts_with("Complete file · 1 row"));
    }
}
