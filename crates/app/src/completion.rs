//! SQL completion in query tabs: the editor asks [`SqlCompletions`] on every
//! edit and on Ctrl+Space, and it answers from [`kuery_core::completion`]
//! with the catalog the tab keeps for its connection. The Redis console
//! completes command names with [`RedisCompletions`], and the document
//! console what its language asks for at the cursor with
//! [`DocumentCompletions`].

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use anyhow::Result;
use gpui_kit::component::input::{CompletionProvider, Rope, RopeExt as _};
use gpui_kit::{App, Task, Window};
use kuery_core::completion::{self, Catalog, SuggestionKind};
use kuery_core::document::language::{self as language, Language};
use kuery_core::document::value::{Document, Value};
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

/// Completes the command name at the start of a console line.
pub struct RedisCompletions;

impl CompletionProvider for RedisCompletions {
    fn completions(
        &self,
        rope: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let text = rope.to_string();
        let offset = offset.min(text.len());
        let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
        let typed = text[line_start..offset].trim_start();
        let invoked = trigger.trigger_kind == CompletionTriggerKind::INVOKED;
        let in_name = !typed.contains(char::is_whitespace) && !typed.starts_with('#');
        if !in_name || (typed.is_empty() && !invoked) {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        }
        let range = lsp_types::Range {
            start: rope.offset_to_position(offset - typed.len()),
            end: rope.offset_to_position(offset),
        };
        let items = kuery_core::redis::known_commands()
            .into_iter()
            .filter(|name| {
                name.len() > typed.len()
                    && name
                        .get(..typed.len())
                        .is_some_and(|head| head.eq_ignore_ascii_case(typed))
            })
            .map(|name| CompletionItem {
                kind: Some(CompletionItemKind::FUNCTION),
                filter_text: Some(name[..typed.len()].to_string()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: format!("{name} "),
                })),
                label: name.to_string(),
                ..CompletionItem::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _: usize, _: &str, _: &mut App) -> bool {
        true
    }
}

/// What a document console completes from: the collections of the
/// database it works in, and the fields of the documents it has shown and
/// of the schemas sampled, by collection. The console fills it; the
/// provider reads it.
#[derive(Debug, Default)]
pub struct DocumentNames {
    pub collections: Vec<String>,
    pub fields: HashMap<String, BTreeSet<String>>,
    /// Collections whose schema completion asked for, so it asks once.
    pub asked: HashSet<String>,
}

impl DocumentNames {
    /// Keeps paths a filter names fields by (`items.sku`), from a schema.
    pub fn learn_paths(&mut self, collection: &str, paths: impl IntoIterator<Item = String>) {
        self.fields
            .entry(collection.to_string())
            .or_default()
            .extend(paths);
    }

    /// Keeps the fields of documents a statement on `collection` returned:
    /// the top-level ones and the paths into nested documents.
    pub fn learn(&mut self, collection: &str, documents: &[Document]) {
        let known = self.fields.entry(collection.to_string()).or_default();
        for document in documents {
            field_paths(document, "", 0, known);
        }
    }
}

/// How deep into nested documents completion offers paths.
const FIELD_DEPTH: usize = 3;

fn field_paths(document: &Document, prefix: &str, depth: usize, out: &mut BTreeSet<String>) {
    for (name, value) in &document.0 {
        let path = format!("{prefix}{name}");
        let nested = match value {
            Value::Object(inner) => Some(inner),
            // `items.sku` finds documents whose items hold that sku.
            Value::Array(items) => items.iter().find_map(|item| match item {
                Value::Object(inner) => Some(inner),
                _ => None,
            }),
            _ => None,
        };
        if let Some(inner) = nested
            && depth + 1 < FIELD_DEPTH
        {
            field_paths(inner, &format!("{path}."), depth + 1, out);
        }
        out.insert(path);
    }
}

pub type SharedNames = Rc<RefCell<DocumentNames>>;

/// Asks for a collection's fields (its schema), which arrive later in the
/// names; called once a collection, outside the editor's update.
pub type WantFields = Rc<dyn Fn(String, &mut App)>;

/// Completes a document console from what its language reads at the
/// cursor (see [`Language::context`]).
pub struct DocumentCompletions {
    language: &'static Language,
    names: SharedNames,
    want_fields: Option<WantFields>,
}

impl DocumentCompletions {
    pub fn new(language: &'static Language, names: SharedNames) -> Self {
        Self {
            language,
            names,
            want_fields: None,
        }
    }

    /// Asks for the fields of a collection completion has none of the
    /// schema of yet.
    pub fn wanting_fields(mut self, want: WantFields) -> Self {
        self.want_fields = Some(want);
        self
    }
}

/// One suggestion: what the menu shows, what it inserts, and its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSuggestion {
    pub label: String,
    pub insert: String,
    pub kind: CompletionItemKind,
}

/// The suggestions for what the text before the cursor asks, and how many
/// bytes before the cursor they replace. Without a prefix, only when asked
/// (Ctrl+Space) or right after `db.`/`db.orders.`.
pub fn document_suggestions(
    language: &Language,
    names: &DocumentNames,
    context: &language::CompletionContext,
    invoked: bool,
) -> (usize, Vec<DocumentSuggestion>) {
    use language::CompletionContext as Context;
    let words = |prefix: &str,
                 candidates: &mut dyn Iterator<Item = (String, String)>,
                 kind: CompletionItemKind| {
        let typed = prefix.to_lowercase();
        candidates
            .filter(|(label, _)| {
                label.len() > prefix.len() && label.to_lowercase().starts_with(&typed)
            })
            .map(|(label, insert)| DocumentSuggestion {
                label,
                insert,
                kind,
            })
            .collect::<Vec<_>>()
    };
    let (prefix, suggestions) = match context {
        Context::Collection { prefix } => {
            let mut names = names
                .collections
                .iter()
                .map(|name| (name.clone(), (language.collection_ref)(name)));
            (
                prefix,
                words(prefix, &mut names, CompletionItemKind::STRUCT),
            )
        }
        Context::Method { prefix, .. } => {
            let mut methods = language
                .methods
                .iter()
                .map(|m| (m.to_string(), m.to_string()));
            (
                prefix,
                words(prefix, &mut methods, CompletionItemKind::METHOD),
            )
        }
        Context::Operator { prefix } => {
            let mut operators = language
                .operators
                .iter()
                .map(|o| (o.to_string(), o.to_string()));
            (
                prefix,
                words(prefix, &mut operators, CompletionItemKind::OPERATOR),
            )
        }
        Context::Stage { prefix } => {
            let mut stages = language
                .stages
                .iter()
                .map(|s| (s.to_string(), s.to_string()));
            (
                prefix,
                words(prefix, &mut stages, CompletionItemKind::KEYWORD),
            )
        }
        Context::Field { collection, prefix } => {
            if prefix.is_empty() && !invoked {
                return (0, Vec::new());
            }
            let fields: BTreeSet<&String> =
                match collection.as_ref().and_then(|c| names.fields.get(c)) {
                    Some(fields) => fields.iter().collect(),
                    None => names.fields.values().flatten().collect(),
                };
            let mut fields = fields
                .into_iter()
                .map(|field| (field.clone(), (language.key)(field)));
            (
                prefix,
                words(prefix, &mut fields, CompletionItemKind::FIELD),
            )
        }
        Context::Nothing => return (0, Vec::new()),
    };
    (prefix.len(), suggestions)
}

impl CompletionProvider for DocumentCompletions {
    fn completions(
        &self,
        rope: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let text = rope.to_string();
        let offset = offset.min(text.len());
        let invoked = trigger.trigger_kind == CompletionTriggerKind::INVOKED;
        let context = (self.language.context)(&text, offset);
        if let (
            language::CompletionContext::Field {
                collection: Some(collection),
                ..
            },
            Some(want),
        ) = (&context, &self.want_fields)
            && self.names.borrow_mut().asked.insert(collection.clone())
        {
            let (want, collection) = (want.clone(), collection.clone());
            cx.defer(move |cx| want(collection, cx));
        }
        let (typed, suggestions) =
            document_suggestions(self.language, &self.names.borrow(), &context, invoked);
        let range = lsp_types::Range {
            start: rope.offset_to_position(offset.saturating_sub(typed)),
            end: rope.offset_to_position(offset),
        };
        let items = suggestions
            .into_iter()
            .map(|suggestion| CompletionItem {
                kind: Some(suggestion.kind),
                filter_text: Some(
                    suggestion
                        .label
                        .get(..typed)
                        .unwrap_or_default()
                        .to_string(),
                ),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: suggestion.insert,
                })),
                label: suggestion.label,
                ..CompletionItem::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

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

#[cfg(test)]
mod tests {
    use kuery_core::document::language::CompletionContext as Context;

    use super::*;

    fn names() -> DocumentNames {
        let mut names = DocumentNames {
            collections: vec!["orders".into(), "order-lines".into(), "customers".into()],
            ..DocumentNames::default()
        };
        let order = kuery_mongo::literal::parse_document(
            r#"{_id: 1, status: "paid", total: 5, items: [{sku: "a", qty: 1}], ship: {city: "x"}}"#,
        )
        .unwrap();
        names.learn("orders", &[order]);
        let customer =
            kuery_mongo::literal::parse_document(r#"{_id: 1, "first name": "a"}"#).unwrap();
        names.learn("customers", &[customer]);
        names
    }

    fn labels(context: Context, invoked: bool) -> Vec<(String, String)> {
        document_suggestions(&kuery_mongo::LANGUAGE, &names(), &context, invoked)
            .1
            .into_iter()
            .map(|suggestion| (suggestion.label, suggestion.insert))
            .collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(label, insert)| (label.to_string(), insert.to_string()))
            .collect()
    }

    #[test]
    fn completes_collections_after_db() {
        assert_eq!(
            labels(
                Context::Collection {
                    prefix: "ord".into()
                },
                false
            ),
            pairs(&[
                ("orders", "orders"),
                ("order-lines", "getCollection(\"order-lines\")")
            ])
        );
        assert_eq!(
            labels(
                Context::Collection {
                    prefix: String::new()
                },
                false
            )
            .len(),
            3
        );
    }

    #[test]
    fn completes_methods_operators_and_stages() {
        let method = labels(
            Context::Method {
                collection: "orders".into(),
                prefix: "find".into(),
            },
            false,
        );
        assert!(
            method.contains(&("findOne".into(), "findOne".into())),
            "{method:?}"
        );
        assert!(!method.iter().any(|(label, _)| label == "find"));
        assert!(
            labels(
                Context::Operator {
                    prefix: "$i".into()
                },
                false
            )
            .contains(&("$in".into(), "$in".into()))
        );
        let stages = labels(
            Context::Stage {
                prefix: "$gr".into(),
            },
            false,
        );
        assert_eq!(
            stages,
            pairs(&[("$group", "$group"), ("$graphLookup", "$graphLookup")])
        );
    }

    #[test]
    fn completes_fields_the_console_has_seen() {
        let fields = labels(
            Context::Field {
                collection: Some("orders".into()),
                prefix: "s".into(),
            },
            false,
        );
        assert_eq!(
            fields,
            pairs(&[
                ("ship", "ship"),
                ("ship.city", "\"ship.city\""),
                ("status", "status")
            ])
        );
        let items = labels(
            Context::Field {
                collection: Some("orders".into()),
                prefix: "items.".into(),
            },
            false,
        );
        assert_eq!(
            items,
            pairs(&[
                ("items.qty", "\"items.qty\""),
                ("items.sku", "\"items.sku\"")
            ])
        );
        // A collection the console has not shown: every field it knows.
        let any = labels(
            Context::Field {
                collection: None,
                prefix: "fir".into(),
            },
            false,
        );
        assert_eq!(any, pairs(&[("first name", "\"first name\"")]));
        // Without a prefix, only when asked.
        let field = |invoked| {
            labels(
                Context::Field {
                    collection: Some("orders".into()),
                    prefix: String::new(),
                },
                invoked,
            )
        };
        assert!(field(false).is_empty());
        assert_eq!(field(true).len(), 8);
        assert!(labels(Context::Nothing, true).is_empty());
    }

    #[test]
    fn completes_the_paths_of_a_sampled_schema() {
        let mut names = names();
        let documents = [kuery_mongo::literal::parse_document(
            r#"{_id: 1, lines: [{sku: "a", extra: {deep: 1}}]}"#,
        )
        .unwrap()];
        let schema = kuery_core::document::schema::infer(&documents);
        names.learn_paths("orders", schema.query_paths());
        let fields = document_suggestions(
            &kuery_mongo::LANGUAGE,
            &names,
            &Context::Field {
                collection: Some("orders".into()),
                prefix: "lines".into(),
            },
            false,
        )
        .1
        .into_iter()
        .map(|suggestion| suggestion.label)
        .collect::<Vec<_>>();
        assert_eq!(fields, ["lines.extra", "lines.extra.deep", "lines.sku"]);
    }
}
