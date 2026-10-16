//! Writes on Redis, from the console or a key tab. Redis has no
//! transactions: each command applies as soon as it runs, and nothing can
//! be rolled back. So before any runs, [`approve`] makes sure the
//! environment's policy is met: writes allowed for the connection session
//! on production (typing its name), then a review of the commands, with what
//! their keys hold now, whenever the policy asks for one (risky commands on
//! development, every write on staging and production). Every write that
//! runs goes to the audit log ([`audit`]).

use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, FontWeight, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Window, div, px,
};
use kuery_core::redis::{Class, classify, danger, join_command, overwritten_key, written_keys};
use kuery_redis::{KeyKind, KeyPreview};

use crate::assets::AppIcon;
use crate::audit::{self, AuditAction, AuditRecord};
use crate::connection::RedisDb;
use crate::redis_key_tab::ttl_text;
use crate::theme::{self, color};
use crate::ui::{format_bytes, thousands};
use crate::write_mode::{self, icon_box};

/// Keys previewed in a review at most.
const MAX_PREVIEWS: usize = 20;

/// A command about to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub args: Vec<String>,
    /// The command as one line, for the review and the audit log.
    pub line: String,
    pub writes: bool,
    /// Why it is risky, if it is.
    pub danger: Option<String>,
}

impl Planned {
    pub fn new(args: Vec<String>) -> Self {
        Self {
            line: join_command(&args),
            writes: classify(&args) == Class::Write,
            danger: danger(&args),
            args,
        }
    }
}

type Approved = Rc<dyn Fn(&mut Window, &mut App)>;

/// Calls `approved` once `commands` may run on `db`, asking first what the
/// environment's policy asks for. Nothing happens if the user cancels.
pub fn approve(
    db: RedisDb,
    database: i64,
    commands: Vec<Planned>,
    window: &mut Window,
    cx: &mut App,
    approved: impl Fn(&mut Window, &mut App) + 'static,
) {
    approve_with(
        db,
        database,
        Rc::new(commands),
        window,
        cx,
        Rc::new(approved),
    );
}

fn approve_with(
    db: RedisDb,
    database: i64,
    commands: Rc<Vec<Planned>>,
    window: &mut Window,
    cx: &mut App,
    approved: Approved,
) {
    if !commands.iter().any(|command| command.writes) {
        approved(window, cx);
        return;
    }
    if !db.allows_writes() {
        let again = db.clone();
        write_mode::allow_redis_writes(db, window, cx, move |window, cx| {
            approve_with(
                again.clone(),
                database,
                commands.clone(),
                window,
                cx,
                approved.clone(),
            )
        });
        return;
    }
    let mut seen = HashSet::new();
    let keys: Vec<String> = commands
        .iter()
        .filter(|command| command.writes)
        .flat_map(|command| written_keys(&command.args))
        .filter(|key| seen.insert(key.clone()))
        .take(MAX_PREVIEWS)
        .collect();
    let fetch = db.session.preview_keys(database, keys);
    window
        .spawn(cx, async move |cx| {
            let previews = fetch.await.map_err(|error| error.to_string());
            cx.update(|window, cx| {
                let found = previews.as_deref().unwrap_or_default();
                let exists = |key: &str| found.iter().any(|p| p.key == key && p.exists());
                let rows: Vec<Row> = commands
                    .iter()
                    .map(|command| {
                        let overwrite = overwritten_key(&command.args)
                            .filter(|key| exists(key))
                            .map(|key| format!("replaces the existing key {key}"));
                        Row {
                            line: command.line.clone(),
                            writes: command.writes,
                            danger: command.danger.clone().or(overwrite),
                            previews: written_keys(&command.args)
                                .iter()
                                .filter_map(|key| found.iter().find(|p| &p.key == key).cloned())
                                .collect(),
                        }
                    })
                    .collect();
                let dangerous = rows.iter().any(|row| row.danger.is_some());
                if !db.environment.write_policy().confirms(dangerous) {
                    approved(window, cx);
                    return;
                }
                let review = cx.new(|_| Review {
                    connection: db.name.clone(),
                    database,
                    rows,
                    preview_error: previews.err(),
                    approved: approved.clone(),
                });
                window.open_dialog(cx, move |modal, _, _| {
                    modal
                        .w(px(680.))
                        .overlay_closable(false)
                        .child(review.clone())
                });
            })
            .ok();
        })
        .detach();
}

/// Logs a write that ran (or failed) to the audit log, off the UI thread.
pub fn audit(
    db: &RedisDb,
    project: &Path,
    database: i64,
    line: &str,
    outcome: String,
    failed: bool,
    cx: &App,
) {
    let record = AuditRecord {
        at: AuditRecord::now_at(),
        project: project.display().to_string(),
        connection: format!("{} (db{database})", db.name),
        environment: db.environment.label().to_string(),
        mode: "Redis write".into(),
        action: AuditAction::Run,
        sql: line.to_string(),
        outcome,
        changed: None,
        failed,
    };
    cx.background_spawn(async move {
        if let Err(error) = audit::append(&record) {
            eprintln!("kuery: could not write the audit log: {error}");
        }
    })
    .detach();
}

/// One command in the review.
struct Row {
    line: String,
    writes: bool,
    danger: Option<String>,
    previews: Vec<KeyPreview>,
}

struct Review {
    connection: SharedString,
    database: i64,
    rows: Vec<Row>,
    preview_error: Option<String>,
    approved: Approved,
}

/// A key as it is now, on one line: `hash · 4 fields · no expiry · a=1, …`.
fn describe(preview: &KeyPreview) -> String {
    if !preview.exists() {
        return "new key".into();
    }
    let size = preview.length.map(|length| match preview.kind {
        KeyKind::String => format_bytes(length),
        KeyKind::Hash => format!("{} fields", thousands(length)),
        KeyKind::List => format!("{} items", thousands(length)),
        KeyKind::Set | KeyKind::ZSet => format!("{} members", thousands(length)),
        KeyKind::Stream => format!("{} entries", thousands(length)),
        KeyKind::Missing | KeyKind::Other => thousands(length),
    });
    let mut parts = vec![preview.kind.label().to_string()];
    parts.extend(size);
    parts.push(ttl_text(preview.ttl_ms));
    if !preview.sample.is_empty() {
        parts.push(preview.sample.clone());
    }
    parts.join(" · ")
}

impl Render for Review {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let writes = self.rows.iter().filter(|row| row.writes).count();
        let risky = self.rows.iter().any(|row| row.danger.is_some());
        let label = if writes == 1 {
            "1 write".to_string()
        } else {
            format!("{writes} writes")
        };
        v_flex()
            .gap_4()
            .text_size(px(13.))
            .child(
                h_flex()
                    .gap(px(14.))
                    .items_start()
                    .child(icon_box(
                        if risky {
                            AppIcon::FullWrite
                        } else {
                            AppIcon::Pencil
                        },
                        if risky { theme::RED } else { theme::AMBER },
                    ))
                    .child(
                        v_flex()
                            .flex_1()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("Run {label} on {}?", self.connection)),
                            )
                            .child(div().text_color(color(0xc7c7cc)).child(format!(
                                "In db{}, in this order. Redis has no rollback: each command \
                                 applies as soon as it runs.",
                                self.database
                            ))),
                    ),
            )
            .child(
                v_flex()
                    .gap(px(2.))
                    .max_h(px(360.))
                    .overflow_hidden()
                    .rounded(px(9.))
                    .border_1()
                    .border_color(color(theme::HAIRLINE))
                    .bg(color(0x18181a))
                    .children(self.rows.iter().map(|row| {
                        let (icon, tone, note) = match (&row.danger, row.writes) {
                            (Some(reason), _) => {
                                (IconName::TriangleAlert, theme::RED, Some(reason.clone()))
                            }
                            (None, true) => (IconName::Info, theme::AMBER, None),
                            (None, false) => (IconName::Search, theme::TEXT_TERTIARY, None),
                        };
                        v_flex()
                            .px(px(12.))
                            .py(px(8.))
                            .gap(px(4.))
                            .child(
                                h_flex()
                                    .gap(px(9.))
                                    .child(Icon::new(icon).size(px(13.)).text_color(color(tone)))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(mono.clone())
                                            .text_size(px(12.5))
                                            .child(row.line.clone()),
                                    )
                                    .when(!row.writes, |line| {
                                        line.child(
                                            div()
                                                .text_size(px(11.5))
                                                .text_color(color(theme::TEXT_QUIET))
                                                .child("reads"),
                                        )
                                    }),
                            )
                            .children(note.map(|note| {
                                div()
                                    .pl(px(22.))
                                    .text_size(px(12.))
                                    .text_color(color(theme::RED))
                                    .child(note)
                            }))
                            .children(row.previews.iter().map(|preview| {
                                h_flex()
                                    .pl(px(22.))
                                    .gap(px(8.))
                                    .text_size(px(12.))
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .font_family(mono.clone())
                                            .text_color(color(theme::TEXT_SECONDARY))
                                            .child(preview.key.clone()),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(color(theme::TEXT_TERTIARY))
                                            .child(describe(preview)),
                                    )
                            }))
                    })),
            )
            .children(self.preview_error.clone().map(|error| {
                div()
                    .text_size(px(12.))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(format!("Could not read the keys as they are now: {error}"))
            }))
            .child(
                h_flex()
                    .pt_1()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        Button::new("cancel-redis-writes")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("run-redis-writes")
                            .map(|button| {
                                if risky {
                                    button.danger()
                                } else {
                                    button.primary()
                                }
                            })
                            .label(format!("Run {label}"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                window.close_dialog(cx);
                                (this.approved)(window, cx);
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(line: &str) -> Planned {
        Planned::new(kuery_core::redis::split_command(line).unwrap())
    }

    #[test]
    fn plans_commands() {
        let set = planned("SET greeting \"hello world\"");
        assert!(set.writes && set.danger.is_none());
        assert_eq!(set.line, "SET greeting \"hello world\"");
        assert!(!planned("GET greeting").writes);
        assert!(planned("FLUSHDB").danger.is_some());
    }

    #[test]
    fn describes_keys_before_they_change() {
        let preview = KeyPreview {
            key: "user:1".into(),
            kind: KeyKind::Hash,
            ttl_ms: None,
            length: Some(4),
            sample: "name=User 1, …".into(),
        };
        assert_eq!(
            describe(&preview),
            "hash · 4 fields · no expiry · name=User 1, …"
        );
        let missing = KeyPreview {
            kind: KeyKind::Missing,
            length: None,
            sample: String::new(),
            ..preview
        };
        assert_eq!(describe(&missing), "new key");
    }
}
