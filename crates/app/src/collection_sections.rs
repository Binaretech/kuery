//! What a collection's tab shows besides its documents: the schema they
//! hold, sampled, beside the one the engine declares, if it checks one;
//! and its indexes, with how often each is used where the user may know.
//! Indexes are created and dropped from the console, where the write
//! review sees them.

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px, relative,
};
use kuery_core::document::catalog::{DocIndex, IndexUsage};
use kuery_core::document::language::{Language, Style};
use kuery_core::document::schema::{FieldSchema, Sampled};
use kuery_core::document::value::{Document, Value};

use crate::theme::{self, color};
use crate::ui::{format_bytes, thousands, tooltip};

/// A field's presence: `100 %`, `40 %`, `<1 %` for a few documents.
pub fn presence(field: &FieldSchema, documents: usize) -> String {
    let share = field.presence(documents) * 100.0;
    match share {
        0.0 => "0 %".into(),
        share if share < 1.0 => "<1 %".into(),
        share => format!("{share:.0} %"),
    }
}

/// The schema sampled, field by field, beside the declared one.
pub fn schema_view(
    sampled: &Sampled,
    declared: Option<&Document>,
    language: &Language,
    mono: SharedString,
) -> AnyElement {
    let documents = sampled.schema.documents;
    let header = |text: String| {
        div()
            .px_3()
            .py_2()
            .flex_shrink_0()
            .text_size(px(11.5))
            .text_color(color(theme::TEXT_TERTIARY))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(text)
    };
    let rows = sampled.schema.fields.iter().map(|field| {
        let mixed = field.mixed();
        let share = field.presence(documents);
        h_flex()
            .h(px(26.))
            .px_3()
            .gap_3()
            .text_size(px(12.5))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(px(field.depth() as f32 * 14.))
                    .truncate()
                    .font_family(mono.clone())
                    .child(field.path.clone()),
            )
            .child(
                h_flex()
                    .w(px(110.))
                    .flex_shrink_0()
                    .gap_2()
                    .child(
                        div()
                            .w(px(50.))
                            .h(px(5.))
                            .rounded_full()
                            .bg(color(theme::HAIRLINE))
                            .child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .w(relative(share as f32))
                                    .bg(color(theme::ACCENT)),
                            ),
                    )
                    .child(
                        div()
                            .text_color(color(theme::TEXT_SECONDARY))
                            .child(presence(field, documents)),
                    ),
            )
            .child(
                div()
                    .w(px(260.))
                    .flex_shrink_0()
                    .truncate()
                    .text_color(color(if mixed {
                        theme::AMBER
                    } else {
                        theme::TEXT_SECONDARY
                    }))
                    .child(field.label()),
            )
            .child(div().w(px(44.)).flex_shrink_0().when(mixed, |cell| {
                cell.text_size(px(10.5))
                    .text_color(color(theme::AMBER))
                    .child("mixed")
            }))
    });
    let sampled_column = v_flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .child(header(sampled.label()))
        .child(
            div()
                .id("collection-schema-fields")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .when(sampled.schema.fields.is_empty(), |list| {
                    list.child(
                        div()
                            .p_3()
                            .text_color(color(theme::TEXT_TERTIARY))
                            .child("The collection holds no documents to sample"),
                    )
                })
                .children(rows),
        );
    let declared_column = v_flex()
        .w(px(360.))
        .h_full()
        .flex_shrink_0()
        .bg(color(theme::TOOLBAR))
        .border_l_1()
        .border_color(color(theme::HAIRLINE))
        .child(header("Declared".into()))
        .child(
            div()
                .id("collection-schema-declared")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p_3()
                .text_size(px(12.))
                .map(|body| match declared {
                    Some(validator) => body
                        .font_family(mono.clone())
                        .text_color(color(theme::TEXT))
                        .child((language.render)(
                            &Value::Object(validator.clone()),
                            Style::Pretty,
                        )),
                    None => body
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child("No validator: documents may hold any fields"),
                }),
        );
    h_flex()
        .size_full()
        .items_start()
        .child(sampled_column)
        .child(declared_column)
        .into_any_element()
}

/// What an index is besides its keys: `unique, TTL 1 h`, with a partial
/// index's filter as the language writes it.
pub fn properties(index: &DocIndex, render: fn(&Value, Style) -> String) -> String {
    let mut properties: Vec<String> = Vec::new();
    if index.unique {
        properties.push("unique".into());
    }
    if index.sparse {
        properties.push("sparse".into());
    }
    if let Some(filter) = &index.partial {
        properties.push(format!(
            "partial {}",
            render(&Value::Object(filter.clone()), Style::Compact)
        ));
    }
    if let Some(seconds) = index.ttl {
        properties.push(format!("TTL {}", ttl(seconds)));
    }
    if index.hidden {
        properties.push("hidden".into());
    }
    properties.join(", ")
}

/// `3600` → `1 h`.
fn ttl(seconds: u64) -> String {
    match seconds {
        0 => "0 s (at the date)".into(),
        s if s % 86_400 == 0 => format!("{} d", s / 86_400),
        s if s % 3_600 == 0 => format!("{} h", s / 3_600),
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

/// How often an index was used, as the Usage column says it.
pub fn usage(usage: Option<IndexUsage>) -> String {
    match usage {
        Some(usage) => match usage.operations {
            0 => "unused".into(),
            1 => "1 use".into(),
            n => format!("{} uses", thousands(n)),
        },
        None => "—".into(),
    }
}

/// The indexes, one a row.
pub fn index_view(indexes: &[DocIndex], language: &Language, mono: SharedString) -> AnyElement {
    let cell = |width: f32| div().w(px(width)).flex_shrink_0().truncate();
    let header = h_flex()
        .h(px(30.))
        .px_3()
        .gap_3()
        .flex_shrink_0()
        .text_size(px(11.5))
        .text_color(color(theme::TEXT_TERTIARY))
        .border_b_1()
        .border_color(color(theme::HAIRLINE))
        .child(cell(200.).child("Name"))
        .child(div().flex_1().min_w_0().child("Keys"))
        .child(cell(220.).child("Properties"))
        .child(cell(80.).text_right().child("Size"))
        .child(cell(90.).text_right().child("Usage"));
    let rows = indexes.iter().enumerate().map(|(row, index)| {
        let used_tip = match index.usage {
            Some(IndexUsage {
                since: Some(since), ..
            }) => format!(
                "Counted since {}, when the server last started",
                (language.render)(&Value::DateTime(since), Style::Compact)
            ),
            Some(_) => "Counted since the server last started".into(),
            None => "This user may not read how often indexes are used".into(),
        };
        h_flex()
            .h(px(28.))
            .px_3()
            .gap_3()
            .text_size(px(12.5))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                cell(200.)
                    .font_family(mono.clone())
                    .child(index.name.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(mono.clone())
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child((language.render)(
                        &Value::Object(index.keys.clone()),
                        Style::Compact,
                    )),
            )
            .child(
                cell(220.)
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(properties(index, language.render)),
            )
            .child(
                cell(80.)
                    .text_right()
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(index.size.map(format_bytes).unwrap_or_else(|| "—".into())),
            )
            .child(
                div()
                    .id(("collection-index-usage", row))
                    .w(px(90.))
                    .flex_shrink_0()
                    .text_right()
                    .text_color(color(match index.usage {
                        Some(IndexUsage { operations: 0, .. }) => theme::AMBER,
                        _ => theme::TEXT_SECONDARY,
                    }))
                    .child(usage(index.usage))
                    .tooltip(tooltip(used_tip)),
            )
    });
    v_flex()
        .size_full()
        .child(header)
        .child(
            div()
                .id("collection-indexes")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .when(indexes.is_empty(), |list| {
                    list.child(
                        div()
                            .p_3()
                            .text_color(color(theme::TEXT_TERTIARY))
                            .child("No indexes"),
                    )
                })
                .children(rows),
        )
        .child(
            div()
                .px_3()
                .py_2()
                .flex_shrink_0()
                .text_size(px(12.))
                .text_color(color(theme::TEXT_TERTIARY))
                .border_t_1()
                .border_color(color(theme::HAIRLINE))
                .child(
                    "Create and drop indexes from the console: they go through the write review.",
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use kuery_core::document::schema::infer;

    use super::*;

    fn index() -> DocIndex {
        DocIndex {
            name: "at_1".into(),
            keys: Document(vec![("at".into(), Value::Int32(1))]),
            unique: false,
            sparse: false,
            partial: None,
            ttl: None,
            hidden: false,
            size: None,
            usage: None,
        }
    }

    #[test]
    fn says_what_an_index_is_and_how_often_it_is_used() {
        let render = kuery_mongo::LANGUAGE.render;
        assert_eq!(properties(&index(), render), "");
        let every = DocIndex {
            unique: true,
            sparse: true,
            partial: Some(Document(vec![(
                "status".into(),
                Value::String("paid".into()),
            )])),
            ttl: Some(7_200),
            hidden: true,
            ..index()
        };
        assert_eq!(
            properties(&every, render),
            "unique, sparse, partial {status: \"paid\"}, TTL 2 h, hidden"
        );
        assert_eq!(ttl(86_400 * 3), "3 d");
        assert_eq!(ttl(90), "90 s");
        assert_eq!(usage(None), "—");
        let used = |operations| {
            usage(Some(IndexUsage {
                operations,
                since: None,
            }))
        };
        assert_eq!(
            (used(0), used(1), used(12_345)),
            ("unused".into(), "1 use".into(), "12,345 uses".into())
        );
    }

    #[test]
    fn says_how_many_documents_have_a_field() {
        let documents: Vec<Document> = (0..300)
            .map(|i| {
                let mut fields = vec![("a".to_string(), Value::Int32(i))];
                if i == 0 {
                    fields.push(("rare".into(), Value::Null));
                }
                if i % 3 == 0 {
                    fields.push(("third".into(), Value::Null));
                }
                Document(fields)
            })
            .collect();
        let schema = infer(&documents);
        let shown: Vec<String> = schema
            .fields
            .iter()
            .map(|field| presence(field, schema.documents))
            .collect();
        assert_eq!(shown, ["100 %", "<1 %", "33 %"]);
    }
}
