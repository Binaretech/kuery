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
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    Styled as _, Window, div, px,
};
use kuery_core::analysis::{self, StatementKind};
use kuery_core::read_only::Rejection;

use kuery_pg::StatementImpact;

use crate::connection::{Db, Environment, WriteKind, WritePolicy};
use crate::theme::{self, color};
use crate::ui::{muted, thousands};

/// What to do with a script in write mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Review {
    /// Run it; `writes` says whether it goes into the write transaction.
    Run { writes: bool },
    /// Ask first, then run it in the write transaction.
    Confirm { message: String, detail: String },
    /// Do not run it.
    Refuse(String),
}

/// Most statements listed in a confirmation.
const LISTED: usize = 8;

pub fn review(sql: &str, connection: &str, environment: Environment, kind: WriteKind) -> Review {
    let policy = environment.write_policy();
    let analysis = match analysis::analyze(sql) {
        Ok(analysis) => analysis,
        Err(Rejection::Empty) => return Review::Refuse("There is no statement to run".into()),
        Err(error) if policy.allows_unanalysed() => {
            return Review::Confirm {
                message: "Kuery could not analyse this SQL".into(),
                detail: format!(
                    "{error}.\n\n{} without the usual checks. Read it again before \
                     running it.",
                    match kind {
                        WriteKind::Transactional => "It will run in the open transaction",
                        WriteKind::Full => "It will be applied right away",
                    }
                ),
            };
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
    let dangers: Vec<String> = analysis.dangers().map(|d| format!("⚠ {d}")).collect();
    if !policy.confirms(!dangers.is_empty()) {
        return Review::Run { writes: true };
    }

    let writes: Vec<&str> = analysis
        .statements
        .iter()
        .filter(|s| s.kind != StatementKind::Read)
        .map(|s| s.summary.as_str())
        .collect();
    let mut lines: Vec<String> = writes
        .iter()
        .take(LISTED)
        .map(|summary| format!("• {summary}"))
        .collect();
    if writes.len() > LISTED {
        lines.push(format!("… and {} more", writes.len() - LISTED));
    }
    if !dangers.is_empty() {
        lines.push(String::new());
        lines.extend(dangers);
    }
    lines.push(String::new());
    lines.push(match kind {
        WriteKind::Transactional => "Changes stay in a transaction until you commit them.".into(),
        WriteKind::Full => {
            "The script is applied as soon as it finishes, all of it or nothing. There is no \
             Roll Back."
                .into()
        }
    });
    let changes = plural(writes.len(), "change");
    let environment = environment.label();
    Review::Confirm {
        message: match kind {
            WriteKind::Transactional => format!("Run {changes} on {connection} ({environment})?"),
            WriteKind::Full => format!("Apply {changes} to {connection} ({environment}) now?"),
        },
        detail: lines.join("\n"),
    }
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
pub fn describe_impact(sql: &str, impacts: &[StatementImpact]) -> Vec<String> {
    let summaries: Option<Vec<String>> = analysis::analyze(sql)
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
    let title = match kind {
        WriteKind::Transactional => "Allow writes on production",
        WriteKind::Full => "Allow full writes on production",
    };
    let dialog = cx.new(|cx| AllowWrites::new(db, kind, Rc::new(allowed), window, cx));
    let first_build = std::cell::Cell::new(true);
    window.open_dialog(cx, move |modal, window, cx| {
        if first_build.replace(false) {
            let input = dialog.read(cx).input.focus_handle(cx);
            window.focus(&input, cx);
        }
        modal
            .title(title)
            .w(px(500.))
            .overlay_closable(false)
            .child(dialog.clone())
    });
}

/// Runs once writes are allowed.
type OnAllowed = Rc<dyn Fn(&mut Window, &mut App)>;

struct AllowWrites {
    db: Db,
    kind: WriteKind,
    input: Entity<InputState>,
    allowed: OnAllowed,
    error: Option<String>,
}

impl AllowWrites {
    fn new(
        db: Db,
        kind: WriteKind,
        allowed: OnAllowed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(db.name.clone()));
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.confirm(window, cx);
            }
        })
        .detach();
        Self {
            db,
            kind,
            input,
            allowed,
            error: None,
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.read(cx).value().trim() != self.db.name.as_ref() {
            self.error = Some(format!("Type \u{201c}{}\u{201d} exactly", self.db.name));
            cx.notify();
            return;
        }
        self.db.allow(self.kind);
        window.close_dialog(cx);
        (self.allowed)(window, cx);
    }
}

impl Render for AllowWrites {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .text_size(px(13.))
            .child(div().child(match self.kind {
                WriteKind::Transactional => format!(
                    "{} ({}) is a production database. Query tabs on it will be able to \
                     change data until it disconnects or you quit Kuery. Nothing is saved \
                     in the project.",
                    self.db.name, self.db.target
                ),
                WriteKind::Full => format!(
                    "Full write applies each script to {} ({}) as soon as it finishes, \
                     with no Roll Back. This lasts until it disconnects or you quit Kuery, \
                     and is never saved in the project.",
                    self.db.name, self.db.target
                ),
            }))
            .child(muted(match self.kind {
                WriteKind::Transactional => {
                    "Every change is still confirmed and stays in a transaction until you \
                     commit it."
                }
                WriteKind::Full => "Every change is still confirmed before it is applied.",
            }))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_color(color(theme::TEXT_SECONDARY))
                            .child(format!("Type {} to confirm", self.db.name)),
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
                    .pt_2()
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
                            .label(match self.kind {
                                WriteKind::Transactional => "Allow Writes",
                                WriteKind::Full => "Allow Full Writes",
                            })
                            .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review_in(environment: Environment, sql: &str) -> Review {
        review(sql, "db", environment, WriteKind::Transactional)
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
        let Review::Confirm { message, detail } =
            review_in(Environment::Development, "delete from orders")
        else {
            panic!("a DELETE without WHERE should be confirmed");
        };
        assert_eq!(message, "Run 1 change on db (DEVELOPMENT)?");
        assert!(detail.contains("• DELETE FROM orders"), "{detail}");
        assert!(
            detail.contains("⚠ DELETE FROM orders has no WHERE"),
            "{detail}"
        );
    }

    #[test]
    fn staging_and_production_ask_before_every_change() {
        for environment in [Environment::Staging, Environment::Production] {
            assert!(matches!(
                review_in(environment, "insert into t values (1); insert into t values (2)"),
                Review::Confirm { message, .. } if message.starts_with("Run 2 changes on db")
            ));
        }
    }

    #[test]
    fn unanalysable_sql_is_confirmed_but_never_runs_on_production() {
        assert!(matches!(
            review_in(Environment::Staging, "updat t"),
            Review::Confirm { .. }
        ));
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
    fn long_scripts_list_the_first_changes() {
        let sql = (0..12)
            .map(|i| format!("insert into t values ({i});"))
            .collect::<String>();
        let Review::Confirm { detail, .. } = review_in(Environment::Staging, &sql) else {
            panic!("staging confirms writes");
        };
        assert_eq!(detail.matches("• INSERT INTO t").count(), LISTED);
        assert!(detail.contains("… and 4 more"));
    }

    #[test]
    fn full_write_says_changes_apply_right_away() {
        let Review::Confirm { message, detail } = review(
            "insert into t values (1)",
            "db",
            Environment::Staging,
            WriteKind::Full,
        ) else {
            panic!("staging confirms writes");
        };
        assert_eq!(message, "Apply 1 change to db (STAGING) now?");
        assert!(detail.contains("There is no Roll Back"), "{detail}");
        assert!(matches!(
            review("begin", "db", Environment::Development, WriteKind::Full),
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
            describe_impact("update orders set a = 1; select 1", &impacts),
            [
                "UPDATE orders: 1,204 rows would change",
                "SELECT: 1 row returned"
            ]
        );
        // When the analysis does not line up, statements are numbered.
        assert_eq!(
            describe_impact("updat t", &impacts[..1]),
            ["Statement 1: 1,204 rows would change"]
        );
    }
}
