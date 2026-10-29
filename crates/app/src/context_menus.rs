//! Building blocks for right-click menus: items that run something on a
//! view, copy text, or reveal a file.

use std::path::PathBuf;

use gpui_kit::component::IconName;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::{ClipboardItem, Context, SharedString, WeakEntity, Window};
use kuery_core::export::json_string;

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

/// [`item`] with an icon.
pub fn item_icon<V: 'static>(
    label: impl Into<SharedString>,
    icon: impl Into<gpui_kit::component::Icon>,
    view: &WeakEntity<V>,
    run: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
) -> PopupMenuItem {
    item(label, view, run).icon(icon)
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
    PopupMenuItem::new(REVEAL_LABEL)
        .icon(IconName::FolderOpen)
        .on_click(move |_, _, cx| cx.reveal_path(&path))
}

/// A right-clicked grid cell, with its whole row, for the grids' menus.
pub struct Cell {
    pub row: usize,
    pub column: usize,
    pub columns: Vec<String>,
    /// The row's values; `None` is NULL, or an absent field.
    pub values: Vec<Option<String>>,
    /// The cell is a field the document lacks, not a NULL.
    pub absent: bool,
}

impl Cell {
    pub fn column_name(&self) -> &str {
        self.columns.get(self.column).map_or("", String::as_str)
    }

    pub fn value(&self) -> Option<&str> {
        self.values.get(self.column).and_then(|v| v.as_deref())
    }

    /// What Copy Value copies: NULL as `NULL`, a field the document lacks
    /// as nothing.
    pub fn copied_value(&self) -> String {
        match self.value() {
            Some(value) => value.to_string(),
            None if self.absent => String::new(),
            None => "NULL".to_string(),
        }
    }
}

/// Copy Value, Copy Row, Copy Row as JSON and Copy Column Name. `row_json`
/// replaces the row's text as JSON when the grid knows the row better (a
/// document's own fields and types).
pub fn copy_cell_items(menu: PopupMenu, cell: &Cell, row_json: Option<String>) -> PopupMenu {
    let row_json = row_json.unwrap_or_else(|| row_as_json(&cell.columns, &cell.values));
    menu.item(copy("Copy Value", cell.copied_value()).icon(IconName::Copy))
        .item(copy("Copy Row", row_as_tsv(&cell.values)))
        .item(copy("Copy Row as JSON", row_json))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_null_as_null_and_an_absent_field_as_nothing() {
        let cell = |values: Vec<Option<String>>, absent| Cell {
            row: 0,
            column: 0,
            columns: vec!["coupon".into()],
            values,
            absent,
        };
        assert_eq!(
            cell(vec![Some("SAVE7".into())], false).copied_value(),
            "SAVE7"
        );
        assert_eq!(cell(vec![None], false).copied_value(), "NULL");
        assert_eq!(cell(vec![None], true).copied_value(), "");
    }

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
    }
}
