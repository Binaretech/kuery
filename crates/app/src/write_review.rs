//! The review before writes run, whichever the family. A family describes
//! its steps ([`Planned`]), whether writes are allowed on the connection
//! session and how to allow them, and how to preview what each step would
//! touch ([`Detail`]). [`approve`] then makes sure the environment's policy
//! is met: writes allowed for the connection session on production (typing
//! its name), then a review of the steps whenever the policy asks for one
//! (risky steps on development, every write on staging and production).

use std::rc::Rc;

use futures::future::LocalBoxFuture;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, FontWeight, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Window, div, px,
};

use crate::assets::AppIcon;
use crate::connection::{Environment, WritePolicy};
use crate::theme::{self, color};
use crate::write_mode::icon_box;

/// A statement or command about to run, whichever the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    /// As one line, for the audit log.
    pub line: String,
    /// What the review and error messages show: the line, or a short
    /// description of a long one.
    pub shown: String,
    pub writes: bool,
    /// Why it is risky, if it is.
    pub danger: Option<String>,
}

/// One line under a planned step in the review: a key and what it holds
/// now, a collection and how many documents the write would touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    pub subject: String,
    pub text: String,
    /// Why what was found makes the step risky, if it does.
    pub danger: Option<String>,
}

/// The details of each planned step, in order, or why they could not be
/// read.
pub type Preview = LocalBoxFuture<'static, Result<Vec<Vec<Detail>>, String>>;

/// Starts reading the details of the planned steps.
pub type FetchPreview = Rc<dyn Fn(&[Planned]) -> Preview>;

/// Runs once the steps may run.
pub type Approved = Rc<dyn Fn(&mut Window, &mut App)>;

/// Asks to allow writes, then calls back.
pub type Unlock = Rc<dyn Fn(&mut Window, &mut App, Approved)>;

/// What a family's review asks for: who to ask, how to unlock, how to
/// preview.
#[derive(Clone)]
pub struct Request {
    pub connection: SharedString,
    pub environment: Environment,
    /// Whether writes are allowed for the connection session now.
    pub allows: Rc<dyn Fn() -> bool>,
    /// Asks to allow them (typing the connection name), then calls back.
    pub unlock: Unlock,
    /// The review's subtitle: `In db0, in this order.` and the engine's
    /// note (`Redis has no rollback: each command applies as soon as it
    /// runs.`).
    pub heading: String,
    pub planned: Rc<Vec<Planned>>,
    /// Details for each planned step, fetched off the UI thread.
    pub preview: FetchPreview,
    /// Whether the user stopped the preview: once it ends, nothing more
    /// happens, neither the review nor the run.
    pub stopped: Rc<dyn Fn() -> bool>,
}

/// Calls `approved` once the steps may run, asking first what the
/// environment's policy asks for. Nothing happens if the user cancels.
pub fn approve(
    request: Request,
    window: &mut Window,
    cx: &mut App,
    approved: impl Fn(&mut Window, &mut App) + 'static,
) {
    approve_with(request, window, cx, Rc::new(approved));
}

fn approve_with(request: Request, window: &mut Window, cx: &mut App, approved: Approved) {
    let writes = request.planned.iter().any(|step| step.writes);
    let policy = request.environment.write_policy();
    match next(writes, (request.allows)(), policy, None) {
        Next::Run => approved(window, cx),
        Next::Unlock => {
            let unlock = request.unlock.clone();
            unlock(
                window,
                cx,
                Rc::new(move |window, cx| {
                    approve_with(request.clone(), window, cx, approved.clone())
                }),
            );
        }
        Next::Preview | Next::Review => preview(request, window, cx, approved),
    }
}

/// Reads what the planned steps would touch, then runs them or opens the
/// review, as the policy asks.
fn preview(request: Request, window: &mut Window, cx: &mut App, approved: Approved) {
    let fetch = (request.preview)(&request.planned);
    window
        .spawn(cx, async move |cx| {
            let details = fetch.await;
            if (request.stopped)() {
                return;
            }
            cx.update(|window, cx| {
                let rows = rows(&request.planned, details.as_deref().ok());
                let dangerous = rows.iter().any(|row| row.danger.is_some());
                let writes = rows.iter().any(|row| row.writes);
                let policy = request.environment.write_policy();
                match next(writes, (request.allows)(), policy, Some(dangerous)) {
                    Next::Run => approved(window, cx),
                    Next::Review => {
                        let review = cx.new(|_| Review {
                            connection: request.connection.clone(),
                            heading: request.heading.clone(),
                            rows,
                            preview_error: details.err(),
                            approved: approved.clone(),
                        });
                        window.open_dialog(cx, move |modal, _, _| {
                            modal
                                .w(px(680.))
                                .overlay_closable(false)
                                .child(review.clone())
                        });
                    }
                    // Writes were locked while the preview was read: start
                    // over, so they are allowed again first.
                    Next::Unlock | Next::Preview => approve_with(request, window, cx, approved),
                }
            })
            .ok();
        })
        .detach();
}

/// What the review does next with a family's planned steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// Nothing writes, or the policy does not ask for a review: run them.
    Run,
    /// Writes are not allowed for the connection session yet: allow them
    /// first, then decide again.
    Unlock,
    /// Read what the steps would touch, then decide again.
    Preview,
    /// Show the review and run them only if the user says so.
    Review,
}

/// The gate every family's writes pass through. `dangerous` is `None` until
/// the preview is read, and then whether any step is risky.
fn next(writes: bool, allows: bool, policy: WritePolicy, dangerous: Option<bool>) -> Next {
    if !writes {
        return Next::Run;
    }
    if !allows {
        return Next::Unlock;
    }
    match dangerous {
        None => Next::Preview,
        Some(dangerous) if policy.confirms(dangerous) => Next::Review,
        Some(_) => Next::Run,
    }
}

/// One step in the review.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    line: String,
    writes: bool,
    danger: Option<String>,
    details: Vec<Detail>,
}

/// The rows of the review: a step's danger is its own, or the first its
/// details found. Without details, the steps show alone.
fn rows(planned: &[Planned], details: Option<&[Vec<Detail>]>) -> Vec<Row> {
    planned
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let details = details
                .and_then(|details| details.get(index))
                .cloned()
                .unwrap_or_default();
            Row {
                line: step.shown.clone(),
                writes: step.writes,
                danger: step
                    .danger
                    .clone()
                    .or_else(|| details.iter().find_map(|detail| detail.danger.clone())),
                details,
            }
        })
        .collect()
}

/// `Run 2 writes on prod?` and the button's `Run 2 writes`.
fn title(writes: usize, connection: &str) -> (String, String) {
    let label = if writes == 1 {
        "1 write".to_string()
    } else {
        format!("{writes} writes")
    };
    (
        format!("Run {label} on {connection}?"),
        format!("Run {label}"),
    )
}

struct Review {
    connection: SharedString,
    heading: String,
    rows: Vec<Row>,
    preview_error: Option<String>,
    approved: Approved,
}

impl Render for Review {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let writes = self.rows.iter().filter(|row| row.writes).count();
        let risky = self.rows.iter().any(|row| row.danger.is_some());
        let (title, button) = title(writes, &self.connection);
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
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_color(color(0xc7c7cc))
                                    .child(self.heading.clone()),
                            ),
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
                            .children(row.details.iter().map(|detail| {
                                h_flex()
                                    .pl(px(22.))
                                    .gap(px(8.))
                                    .text_size(px(12.))
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .font_family(mono.clone())
                                            .text_color(color(theme::TEXT_SECONDARY))
                                            .child(detail.subject.clone()),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(color(theme::TEXT_TERTIARY))
                                            .child(detail.text.clone()),
                                    )
                            }))
                    })),
            )
            .children(self.preview_error.clone().map(|error| {
                div()
                    .text_size(px(12.))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(error)
            }))
            .child(
                h_flex()
                    .pt_1()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        Button::new("cancel-writes")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("run-writes")
                            .map(|button| {
                                if risky {
                                    button.danger()
                                } else {
                                    button.primary()
                                }
                            })
                            .label(button)
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

    fn step(line: &str, writes: bool, danger: Option<&str>) -> Planned {
        Planned {
            line: line.into(),
            shown: line.into(),
            writes,
            danger: danger.map(Into::into),
        }
    }

    fn detail(subject: &str, danger: Option<&str>) -> Detail {
        Detail {
            subject: subject.into(),
            text: "new key".into(),
            danger: danger.map(Into::into),
        }
    }

    #[test]
    fn writes_are_allowed_first_then_reviewed_as_the_policy_asks() {
        use Next::*;
        use WritePolicy::*;
        // (writes, allows, policy, dangerous) -> next
        let table = [
            // Reads run straight away, even on a locked production session.
            (false, false, SessionUnlock, None, Run),
            (false, true, ConfirmDangerous, Some(true), Run),
            // A production session that does not allow writes unlocks first,
            // whatever the preview would find.
            (true, false, SessionUnlock, None, Unlock),
            (true, false, SessionUnlock, Some(false), Unlock),
            (true, false, SessionUnlock, Some(true), Unlock),
            // Once allowed, the steps are previewed before anything runs.
            (true, true, SessionUnlock, None, Preview),
            (true, true, ConfirmWrites, None, Preview),
            (true, true, ConfirmDangerous, None, Preview),
            // Production and staging review every write.
            (true, true, SessionUnlock, Some(false), Review),
            (true, true, ConfirmWrites, Some(false), Review),
            (true, true, ConfirmWrites, Some(true), Review),
            // Development reviews only risky ones.
            (true, true, ConfirmDangerous, Some(false), Run),
            (true, true, ConfirmDangerous, Some(true), Review),
        ];
        for (writes, allows, policy, dangerous, expected) in table {
            assert_eq!(
                next(writes, allows, policy, dangerous),
                expected,
                "writes {writes}, allows {allows}, {policy:?}, dangerous {dangerous:?}"
            );
        }
    }

    #[test]
    fn titles_count_writes() {
        assert_eq!(
            title(1, "prod"),
            ("Run 1 write on prod?".into(), "Run 1 write".into())
        );
        assert_eq!(
            title(3, "prod"),
            ("Run 3 writes on prod?".into(), "Run 3 writes".into())
        );
    }

    #[test]
    fn a_step_is_risky_by_itself_or_by_what_its_details_found() {
        let planned = [
            step("FLUSHDB", true, Some("deletes every key")),
            step("RENAME a b", true, None),
            step("GET a", false, None),
        ];
        let details = vec![
            Vec::new(),
            vec![
                detail("a", None),
                detail("b", Some("replaces the existing key b")),
            ],
        ];
        let reviewed = rows(&planned, Some(&details));
        assert_eq!(reviewed[0].danger.as_deref(), Some("deletes every key"));
        assert_eq!(
            reviewed[1].danger.as_deref(),
            Some("replaces the existing key b")
        );
        assert_eq!(reviewed[1].details.len(), 2);
        // A step past the details shows alone.
        assert!(reviewed[2].danger.is_none() && reviewed[2].details.is_empty());
        // Without details, only a step's own danger counts.
        let alone = rows(&planned, None);
        assert!(alone[1].danger.is_none() && alone[1].details.is_empty());
        assert!(alone[0].danger.is_some());
    }
}
