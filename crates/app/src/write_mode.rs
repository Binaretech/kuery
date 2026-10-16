//! Write mode for query tabs: what a script needs before it runs, and the
//! dialog that allows writes on a production connection for its session.
//!
//! The flow, per tab: write mode opens a connection of its own. In
//! transactional write ([`WriteKind::Transactional`]) scripts that write run
//! inside a transaction that stays open until Commit or Roll Back; in full
//! write ([`WriteKind::Full`]) each script is kept as soon as it finishes.
//! Each environment decides what needs confirmation (see [`WritePolicy`]).

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, WeakEntity, Window, div, px,
};
use kuery_core::analysis::{self, StatementKind};
use kuery_core::read_only::Rejection;

use kuery_core::session::StatementImpact;
use kuery_core::sql::Dialect;

use crate::assets::AppIcon;
use crate::connection::{Db, Environment, RedisDb, WriteKind, WritePolicy};
use crate::theme::{self, color};
use crate::ui::thousands;

/// What to do with a script in write mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Review {
    /// Run it; `writes` says whether it goes into the write transaction.
    Run { writes: bool },
    /// Show the review dialog first, then run it as a write.
    Confirm(Confirmation),
    /// Do not run it.
    Refuse(String),
}

/// What the review dialog shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    /// `Review 2 changes`, `Apply 2 changes now?`…
    pub title: String,
    /// Every statement of the script, in order (none when it cannot be
    /// analysed).
    pub rows: Vec<ReviewRow>,
    /// Set when the SQL could not be analysed.
    pub warning: Option<String>,
    /// Where the changes go: a transaction, or applied right away.
    pub note: String,
    /// The run button: `Run 2 changes`, `Apply 1 change`, `Run anyway`.
    pub run_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRow {
    pub summary: String,
    pub check: Check,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Read,
    /// A change with nothing flagged.
    Targeted,
    /// Flagged, with why (e.g. no WHERE).
    Risky(String),
}

fn note(kind: WriteKind) -> String {
    match kind {
        WriteKind::Transactional => "Changes stay in a transaction until you commit them.".into(),
        WriteKind::Full => {
            "The script is applied as soon as it finishes, all of it or nothing. There is no \
             Roll Back."
                .into()
        }
    }
}

pub fn review(dialect: Dialect, sql: &str, environment: Environment, kind: WriteKind) -> Review {
    let policy = environment.write_policy();
    let analysis = match analysis::analyze_in(dialect, sql) {
        Ok(analysis) => analysis,
        Err(Rejection::Empty) => return Review::Refuse("There is no statement to run".into()),
        Err(error) if policy.allows_unanalysed() => {
            return Review::Confirm(Confirmation {
                title: "Kuery could not analyse this SQL".into(),
                rows: Vec::new(),
                warning: Some(format!(
                    "{error}. {} without the usual checks: read it again before running it.",
                    match kind {
                        WriteKind::Transactional => "It will run in the open transaction",
                        WriteKind::Full => "It will be applied right away",
                    }
                )),
                note: note(kind),
                run_label: "Run anyway".into(),
            });
        }
        Err(error) => {
            return Review::Refuse(format!(
                "{error}. SQL that cannot be analysed does not run on production."
            ));
        }
    };
    if let Some(statement) = analysis.transaction_control() {
        return Review::Refuse(match kind {
            WriteKind::Transactional => format!(
                "{} is not needed: write mode keeps the transaction open until you use \
                 Commit or Roll Back",
                statement.summary
            ),
            WriteKind::Full => format!(
                "{} is not needed: full write applies each script as one unit. For a \
                 transaction you commit, switch to Write",
                statement.summary
            ),
        });
    }
    if !analysis.writes() {
        return Review::Run { writes: false };
    }
    let dangerous = analysis.dangers().next().is_some();
    if !policy.confirms(dangerous) {
        return Review::Run { writes: true };
    }
    let rows: Vec<ReviewRow> = analysis
        .statements
        .iter()
        .map(|statement| ReviewRow {
            summary: statement.summary.clone(),
            check: match (statement.kind, statement.dangers.first()) {
                (_, Some(danger)) => Check::Risky(danger.to_string()),
                (StatementKind::Read, None) => Check::Read,
                (_, None) => Check::Targeted,
            },
        })
        .collect();
    let changes = plural(
        rows.iter().filter(|row| row.check != Check::Read).count(),
        "change",
    );
    Review::Confirm(Confirmation {
        title: match kind {
            WriteKind::Transactional => format!("Review {changes}"),
            WriteKind::Full => format!("Apply {changes} now?"),
        },
        rows,
        warning: None,
        note: note(kind),
        run_label: match kind {
            WriteKind::Transactional => format!("Run {changes}"),
            WriteKind::Full => format!("Apply {changes}"),
        },
    })
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// One line per statement of a previewed script, e.g. `UPDATE orders: 1,204
/// rows would change`. Statements are named from the analysis when it lines
/// up with what the server ran, and numbered otherwise.
pub fn describe_impact(dialect: Dialect, sql: &str, impacts: &[StatementImpact]) -> Vec<String> {
    let summaries: Option<Vec<String>> = analysis::analyze_in(dialect, sql)
        .ok()
        .map(|a| a.statements.into_iter().map(|s| s.summary).collect())
        .filter(|summaries: &Vec<String>| summaries.len() == impacts.len());
    impacts
        .iter()
        .enumerate()
        .map(|(index, impact)| {
            let name = summaries
                .as_ref()
                .map_or_else(|| format!("Statement {}", index + 1), |s| s[index].clone());
            let rows = match impact.rows {
                1 => "1 row".to_string(),
                n => format!("{} rows", thousands(n)),
            };
            if impact.returns_rows {
                format!("{name}: {rows} returned")
            } else {
                format!("{name}: {rows} would change")
            }
        })
        .collect()
}

/// Short description of the policy for the write-mode banner.
pub fn policy_hint(policy: WritePolicy) -> &'static str {
    match policy {
        WritePolicy::ConfirmDangerous => "asks before risky statements",
        WritePolicy::ConfirmWrites => "asks before every change",
        WritePolicy::SessionUnlock => "allowed for this session only · asks before every change",
    }
}

/// Asks to type the connection name, then allows writes of `kind` on it
/// until it disconnects or the app quits. `allowed` runs after that.
pub fn allow_writes(
    db: Db,
    kind: WriteKind,
    window: &mut Window,
    cx: &mut App,
    allowed: impl Fn(&mut Window, &mut App) + 'static,
) {
    let subject = Subject {
        name: db.name.clone(),
        target: db.target.clone(),
        scope: Scope::Postgres(kind),
        allow: Rc::new(move || db.allow(kind)),
    };
    open_allow(subject, window, cx, allowed);
}

/// The same for a Redis connection: its console and key tabs may write
/// until it disconnects or the app quits.
pub fn allow_redis_writes(
    db: RedisDb,
    window: &mut Window,
    cx: &mut App,
    allowed: impl Fn(&mut Window, &mut App) + 'static,
) {
    let subject = Subject {
        name: db.name.clone(),
        target: db.target.clone(),
        scope: Scope::Redis,
        allow: Rc::new(move || db.allow_writes()),
    };
    open_allow(subject, window, cx, allowed);
}

fn open_allow(
    subject: Subject,
    window: &mut Window,
    cx: &mut App,
    allowed: impl Fn(&mut Window, &mut App) + 'static,
) {
    let dialog = cx.new(|cx| AllowWrites::new(subject, Rc::new(allowed), window, cx));
    let first_build = std::cell::Cell::new(true);
    window.open_dialog(cx, move |modal, window, cx| {
        if first_build.replace(false) {
            let input = dialog.read(cx).input.focus_handle(cx);
            window.focus(&input, cx);
        }
        modal
            .w(px(520.))
            .overlay_closable(false)
            .child(dialog.clone())
    });
}

/// Runs once writes are allowed.
type OnAllowed = Rc<dyn Fn(&mut Window, &mut App)>;

/// What a production connection is being allowed to do.
enum Scope {
    Postgres(WriteKind),
    Redis,
}

/// The connection writes are allowed on, and how to allow them.
struct Subject {
    name: SharedString,
    target: SharedString,
    scope: Scope,
    allow: Rc<dyn Fn()>,
}

struct AllowWrites {
    subject: Subject,
    input: Entity<InputState>,
    allowed: OnAllowed,
    error: Option<String>,
}

impl AllowWrites {
    fn new(
        subject: Subject,
        allowed: OnAllowed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(subject.name.clone()));
        cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.confirm(window, cx),
                // The button turns on as the name is typed.
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        )
        .detach();
        Self {
            subject,
            input,
            allowed,
            error: None,
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.read(cx).value().trim() != self.subject.name.as_ref() {
            self.error = Some(format!(
                "Type \u{201c}{}\u{201d} exactly",
                self.subject.name
            ));
            cx.notify();
            return;
        }
        (self.subject.allow)();
        window.close_dialog(cx);
        (self.allowed)(window, cx);
    }
}

impl Render for AllowWrites {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let subject = &self.subject;
        let matches = self.input.read(cx).value().trim() == subject.name.as_ref();
        let point = |icon: AppIcon, text: &'static str| {
            h_flex()
                .gap(px(9.))
                .child(
                    Icon::new(icon)
                        .size(px(14.))
                        .text_color(color(theme::TEXT_TERTIARY)),
                )
                .child(text)
        };
        let (icon, title, description, confirmed, button) = match subject.scope {
            Scope::Postgres(WriteKind::Transactional) => (
                AppIcon::Pencil,
                "Allow writes on production?",
                format!(
                    "Query tabs on {} ({}) will be able to change data.",
                    subject.name, subject.target
                ),
                "Every change is confirmed and stays in a transaction until you commit",
                "Allow Writes",
            ),
            Scope::Postgres(WriteKind::Full) => (
                AppIcon::FullWrite,
                "Allow full writes on production?",
                format!(
                    "Each script runs against {} and is applied as soon as it finishes. There \
                     is no Roll Back.",
                    subject.name
                ),
                "Every change is still confirmed before it runs",
                "Allow Full Writes",
            ),
            Scope::Redis => (
                AppIcon::Pencil,
                "Allow writes on production?",
                format!(
                    "The console and key tabs on {} ({}) will be able to change data. Redis \
                     has no transactions: each command applies as soon as it runs.",
                    subject.name, subject.target
                ),
                "Every change is confirmed before it runs",
                "Allow Writes",
            ),
        };
        v_flex()
            .gap_4()
            .text_size(px(13.))
            .child(
                h_flex()
                    .gap(px(14.))
                    .items_start()
                    .child(icon_box(icon, theme::AMBER))
                    .child(
                        v_flex()
                            .flex_1()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .child(div().text_color(color(0xc7c7cc)).child(description)),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .p(px(12.))
                    .rounded(px(9.))
                    .bg(color(0x18181a))
                    .border_1()
                    .border_color(color(theme::HAIRLINE))
                    .text_size(px(12.5))
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(point(
                        AppIcon::History,
                        "Lasts until the connection closes or you quit Kuery",
                    ))
                    .child(point(AppIcon::Pencil, confirmed))
                    .child(point(
                        AppIcon::Audit,
                        "Logged in the audit log, never saved in the project",
                    )),
            )
            .child(
                v_flex()
                    .gap(px(7.))
                    .child(
                        h_flex()
                            .gap_1()
                            .text_size(px(12.5))
                            .text_color(color(0xc7c7cc))
                            .child("Type")
                            .child(
                                div()
                                    .font_family(mono)
                                    .text_color(color(theme::TEXT))
                                    .child(subject.name.clone()),
                            )
                            .child("to confirm"),
                    )
                    .child(Input::new(&self.input)),
            )
            .children(self.error.clone().map(|error| {
                div()
                    .text_size(px(12.))
                    .text_color(color(theme::RED))
                    .child(error)
            }))
            .child(
                h_flex()
                    .pt_1()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        Button::new("cancel-allow-writes")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("allow-writes")
                            .danger()
                            .label(button)
                            .disabled(!matches)
                            .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
    }
}

/// A 36 px rounded square with an icon, for dialog headers.
pub fn icon_box(icon: AppIcon, tone: u32) -> impl IntoElement {
    div()
        .size(px(36.))
        .flex_shrink_0()
        .rounded(px(9.))
        .bg(color(theme::ACTIVE))
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon).size(px(18.)).text_color(color(tone)))
}

/// Runs a preview for the review dialog; the dialog shows the result.
type OnPreview = Rc<dyn Fn(WeakEntity<ReviewDialog>, &mut Window, &mut App)>;

/// The review shown before a script that writes: its statements and what
/// was flagged, the rows a preview counted, and the button that runs it.
pub struct ReviewDialog {
    confirmation: Confirmation,
    connection: SharedString,
    environment: Environment,
    kind: WriteKind,
    preview: PreviewState,
    on_preview: OnPreview,
    on_run: OnAllowed,
}

enum PreviewState {
    None,
    Running,
    Done(Vec<StatementImpact>),
    Failed(String),
}

/// Opens the review dialog over `window`. `on_preview` runs the preview and
/// reports back with [`ReviewDialog::set_preview`]; `on_run` runs the script
/// once the dialog has closed.
pub fn open_review(
    confirmation: Confirmation,
    db: &Db,
    kind: WriteKind,
    window: &mut Window,
    cx: &mut App,
    on_preview: impl Fn(WeakEntity<ReviewDialog>, &mut Window, &mut App) + 'static,
    on_run: impl Fn(&mut Window, &mut App) + 'static,
) {
    let dialog = cx.new(|_| ReviewDialog {
        confirmation,
        connection: db.name.clone(),
        environment: db.environment,
        kind,
        preview: PreviewState::None,
        on_preview: Rc::new(on_preview),
        on_run: Rc::new(on_run),
    });
    window.open_dialog(cx, move |modal, _, _| {
        modal
            .w(px(660.))
            .overlay_closable(false)
            .child(dialog.clone())
    });
}

impl ReviewDialog {
    /// The preview's outcome: impacts, or why it failed.
    pub fn set_preview(
        &mut self,
        result: Result<Vec<StatementImpact>, String>,
        cx: &mut Context<Self>,
    ) {
        self.preview = match result {
            Ok(impacts) => PreviewState::Done(impacts),
            Err(error) => PreviewState::Failed(error),
        };
        cx.notify();
    }

    fn start_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.preview, PreviewState::Running) {
            return;
        }
        self.preview = PreviewState::Running;
        cx.notify();
        (self.on_preview)(cx.weak_entity(), window, cx);
    }

    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        (self.on_run)(window, cx);
    }

    /// The counted rows per statement, when the preview lines up with them.
    fn counts(&self) -> Option<&[StatementImpact]> {
        match &self.preview {
            PreviewState::Done(impacts) if impacts.len() == self.confirmation.rows.len() => {
                Some(impacts)
            }
            _ => None,
        }
    }
}

impl Render for ReviewDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let counts = self.counts().map(<[StatementImpact]>::to_vec);
        let mut table = v_flex()
            .rounded(px(9.))
            .border_1()
            .border_color(color(theme::HAIRLINE))
            .overflow_hidden()
            .text_size(px(12.5))
            .child(
                h_flex()
                    .h(px(32.))
                    .bg(color(0x18181a))
                    .text_size(px(11.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(div().flex_1().px(px(14.)).child("Statement"))
                    .child(div().w(px(130.)).px(px(14.)).child("Check"))
                    .child(div().w(px(130.)).px(px(14.)).text_right().child("Rows")),
            );
        for (index, row) in self.confirmation.rows.iter().enumerate() {
            let (badge, tone, fill, why) = match &row.check {
                Check::Risky(why) => (
                    "Risky",
                    0xff8a80,
                    color(theme::RED).opacity(0.12),
                    Some(why.clone()),
                ),
                Check::Targeted => ("Change", 0xc7c7cc, color(theme::ACTIVE), None),
                Check::Read => ("Read", theme::TEXT_TERTIARY, color(theme::ACTIVE), None),
            };
            let rows = counts.as_ref().map(|counts| {
                let impact = counts[index];
                if impact.returns_rows {
                    format!("{} returned", thousands(impact.rows))
                } else {
                    thousands(impact.rows)
                }
            });
            table = table.child(
                h_flex()
                    .min_h(px(44.))
                    .border_t_1()
                    .border_color(color(theme::HAIRLINE))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .px(px(14.))
                            .py(px(8.))
                            .gap(px(3.))
                            .child(
                                div()
                                    .font_family(mono.clone())
                                    .truncate()
                                    .child(row.summary.clone()),
                            )
                            .children(why.map(|why| {
                                div()
                                    .text_size(px(11.5))
                                    .text_color(color(theme::TEXT_SECONDARY))
                                    .child(why)
                            })),
                    )
                    .child(
                        div().w(px(130.)).px(px(14.)).child(
                            h_flex()
                                .h(px(22.))
                                .px(px(8.))
                                .gap(px(5.))
                                .rounded(px(11.))
                                .bg(fill)
                                .text_color(color(tone))
                                .text_size(px(11.5))
                                .font_weight(FontWeight::MEDIUM)
                                .when(matches!(row.check, Check::Risky(_)), |b| {
                                    b.child(Icon::new(IconName::TriangleAlert).size(px(11.)))
                                })
                                .child(badge),
                        ),
                    )
                    .child(
                        div()
                            .w(px(130.))
                            .px(px(14.))
                            .text_right()
                            .font_family(mono.clone())
                            .text_color(color(if rows.is_some() {
                                theme::TEXT
                            } else {
                                theme::TEXT_QUIET
                            }))
                            .child(rows.unwrap_or_else(|| "—".into())),
                    ),
            );
        }

        let previewed = matches!(self.preview, PreviewState::Done(_));
        let extra_lines: Vec<String> = match &self.preview {
            PreviewState::Done(impacts) if counts.is_none() => {
                describe_impact_rows(&self.confirmation, impacts)
            }
            _ => Vec::new(),
        };

        v_flex()
            .gap_4()
            .text_size(px(13.))
            .child(
                h_flex()
                    .gap(px(14.))
                    .child(icon_box(
                        match self.kind {
                            WriteKind::Transactional => AppIcon::Pencil,
                            WriteKind::Full => AppIcon::FullWrite,
                        },
                        0xc7c7cc,
                    ))
                    .child(
                        v_flex()
                            .gap(px(3.))
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(self.confirmation.title.clone()),
                            )
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .text_size(px(12.5))
                                    .text_color(color(theme::TEXT_SECONDARY))
                                    .child(
                                        div()
                                            .size(px(6.))
                                            .rounded_full()
                                            .bg(color(self.environment.color())),
                                    )
                                    .child(format!(
                                        "{} · {} · {}",
                                        self.connection,
                                        self.environment.label().to_lowercase(),
                                        self.kind.label()
                                    )),
                            ),
                    ),
            )
            .children(self.confirmation.warning.clone().map(|warning| {
                h_flex()
                    .gap(px(9.))
                    .items_start()
                    .p(px(12.))
                    .rounded(px(9.))
                    .bg(color(theme::AMBER).opacity(0.08))
                    .border_1()
                    .border_color(color(theme::AMBER).opacity(0.3))
                    .text_size(px(12.5))
                    .text_color(color(0xffcf7f))
                    .child(Icon::new(IconName::TriangleAlert).size(px(14.)))
                    .child(div().flex_1().child(warning))
            }))
            .when(!self.confirmation.rows.is_empty(), |col| {
                col.child(
                    div()
                        .id("review-statements")
                        .max_h(px(320.))
                        .overflow_y_scroll()
                        .child(table),
                )
            })
            .children(extra_lines.into_iter().map(|line| {
                div()
                    .text_size(px(12.5))
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(line)
            }))
            .children(match &self.preview {
                PreviewState::Failed(error) => Some(
                    div()
                        .text_size(px(12.5))
                        .text_color(color(theme::RED))
                        .child(format!("The preview failed: {error}")),
                ),
                _ => None,
            })
            .child(
                h_flex()
                    .gap(px(8.))
                    .text_size(px(12.))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .when(previewed, |row| {
                        row.child(Icon::new(IconName::Eye).size(px(13.)))
                            .child("Row counts from a preview that was undone ·")
                    })
                    .child(self.confirmation.note.clone()),
            )
            .child(
                h_flex()
                    .pt_1()
                    .gap_2()
                    .child(
                        Button::new("review-preview")
                            .icon(Icon::new(match self.preview {
                                PreviewState::None => IconName::Eye,
                                _ => IconName::RefreshCw,
                            }))
                            .label(match self.preview {
                                PreviewState::None => "Preview Impact",
                                PreviewState::Running => "Previewing…",
                                _ => "Preview Again",
                            })
                            .disabled(matches!(self.preview, PreviewState::Running))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.start_preview(window, cx)),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("review-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("review-run")
                            .primary()
                            .icon(Icon::new(IconName::Play))
                            .label(self.confirmation.run_label.clone())
                            .disabled(matches!(self.preview, PreviewState::Running))
                            .on_click(cx.listener(|this, _, window, cx| this.run(window, cx))),
                    ),
            )
    }
}

/// When the preview does not line up with the analysed statements (or there
/// are none), its lines, numbered.
fn describe_impact_rows(confirmation: &Confirmation, impacts: &[StatementImpact]) -> Vec<String> {
    impacts
        .iter()
        .enumerate()
        .map(|(index, impact)| {
            let name = confirmation.rows.get(index).map_or_else(
                || format!("Statement {}", index + 1),
                |row| row.summary.clone(),
            );
            let rows = match impact.rows {
                1 => "1 row".to_string(),
                n => format!("{} rows", thousands(n)),
            };
            if impact.returns_rows {
                format!("{name}: {rows} returned")
            } else {
                format!("{name}: {rows} would change")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review_in(environment: Environment, sql: &str) -> Review {
        review(
            Dialect::Postgres,
            sql,
            environment,
            WriteKind::Transactional,
        )
    }

    fn confirmation(review: Review) -> Confirmation {
        match review {
            Review::Confirm(confirmation) => confirmation,
            other => panic!("expected a confirmation, got {other:?}"),
        }
    }

    #[test]
    fn reads_run_without_asking_anywhere() {
        for environment in [
            Environment::Development,
            Environment::Staging,
            Environment::Production,
        ] {
            assert_eq!(
                review_in(environment, "select * from orders"),
                Review::Run { writes: false }
            );
        }
    }

    #[test]
    fn development_only_asks_before_risky_statements() {
        assert_eq!(
            review_in(
                Environment::Development,
                "update orders set total = 0 where id = 1"
            ),
            Review::Run { writes: true }
        );
        let confirmation = confirmation(review_in(
            Environment::Development,
            "delete from orders; select 1",
        ));
        assert_eq!(confirmation.title, "Review 1 change");
        assert_eq!(confirmation.run_label, "Run 1 change");
        assert_eq!(confirmation.rows[0].summary, "DELETE FROM orders");
        assert!(
            matches!(&confirmation.rows[0].check, Check::Risky(why) if why.contains("has no WHERE"))
        );
        assert_eq!(confirmation.rows[1].check, Check::Read);
    }

    #[test]
    fn staging_and_production_ask_before_every_change() {
        for environment in [Environment::Staging, Environment::Production] {
            let confirmation = confirmation(review_in(
                environment,
                "insert into t values (1); insert into t values (2)",
            ));
            assert_eq!(confirmation.title, "Review 2 changes");
            assert!(
                confirmation
                    .rows
                    .iter()
                    .all(|row| row.check == Check::Targeted)
            );
        }
    }

    #[test]
    fn unanalysable_sql_is_confirmed_but_never_runs_on_production() {
        let confirmation = confirmation(review_in(Environment::Staging, "updat t"));
        assert!(confirmation.rows.is_empty());
        assert!(confirmation.warning.is_some());
        assert_eq!(confirmation.run_label, "Run anyway");
        assert!(matches!(
            review_in(Environment::Production, "updat t"),
            Review::Refuse(_)
        ));
    }

    #[test]
    fn the_transaction_belongs_to_kuery() {
        assert!(matches!(
            review_in(Environment::Development, "update t set a = 1 where id = 1; commit"),
            Review::Refuse(message) if message.starts_with("COMMIT is not needed")
        ));
        assert!(matches!(
            review_in(Environment::Development, "begin"),
            Review::Refuse(_)
        ));
    }

    #[test]
    fn full_write_says_changes_apply_right_away() {
        let confirmation = confirmation(review(
            Dialect::Postgres,
            "insert into t values (1)",
            Environment::Staging,
            WriteKind::Full,
        ));
        assert_eq!(confirmation.title, "Apply 1 change now?");
        assert_eq!(confirmation.run_label, "Apply 1 change");
        assert!(confirmation.note.contains("There is no Roll Back"));
        assert!(matches!(
            review(Dialect::Postgres, "begin", Environment::Development, WriteKind::Full),
            Review::Refuse(message) if message.contains("switch to Write")
        ));
    }

    #[test]
    fn describes_what_a_preview_found() {
        let impacts = [
            StatementImpact {
                rows: 1_204,
                returns_rows: false,
            },
            StatementImpact {
                rows: 1,
                returns_rows: true,
            },
        ];
        assert_eq!(
            describe_impact(
                Dialect::Postgres,
                "update orders set a = 1; select 1",
                &impacts
            ),
            [
                "UPDATE orders: 1,204 rows would change",
                "SELECT: 1 row returned"
            ]
        );
        // When the analysis does not line up, statements are numbered.
        assert_eq!(
            describe_impact(Dialect::Postgres, "updat t", &impacts[..1]),
            ["Statement 1: 1,204 rows would change"]
        );
    }
}
