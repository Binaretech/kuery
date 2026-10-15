//! Connection editor, shown in a dialog: add, edit, test and delete one of the
//! project's connections. The password goes to the system keychain; the rest
//! to `.dbadmin/project.toml`.

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{Disableable as _, IndexPath, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Window, div, px,
};

use crate::app;
use crate::project::{ConnectionSpec, ProjectModel, new_id};
use crate::secrets::SecretStore;
use crate::theme::{self, color};
use crate::ui::muted;

type TextSelect = SelectState<SearchableVec<SharedString>>;

const SSL_MODES: [&str; 6] = [
    "default",
    "disable",
    "prefer",
    "require",
    "verify-ca",
    "verify-full",
];
const ENVIRONMENTS: [&str; 4] = ["detect from host", "development", "staging", "production"];

enum TestState {
    Idle,
    Testing,
    Passed(String),
    Failed(String),
}

pub struct ConnectionEditor {
    model: Entity<ProjectModel>,
    /// `None` for a new connection.
    id: Option<String>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    has_saved_password: bool,
    sslmode: Entity<TextSelect>,
    environment: Entity<TextSelect>,
    test: TestState,
    error: Option<String>,
    confirm_delete: bool,
}

impl ConnectionEditor {
    pub fn new(
        model: Entity<ProjectModel>,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let spec = id
            .as_deref()
            .and_then(|id| model.read(cx).project.connection(id).cloned());
        let text = |value: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let (value, placeholder) = (value.to_string(), placeholder.to_string());
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let name = text(
            spec.as_ref().map_or("", |s| &s.name),
            "local-dev",
            window,
            cx,
        );
        let host = text(
            spec.as_ref().map_or("localhost", |s| &s.host),
            "localhost",
            window,
            cx,
        );
        let port = text(
            &spec.as_ref().map_or(5432, |s| s.port).to_string(),
            "5432",
            window,
            cx,
        );
        let database = text(
            spec.as_ref().map_or("", |s| &s.database),
            "postgres",
            window,
            cx,
        );
        let user = text(
            spec.as_ref().map_or("", |s| &s.user),
            "postgres",
            window,
            cx,
        );
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));

        let select =
            |items: &[&str], selected: usize, window: &mut Window, cx: &mut Context<Self>| {
                let items: Vec<SharedString> =
                    items.iter().map(|i| SharedString::from(*i)).collect();
                cx.new(|cx| {
                    SelectState::new(
                        SearchableVec::new(items),
                        Some(IndexPath::new(selected)),
                        window,
                        cx,
                    )
                })
            };
        let ssl_index = spec
            .as_ref()
            .and_then(|s| s.sslmode.as_deref())
            .and_then(|mode| SSL_MODES.iter().position(|m| *m == mode))
            .unwrap_or(0);
        let environment_index = spec
            .as_ref()
            .and_then(|s| s.environment.as_deref())
            .and_then(|e| ENVIRONMENTS.iter().position(|m| *m == e))
            .unwrap_or(0);
        let sslmode = select(&SSL_MODES, ssl_index, window, cx);
        let environment = select(&ENVIRONMENTS, environment_index, window, cx);

        let mut editor = Self {
            model,
            id,
            name,
            host,
            port,
            database,
            user,
            password,
            has_saved_password: false,
            sslmode,
            environment,
            test: TestState::Idle,
            error: None,
            confirm_delete: false,
        };
        editor.load_password_state(cx);
        editor
    }

    /// Opens the editor in a dialog over `window`: a new connection when `id`
    /// is `None`. The dialog closes itself once the connection is saved or
    /// deleted; the sidebar picks the change up from the project model.
    pub fn open(
        model: Entity<ProjectModel>,
        id: Option<String>,
        window: &mut Window,
        cx: &mut gpui_kit::App,
    ) {
        let title = if id.is_some() {
            "Edit Connection"
        } else {
            "New Connection"
        };
        let editor = cx.new(|cx| Self::new(model, id, window, cx));
        let first_build = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                let name = editor.read(cx).name.focus_handle(cx);
                window.focus(&name, cx);
            }
            dialog
                .title(title)
                .w(px(560.))
                .overlay_closable(false)
                .child(editor.clone())
        });
    }

    /// Whether the keychain already has a password, read off the UI thread
    /// (macOS may ask the user for permission).
    fn load_password_state(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.id.clone() else { return };
        let secrets: Arc<dyn SecretStore> = app::state(cx).secrets.clone();
        let read = cx.background_spawn(async move { secrets.get(&id) });
        cx.spawn(async move |this, cx| {
            let saved = matches!(read.await, Ok(Some(_)));
            this.update(cx, |this, cx| {
                this.has_saved_password = saved;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn selected(
        select: &Entity<TextSelect>,
        options: &[&str],
        cx: &gpui_kit::App,
    ) -> Option<String> {
        let index = select
            .read(cx)
            .selected_index(cx)
            .map(|i| i.row)
            .unwrap_or(0);
        (index > 0).then(|| options[index.min(options.len() - 1)].to_string())
    }

    /// The form as a connection, or why it is not valid.
    fn spec(&self, cx: &gpui_kit::App) -> Result<ConnectionSpec, String> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let name = value(&self.name);
        let host = value(&self.host);
        let database = value(&self.database);
        let user = value(&self.user);
        if name.is_empty() {
            return Err("Give the connection a name".into());
        }
        if host.is_empty() || database.is_empty() || user.is_empty() {
            return Err("Host, database and user are required".into());
        }
        let port = value(&self.port)
            .parse::<u16>()
            .map_err(|_| "The port must be a number between 1 and 65535".to_string())?;
        Ok(ConnectionSpec {
            id: self.id.clone().unwrap_or_else(new_id),
            name,
            host,
            port,
            database,
            user,
            sslmode: Self::selected(&self.sslmode, &SSL_MODES, cx),
            environment: Self::selected(&self.environment, &ENVIRONMENTS, cx),
        })
    }

    /// The password typed in the form, or the saved one if the field is empty.
    fn password(&self, cx: &gpui_kit::App) -> Option<String> {
        let typed = self.password.read(cx).value().to_string();
        (!typed.is_empty()).then_some(typed)
    }

    fn test_connection(&mut self, cx: &mut Context<Self>) {
        let spec = match self.spec(cx) {
            Ok(spec) => spec,
            Err(error) => {
                self.test = TestState::Failed(error);
                cx.notify();
                return;
            }
        };
        let typed = self.password(cx);
        let state = app::state(cx);
        let (engine, secrets) = (state.engine.clone(), state.secrets.clone());
        let saved_id = self.id.clone();
        self.error = None;
        self.test = TestState::Testing;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let password = match typed {
                Some(password) => Some(password),
                None => match saved_id {
                    Some(id) => cx
                        .background_spawn(async move { secrets.get(&id) })
                        .await
                        .ok()
                        .flatten(),
                    None => None,
                },
            };
            let result = engine
                .connect_with(
                    spec.conninfo(password.as_deref()),
                    dbadmin_pg::Access::ReadOnly,
                )
                .await;
            this.update(cx, |this, cx| {
                this.test = match result {
                    Ok(session) => TestState::Passed(format!(
                        "Connected · PostgreSQL {}",
                        session.server_version()
                    )),
                    Err(error) => TestState::Failed(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let spec = match self.spec(cx) {
            Ok(spec) => spec,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        if let Some(password) = self.password(cx) {
            let secrets = app::state(cx).secrets.clone();
            if let Err(error) = secrets.set(&spec.id, &password) {
                self.error = Some(error);
                cx.notify();
                return;
            }
            self.has_saved_password = true;
        }
        let saved = self.model.update(cx, |model, cx| {
            let mut project = model.project.clone();
            project.upsert_connection(spec.clone())?;
            project.save()?;
            model.project = project;
            cx.notify();
            Ok::<_, String>(())
        });
        match saved {
            Ok(()) => window.close_dialog(cx),
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.id.clone() else { return };
        if !self.confirm_delete {
            self.confirm_delete = true;
            cx.notify();
            return;
        }
        let removed = self.model.update(cx, |model, cx| {
            let mut project = model.project.clone();
            project.remove_connection(&id);
            project.save()?;
            model.project = project;
            cx.notify();
            Ok::<_, String>(())
        });
        match removed {
            Ok(()) => {
                let _ = app::state(cx).secrets.delete(&id);
                window.close_dialog(cx);
            }
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }
}

fn field(label: &'static str, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .gap_3()
        .child(
            div()
                .w(LABEL_WIDTH)
                .flex_shrink_0()
                .text_color(color(theme::TEXT_SECONDARY))
                .child(label),
        )
        .child(div().flex_1().min_w_0().child(control))
}

const LABEL_WIDTH: gpui_kit::Pixels = px(104.);

impl Render for ConnectionEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let password_hint = if self.has_saved_password {
            "Saved in the system keychain · leave empty to keep it"
        } else {
            "Stored in the system keychain, not in the project"
        };
        let status = match (&self.error, &self.test) {
            (Some(error), _) => Some((error.clone(), theme::RED)),
            (None, TestState::Idle) => None,
            (None, TestState::Testing) => Some(("Testing…".to_string(), theme::TEXT_TERTIARY)),
            (None, TestState::Passed(message)) => Some((message.clone(), theme::GREEN)),
            (None, TestState::Failed(message)) => Some((message.clone(), theme::RED)),
        };
        let indent = LABEL_WIDTH + px(12.);

        v_flex()
            .gap_4()
            .text_size(px(13.))
            .child(muted("PostgreSQL · saved in .dbadmin/project.toml"))
            .child(
                v_flex()
                    .gap_3()
                    .child(field("Name", Input::new(&self.name)))
                    .child(field("Host", Input::new(&self.host)))
                    .child(field("Port", Input::new(&self.port)))
                    .child(field("Database", Input::new(&self.database)))
                    .child(field("User", Input::new(&self.user)))
                    .child(field("Password", Input::new(&self.password).mask_toggle()))
                    .child(field("", muted(password_hint)))
                    .child(field("SSL mode", Select::new(&self.sslmode)))
                    .child(field("Environment", Select::new(&self.environment)))
                    .child(field(
                        "",
                        muted("Production connections always open read-only and cannot edit rows."),
                    )),
            )
            .children(status.map(|(message, tone)| {
                div()
                    .pl(indent)
                    .text_size(px(12.))
                    .text_color(color(tone))
                    .child(message)
            }))
            .child(
                h_flex()
                    .pt_2()
                    .gap_2()
                    .when(self.id.is_some(), |row| {
                        row.child(
                            Button::new("delete-connection")
                                .ghost()
                                .label(if self.confirm_delete {
                                    "Click again to delete"
                                } else {
                                    "Delete"
                                })
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.delete(window, cx)),
                                ),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("test-connection")
                            .label("Test Connection")
                            .disabled(matches!(self.test, TestState::Testing))
                            .on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))),
                    )
                    .child(
                        Button::new("cancel-connection")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("save-connection")
                            .primary()
                            .label("Save")
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
    }
}
