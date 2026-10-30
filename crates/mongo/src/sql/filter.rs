//! MongoDB filters as the lowering builds them, and how they print: each
//! condition on its own until the end, then merged into as few documents
//! as say the same, so `{status: {$not: {$type: "array"}, $eq: "paid"}}`
//! reads like a filter written by hand.

use kuery_core::document::value::{Document, Value};

/// A filter of `$match`.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Filter {
    /// Every document.
    All,
    /// No document.
    None,
    And(Vec<Filter>),
    Or(Vec<Filter>),
    /// `{path: {operator: value}}`.
    Is {
        path: String,
        operator: &'static str,
        value: Value,
    },
}

impl Filter {
    pub(super) fn is(path: &str, operator: &'static str, value: Value) -> Filter {
        Filter::Is {
            path: path.to_string(),
            operator,
            value,
        }
    }

    /// Every one of `filters`.
    pub(super) fn and(filters: impl IntoIterator<Item = Filter>) -> Filter {
        let mut all = Vec::new();
        for filter in filters {
            match filter {
                Filter::All => {}
                Filter::None => return Filter::None,
                Filter::And(inner) => all.extend(inner),
                filter => all.push(filter),
            }
        }
        let mut unique: Vec<Filter> = Vec::with_capacity(all.len());
        for filter in all {
            if !unique.contains(&filter) {
                unique.push(filter);
            }
        }
        match unique.len() {
            0 => Filter::All,
            1 => unique.pop().unwrap(),
            _ => Filter::And(unique),
        }
    }

    /// Any one of `filters`.
    pub(super) fn or(filters: impl IntoIterator<Item = Filter>) -> Filter {
        let mut any = Vec::new();
        for filter in filters {
            match filter {
                Filter::None => {}
                Filter::All => return Filter::All,
                Filter::Or(inner) => any.extend(inner),
                filter => any.push(filter),
            }
        }
        let mut unique: Vec<Filter> = Vec::with_capacity(any.len());
        for filter in any {
            if !unique.contains(&filter) {
                unique.push(filter);
            }
        }
        match unique.len() {
            0 => Filter::None,
            1 => unique.pop().unwrap(),
            _ => Filter::Or(unique),
        }
    }

    /// The filter document.
    pub(super) fn document(&self) -> Document {
        match self {
            Filter::All => Document::default(),
            // `$expr` takes an expression, and `false` is one no document
            // meets.
            Filter::None => doc([("$expr", Value::Bool(false))]),
            Filter::Is {
                path,
                operator,
                value,
            } => doc([(
                path.as_str(),
                Value::Object(doc([(*operator, value.clone())])),
            )]),
            Filter::Or(filters) => doc([(
                "$or",
                Value::Array(
                    filters
                        .iter()
                        .map(|filter| Value::Object(filter.document()))
                        .collect(),
                ),
            )]),
            Filter::And(filters) => {
                let mut documents = merged(filters);
                match documents.len() {
                    1 => documents.pop().unwrap(),
                    _ => doc([(
                        "$and",
                        Value::Array(documents.into_iter().map(Value::Object).collect()),
                    )]),
                }
            }
        }
    }
}

/// The conditions in as few documents as hold them, each document taking
/// every condition it has no key for yet: one operator per path, and one
/// `$or`. More than one only when a path takes an operator twice
/// (`$not`, say).
fn merged(filters: &[Filter]) -> Vec<Document> {
    let mut documents: Vec<Document> = Vec::new();
    for filter in filters {
        if !documents.iter_mut().any(|document| merge(document, filter)) {
            let mut document = Document::default();
            if !merge(&mut document, filter) {
                document = filter.document();
            }
            documents.push(document);
        }
    }
    documents
}

/// Adds a condition to a filter document, unless it has its key already.
fn merge(document: &mut Document, filter: &Filter) -> bool {
    match filter {
        Filter::Is {
            path,
            operator,
            value,
        } => match document.0.iter_mut().find(|(name, _)| name == path) {
            Some((_, Value::Object(operators))) if operators.get(operator).is_none() => {
                operators.0.push((operator.to_string(), value.clone()));
                true
            }
            Some(_) => false,
            None => {
                document.0.push((
                    path.clone(),
                    Value::Object(doc([(*operator, value.clone())])),
                ));
                true
            }
        },
        Filter::Or(_) if document.get("$or").is_none() => {
            document.0.extend(filter.document().0);
            true
        }
        _ => false,
    }
}

pub(super) fn doc<'a>(fields: impl IntoIterator<Item = (&'a str, Value)>) -> Document {
    Document(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
    )
}
