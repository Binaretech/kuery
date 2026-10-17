//! Keyboard shortcuts: the defaults from the command registry plus the user's
//! keymap file, which can rebind or unbind any of them.
//!
//! The file lives in the user's config directory (`KUERY_KEYMAP` overrides
//! the path):
//!
//! ```toml
//! [bindings]
//! "cmd-w" = "tab.close"      # shortcut = command id
//! "secondary-t" = ""         # an empty command unbinds a default shortcut
//! ```
//!
//! "Reload Keymap" applies changes without restarting: the keymap is rebuilt
//! from gpui-component's own bindings (captured at startup), the defaults and
//! the file.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::{App, DummyKeyboardMapper, Global, KeyBinding, KeyBindingContextPredicate};
use serde::Deserialize;

use crate::commands::{self, COMMANDS, Command, OpenKeymapFile, ReloadKeymap};
use crate::palette;

const IS_MAC: bool = cfg!(target_os = "macos");

struct KeymapState {
    /// gpui-component's bindings (inputs, tables…), captured before ours.
    base: Vec<KeyBinding>,
    path: PathBuf,
    /// Display form of each command's shortcuts, e.g. `tab.close` → `["⌘W"]`.
    shortcuts: Shortcuts,
    problems: Vec<String>,
}

impl Global for KeymapState {}

/// Captures the existing bindings, applies the defaults and the user file,
/// and registers the keymap commands.
pub fn init(cx: &mut App) {
    let base = cx.key_bindings().borrow().bindings().cloned().collect();
    cx.set_global(KeymapState {
        base,
        path: keymap_path(),
        shortcuts: HashMap::new(),
        problems: Vec::new(),
    });
    cx.on_action(|_: &ReloadKeymap, cx| reload(cx));
    cx.on_action(|_: &OpenKeymapFile, cx| open_file(cx));
    reload(cx);
}

/// Rebuilds the keymap from the defaults and the user file.
pub fn reload(cx: &mut App) {
    let path = cx.global::<KeymapState>().path.clone();
    let mut problems = Vec::new();
    let user = match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).unwrap_or_else(|error| {
            problems.push(format!("{}: {error}", path.display()));
            Vec::new()
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            problems.push(format!("Could not read {}: {error}", path.display()));
            Vec::new()
        }
    };
    let (resolved, resolve_problems) = resolve(&user);
    problems.extend(resolve_problems);

    let mut bindings = cx.global::<KeymapState>().base.clone();
    let (command_bindings, shortcuts, binding_problems) = command_bindings(&resolved);
    bindings.extend(command_bindings);
    problems.extend(binding_problems);
    bindings.extend(crate::query_tab::bindings());
    bindings.extend(palette::bindings());

    cx.clear_key_bindings();
    cx.bind_keys(bindings);
    let state = cx.global_mut::<KeymapState>();
    state.shortcuts = shortcuts;
    state.problems = problems;
    // The menu bar shows shortcuts, so it is rebuilt with the new ones.
    crate::menus::set_menus(cx);
}

type Shortcuts = HashMap<&'static str, Vec<String>>;

/// The bindings for the resolved shortcuts, the display form of each
/// command's shortcuts, and the shortcuts that could not be parsed.
fn command_bindings(
    resolved: &[(String, &'static Command)],
) -> (Vec<KeyBinding>, Shortcuts, Vec<String>) {
    let mut bindings = Vec::new();
    let mut shortcuts = Shortcuts::new();
    let mut problems = Vec::new();
    for (keys, command) in resolved {
        let mut valid = true;
        for context in binding_contexts(command.context) {
            let context = context
                .and_then(|c| KeyBindingContextPredicate::parse(&c).ok())
                .map(Rc::new);
            match KeyBinding::load(
                keys,
                (command.action)(),
                context,
                false,
                None,
                &DummyKeyboardMapper,
            ) {
                Ok(binding) => bindings.push(binding),
                Err(_) => valid = false,
            }
        }
        if valid {
            shortcuts
                .entry(command.id)
                .or_default()
                .push(display(keys, IS_MAC));
        } else {
            problems.push(format!("\"{keys}\" is not a valid shortcut"));
        }
    }
    (bindings, shortcuts, problems)
}

/// Where a command's shortcut is bound: its own context, and again for a text
/// field focused inside it. Inputs bind keys of their own (`secondary-enter`
/// inserts a line), and the deeper context wins, so without the second
/// binding a shortcut would do nothing while typing in the SQL editor.
fn binding_contexts(context: Option<&str>) -> [Option<String>; 2] {
    let input = match context {
        Some(context) if context.contains("||") => format!("({context}) > Input"),
        Some(context) => format!("{context} > Input"),
        None => "Input".to_string(),
    };
    [context.map(str::to_owned), Some(input)]
}

/// The first shortcut of a command, for the palette and tooltips.
pub fn shortcut(cx: &App, id: &str) -> Option<String> {
    cx.try_global::<KeymapState>()?
        .shortcuts
        .get(id)?
        .first()
        .cloned()
}

pub fn problems(cx: &App) -> Vec<String> {
    cx.try_global::<KeymapState>()
        .map(|state| state.problems.clone())
        .unwrap_or_default()
}

fn open_file(cx: &mut App) {
    let path = cx.global::<KeymapState>().path.clone();
    if !path.exists() {
        let written = path
            .parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .and_then(|_| std::fs::write(&path, template()));
        if let Err(error) = written {
            cx.global_mut::<KeymapState>()
                .problems
                .push(format!("Could not create {}: {error}", path.display()));
            return;
        }
    }
    cx.open_with_system(&path);
}

/// `KUERY_KEYMAP`, or `keymap.toml` in the platform's config directory.
fn keymap_path() -> PathBuf {
    std::env::var_os("KUERY_KEYMAP")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::config_dir().join("keymap.toml"))
}

fn template() -> String {
    let mut text = String::from(
        "# Kuery keymap.\n\
         # Bind a shortcut to a command id, or to \"\" to unbind a default shortcut.\n\
         # Modifiers: cmd, ctrl, alt, shift, and secondary (cmd on macOS, ctrl elsewhere).\n\
         # Several keys in a row are separated by spaces, e.g. \"secondary-k secondary-t\".\n\
         # Save the file, then run \"Reload Keymap\" from the command palette.\n\
         \n\
         [bindings]\n\
         # \"secondary-w\" = \"tab.close\"\n\
         \n\
         # Commands and their default shortcuts:\n",
    );
    for command in COMMANDS {
        text.push_str(&format!(
            "#   {:<20} {:<20} {}\n",
            command.id,
            command.title,
            command.defaults.join(", ")
        ));
    }
    text
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    bindings: BTreeMap<String, String>,
}

/// Parses the keymap file into `(shortcut, command)` pairs; `None` unbinds.
fn parse(text: &str) -> Result<Vec<(String, Option<String>)>, String> {
    let file: File = toml::from_str(text).map_err(|error| error.message().to_string())?;
    Ok(file
        .bindings
        .into_iter()
        .map(|(keys, command)| {
            let command = command.trim().to_string();
            (keys, (!command.is_empty()).then_some(command))
        })
        .collect())
}

/// Defaults overridden by the user's entries. A shortcut can run different
/// commands in different contexts (`secondary-s` saves a query in a query tab
/// and the pending changes in a table tab); binding it to a command replaces
/// what it did in that command's context, and unbinding removes it everywhere.
fn resolve(user: &[(String, Option<String>)]) -> (Vec<(String, &'static Command)>, Vec<String>) {
    let mut by_keys: BTreeMap<(Option<&'static str>, String), &'static Command> = BTreeMap::new();
    for command in COMMANDS {
        for keys in command.defaults {
            by_keys.insert((command.context, normalize(keys, IS_MAC)), command);
        }
    }
    let mut problems = Vec::new();
    for (keys, command) in user {
        let normalized = normalize(keys, IS_MAC);
        match command {
            None => by_keys.retain(|(_, bound), _| *bound != normalized),
            Some(id) => match commands::find(id) {
                Some(command) => {
                    by_keys.insert((command.context, normalized), command);
                }
                None => problems.push(format!("\"{keys}\": unknown command \"{id}\"")),
            },
        }
    }
    // Keep the registry's order so the first shortcut shown is the default one.
    let mut resolved: Vec<(String, &'static Command)> = by_keys
        .into_iter()
        .map(|((_, keys), command)| (keys, command))
        .collect();
    resolved.sort_by_key(|(keys, command)| {
        let position = COMMANDS
            .iter()
            .position(|c| c.id == command.id)
            .unwrap_or(usize::MAX);
        let default_rank = command
            .defaults
            .iter()
            .position(|d| normalize(d, IS_MAC) == *keys)
            .unwrap_or(usize::MAX);
        (position, default_rank)
    });
    (resolved, problems)
}

/// One keystroke sequence in canonical form, so `cmd-w`, `secondary-w` and
/// `CMD-W` are the same shortcut: modifiers in a fixed order, then the key.
fn normalize(keys: &str, mac: bool) -> String {
    keys.split_whitespace()
        .map(|stroke| {
            let (modifiers, key) = split_stroke(stroke);
            let mut ordered = Vec::new();
            for wanted in ["ctrl", "alt", "shift", "cmd", "fn"] {
                if modifiers
                    .iter()
                    .any(|m| canonical_modifier(m, mac) == wanted)
                {
                    ordered.push(wanted.to_string());
                }
            }
            ordered.push(key.to_lowercase());
            ordered.join("-")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn canonical_modifier(modifier: &str, mac: bool) -> &'static str {
    match modifier.to_lowercase().as_str() {
        "secondary" => {
            if mac {
                "cmd"
            } else {
                "ctrl"
            }
        }
        "cmd" | "command" | "super" | "win" | "platform" => "cmd",
        "ctrl" | "control" => "ctrl",
        "alt" | "option" | "opt" => "alt",
        "shift" => "shift",
        "fn" => "fn",
        _ => "",
    }
}

/// `cmd-shift--` → (`[cmd, shift]`, `-`): the last part is the key, even when
/// the key is `-` itself.
fn split_stroke(stroke: &str) -> (Vec<&str>, &str) {
    if let Some(prefix) = stroke.strip_suffix("--") {
        let modifiers = prefix.split('-').filter(|m| !m.is_empty()).collect();
        return (modifiers, "-");
    }
    let mut parts: Vec<&str> = stroke.split('-').collect();
    let key = parts.pop().unwrap_or_default();
    (parts, key)
}

/// `secondary-shift-w` → `⇧⌘W` on macOS and `Ctrl+Shift+W` elsewhere.
fn display(keys: &str, mac: bool) -> String {
    normalize(keys, mac)
        .split(' ')
        .map(|stroke| {
            let (modifiers, key) = split_stroke(stroke);
            let key = display_key(key, mac);
            if mac {
                let symbols: String = modifiers
                    .iter()
                    .map(|m| match *m {
                        "ctrl" => "⌃",
                        "alt" => "⌥",
                        "shift" => "⇧",
                        "cmd" => "⌘",
                        _ => "fn",
                    })
                    .collect();
                format!("{symbols}{key}")
            } else {
                let mut parts: Vec<String> = modifiers
                    .iter()
                    .map(|m| match *m {
                        "ctrl" => "Ctrl".to_string(),
                        "alt" => "Alt".to_string(),
                        "shift" => "Shift".to_string(),
                        "cmd" => "Super".to_string(),
                        other => other.to_string(),
                    })
                    .collect();
                parts.push(key);
                parts.join("+")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn display_key(key: &str, mac: bool) -> String {
    match key {
        "enter" => if mac { "↵" } else { "Enter" }.into(),
        "escape" => "Esc".into(),
        "tab" => if mac { "⇥" } else { "Tab" }.into(),
        "backspace" => if mac { "⌫" } else { "Backspace" }.into(),
        "delete" => if mac { "⌦" } else { "Delete" }.into(),
        "space" => "Space".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        other => other.to_uppercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_equivalent_shortcuts() {
        assert_eq!(normalize("secondary-w", true), "cmd-w");
        assert_eq!(normalize("secondary-w", false), "ctrl-w");
        assert_eq!(normalize("CMD-Shift-W", true), "shift-cmd-w");
        assert_eq!(
            normalize("shift-cmd-w", true),
            normalize("cmd-shift-w", true)
        );
        assert_eq!(normalize("secondary-k secondary-t", true), "cmd-k cmd-t");
        assert_eq!(normalize("cmd--", true), "cmd--");
    }

    #[test]
    fn displays_shortcuts_per_platform() {
        assert_eq!(display("secondary-shift-w", true), "⇧⌘W");
        assert_eq!(display("secondary-shift-w", false), "Ctrl+Shift+W");
        assert_eq!(display("secondary-enter", true), "⌘↵");
        assert_eq!(display("ctrl-shift-tab", false), "Ctrl+Shift+Tab");
        assert_eq!(display("secondary-k secondary-t", true), "⌘K ⌘T");
    }

    #[test]
    fn parses_bindings_and_unbinds() {
        let parsed =
            parse("[bindings]\n\"cmd-w\" = \"tab.close\"\n\"secondary-t\" = \"\"\n").unwrap();
        assert!(parsed.contains(&("cmd-w".into(), Some("tab.close".into()))));
        assert!(parsed.contains(&("secondary-t".into(), None)));
        assert!(parse("[bindings]\n\"cmd-w\" = 3\n").is_err());
        assert_eq!(parse("").unwrap(), vec![]);
    }

    fn command_for<'a>(resolved: &'a [(String, &'static Command)], keys: &str) -> Option<&'a str> {
        let keys = normalize(keys, IS_MAC);
        resolved.iter().find(|(k, _)| *k == keys).map(|(_, c)| c.id)
    }

    fn commands_for(resolved: &[(String, &'static Command)], keys: &str) -> Vec<&'static str> {
        let keys = normalize(keys, IS_MAC);
        resolved
            .iter()
            .filter(|(k, _)| *k == keys)
            .map(|(_, c)| c.id)
            .collect()
    }

    /// What a keystroke runs with the focus in the SQL editor of a query tab.
    fn action_in_query_editor(keys: &str) -> Option<String> {
        action_in_editor(keys, "QueryTab")
    }

    fn action_in_editor(keys: &str, tab: &str) -> Option<String> {
        use gpui_kit::component::input::Enter;
        use gpui_kit::{KeyContext, Keymap, Keystroke};

        // The editor's own binding, registered before ours as at startup.
        let mut bindings = vec![KeyBinding::new(
            "secondary-enter",
            Enter {
                secondary: true,
                shift: false,
            },
            Some("Input"),
        )];
        let (resolved, _) = resolve(&[]);
        bindings.extend(command_bindings(&resolved).0);
        bindings.extend(crate::query_tab::bindings());
        let keymap = Keymap::new(bindings);
        let stack = ["Workspace", tab, "Input mode=editor"]
            .map(|context| KeyContext::parse(context).unwrap());
        let keys = keys.replace("secondary", if IS_MAC { "cmd" } else { "ctrl" });
        let (matched, _) = keymap.bindings_for_input(&[Keystroke::parse(&keys).unwrap()], &stack);
        matched
            .first()
            .map(|binding| binding.action().name().to_string())
    }

    #[test]
    fn secondary_enter_runs_the_query_from_the_editor() {
        assert_eq!(
            action_in_query_editor("secondary-enter").as_deref(),
            Some("kuery::RunQuery")
        );
        assert_eq!(
            action_in_query_editor("secondary-shift-enter").as_deref(),
            Some("input::Enter")
        );
    }

    #[test]
    fn file_sql_editor_reuses_run_format_cancel_and_newline_bindings() {
        for (keys, action) in [
            ("secondary-enter", "kuery::RunQuery"),
            ("secondary-shift-enter", "input::Enter"),
            ("secondary-shift-f", "kuery::FormatSql"),
            ("secondary-.", "kuery::CancelQuery"),
        ] {
            assert_eq!(
                action_in_editor(keys, "FileQueryTab").as_deref(),
                Some(action)
            );
        }
    }

    #[test]
    fn shortcuts_also_apply_inside_text_fields() {
        assert_eq!(
            binding_contexts(Some("QueryTab")),
            [Some("QueryTab".into()), Some("QueryTab > Input".into())]
        );
        assert_eq!(binding_contexts(None), [None, Some("Input".into())]);
        for context in binding_contexts(Some("QueryTab")).into_iter().flatten() {
            assert!(
                KeyBindingContextPredicate::parse(&context).is_ok(),
                "{context}"
            );
        }
    }

    #[test]
    fn one_shortcut_can_mean_different_things_in_different_contexts() {
        let (resolved, _) = resolve(&[]);
        let mut save = commands_for(&resolved, "secondary-s");
        save.sort();
        assert_eq!(save, ["query.save", "table.save"]);

        let (resolved, _) = resolve(&[("secondary-s".to_string(), None)]);
        assert!(commands_for(&resolved, "secondary-s").is_empty());
    }

    #[test]
    fn defaults_close_the_tab_with_secondary_w() {
        let (resolved, problems) = resolve(&[]);
        assert!(problems.is_empty());
        assert_eq!(command_for(&resolved, "secondary-w"), Some("tab.close"));
        assert_eq!(
            command_for(&resolved, "secondary-shift-w"),
            Some("window.close")
        );
    }

    #[test]
    fn user_entries_rebind_unbind_and_report_unknown_commands() {
        let user = vec![
            ("secondary-w".to_string(), Some("window.close".to_string())),
            (
                "secondary-shift-t".to_string(),
                Some("tab.new_query".to_string()),
            ),
            ("secondary-t".to_string(), None),
            (
                "secondary-j".to_string(),
                Some("does.not.exist".to_string()),
            ),
        ];
        let (resolved, problems) = resolve(&user);
        assert_eq!(command_for(&resolved, "secondary-w"), Some("window.close"));
        assert_eq!(
            command_for(&resolved, "secondary-shift-t"),
            Some("tab.new_query")
        );
        assert_eq!(command_for(&resolved, "secondary-t"), None);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("does.not.exist"));
    }

    #[test]
    fn the_template_lists_every_command() {
        let text = template();
        for command in COMMANDS {
            assert!(text.contains(command.id), "{}", command.id);
        }
        // The template itself is a valid, empty keymap.
        assert_eq!(parse(&text).unwrap(), vec![]);
    }
}
