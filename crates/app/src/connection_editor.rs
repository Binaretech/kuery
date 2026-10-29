//! Connection editor, shown in a dialog: add, edit, test and delete one of the
//! project's connections. The driver is picked from a searchable list of
//! every driver Kuery has, grouped by family; the rest of the form follows
//! the driver's descriptor (see [`kuery_core::driver`]): its default port,
//! what its database and user fields mean, how it asks about TLS, and its
//! other options. Pasting a connection string into the host field, for a
//! driver that reads one, fills the form instead. The password goes to the
//! system keychain; the rest to `.kuery/project.toml`. Any connection can go
//! through an SSH host (see [`crate::tunnel`]).

use std::collections::BTreeMap;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::searchable_list::{SearchableGroup, SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Disableable as _, IndexPath, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, IntoElement,
    ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _, Window, div, px,
};
use kuery_core::driver::{self, DriverInfo, Family, OptionField, OptionKind, ParsedUri};

use crate::app;
use crate::connection::Environment;
use crate::drivers::{self, family_icon};
use crate::project::{ConnectionSpec, ProjectModel, SshSpec, new_id};
use crate::secrets::SecretStore;
use crate::theme::{self, color};
use crate::ui::muted;

type TextSelect = SelectState<SearchableVec<SharedString>>;
type DriverSelect = SelectState<SearchableVec<SearchableGroup<DriverItem>>>;

const ENVIRONMENTS: [&str; 4] = [
    "detect from host",
    Environment::Development.name(),
    Environment::Staging.name(),
    Environment::Production.name(),
];

/// The entry of [`ENVIRONMENTS`] for the environment the project file names.
fn environment_index(stored: Option<&str>) -> usize {
    stored
        .map(|name| Environment::parse(name).name())
        .and_then(|name| ENVIRONMENTS.iter().position(|m| *m == name))
        .unwrap_or(0)
}

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
    let infos: Vec<&'static DriverInfo> = drivers::all().into_iter().map(|d| d.info()).collect();
    let mut families: Vec<Family> = Vec::new();
    for info in &infos {
        if !families.contains(&info.family) {
            families.push(info.family);
        }
    }
    SearchableVec::new(
        families
            .into_iter()
            .map(|family| {
                SearchableGroup::new(family.label()).items(
                    infos
                        .iter()
                        .filter(|info| info.family == family)
                        .map(|info| DriverItem(info)),
                )
            })
            .collect::<Vec<_>>(),
    )
}

/// What a pasted connection string puts in the form.
#[derive(Debug, PartialEq)]
struct FormValues {
    host: String,
    port: String,
    database: String,
    user: String,
    /// Typed into the password field, so Save keeps it only in the keychain.
    password: Option<String>,
    tls: Option<String>,
    options: BTreeMap<String, String>,
}

/// The form's fields for a split connection string: a port it does not name
/// is the driver's default.
fn apply_uri(parsed: ParsedUri, default_port: u16) -> FormValues {
    FormValues {
        host: parsed.host,
        port: parsed.port.unwrap_or(default_port).to_string(),
        database: parsed.database,
        user: parsed.user,
        password: parsed.password,
        tls: parsed.tls,
        options: parsed.options,
    }
}

/// Whether a change to the host field brought more than one character at
/// once: a paste, not a key typed.
fn pasted(previous: &str, text: &str) -> bool {
    text.chars().count() > previous.chars().count() + 1
}

/// Refuses a host that holds a connection string the driver could not
/// split, or a user and password: they stay in the field, and must never
/// reach the project file.
fn check_host(host: &str) -> Result<(), String> {
    if host.contains("://") {
        return Err("The host is a name or an address, not a connection string".into());
    }
    if host.contains('@') {
        return Err(
            "The host is a name or an address: the user and password have fields of their own"
                .into(),
        );
    }
    Ok(())
}

/// The control of one of the driver's options.
enum OptionInput {
    /// A text, or a file's path.
    Text(Entity<InputState>),
    Toggle(bool),
    /// The driver's default first, then its choices.
    Choice(Entity<TextSelect>),
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
    /// The host field's text after its last change, to tell a pasted
    /// connection string from one being typed.
    host_text: String,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    has_saved_password: bool,
    /// The driver's TLS choices (see [`driver::Tls::options`]).
    tls: Entity<TextSelect>,
    /// The driver's other options, in its order.
    options: Vec<(&'static OptionField, OptionInput)>,
    environment: Entity<TextSelect>,
    /// Through an SSH host, and its fields.
    ssh: bool,
    ssh_host: Entity<InputState>,
    ssh_port: Entity<InputState>,
    ssh_user: Entity<InputState>,
    ssh_identity: Entity<InputState>,
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
            .unwrap_or(&kuery_pg::DRIVER.info);
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
        let ssh = spec.as_ref().and_then(|s| s.ssh.clone());
        let ssh_host = text(
            ssh.as_ref().map_or("", |s| &s.host),
            "bastion.example.com or a ~/.ssh/config alias",
            window,
            cx,
        );
        let ssh_port = text(
            &ssh.as_ref()
                .and_then(|s| s.port)
                .map(|port| port.to_string())
                .unwrap_or_default(),
            "22",
            window,
            cx,
        );
        let ssh_user = text(
            ssh.as_ref().map_or("", |s| &s.user),
            "from ~/.ssh/config",
            window,
            cx,
        );
        let ssh_identity = text(
            ssh.as_ref().map_or("", |s| &s.identity_file),
            "~/.ssh/id_ed25519, or the SSH agent",
            window,
            cx,
        );

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
        let environment_index =
            environment_index(spec.as_ref().and_then(|s| s.environment.as_deref()));
        let environment = select(ENVIRONMENTS.to_vec(), environment_index, window, cx);
        let options = option_inputs(
            info,
            &spec.as_ref().map(|s| s.options.clone()).unwrap_or_default(),
            window,
            cx,
        );
        // A pasted connection string fills the form; the host and the
        // environment decide which options are offered.
        cx.subscribe_in(&host, window, |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::Change) {
                this.host_changed(window, cx);
            }
        })
        .detach();
        cx.subscribe(&ssh_host, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(
            &environment,
            |_, _, _: &SelectEvent<SearchableVec<SharedString>>, cx| cx.notify(),
        )
        .detach();

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
                if let Some(driver) = drivers::find(id) {
                    this.set_driver(driver.info(), window, cx);
                }
            },
        )
        .detach();

        let host_text = host.read(cx).value().trim().to_string();
        let mut editor = Self {
            model,
            id,
            driver: info,
            driver_select,
            name,
            host,
            host_text,
            port,
            database,
            user,
            password,
            has_saved_password: false,
            tls,
            options,
            environment,
            ssh: ssh.is_some(),
            ssh_host,
            ssh_port,
            ssh_user,
            ssh_identity,
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
        self.options = option_inputs(info, &BTreeMap::new(), window, cx);
        self.driver = info;
        self.test = TestState::Idle;
        self.error = None;
        cx.notify();
    }

    /// A connection string pasted into the host field fills the form, for a
    /// driver that reads one. The string itself is never kept: its password
    /// goes to the password field, and so only ever to the keychain. One
    /// being typed is not split as it grows (`mongodb://k` would already
    /// fill the host with `k`); Save refuses it whole.
    fn host_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.host.read(cx).value().trim().to_string();
        let previous = std::mem::replace(&mut self.host_text, text.clone());
        if let Some(split) = self.driver.uri
            && text.contains("://")
            && pasted(&previous, &text)
        {
            match split(&text) {
                Ok(parsed) => {
                    let values = apply_uri(parsed, self.driver.default_port);
                    // Filling the field sends no change of its own.
                    self.host_text = values.host.clone();
                    self.fill(values, window, cx);
                    self.error = None;
                }
                Err(error) => self.error = Some(error),
            }
            self.test = TestState::Idle;
        }
        cx.notify();
    }

    fn fill(&mut self, values: FormValues, window: &mut Window, cx: &mut Context<Self>) {
        let inputs = [
            (&self.host, values.host),
            (&self.port, values.port),
            (&self.database, values.database),
            (&self.user, values.user),
        ];
        for (input, value) in inputs {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        if let Some(password) = values.password {
            self.password
                .update(cx, |input, cx| input.set_value(password, window, cx));
        }
        let tls = self.driver.tls.index(values.tls.as_deref());
        self.tls.update(cx, |select, cx| {
            select.set_selected_index(Some(IndexPath::new(tls)), window, cx)
        });
        self.options = option_inputs(self.driver, &values.options, window, cx);
    }

    /// The environment the form's settings give, as it changes: the options
    /// offered depend on it.
    fn environment(&self, cx: &App) -> Environment {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let selected = Self::selected_index(&self.environment, cx);
        ConnectionSpec {
            id: String::new(),
            name: String::new(),
            driver: self.driver.id.to_string(),
            host: value(&self.host),
            port: 0,
            database: value(&self.database),
            user: String::new(),
            sslmode: None,
            environment: (selected > 0)
                .then(|| ENVIRONMENTS[selected.min(ENVIRONMENTS.len() - 1)].to_string()),
            ssh: self.ssh.then(|| SshSpec {
                host: value(&self.ssh_host),
                port: None,
                user: String::new(),
                identity_file: String::new(),
            }),
            options: BTreeMap::new(),
        }
        .environment()
    }

    /// The options as the connection keeps them: empty ones and toggles that
    /// are off left out, and on production those it may not have.
    fn option_values(&self, environment: Environment, cx: &App) -> BTreeMap<String, String> {
        self.options
            .iter()
            .filter(|(field, _)| field.production || environment != Environment::Production)
            .filter_map(|(field, input)| {
                let value = match (input, field.kind) {
                    (OptionInput::Text(input), _) => input.read(cx).value().trim().to_string(),
                    (OptionInput::Toggle(on), _) => match on {
                        true => "true".to_string(),
                        false => String::new(),
                    },
                    (OptionInput::Choice(select), OptionKind::Choice(choices)) => {
                        Self::selected_index(select, cx)
                            .checked_sub(1)
                            .and_then(|index| choices.get(index))
                            .map(|choice| choice.to_string())
                            .unwrap_or_default()
                    }
                    (OptionInput::Choice(_), _) => String::new(),
                };
                (!value.is_empty()).then(|| (field.key.to_string(), value))
            })
            .collect()
    }

    fn set_toggle(&mut self, position: usize, on: bool, cx: &mut Context<Self>) {
        if let Some((_, input @ OptionInput::Toggle(_))) = self.options.get_mut(position) {
            *input = OptionInput::Toggle(on);
            self.test = TestState::Idle;
            cx.notify();
        }
    }

    /// Asks for a file and puts its path in the option's field.
    fn choose_file(&mut self, position: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((field, OptionInput::Text(input))) = self.options.get(position) else {
            return;
        };
        let input = input.clone();
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(field.label.into()),
        });
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            if let Some(path) = paths.into_iter().next() {
                input
                    .update_in(cx, |input, window, cx| {
                        input.set_value(path.display().to_string(), window, cx)
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// One option's row, and its help under it.
    fn option_row(
        &self,
        position: usize,
        option: &'static OptionField,
        input: &OptionInput,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let control = match input {
            OptionInput::Text(input) if option.kind == OptionKind::File => h_flex()
                .gap_2()
                .child(div().flex_1().min_w_0().child(Input::new(input)))
                .child(
                    Button::new(("connection-option-file", position))
                        .label("Choose…")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.choose_file(position, window, cx)
                        })),
                )
                .into_any_element(),
            OptionInput::Text(input) => Input::new(input).into_any_element(),
            OptionInput::Toggle(on) => Checkbox::new(("connection-option", position))
                .checked(*on)
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    this.set_toggle(position, *checked, cx)
                }))
                .into_any_element(),
            OptionInput::Choice(select) => Select::new(select).into_any_element(),
        };
        v_flex()
            .gap_3()
            .child(field(option.label, control))
            .children(option.help.map(field_help))
            .into_any_element()
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
        check_host(&host)?;
        self.driver.validate(&host, &database, &user)?;
        // A database file has no server: no host, port, user or TLS.
        let file = self.driver.file;
        let (host, user) = if file {
            (String::new(), String::new())
        } else {
            (host, user)
        };
        let port = if file {
            0
        } else {
            value(&self.port)
                .parse::<u16>()
                .map_err(|_| "The port must be a number between 1 and 65535".to_string())?
        };
        let environment = Self::selected_index(&self.environment, cx);
        let ssh = if self.ssh && !file {
            let host = value(&self.ssh_host);
            if host.is_empty() {
                return Err("Name the SSH host to connect through".into());
            }
            let port = value(&self.ssh_port);
            let port =
                if port.is_empty() {
                    None
                } else {
                    Some(port.parse::<u16>().map_err(|_| {
                        "The SSH port must be a number between 1 and 65535".to_string()
                    })?)
                };
            Some(SshSpec {
                host,
                port,
                user: value(&self.ssh_user),
                identity_file: value(&self.ssh_identity),
            })
        } else {
            None
        };
        let mut spec = ConnectionSpec {
            id: self.id.clone().unwrap_or_else(new_id),
            name,
            driver: self.driver.id.to_string(),
            host,
            port,
            database,
            user,
            sslmode: (!file)
                .then(|| self.driver.tls.stored(Self::selected_index(&self.tls, cx)))
                .flatten(),
            environment: (environment > 0)
                .then(|| ENVIRONMENTS[environment.min(ENVIRONMENTS.len() - 1)].to_string()),
            ssh,
            options: BTreeMap::new(),
        };
        spec.options = self.option_values(spec.environment(), cx);
        self.driver.check_options(&spec.options)?;
        Ok(spec)
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
        let spec = spec.resolved(&self.model.read(cx).project.root);
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

/// A line of help under a field.
fn field_help(help: &'static str) -> impl IntoElement {
    field("", muted(help))
}

/// The controls of a driver's options, holding `values`.
fn option_inputs(
    info: &'static DriverInfo,
    values: &BTreeMap<String, String>,
    window: &mut Window,
    cx: &mut Context<ConnectionEditor>,
) -> Vec<(&'static OptionField, OptionInput)> {
    info.options
        .iter()
        .map(|field| {
            let value = values.get(field.key).cloned().unwrap_or_default();
            let input = match field.kind {
                OptionKind::Text | OptionKind::File => OptionInput::Text(cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(field.placeholder)
                        .default_value(value)
                })),
                OptionKind::Toggle => OptionInput::Toggle(value == "true"),
                OptionKind::Choice(choices) => {
                    let selected = choices
                        .iter()
                        .position(|choice| *choice == value)
                        .map_or(0, |index| index + 1);
                    let items: Vec<SharedString> = std::iter::once("default")
                        .chain(choices.iter().copied())
                        .map(SharedString::from)
                        .collect();
                    OptionInput::Choice(cx.new(|cx| {
                        SelectState::new(
                            SearchableVec::new(items),
                            Some(IndexPath::new(selected)),
                            window,
                            cx,
                        )
                    }))
                }
            };
            (field, input)
        })
        .collect()
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
        let weak = cx.entity().downgrade();
        let tls_label = match info.tls {
            driver::Tls::Modes(_) => "SSL mode",
            driver::Tls::Toggle => "TLS",
        };
        let production = self.environment(cx) == Environment::Production;
        let options: Vec<AnyElement> = self
            .options
            .iter()
            .enumerate()
            .filter(|(_, (field, _))| field.production || !production)
            .map(|(position, (field, input))| self.option_row(position, field, input, cx))
            .collect();

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
                    .when(!info.file, |form| {
                        form.child(field("Host", Input::new(&self.host)))
                            .children(info.host_help.map(field_help))
                            .child(field("Port", Input::new(&self.port)))
                    })
                    .child(field(info.database.label, Input::new(&self.database)))
                    .children(info.database.help.map(field_help))
                    .when(!info.file, |form| {
                        form.child(field(info.user.label, Input::new(&self.user)))
                            .children(info.user.help.map(field_help))
                            .child(field("Password", Input::new(&self.password).mask_toggle()))
                            .child(field("", muted(password_hint)))
                            .child(field(tls_label, Select::new(&self.tls)))
                            .children(options)
                    })
                    .when(!info.file, |form| {
                        form.child(field(
                            "SSH tunnel",
                            Checkbox::new("connection-ssh")
                                .label("Connect through an SSH host")
                                .checked(self.ssh)
                                .on_click(move |checked, _, cx| {
                                    weak.update(cx, |this, cx| {
                                        this.ssh = *checked;
                                        this.test = TestState::Idle;
                                        cx.notify();
                                    })
                                    .ok();
                                }),
                        ))
                    })
                    .when(self.ssh && !info.file, |form| {
                        form.child(field("SSH host", Input::new(&self.ssh_host)))
                            .child(field("SSH port", Input::new(&self.ssh_port)))
                            .child(field("SSH user", Input::new(&self.ssh_user)))
                            .child(field("Identity file", Input::new(&self.ssh_identity)))
                            .child(field(
                                "",
                                muted(
                                    "Uses your ssh: ~/.ssh/config, the SSH agent and known_hosts, \
                                     never an SSH password. Host and Port above are the server \
                                     as the SSH host sees it.",
                                ),
                            ))
                    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_string_fills_the_form() {
        let options: BTreeMap<String, String> = [("authSource", "admin"), ("srv", "true")]
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .into();
        let parsed = ParsedUri {
            host: "cluster0.x.mongodb.net".into(),
            port: None,
            database: "shop".into(),
            user: "u".into(),
            password: Some("p@ss".into()),
            tls: Some("require".into()),
            options: options.clone(),
        };
        assert_eq!(
            apply_uri(parsed, 27017),
            FormValues {
                host: "cluster0.x.mongodb.net".into(),
                port: "27017".into(),
                database: "shop".into(),
                user: "u".into(),
                password: Some("p@ss".into()),
                tls: Some("require".into()),
                options,
            }
        );
        let one = ParsedUri {
            host: "localhost".into(),
            port: Some(27019),
            database: String::new(),
            user: String::new(),
            password: None,
            tls: None,
            options: BTreeMap::new(),
        };
        let values = apply_uri(one, 27017);
        assert_eq!((values.port.as_str(), values.password), ("27019", None));
    }

    #[test]
    fn only_a_pasted_string_is_split() {
        assert!(pasted("localhost", "mongodb://kuery:secret@db/shop"));
        assert!(pasted("", "mongodb://db"));
        // Typed a key at a time, the string is never split part way.
        assert!(!pasted("mongodb://", "mongodb://k"));
        assert!(!pasted(
            "mongodb://kuery:secret@db/sho",
            "mongodb://kuery:secret@db/shop"
        ));
        assert!(!pasted("mongodb://k", "mongodb://"));
    }

    #[test]
    fn a_host_never_keeps_credentials() {
        assert!(check_host("db.example.com").is_ok());
        assert!(check_host("a:27017,b:27018").is_ok());
        assert!(check_host("[::1]").is_ok());
        // What is left of a typed connection string after its scheme.
        for refused in [
            "kuery:secret@db/shop",
            "mongodb://kuery:secret@db/shop",
            "user@db",
        ] {
            let error = check_host(refused).unwrap_err();
            assert!(!error.contains("secret"), "{error}");
        }
    }

    #[test]
    fn saving_keeps_the_environment_the_file_names() {
        let local = "postgres://localhost/";
        assert_eq!(environment_index(None), 0);
        for stored in [
            "production",
            "prod",
            "PRODUCTION",
            "Dev",
            "local",
            "stage",
            "qa?",
        ] {
            // The first entry, detect from host, saves no environment.
            let index = environment_index(Some(stored));
            let saved = (index > 0).then(|| ENVIRONMENTS[index]);
            assert_eq!(
                Environment::detect(local, saved),
                Environment::detect(local, Some(stored)),
                "{stored} saved as {saved:?}"
            );
        }
    }
}
