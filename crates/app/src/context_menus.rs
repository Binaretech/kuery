//! Building blocks for right-click menus: items that run something on a
//! view, copy text, or reveal a file.

use std::path::PathBuf;

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::{ClipboardItem, Context, SharedString, WeakEntity, Window};

/// "Reveal in Finder" and its equivalents on the other platforms.
pub const REVEAL_LABEL: &str = if cfg!(target_os = "macos") {
    "Reveal in Finder"
} else if cfg!(target_os = "windows") {
    "Show in Explorer"
} else {
    "Open Containing Folder"
};

/// An item that runs `run` on `view`.
pub fn item<V: 'static>(
    label: impl Into<SharedString>,
    view: &WeakEntity<V>,
    run: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> PopupMenuItem {
    let view = view.clone();
    PopupMenuItem::new(label).on_click(move |_, window, cx| {
        view.update(cx, |view, cx| run(view, window, cx)).ok();
    })
}

/// An item that copies `text` to the clipboard.
pub fn copy(label: impl Into<SharedString>, text: impl Into<String>) -> PopupMenuItem {
    let text = text.into();
    PopupMenuItem::new(label).on_click(move |_, _, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
    })
}

/// An item that shows `path` in the platform's file manager.
pub fn reveal(path: PathBuf) -> PopupMenuItem {
    PopupMenuItem::new(REVEAL_LABEL).on_click(move |_, _, cx| cx.reveal_path(&path))
}

/// A right-clicked grid cell, with its whole row, for the grids' menus.
pub struct Cell {
    pub row: usize,
    pub column: usize,
    pub columns: Vec<String>,
    /// The row's values; `None` is NULL.
    pub values: Vec<Option<String>>,
}

impl Cell {
    pub fn column_name(&self) -> &str {
        self.columns.get(self.column).map_or("", String::as_str)
    }

    pub fn value(&self) -> Option<&str> {
        self.values.get(self.column).and_then(|v| v.as_deref())
    }
}

/// Copy Value, Copy Row, Copy Row as JSON and Copy Column Name.
pub fn copy_cell_items(menu: PopupMenu, cell: &Cell) -> PopupMenu {
    menu.item(copy(
        "Copy Value",
        cell.value().unwrap_or("NULL").to_string(),
    ))
    .item(copy("Copy Row", row_as_tsv(&cell.values)))
    .item(copy(
        "Copy Row as JSON",
        row_as_json(&cell.columns, &cell.values),
    ))
    .item(copy("Copy Column Name", cell.column_name().to_string()))
}

/// Tab-separated values, NULL as empty, ready to paste into a spreadsheet.
pub fn row_as_tsv(values: &[Option<String>]) -> String {
    values
        .iter()
        .map(|value| value.as_deref().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\t")
}

/// `{"column": "value", "other": null}`. Values stay strings: the grid shows
/// text, not typed values.
pub fn row_as_json(columns: &[String], values: &[Option<String>]) -> String {
    let fields: Vec<String> = columns
        .iter()
        .zip(values)
        .map(|(column, value)| {
            let value = value.as_deref().map_or("null".to_string(), json_string);
            format!("{}: {value}", json_string(column))
        })
        .collect();
    format!("{{{}}}", fields.join(", "))
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_rows_as_tsv_and_json() {
        let columns = vec!["id".to_string(), "note".to_string(), "gone".to_string()];
        let values = vec![
            Some("1".to_string()),
            Some("say \"hi\"\n\tok".to_string()),
            None,
        ];
        assert_eq!(row_as_tsv(&values), "1\tsay \"hi\"\n\tok\t");
        assert_eq!(
            row_as_json(&columns, &values),
            r#"{"id": "1", "note": "say \"hi\"\n\tok", "gone": null}"#
        );
        assert_eq!(json_string("\u{1}"), r#""\u0001""#);
    }
}
