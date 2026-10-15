//! A table's "Definition" view: columns, indexes, constraints and triggers.

use dbadmin_core::catalog::{ConstraintKind, TableDefinition, TableInfo};
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

use crate::assets::AppIcon;
use crate::theme::{self, color};
use crate::ui::{format_bytes, group, group_row, muted, tag, thousands};

pub fn render(
    info: &TableInfo,
    definition: &TableDefinition,
    mono: gpui_kit::SharedString,
) -> AnyElement {
    let code = |text: String| {
        div()
            .font_family(mono.clone())
            .text_size(px(11.5))
            .text_color(color(theme::TEXT_SECONDARY))
            .child(text)
    };

    let summary = h_flex()
        .gap_4()
        .flex_wrap()
        .child(summary_item("Type", info.kind.label().to_string()))
        .child(summary_item(
            "Rows (estimate)",
            info.estimated_rows
                .map(thousands)
                .unwrap_or_else(|| "not analyzed".into()),
        ))
        .child(summary_item("Primary key", {
            let pk = definition.primary_key();
            if pk.is_empty() {
                "None".into()
            } else {
                pk.join(", ")
            }
        }));

    let mut columns = v_flex().child(
        header_row()
            .child(cell(200.).child("Name"))
            .child(cell(170.).child("Type"))
            .child(cell(190.).child("Default"))
            .child(cell(90.).child("Nullable"))
            .child(div().flex_1().child("References")),
    );
    for column in &definition.columns {
        columns = columns.child(
            group_row()
                .child(
                    h_flex()
                        .w(px(200.))
                        .gap_1()
                        .children(column.primary_key.then(|| {
                            Icon::new(AppIcon::Key)
                                .size(px(12.))
                                .text_color(color(theme::AMBER))
                        }))
                        .children(column.foreign_key.is_some().then(|| {
                            Icon::new(AppIcon::Link)
                                .size(px(12.))
                                .text_color(color(0xc49bff))
                        }))
                        .child(
                            div()
                                .font_family(mono.clone())
                                .text_size(px(12.))
                                .child(column.name.clone()),
                        ),
                )
                .child(
                    cell(170.)
                        .font_family(mono.clone())
                        .text_size(px(12.))
                        .text_color(color(0x5dd8ff))
                        .child(column.data_type.clone()),
                )
                .child(
                    cell(190.)
                        .font_family(mono.clone())
                        .text_size(px(12.))
                        .text_color(color(0xff8170))
                        .truncate()
                        .child(if column.generated {
                            "generated".to_string()
                        } else {
                            column.default.clone().unwrap_or_default()
                        }),
                )
                .child(cell(90.).child(if column.nullable {
                    muted("NULL")
                } else {
                    tag("NOT NULL", theme::TEXT_SECONDARY)
                }))
                .child(
                    div()
                        .flex_1()
                        .font_family(mono.clone())
                        .text_size(px(12.))
                        .text_color(color(0x4da3ff))
                        .child(
                            column
                                .foreign_key
                                .as_ref()
                                .map(|fk| format!("→ {}.{}", fk.table, fk.column))
                                .unwrap_or_default(),
                        ),
                ),
        );
    }

    let mut indexes = v_flex();
    for index in &definition.indexes {
        indexes = indexes.child(
            group_row()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .font_family(mono.clone())
                                        .text_size(px(12.))
                                        .child(index.name.clone()),
                                )
                                .children(index.primary.then(|| tag("PRIMARY", theme::AMBER)))
                                .children(
                                    (index.unique && !index.primary)
                                        .then(|| tag("UNIQUE", 0x4da3ff)),
                                ),
                        )
                        .child(code(index.definition.clone())),
                )
                .child(muted(
                    index.size_bytes.map(format_bytes).unwrap_or_default(),
                )),
        );
    }

    let mut constraints = v_flex();
    for constraint in &definition.constraints {
        let tone = match constraint.kind {
            ConstraintKind::PrimaryKey => theme::AMBER,
            ConstraintKind::ForeignKey => 0xc49bff,
            ConstraintKind::Unique => 0x4da3ff,
            ConstraintKind::Check => theme::GREEN,
            _ => theme::TEXT_SECONDARY,
        };
        constraints = constraints.child(
            group_row()
                .child(div().w(px(110.)).child(tag(constraint.kind.label(), tone)))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            div()
                                .font_family(mono.clone())
                                .text_size(px(12.))
                                .child(constraint.name.clone()),
                        )
                        .child(code(constraint.definition.clone())),
                ),
        );
    }

    let mut triggers = v_flex();
    for trigger in &definition.triggers {
        triggers = triggers.child(
            group_row()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            div()
                                .font_family(mono.clone())
                                .text_size(px(12.))
                                .child(trigger.name.clone()),
                        )
                        .child(code(trigger.definition.clone())),
                )
                .child(tag(
                    if trigger.enabled {
                        "ENABLED"
                    } else {
                        "DISABLED"
                    },
                    if trigger.enabled {
                        theme::GREEN
                    } else {
                        theme::TEXT_TERTIARY
                    },
                )),
        );
    }

    let empty = |text: &'static str| group_row().child(muted(text));

    div()
        .id("definition")
        .size_full()
        .overflow_y_scroll()
        .child(
            v_flex()
                .p_6()
                .gap_6()
                .max_w(px(1100.))
                .children(info.comment.clone().map(muted))
                .child(summary)
                .child(group(
                    "Columns",
                    Some(definition.columns.len().to_string()),
                    columns,
                ))
                .child(group(
                    "Indexes",
                    Some(definition.indexes.len().to_string()),
                    if definition.indexes.is_empty() {
                        empty("No indexes").into_any_element()
                    } else {
                        indexes.into_any_element()
                    },
                ))
                .child(group(
                    "Constraints",
                    Some(definition.constraints.len().to_string()),
                    if definition.constraints.is_empty() {
                        empty("No constraints").into_any_element()
                    } else {
                        constraints.into_any_element()
                    },
                ))
                .child(group(
                    "Triggers",
                    Some(definition.triggers.len().to_string()),
                    if definition.triggers.is_empty() {
                        empty("No triggers").into_any_element()
                    } else {
                        triggers.into_any_element()
                    },
                )),
        )
        .into_any_element()
}

fn summary_item(label: &'static str, value: String) -> Div {
    v_flex()
        .gap(px(2.))
        .child(
            div()
                .text_size(px(11.))
                .text_color(color(theme::TEXT_TERTIARY))
                .child(label),
        )
        .child(div().font_weight(FontWeight::MEDIUM).child(value))
}

fn header_row() -> Div {
    h_flex()
        .h(px(28.))
        .px_3()
        .gap_3()
        .bg(color(theme::TOOLBAR))
        .border_b_1()
        .border_color(color(theme::HAIRLINE))
        .text_size(px(11.5))
        .text_color(color(theme::TEXT_SECONDARY))
}

fn cell(width: f32) -> Div {
    div().w(px(width)).flex_shrink_0()
}
