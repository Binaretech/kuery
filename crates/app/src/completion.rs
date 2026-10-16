//! SQL completion in query tabs: the editor asks [`SqlCompletions`] on every
//! edit and on Ctrl+Space, and it answers from [`kuery_core::completion`]
//! with the catalog the tab keeps for its connection.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use gpui_kit::component::input::{CompletionProvider, Rope, RopeExt as _};
use gpui_kit::{App, Task, Window};
use kuery_core::completion::{self, Catalog, SuggestionKind};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    CompletionTriggerKind, TextEdit,
};

/// The tab replaces the catalog when it (re)loads; the provider reads it.
pub type SharedCatalog = Rc<RefCell<Catalog>>;

pub struct SqlCompletions {
    catalog: SharedCatalog,
}

impl SqlCompletions {
    pub fn new(catalog: SharedCatalog) -> Self {
        Self { catalog }
    }
}

impl CompletionProvider for SqlCompletions {
    fn completions(
        &self,
        rope: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let invoked = trigger.trigger_kind == CompletionTriggerKind::INVOKED;
        let result =
            completion::complete(&rope.to_string(), offset, &self.catalog.borrow(), invoked);
        let range = lsp_types::Range {
            start: rope.offset_to_position(result.replace.start),
            end: rope.offset_to_position(result.replace.end),
        };
        let typed = result.prefix.to_lowercase();
        let items = result
            .suggestions
            .into_iter()
            .map(|suggestion| {
                // The menu highlights `filter_text`'s length from the start
                // of the label: only what was typed, when the label starts
                // with it (`sales.invoices` for `inv` does not).
                let highlighted = suggestion
                    .label
                    .get(..result.prefix.len())
                    .filter(|head| head.to_lowercase() == typed)
                    .unwrap_or_default()
                    .to_owned();
                CompletionItem {
                    kind: Some(item_kind(suggestion.kind)),
                    detail: Some(suggestion.detail),
                    filter_text: Some(highlighted),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range,
                        new_text: suggestion.insert,
                    })),
                    label: suggestion.label,
                    ..CompletionItem::default()
                }
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    /// Every edit asks: the engine decides from the context, and an empty
    /// answer closes the menu (after `,` or `)`, say).
    fn is_completion_trigger(&self, _: usize, _: &str, _: &mut App) -> bool {
        true
    }
}

fn item_kind(kind: SuggestionKind) -> CompletionItemKind {
    match kind {
        SuggestionKind::Keyword => CompletionItemKind::KEYWORD,
        SuggestionKind::Schema => CompletionItemKind::MODULE,
        SuggestionKind::Table => CompletionItemKind::STRUCT,
        SuggestionKind::View => CompletionItemKind::INTERFACE,
        SuggestionKind::Alias => CompletionItemKind::VARIABLE,
        SuggestionKind::Column => CompletionItemKind::FIELD,
    }
}
