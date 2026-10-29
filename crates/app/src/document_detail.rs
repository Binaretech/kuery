//! One document beside a grid, read from the document itself and never
//! from the grid's text: as a typed literal in a read-only editor, which
//! reads back the same (`NumberLong("5")`, `ObjectId("…")`), or as a tree of
//! its fields with their types, whose menu copies a field's path or value
//! and filters the collection by it.

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::tree::{TreeItem, TreeState, tree};
use gpui_kit::component::{Icon, IconName, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, FontWeight, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, div,
    px,
};
use kuery_core::document::language::{Language, Style};
use kuery_core::document::value::{Document, FieldPath, PathPart, Value};

use crate::assets::AppIcon;
use crate::chrome;
use crate::context_menus::copy;
use crate::theme::{self, color};
use crate::ui::placeholder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailMode {
    Text,
    Tree,
}

pub enum DetailEvent {
    /// Filter the collection with this filter bar text, which finds the
    /// documents holding a field's value (see [`Language::field_filter`]).
    FilterBy(String),
    Close,
}

/// A field of the tree, by its node's id.
struct Node {
    path: FieldPath,
    value: Value,
}

/// One document beside a grid: as a typed literal in a read-only editor, or
/// as a tree with type badges.
pub struct DocumentDetail {
    language: &'static Language,
    document: Option<Document>,
    mode: DetailMode,
    editor: Entity<EditorState>,
    tree: Entity<TreeState>,
    /// The tree's fields, by node id; replaced with each document.
    nodes: Rc<HashMap<SharedString, Node>>,
    weak: WeakEntity<Self>,
}

impl EventEmitter<DetailEvent> for DocumentDetail {}

impl DocumentDetail {
    pub fn new(language: &'static Language, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(language.editor)
                .line_number(true)
                .folding(true)
                .indent_guides(true)
        });
        let tree = cx.new(|cx| TreeState::new(cx));
        Self {
            language,
            document: None,
            mode: DetailMode::Text,
            editor,
            tree,
            nodes: Rc::new(HashMap::new()),
            weak: cx.entity().downgrade(),
        }
    }

    pub fn show(
        &mut self,
        document: Option<Document>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let render = self.language.render;
        let text = document
            .as_ref()
            .map(|document| render(&Value::Object(document.clone()), Style::Pretty))
            .unwrap_or_default();
        self.editor
            .update(cx, |editor, cx| editor.set_value(text, window, cx));
        let mut nodes = HashMap::new();
        let items = document
            .as_ref()
            .map(|document| fields(document, &FieldPath(Vec::new()), "", &mut nodes))
            .unwrap_or_default();
        self.tree.update(cx, |tree, cx| tree.set_items(items, cx));
        self.nodes = Rc::new(nodes);
        self.document = document;
        cx.notify();
    }

    /// `Document 7`, after the document's key.
    fn title(&self) -> String {
        match self.document.as_ref().and_then(Document::key) {
            Some(key) => format!(
                "Document {}",
                (self.language.render)(&key.0, Style::Compact)
            ),
            None => "Document".to_string(),
        }
    }

    fn render_tree(&self) -> impl IntoElement {
        let nodes = self.nodes.clone();
        let menu_nodes = self.nodes.clone();
        let render = self.language.render;
        let field_filter = self.language.field_filter;
        let detail = self.weak.clone();
        tree(&self.tree, move |ix, entry, _, _, _| {
            let item = entry.item();
            let node = nodes.get(&item.id);
            let preview = node.map(|node| preview(&node.value, render));
            let kind = node.map_or("", |node| node.value.type_name());
            ListItem::new(ix)
                .pl(px(10. + 14. * entry.depth() as f32))
                .child(
                    h_flex()
                        .w_full()
                        .gap(px(6.))
                        .text_size(px(12.))
                        .child(
                            div()
                                .w(px(12.))
                                .flex_shrink_0()
                                .children(entry.is_folder().then(|| {
                                    Icon::new(if entry.is_expanded() {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .size(px(11.))
                                    .text_color(color(theme::TEXT_TERTIARY))
                                })),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .font_weight(FontWeight::MEDIUM)
                                .child(item.label.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(color(theme::TEXT_SECONDARY))
                                .children(preview),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(10.5))
                                .text_color(color(theme::TEXT_TERTIARY))
                                .child(kind),
                        ),
                )
        })
        .context_menu(move |_, entry, menu, _, _| {
            let Some(node) = menu_nodes.get(&entry.item().id) else {
                return menu;
            };
            let copied = copy_text(&node.value, render);
            let filter = field_filter(&node.path, Some(&node.value));
            let detail = detail.clone();
            let menu = menu
                .item(copy("Copy Path", node.path.dotted()).icon(IconName::Copy))
                .item(copy("Copy Value", copied));
            match filter {
                Some(filter) => menu.separator().item(
                    PopupMenuItem::new("Filter by This Value")
                        .icon(AppIcon::Filter)
                        .on_click(move |_, _, cx| {
                            let filter = filter.clone();
                            detail
                                .update(cx, |_, cx| cx.emit(DetailEvent::FilterBy(filter)))
                                .ok();
                        }),
                ),
                None => menu,
            }
        })
    }
}

impl Render for DocumentDetail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self.mode == DetailMode::Text;
        let body = match (&self.document, self.mode) {
            (None, _) => placeholder("No document"),
            (Some(_), DetailMode::Text) => Editor::new(&self.editor)
                .readonly(true)
                .bordered(false)
                .h_full()
                .into_any_element(),
            (Some(_), DetailMode::Tree) => div()
                .size_full()
                .py_1()
                .child(self.render_tree())
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(40.))
                    .px_3()
                    .gap_2()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(color(theme::HAIRLINE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.title()),
                    )
                    .child(
                        chrome::segmented()
                            .child(
                                chrome::segment("detail-text", AppIcon::Code, "Text", text)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.mode = DetailMode::Text;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                chrome::segment(
                                    "detail-tree",
                                    IconName::FolderClosed,
                                    "Tree",
                                    !text,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.mode = DetailMode::Tree;
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                    .child(
                        chrome::icon_button("close-detail", IconName::Close, "Close", true)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DetailEvent::Close))),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// The tree items of a document's fields (or an array's elements) under
/// `path`, recording each field by its node id. Ids are positions (`2/0`),
/// so field names never clash with them.
fn fields(
    document: &Document,
    path: &FieldPath,
    id: &str,
    nodes: &mut HashMap<SharedString, Node>,
) -> Vec<TreeItem> {
    document
        .0
        .iter()
        .enumerate()
        .map(|(position, (name, value))| {
            let path = path.child(PathPart::Field(name.clone()));
            node(position, name.clone(), path, value, id, nodes)
        })
        .collect()
}

fn node(
    position: usize,
    label: String,
    path: FieldPath,
    value: &Value,
    parent: &str,
    nodes: &mut HashMap<SharedString, Node>,
) -> TreeItem {
    let id = match parent {
        "" => position.to_string(),
        parent => format!("{parent}/{position}"),
    };
    let children = match value {
        Value::Object(document) => fields(document, &path, &id, nodes),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let path = path.child(PathPart::Index(index));
                node(index, index.to_string(), path, item, &id, nodes)
            })
            .collect(),
        _ => Vec::new(),
    };
    let id = SharedString::from(id);
    nodes.insert(
        id.clone(),
        Node {
            path,
            value: value.clone(),
        },
    );
    TreeItem::new(id, label).children(children)
}

/// What a node shows after its name: a scalar as its literal, a nested
/// document or array by its size.
fn preview(value: &Value, render: fn(&Value, Style) -> String) -> String {
    match value {
        Value::Object(document) => match document.0.len() {
            1 => "{ 1 field }".to_string(),
            n => format!("{{ {n} fields }}"),
        },
        Value::Array(items) => match items.len() {
            1 => "[ 1 item ]".to_string(),
            n => format!("[ {n} items ]"),
        },
        value => render(value, Style::Compact),
    }
}

/// A value as copied: a string as it is, anything else as the literal.
pub fn copy_text(value: &Value, render: fn(&Value, Style) -> String) -> String {
    match value {
        Value::String(text) => text.clone(),
        value => render(value, Style::Pretty),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(value: &Value, style: Style) -> String {
        (kuery_mongo::LANGUAGE.render)(value, style)
    }

    #[test]
    fn the_tree_knows_each_field_by_its_path() {
        let document = Document(vec![
            ("_id".into(), Value::Int32(7)),
            (
                "items".into(),
                Value::Array(vec![Value::Object(Document(vec![(
                    "sku".into(),
                    Value::String("a".into()),
                )]))]),
            ),
        ]);
        let mut nodes = HashMap::new();
        let items = fields(&document, &FieldPath(Vec::new()), "", &mut nodes);
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].children.len(), 1);
        let paths: Vec<(&str, String)> = ["0", "1", "1/0", "1/0/0"]
            .into_iter()
            .map(|id| (id, nodes[&SharedString::from(id)].path.dotted()))
            .collect();
        assert_eq!(
            paths,
            [
                ("0", "_id".to_string()),
                ("1", "items".to_string()),
                ("1/0", "items.0".to_string()),
                ("1/0/0", "items.0.sku".to_string()),
            ]
        );
        assert_eq!(
            preview(&nodes[&SharedString::from("1")].value, render),
            "[ 1 item ]"
        );
        assert_eq!(copy_text(&Value::String("a".into()), render), "a");
        assert_eq!(copy_text(&Value::Int32(7), render), "7");
    }
}
