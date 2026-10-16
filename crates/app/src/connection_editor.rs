//! Connection editor, shown in a dialog: add, edit, test and delete one of the
//! project's connections. The driver is picked from a searchable list of
//! every driver Kuery has, grouped by family; the rest of the form follows
//! the driver's descriptor (see [`kuery_core::driver`]): its default port,
//! what its database and user fields mean, and how it asks about TLS. The
//! password goes to the system keychain; the rest to `.kuery/project.toml`.

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::searchable_list::{SearchableGroup, SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Disableable as _, IndexPath, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, Focusable as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Window, div, px,
};
use kuery_core::driver::{self, DriverInfo, Family};

use crate::app;
use crate::drivers::{self, family_icon};
use crate::project::{ConnectionSpec, ProjectModel, new_id};
use crate::secrets::SecretStore;
use crate::theme::{self, color};
use crate::ui::muted;

type TextSelect = SelectState<SearchableVec<SharedString>>;
type DriverSelect = SelectState<SearchableVec<SearchableGroup<DriverItem>>>;

const ENVIRONMENTS: [&str; 4] = ["detect from host", "development", "staging", "production"];

/// A driver in the searchable list: its icon and name; found by name,
/// alias or family.
#[derive(Debug, Clone)]
struct DriverItem(&'static DriverInfo);

impl SearchableListItem for DriverItem {
    type Value = &'static str;

    fn title(&self) -> SharedString {
        self.0.name.into()
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        h_flex()
            .gap_2()
            .child(
                family_icon(self.0.family)
                    .size(px(14.))
                    .text_color(color(theme::TEXT_SECONDARY)),
            )
            .child(self.0.name)
    }

    fn value(&self) -> &Self::Value {
        &self.0.id
    }

    fn matches(&self, query: &str) -> bool {
        self.0.matches(query)
    }
}

/// Every driver, grouped by family in the order families first appear.
fn driver_groups() -> SearchableVec<SearchableGroup<DriverItem>> {
    let mut families: Vec<Family> = Vec::new();
    for info in driver::ALL {
        if !families.contains(&info.family) {
            families.push(info.family);
        }
    }
    SearchableVec::new(
        families
            .into_iter()
            .map(|family| {
                SearchableGroup::new(family.label()).items(
                    driver::ALL
                        .iter()
                        .filter(|info| info.family == family)
                        .map(|info| DriverItem(info)),
                )
            })
            .collect::<Vec<_>>(),
    )
}

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
    driver: &'static DriverInfo,
    driver_select: Entity<DriverSelect>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    has_saved_password: bool,
    /// The driver's TLS choices (see [`driver::Tls::options`]).
    tls: Entity<TextSelect>,
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
        let unknown_driver = spec
            .as_ref()
            .filter(|spec| spec.driver_info().is_none())
            .map(|spec| spec.driver.clone());
        let info = spec
            .as_ref()
            .and_then(ConnectionSpec::driver_info)
            .unwrap_or(&driver::POSTGRES);
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
        let default_port = info.default_port.to_string();
        let port = text(
            &spec
                .as_ref()
                .map_or(default_port.clone(), |s| s.port.to_string()),
            &default_port,
            window,
            cx,
        );
        let database = text(
            spec.as_ref().map_or("", |s| &s.database),
            info.database.placeholder,
            window,
            cx,
        );
        let user = text(
            spec.as_ref().map_or("", |s| &s.user),
            info.user.placeholder,
            window,
            cx,
        );
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));

        let select =
            |items: Vec<&str>, selected: usize, window: &mut Window, cx: &mut Context<Self>| {
                let items: Vec<SharedString> = items.into_iter().map(SharedString::from).collect();
                cx.new(|cx| {
                    SelectState::new(
                        SearchableVec::new(items),
                        Some(IndexPath::new(selected)),
                        window,
                        cx,
                    )
                })
            };
        let tls_index = info
            .tls
            .index(spec.as_ref().and_then(|s| s.sslmode.as_deref()));
        let tls = select(info.tls.options(), tls_index, window, cx);
        let environment_index = spec
            .as_ref()
            .and_then(|s| s.environment.as_deref())
            .and_then(|e| ENVIRONMENTS.iter().position(|m| *m == e))
            .unwrap_or(0);
        let environment = select(ENVIRONMENTS.to_vec(), environment_index, window, cx);

        let driver_select = cx.new(|cx| {
            let mut state = SelectState::new(driver_groups(), None, window, cx).searchable(true);
            state.set_selected_value(&info.id, window, cx);
            state
        });
        cx.subscribe_in(
            &driver_select,
            window,
            |this,
             _,
             event: &SelectEvent<SearchableVec<SearchableGroup<DriverItem>>>,
             window,
             cx| {
                let SelectEvent::Confirm(Some(id)) = event else {
                    return;
                };
                if let Some(info) = driver::find(id) {
                    this.set_driver(info, window, cx);
                }
            },
        )
        .detach();

        let mut editor = Self {
            model,
            id,
            driver: info,
            driver_select,
            name,
            host,
            port,
            database,
            user,
            password,
            has_saved_password: false,
            tls,
            environment,
            test: TestState::Idle,
            error: unknown_driver.map(|id| {
                format!(
                    "This version of Kuery has no \u{201c}{id}\u{201d} driver; saving switches \
                     the connection to {}",
                    info.name
                )
            }),
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
        cx: &mut App,
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

    /// Switches the form to another driver: the port follows if it was the
    /// old driver's default, the hints and TLS choices change.
    fn set_driver(
        &mut self,
        info: &'static DriverInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if info.id == self.driver.id {
            return;
        }
        let old_default = self.driver.default_port.to_string();
        let new_default = info.default_port.to_string();
        self.port.update(cx, |port, cx| {
            let current = port.value().trim().to_string();
            if current.is_empty() || current == old_default {
                port.set_value(new_default.clone(), window, cx);
            }
            port.set_placeholder(new_default.clone(), window, cx);
        });
        self.database.update(cx, |input, cx| {
            input.set_placeholder(info.database.placeholder, window, cx)
        });
        self.user.update(cx, |input, cx| {
            input.set_placeholder(info.user.placeholder, window, cx)
        });
        if info.tls != self.driver.tls {
            let options: Vec<SharedString> = info
                .tls
                .options()
                .into_iter()
                .map(SharedString::from)
                .collect();
            self.tls.update(cx, |select, cx| {
                select.set_items(SearchableVec::new(options), window, cx);
                select.set_selected_index(Some(IndexPath::new(0)), window, cx);
            });
        }
        self.driver = info;
        self.test = TestState::Idle;
        self.error = None;
        cx.notify();
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

    fn selected_index(select: &Entity<TextSelect>, cx: &App) -> usize {
        select
            .read(cx)
            .selected_index(cx)
            .map(|i| i.row)
            .unwrap_or(0)
    }

    /// The form as a connection, or why it is not valid.
    fn spec(&self, cx: &App) -> Result<ConnectionSpec, String> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let name = value(&self.name);
        let host = value(&self.host);
        let database = value(&self.database);
        let user = value(&self.user);
        if name.is_empty() {
            return Err("Give the connection a name".into());
        }
        self.driver.validate(&host, &database, &user)?;
        let port = value(&self.port)
            .parse::<u16>()
            .map_err(|_| "The port must be a number between 1 and 65535".to_string())?;
        let environment = Self::selected_index(&self.environment, cx);
        Ok(ConnectionSpec {
            id: self.id.clone().unwrap_or_else(new_id),
            name,
            driver: self.driver.id.to_string(),
            host,
            port,
            database,
            user,
            sslmode: self.driver.tls.stored(Self::selected_index(&self.tls, cx)),
            environment: (environment > 0)
                .then(|| ENVIRONMENTS[environment.min(ENVIRONMENTS.len() - 1)].to_string()),
        })
    }

    /// The password typed in the form, or the saved one if the field is empty.
    fn password(&self, cx: &App) -> Option<String> {
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
        let driver = match drivers::for_spec(&spec) {
            Ok(driver) => driver,
            Err(error) => {
                self.test = TestState::Failed(error);
                cx.notify();
                return;
            }
        };
        let typed = self.password(cx);
        let state = app::state(cx);
        let (engines, secrets) = (state.engines(), state.secrets.clone());
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
            let result = driver.test(&spec, password, &engines).await;
            this.update(cx, |this, cx| {
                this.test = match result {
                    Ok(message) => TestState::Passed(message),
                    Err(error) => TestState::Failed(error),
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
        let info = self.driver;
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
        let tls_label = match info.tls {
            driver::Tls::Modes(_) => "SSL mode",
            driver::Tls::Toggle => "TLS",
        };

        v_flex()
            .gap_4()
            .text_size(px(13.))
            .child(field(
                "Driver",
                Select::new(&self.driver_select)
                    .search_placeholder("Search drivers")
                    .icon(family_icon(info.family))
                    .menu_max_h(px(320.)),
            ))
            .child(
                v_flex()
                    .gap_3()
                    .child(field("Name", Input::new(&self.name)))
                    .child(field("Host", Input::new(&self.host)))
                    .child(field("Port", Input::new(&self.port)))
                    .child(field(info.database.label, Input::new(&self.database)))
                    .children(info.database.help.map(|help| field("", muted(help))))
                    .child(field(info.user.label, Input::new(&self.user)))
                    .children(info.user.help.map(|help| field("", muted(help))))
                    .child(field("Password", Input::new(&self.password).mask_toggle()))
                    .child(field("", muted(password_hint)))
                    .child(field(tls_label, Select::new(&self.tls)))
                    .child(field("Environment", Select::new(&self.environment)))
                    .child(field("", muted(info.production_note))),
            )
            .child(field("", muted("Saved in .kuery/project.toml")))
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
