//! A Postgres server's activity in a tab: its sessions, what each runs and
//! for how long, who blocks whom, refreshed every two seconds on a
//! connection of the tab's own (so a long query elsewhere never holds it
//! up). Sessions that need a look stand out: blocked, blocking, running for
//! more than a minute, or idle in a transaction for that long.
//!
//! A session's query can be cancelled, or the session ended. Both are
//! confirmed first; on production, writes must be allowed for the
//! connection session too, as for any write. Both go to the audit log.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, px,
};
use kuery_core::activity::{Activity, Backend, Concern, State, duration};
use kuery_pg::activity::Signal;
use kuery_pg::{Access, Session};

use crate::assets::AppIcon;
use crate::audit::{self, AuditAction, AuditRecord};
use crate::chrome;
use crate::connection::{Db, WriteKind};
use crate::theme::{self, color};
use crate::write_mode;

const EVERY: Duration = Duration::from_secs(2);

pub enum ActivityEvent {
    /// Open a session's query in a query tab.
    OpenQuery(String),
}

enum Monitor {
    Connecting,
    Ready(Session),
    Failed(String),
}

pub struct ActivityTab {
    db: Db,
    project: PathBuf,
    monitor: Monitor,
    activity: Option<Activity>,
    /// The last refresh failed: why.
    error: Option<String>,
    /// Every session, idle ones and background processes included.
    all: bool,
    paused: bool,
    selected: Option<i32>,
    fetching: bool,
    /// What the last cancel or terminate did.
    notice: Option<(String, u32)>,
    _poll: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl EventEmitter<ActivityEvent> for ActivityTab {}

impl ActivityTab {
    pub fn new(db: Db, project: PathBuf, cx: &mut Context<Self>) -> Self {
        let engine = crate::app::state(cx).engine.clone();
        let connecting = db.reconnect.open(&engine, Access::ReadOnly);
        cx.spawn(async move |this, cx| {
            let result = connecting.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(session) => {
                        this.monitor = Monitor::Ready(session);
                        this.refresh(cx);
                        this.start_polling(cx);
                    }
                    Err(error) => this.monitor = Monitor::Failed(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        Self {
            db,
            project,
            monitor: Monitor::Connecting,
            activity: None,
            error: None,
            all: false,
            paused: false,
            selected: None,
            fetching: false,
            notice: None,
            _poll: None,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn title(&self) -> SharedString {
        "Activity".into()
    }

    /// For the status bar.
    pub fn status(&self) -> Option<String> {
        let activity = self.activity.as_ref()?;
        let (active, idle_in_transaction, waiting) = activity.counts();
        let mut parts = vec![match activity.max_connections {
            Some(max) => format!("{} of {max} connections", activity.connections()),
            None => format!("{} connections", activity.connections()),
        }];
        parts.push(format!("{active} active"));
        if idle_in_transaction > 0 {
            parts.push(format!("{idle_in_transaction} idle in transaction"));
        }
        if waiting > 0 {
            parts.push(format!("{waiting} waiting"));
        }
        Some(parts.join(" · "))
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        self._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(EVERY).await;
                let Ok(()) = this.update(cx, |this, cx| {
                    if !this.paused {
                        this.refresh(cx);
                    }
                }) else {
                    break;
                };
            }
        }));
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Monitor::Ready(session) = &self.monitor else {
            return;
        };
        if self.fetching {
            return;
        }
        self.fetching = true;
        let fetch = kuery_pg::activity::activity(session.clone());
        cx.spawn(async move |this, cx| {
            let result = fetch.await;
            this.update(cx, |this, cx| {
                this.fetching = false;
                match result {
                    Ok(activity) => {
                        this.activity = Some(activity);
                        this.error = None;
                    }
                    Err(error) => this.error = Some(crate::query_tab::describe_error(error)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select(&mut self, pid: i32, cx: &mut Context<Self>) {
        self.selected = if self.selected == Some(pid) {
            None
        } else {
            Some(pid)
        };
        cx.notify();
    }

    /// Cancel or terminate: on production allow writes first, then confirm.
    fn ask(&mut self, pid: i32, signal: Signal, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = self.activity.as_ref().and_then(|a| a.backend(pid)).cloned() else {
            return;
        };
        let weak = cx.entity().downgrade();
        if !self.db.allows(WriteKind::Transactional) {
            write_mode::allow_writes(
                self.db.clone(),
                WriteKind::Transactional,
                window,
                cx,
                move |window, cx| {
                    let backend = backend.clone();
                    weak.update(cx, |this, cx| this.confirm(backend, signal, window, cx))
                        .ok();
                },
            );
            return;
        }
        self.confirm(backend, signal, window, cx);
    }

    fn confirm(
        &mut self,
        backend: Backend,
        signal: Signal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak = cx.entity().downgrade();
        let (title, ok, what) = match signal {
            Signal::Cancel => (
                format!("Cancel the query of session {}?", backend.pid),
                "Cancel Query",
                "Stops the query it runs now. The session stays connected; if it is in a \
                 transaction, the transaction fails and has to be rolled back.",
            ),
            Signal::Terminate => (
                format!("End session {}?", backend.pid),
                "End Session",
                "Closes the session: its open transaction rolls back, its locks are \
                 released and its application loses the connection.",
            ),
        };
        let who = format!(
            "{} · {} · {} · {}",
            if backend.user.is_empty() {
                "?"
            } else {
                &backend.user
            },
            if backend.application.is_empty() {
                "no application name"
            } else {
                &backend.application
            },
            backend.state.label(),
            backend.seconds().map(duration).unwrap_or_default()
        );
        let query = backend.query.chars().take(400).collect::<String>();
        let connection = format!("{} ({})", self.db.name, self.db.environment.label());
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let weak = weak.clone();
            let pid = backend.pid;
            alert
                .title(title.clone())
                .description(
                    v_flex()
                        .gap_2()
                        .child(what)
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(color(theme::TEXT_SECONDARY))
                                .child(format!("On {connection}: {who}")),
                        )
                        .when(!query.is_empty(), |body| {
                            body.child(
                                div()
                                    .p_2()
                                    .rounded(px(6.))
                                    .bg(color(theme::CONTROL))
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_size(px(12.))
                                    .child(query.clone()),
                            )
                        }),
                )
                .confirm()
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(ok)
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text("Keep It"),
                )
                .on_ok(move |_, _, cx| {
                    weak.update(cx, |this, cx| this.send(pid, signal, cx)).ok();
                    true
                })
        });
    }

    fn send(&mut self, pid: i32, signal: Signal, cx: &mut Context<Self>) {
        let Monitor::Ready(session) = &self.monitor else {
            return;
        };
        let sent = kuery_pg::activity::signal(session.clone(), pid, signal);
        let record = |outcome: String, failed: bool| AuditRecord {
            at: AuditRecord::now_at(),
            project: self.project.display().to_string(),
            connection: self.db.name.to_string(),
            environment: self.db.environment.label().to_string(),
            mode: "Server activity".into(),
            action: AuditAction::Run,
            sql: signal.sql(pid),
            outcome,
            changed: None,
            failed,
        };
        let pending = record(String::new(), false);
        cx.spawn(async move |this, cx| {
            let result = sent.await;
            let (message, tone, failed) = match (&result, signal) {
                (Ok(true), Signal::Cancel) => (
                    format!("Cancelled the query of session {pid}"),
                    theme::GREEN,
                    false,
                ),
                (Ok(true), Signal::Terminate) => {
                    (format!("Ended session {pid}"), theme::GREEN, false)
                }
                (Ok(false), _) => (
                    format!("Session {pid} was already gone"),
                    theme::TEXT_TERTIARY,
                    false,
                ),
                (Err(_), _) => (String::new(), theme::RED, true),
            };
            let message = match result {
                Err(error) => crate::query_tab::describe_error(error),
                Ok(_) => message,
            };
            let record = AuditRecord {
                outcome: message.clone(),
                failed,
                ..pending
            };
            cx.background_spawn(async move {
                if let Err(error) = audit::append(&record) {
                    eprintln!("kuery: could not write the audit log: {error}");
                }
            })
            .detach();
            this.update(cx, |this, cx| {
                this.notice = Some((message, tone));
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .h(px(44.))
            .flex_shrink_0()
            .px_3()
            .gap_2()
            .bg(color(theme::TOOLBAR))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                Icon::new(AppIcon::Activity)
                    .size(px(14.))
                    .text_color(color(theme::TEXT_TERTIARY)),
            )
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("Activity · {}", self.db.name)),
            )
            .children(self.status().map(|status| {
                div()
                    .text_size(px(12.))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(status)
            }))
            .child(div().flex_1())
            .child(
                chrome::segmented().children(
                    [
                        ("activity-busy", AppIcon::Activity, "Busy", false),
                        ("activity-all", AppIcon::Table, "All sessions", true),
                    ]
                    .map(|(id, icon, label, all)| {
                        chrome::segment(id, icon, label, self.all == all).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.all = all;
                                cx.notify();
                            },
                        ))
                    }),
                ),
            )
            .child(chrome::toolbar_divider())
            .child(
                chrome::icon_button(
                    "activity-pause",
                    if self.paused {
                        IconName::Play
                    } else {
                        IconName::Pause
                    },
                    if self.paused {
                        "Resume refreshing every 2 seconds"
                    } else {
                        "Pause refreshing"
                    },
                    true,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.paused = !this.paused;
                    if !this.paused {
                        this.refresh(cx);
                    }
                    cx.notify();
                })),
            )
            .child(
                chrome::icon_button("activity-refresh", AppIcon::Reload, "Refresh now", true)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
    }

    /// Who holds up whom, when anyone waits.
    fn render_blockers(&self, activity: &Activity, cx: &mut Context<Self>) -> Option<AnyElement> {
        let blockers = activity.blockers();
        if blockers.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .gap_1()
                .bg(color(0x2a2116))
                .border_b_1()
                .border_color(color(theme::HAIRLINE))
                .children(blockers.into_iter().map(|blocker| {
                    let pid = blocker.pid;
                    let holder = activity.backend(pid);
                    let how = holder
                        .map(|b| {
                            format!(
                                " ({}, {})",
                                b.state.label(),
                                b.seconds().map(duration).unwrap_or_default()
                            )
                        })
                        .unwrap_or_default();
                    let waiting = blocker
                        .waiting
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    h_flex()
                        .gap_2()
                        .text_size(px(12.))
                        .child(
                            Icon::new(IconName::TriangleAlert)
                                .size(px(13.))
                                .text_color(color(theme::AMBER)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(color(theme::TEXT))
                                .child(format!(
                                    "Session {pid}{how} blocks {} {}: {waiting}",
                                    blocker.waiting.len(),
                                    if blocker.waiting.len() == 1 {
                                        "session"
                                    } else {
                                        "sessions"
                                    }
                                )),
                        )
                        .child(
                            div()
                                .id(("activity-select-blocker", pid as u64))
                                .px_2()
                                .rounded(px(5.))
                                .text_color(color(theme::TEXT_SECONDARY))
                                .hover(|s| s.bg(color(theme::ACTIVE)))
                                .child("Show")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.selected = Some(pid);
                                    cx.notify();
                                })),
                        )
                        .when(holder.is_some_and(|b| !b.own), |row| {
                            row.child(
                                div()
                                    .id(("activity-end-blocker", pid as u64))
                                    .px_2()
                                    .rounded(px(5.))
                                    .text_color(color(theme::RED))
                                    .hover(|s| s.bg(color(theme::ACTIVE)))
                                    .child("End Session…")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.ask(pid, Signal::Terminate, window, cx)
                                    })),
                            )
                        })
                }))
                .into_any_element(),
        )
    }

    fn render_rows(&self, activity: &Activity, cx: &mut Context<Self>) -> AnyElement {
        let mono = cx.theme().mono_font_family.clone();
        let blocking = activity.blocking_pids();
        let shown = activity.shown(self.all);
        if shown.is_empty() {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(color(theme::TEXT_TERTIARY))
                .child("Nothing running · All sessions shows the idle ones")
                .into_any_element();
        }
        let header = h_flex()
            .h(px(26.))
            .flex_shrink_0()
            .px_3()
            .gap_3()
            .text_size(px(11.))
            .text_color(color(theme::TEXT_QUIET))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(div().w(px(64.)).child("PID"))
            .child(div().w(px(150.)).child("STATE"))
            .child(div().w(px(76.)).child("FOR"))
            .child(div().w(px(170.)).child("USER · APPLICATION"))
            .child(div().w(px(150.)).child("WAITING FOR"))
            .child(div().flex_1().child("QUERY"));
        let rows = shown.into_iter().map(|backend| {
            let pid = backend.pid;
            let selected = self.selected == Some(pid);
            let concern = backend.concern();
            let blocks = blocking.contains(&pid);
            let (state_tone, flag) = match (blocks, concern) {
                (true, _) => (theme::RED, Some("blocking")),
                (_, Some(Concern::Blocked)) => (theme::AMBER, Some("blocked")),
                (_, Some(Concern::LongQuery)) => (theme::AMBER, Some("long")),
                (_, Some(Concern::IdleTransaction)) => (theme::AMBER, Some("holding locks")),
                _ => match backend.state {
                    State::Active => (theme::GREEN, None),
                    State::IdleInTransaction | State::IdleInFailedTransaction => {
                        (theme::AMBER, None)
                    }
                    _ => (theme::TEXT_QUIET, None),
                },
            };
            let waiting = match (&backend.waiting_for, &backend.wait) {
                (Some(lock), _) => lock.clone(),
                (None, Some(wait)) => wait.clone(),
                (None, None) => String::new(),
            };
            let who = match (backend.user.is_empty(), backend.application.is_empty()) {
                (false, false) => format!("{} · {}", backend.user, backend.application),
                (false, true) => backend.user.clone(),
                (true, _) => backend.kind.clone(),
            };
            let query: String = backend
                .query
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(300)
                .collect();
            h_flex()
                .id(("activity-row", pid as u64))
                .h(px(30.))
                .flex_shrink_0()
                .px_3()
                .gap_3()
                .text_size(px(12.))
                .border_b_1()
                .border_color(color(theme::HAIRLINE))
                .when(selected, |row| row.bg(color(theme::ACTIVE_STRONG)))
                .when(!selected, |row| row.hover(|s| s.bg(color(theme::ACTIVE))))
                .child(
                    div()
                        .w(px(64.))
                        .font_family(mono.clone())
                        .text_color(color(theme::TEXT_SECONDARY))
                        .child(format!("{pid}{}", if backend.own { " ·me" } else { "" })),
                )
                .child(
                    h_flex()
                        .w(px(150.))
                        .gap(px(6.))
                        .child(chrome::dot(state_tone))
                        .child(
                            div()
                                .truncate()
                                .text_color(color(theme::TEXT))
                                .child(backend.state.label().to_string()),
                        )
                        .children(flag.map(|flag| {
                            div()
                                .flex_shrink_0()
                                .text_size(px(10.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(color(state_tone))
                                .child(flag)
                        })),
                )
                .child(
                    div()
                        .w(px(76.))
                        .text_color(color(if concern.is_some() {
                            theme::AMBER
                        } else {
                            theme::TEXT_SECONDARY
                        }))
                        .child(backend.seconds().map(duration).unwrap_or_default()),
                )
                .child(
                    div()
                        .w(px(170.))
                        .truncate()
                        .text_color(color(theme::TEXT_SECONDARY))
                        .child(who),
                )
                .child(
                    div()
                        .w(px(150.))
                        .truncate()
                        .text_color(color(if backend.blocked_by.is_empty() {
                            theme::TEXT_TERTIARY
                        } else {
                            theme::AMBER
                        }))
                        .child(waiting),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(mono.clone())
                        .text_color(color(theme::TEXT_SECONDARY))
                        .child(query),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.select(pid, cx)))
        });
        v_flex()
            .flex_1()
            .min_h_0()
            .child(header)
            .child(
                v_flex()
                    .id("activity-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element()
    }

    /// The selected session in full, and what can be done to it.
    fn render_details(&self, activity: &Activity, cx: &mut Context<Self>) -> Option<AnyElement> {
        let backend = activity.backend(self.selected?)?.clone();
        let pid = backend.pid;
        let mono = cx.theme().mono_font_family.clone();
        let waiting_on_it = activity.waiting_on(pid);
        let mut facts = vec![
            format!("Session {pid}"),
            backend.kind.clone(),
            if backend.database.is_empty() {
                String::new()
            } else {
                format!("database {}", backend.database)
            },
            if backend.client.is_empty() {
                "local".into()
            } else {
                format!("from {}", backend.client)
            },
        ];
        if let Some(seconds) = backend.transaction_seconds {
            facts.push(format!("transaction open {}", duration(seconds)));
        }
        if !backend.blocked_by.is_empty() {
            facts.push(format!(
                "waits for {}",
                backend
                    .blocked_by
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !waiting_on_it.is_empty() {
            facts.push(format!("holds up {}", waiting_on_it.len()));
        }
        facts.retain(|fact| !fact.is_empty());
        let query = backend.query.clone();
        let action = |id: &'static str, label: &'static str, tone: u32| {
            div()
                .id(id)
                .h(px(26.))
                .px_2()
                .flex()
                .items_center()
                .rounded(px(6.))
                .text_size(px(12.))
                .text_color(color(tone))
                .border_1()
                .border_color(color(theme::CONTROL_BORDER))
                .hover(|s| s.bg(color(theme::ACTIVE)))
                .child(label)
        };
        Some(
            v_flex()
                .flex_shrink_0()
                .max_h(px(220.))
                .p_3()
                .gap_2()
                .border_t_1()
                .border_color(color(theme::HAIRLINE))
                .bg(color(theme::TOOLBAR))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(color(theme::TEXT_SECONDARY))
                                .child(facts.join(" · ")),
                        )
                        .when(!query.is_empty(), |bar| {
                            let copied = query.clone();
                            let opened = query.clone();
                            bar.child(
                                action("activity-copy", "Copy Query", theme::TEXT_SECONDARY)
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copied.clone(),
                                        ))
                                    }),
                            )
                            .child(
                                action("activity-open", "Open in Query Tab", theme::TEXT_SECONDARY)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(ActivityEvent::OpenQuery(opened.clone()))
                                    })),
                            )
                        })
                        .when(!backend.own && backend.is_client(), |bar| {
                            bar.when(backend.state == State::Active, |bar| {
                                bar.child(
                                    action("activity-cancel", "Cancel Query…", theme::AMBER)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.ask(pid, Signal::Cancel, window, cx)
                                        })),
                                )
                            })
                            .child(
                                action("activity-terminate", "End Session…", theme::RED).on_click(
                                    cx.listener(move |this, _, window, cx| {
                                        this.ask(pid, Signal::Terminate, window, cx)
                                    }),
                                ),
                            )
                        }),
                )
                .child(
                    div()
                        .id("activity-query")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .font_family(mono)
                        .text_size(px(12.))
                        .text_color(color(theme::TEXT))
                        .child(if query.is_empty() {
                            "No query".to_string()
                        } else {
                            query
                        }),
                )
                .into_any_element(),
        )
    }
}

impl Focusable for ActivityTab {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ActivityTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let message = |text: String, tone: u32| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_6()
                .text_color(color(tone))
                .child(text)
                .into_any_element()
        };
        let mut body: Vec<AnyElement> = Vec::new();
        match (&self.monitor, self.activity.clone()) {
            (Monitor::Failed(error), _) => body.push(message(
                format!("Could not open a connection to watch the server: {error}"),
                theme::RED,
            )),
            (_, None) => body.push(message(
                "Reading the server's activity…".into(),
                theme::TEXT_TERTIARY,
            )),
            (_, Some(activity)) => {
                body.extend(self.render_blockers(&activity, cx));
                body.push(self.render_rows(&activity, cx));
                body.extend(self.render_details(&activity, cx));
            }
        }
        let footer = match (&self.error, &self.notice) {
            (Some(error), _) => Some((format!("Refresh failed: {error}"), theme::RED)),
            (None, Some((notice, tone))) => Some((notice.clone(), *tone)),
            (None, None) => None,
        };
        v_flex()
            .size_full()
            .bg(color(theme::GROUND))
            .track_focus(&self.focus_handle)
            .child(self.render_toolbar(cx))
            .children(body)
            .children(footer.map(|(text, tone)| {
                div()
                    .h(px(28.))
                    .flex_shrink_0()
                    .px_3()
                    .flex()
                    .items_center()
                    .border_t_1()
                    .border_color(color(theme::HAIRLINE))
                    .text_size(px(12.))
                    .text_color(color(tone))
                    .child(text)
            }))
    }
}
