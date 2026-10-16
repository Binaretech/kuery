//! What the Profile tab draws for an analysis: histograms, timelines, value
//! lists, group box plots, cross tabs and stacked periods. Plain elements
//! built from computed results; the options around them live in
//! `profile_view`.

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px, relative,
};
use kuery_core::compare::{Breakdown, Correlation, CrossTab, GroupTable, Key, Series};
use kuery_core::stats::{Bin, CategoricalSummary, NumericSummary, TemporalSummary, TextSummary};

use crate::assets::AppIcon;
use crate::theme::{self, color};
use crate::ui::thousands;

pub const CHART_HEIGHT: f32 = 150.;

/// Series colors for stacked charts; the last is for "Other".
const PALETTE: [u32; 9] = [
    0x2f86ff, 0x32d74b, 0xffb340, 0xbf5af2, 0x64d2ff, 0xff6482, 0xac8e68, 0x5e5ce6, 0x5a5a61,
];

/// One bar: its axis label, its height, and its tooltip.
pub struct Bar {
    pub label: String,
    pub value: f64,
    pub tip: String,
}

impl Bar {
    fn count(label: String, count: usize) -> Self {
        Bar {
            tip: format!("{label} · {}", thousands(count as u64)),
            label,
            value: count as f64,
        }
    }
}

pub fn share(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64 * 100.0
    }
}

/// A chart's frame.
pub fn panel() -> Div {
    v_flex()
        .p(px(14.))
        .gap(px(10.))
        .rounded(px(10.))
        .bg(color(theme::SIDEBAR))
        .border_1()
        .border_color(color(theme::HAIRLINE))
}

/// A quiet line under or inside a chart.
pub fn footnote(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(11.5))
        .text_color(color(theme::TEXT_TERTIARY))
        .child(text.into())
}

/// A chart's title line, with a quiet note on the right.
fn heading(title: impl Into<SharedString>, note: impl Into<SharedString>) -> impl IntoElement {
    h_flex()
        .justify_between()
        .gap_4()
        .text_size(px(12.))
        .child(
            div()
                .text_color(color(theme::TEXT_SECONDARY))
                .child(title.into()),
        )
        .child(
            div()
                .text_color(color(theme::TEXT_TERTIARY))
                .child(note.into()),
        )
}

/// A styled empty state.
pub fn message(text: impl Into<SharedString>) -> Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(color(theme::TEXT_TERTIARY))
        .child(text.into())
}

/// Bars scaled to the tallest, each with a tooltip; labels sit under them
/// (all of them when there are few bars, else the first and last).
pub fn bars(
    id: &'static str,
    bars: Vec<Bar>,
    height: f32,
    log_scale: bool,
    mono: SharedString,
) -> impl IntoElement {
    let scale = |value: f64| {
        let value = value.max(0.0);
        if log_scale { value.ln_1p() } else { value }
    };
    let tallest = bars
        .iter()
        .map(|bar| scale(bar.value))
        .fold(0.0, f64::max)
        .max(1e-9);
    let few = bars.len() <= 12;
    let axis: Vec<String> = if few {
        bars.iter().map(|bar| bar.label.clone()).collect()
    } else {
        [bars.first(), bars.last()]
            .into_iter()
            .map(|bar| bar.map(|b| b.label.clone()).unwrap_or_default())
            .collect()
    };
    let gap = if bars.len() > 60 { 0. } else { 2. };
    v_flex()
        .gap(px(6.))
        .child(
            h_flex()
                .relative()
                .h(px(height))
                .items_end()
                .gap(px(gap))
                .children(bars.into_iter().enumerate().map(|(index, bar)| {
                    let share = (scale(bar.value) / tallest) as f32;
                    let tip = bar.tip;
                    div()
                        .id((id, index))
                        .flex_1()
                        .h_full()
                        .flex()
                        .items_end()
                        .child(
                            div()
                                .w_full()
                                .h(relative(if bar.value > 0.0 {
                                    share.max(0.02)
                                } else {
                                    0.0
                                }))
                                .rounded_t(px(2.))
                                .bg(color(theme::ACCENT).opacity(0.7)),
                        )
                        .hover(|s| s.bg(color(theme::ACTIVE)))
                        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                }))
                .child(baseline()),
        )
        .child(axis_labels(axis, few, gap, mono))
}

fn baseline() -> impl IntoElement {
    div()
        .absolute()
        .bottom_0()
        .left_0()
        .right_0()
        .h(px(1.))
        .bg(color(theme::HAIRLINE))
}

fn axis_labels(labels: Vec<String>, few: bool, gap: f32, mono: SharedString) -> impl IntoElement {
    h_flex()
        .gap(px(gap))
        .justify_between()
        .font_family(mono)
        .text_size(px(11.))
        .text_color(color(theme::TEXT_TERTIARY))
        .children(labels.into_iter().map(move |label| {
            div()
                .when(few, |d| d.flex_1().text_center())
                .truncate()
                .child(label)
        }))
}

pub fn bin_label(bin: &Bin, integers: bool) -> String {
    if integers && bin.end - bin.start == 1.0 || bin.start == bin.end {
        format_number(bin.start)
    } else {
        format!("{} – {}", format_number(bin.start), format_number(bin.end))
    }
}

fn stat_row(label: &'static str, value: String, mono: SharedString) -> impl IntoElement {
    h_flex()
        .justify_between()
        .gap_4()
        .child(div().text_color(color(theme::TEXT_TERTIARY)).child(label))
        .child(div().font_family(mono).child(value))
}

pub fn numeric(numbers: &NumericSummary, log_scale: bool, mono: SharedString) -> AnyElement {
    let integers = numbers
        .histogram
        .iter()
        .all(|bin| bin.start.fract() == 0.0 && bin.end - bin.start == 1.0);
    let histogram = numbers
        .histogram
        .iter()
        .map(|bin| Bar::count(bin_label(bin, integers), bin.count))
        .collect();
    let row = |label, value| stat_row(label, format_number(value), mono.clone());
    let mut chart = panel().child(bars(
        "profile-bin",
        histogram,
        CHART_HEIGHT,
        log_scale,
        mono.clone(),
    ));
    if !integers {
        chart = chart.child(
            h_flex()
                .justify_between()
                .font_family(mono.clone())
                .text_size(px(11.))
                .text_color(color(theme::TEXT_QUIET))
                .children(numbers.histogram.first().map(|b| format_number(b.start)))
                .children(numbers.histogram.last().map(|b| format_number(b.end))),
        );
    }
    v_flex()
        .gap(px(14.))
        .child(chart)
        .child(
            div()
                .grid()
                .grid_cols(3)
                .gap_x(px(28.))
                .gap_y(px(6.))
                .text_size(px(12.5))
                .child(row("min", numbers.min))
                .child(row("mean", numbers.mean))
                .child(row("max", numbers.max))
                .child(row("p25", numbers.p25))
                .child(row("median", numbers.p50))
                .child(row("p75", numbers.p75))
                .child(row("p90", numbers.p90))
                .child(row("p99", numbers.p99))
                .child(row("std dev", numbers.stddev)),
        )
        .into_any_element()
}

pub fn temporal(timeline: &TemporalSummary, mono: SharedString) -> AnyElement {
    let buckets = timeline
        .buckets
        .iter()
        .map(|(label, count)| Bar::count(label.clone(), *count))
        .collect();
    panel()
        .child(bars(
            "profile-bucket",
            buckets,
            CHART_HEIGHT,
            false,
            mono.clone(),
        ))
        .child(
            h_flex()
                .gap_2()
                .text_size(px(12.))
                .text_color(color(theme::TEXT_TERTIARY))
                .child("Range")
                .child(
                    div()
                        .font_family(mono)
                        .text_color(color(theme::TEXT_SECONDARY))
                        .child(format!("{} → {}", timeline.min, timeline.max)),
                ),
        )
        .into_any_element()
}

/// Values with proportional bars; `None` is NULL.
fn value_bars(
    top: &[(Option<String>, usize)],
    total: usize,
    mono: SharedString,
) -> impl IntoElement {
    let largest = top.iter().map(|(_, c)| *c).max().unwrap_or(1).max(1) as f32;
    v_flex()
        .gap(px(4.))
        .text_size(px(12.5))
        .children(top.iter().map(|(value, count)| {
            let quiet = value.as_ref().is_none_or(|v| v.is_empty());
            let text = match value {
                None => "NULL".to_string(),
                Some(v) if v.is_empty() => "(empty)".to_string(),
                Some(v) => v.clone(),
            };
            h_flex()
                .relative()
                .h(px(26.))
                .px(px(10.))
                .gap_3()
                .rounded(px(5.))
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .h_full()
                        .w(relative(*count as f32 / largest))
                        .bg(color(theme::ACCENT).opacity(0.2)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(mono.clone())
                        .when(quiet, |d| {
                            d.italic().text_color(color(theme::TEXT_TERTIARY))
                        })
                        .child(text),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(color(theme::TEXT_SECONDARY))
                        .child(format!(
                            "{} · {:.1}%",
                            thousands(*count as u64),
                            share(*count, total)
                        )),
                )
        }))
}

pub fn categorical(categories: &CategoricalSummary, mono: SharedString) -> AnyElement {
    panel()
        .child(value_bars(&categories.top, categories.total, mono))
        .when(categories.others > 0, |panel| {
            panel.child(footnote(format!(
                "+ {} in other values · {:.1}%",
                thousands(categories.others as u64),
                share(categories.others, categories.total)
            )))
        })
        .into_any_element()
}

pub fn text(summary: &TextSummary, mono: SharedString) -> AnyElement {
    let total: usize = summary.lengths.iter().map(|b| b.count).sum();
    let lengths = summary
        .lengths
        .iter()
        .map(|bin| Bar::count(format!("{} chars", bin_label(bin, true)), bin.count))
        .collect();
    let top: Vec<(Option<String>, usize)> = summary
        .top
        .iter()
        .map(|(value, count)| (Some(value.clone()), *count))
        .collect();
    let blank = if summary.blank > 0 {
        format!(" · {} blank", thousands(summary.blank as u64))
    } else {
        String::new()
    };
    v_flex()
        .gap(px(14.))
        .child(
            panel()
                .child(heading(
                    "Length",
                    format!(
                        "{}–{} chars · mean {:.1}{blank}",
                        summary.min_length, summary.max_length, summary.mean_length
                    ),
                ))
                .child(bars("profile-length", lengths, 80., false, mono.clone())),
        )
        .child(
            panel()
                .child(heading("Most frequent", ""))
                .child(value_bars(&top, total, mono)),
        )
        .into_any_element()
}

pub fn boolean(trues: usize, falses: usize, nulls: usize) -> AnyElement {
    let total = (trues + falses + nulls).max(1);
    let parts = [
        ("true", trues, color(theme::ACCENT)),
        ("false", falses, color(0x5a5a61)),
        ("NULL", nulls, color(theme::HAIRLINE)),
    ];
    panel()
        .child(
            h_flex()
                .h(px(28.))
                .rounded(px(6.))
                .overflow_hidden()
                .children(parts.iter().filter(|(_, count, _)| *count > 0).map(
                    |(_, count, tone)| {
                        div()
                            .h_full()
                            .w(relative(*count as f32 / total as f32))
                            .bg(*tone)
                    },
                )),
        )
        .child(legend(parts.into_iter().map(|(label, count, tone)| {
            (
                label.to_string(),
                format!("{} · {:.1}%", thousands(count as u64), share(count, total)),
                tone,
            )
        })))
        .into_any_element()
}

fn legend(entries: impl Iterator<Item = (String, String, Hsla)>) -> impl IntoElement {
    h_flex()
        .flex_wrap()
        .gap_x(px(18.))
        .gap_y(px(6.))
        .text_size(px(12.5))
        .children(entries.map(|(label, note, tone)| {
            h_flex()
                .gap(px(6.))
                .child(div().size(px(8.)).rounded(px(2.)).bg(tone))
                .child(label)
                .child(div().text_color(color(theme::TEXT_TERTIARY)).child(note))
        }))
}

pub fn key(min: &str, max: &str, mono: SharedString) -> AnyElement {
    let value = |text: &str| {
        div()
            .font_family(mono.clone())
            .text_color(color(theme::TEXT))
            .child(text.to_string())
    };
    panel()
        .child(
            h_flex()
                .gap(px(8.))
                .text_color(color(theme::TEXT_SECONDARY))
                .child(Icon::new(AppIcon::Key).size(px(14.)))
                .child("Every value is different: this looks like a key."),
        )
        .child(
            h_flex()
                .gap_2()
                .text_size(px(12.5))
                .text_color(color(theme::TEXT_TERTIARY))
                .child("From")
                .child(value(min))
                .child("to")
                .child(value(max)),
        )
        .child(footnote(
            "Choose Numeric, Categorical or Text to look at it another way.",
        ))
        .into_any_element()
}

/// How strong an association of 0 to 1 is, in words.
fn strength(value: f64) -> &'static str {
    match value.abs() {
        v if v < 0.1 => "negligible",
        v if v < 0.3 => "weak",
        v if v < 0.5 => "moderate",
        v if v < 0.7 => "strong",
        _ => "very strong",
    }
}

/// A measure by group: a box plot per group on a shared scale, then the
/// numbers.
pub fn groups(table: &GroupTable, measure: &str, group: &str, mono: SharedString) -> AnyElement {
    let (low, high) = (table.overall.min, table.overall.max);
    let span = (high - low).max(1e-9);
    let at = |value: f64| ((value - low) / span) as f32;
    let header = |text: &'static str| {
        div()
            .text_size(px(11.))
            .text_color(color(theme::TEXT_TERTIARY))
            .child(text)
    };
    let number = |value: f64| {
        div()
            .text_right()
            .font_family(mono.clone())
            .child(format_number(value))
    };
    let mut grid = div()
        .grid()
        .grid_cols(7)
        .gap_x(px(14.))
        .gap_y(px(8.))
        .items_center()
        .text_size(px(12.5))
        .child(header(""))
        .child(header("distribution"))
        .child(header("n"))
        .child(header("mean"))
        .child(header("median"))
        .child(header("min"))
        .child(header("max"));
    for (index, stats) in table.groups.iter().enumerate() {
        let tip = format!(
            "{} · p25 {} · median {} · p75 {}",
            stats.key.text(),
            format_number(stats.p25),
            format_number(stats.median),
            format_number(stats.p75)
        );
        grid = grid
            .child(key_label(&stats.key, mono.clone()))
            .child(
                div()
                    .id(("profile-box", index))
                    .relative()
                    .h(px(18.))
                    .min_w(px(160.))
                    .child(
                        // Whiskers from min to max.
                        div()
                            .absolute()
                            .top(px(8.5))
                            .h(px(1.))
                            .left(relative(at(stats.min)))
                            .w(relative((at(stats.max) - at(stats.min)).max(0.002)))
                            .bg(color(theme::TEXT_QUIET)),
                    )
                    .child(
                        // The box from p25 to p75.
                        div()
                            .absolute()
                            .top(px(3.))
                            .h(px(12.))
                            .left(relative(at(stats.p25)))
                            .w(relative((at(stats.p75) - at(stats.p25)).max(0.004)))
                            .rounded(px(2.))
                            .bg(color(theme::ACCENT).opacity(0.45))
                            .border_1()
                            .border_color(color(theme::ACCENT)),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(1.))
                            .h(px(16.))
                            .w(px(2.))
                            .left(relative(at(stats.median)))
                            .bg(color(theme::TEXT)),
                    )
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)),
            )
            .child(
                div()
                    .text_right()
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(thousands(stats.n as u64)),
            )
            .child(number(stats.mean))
            .child(number(stats.median))
            .child(number(stats.min))
            .child(number(stats.max));
    }
    let folded = table.groups.last().is_some_and(|g| g.key == Key::Other);
    panel()
        .child(heading(
            format!("{measure} by {group}"),
            format!(
                "η² {:.2} · {group} explains {:.0}% of the variance",
                table.eta_squared,
                table.eta_squared * 100.0
            ),
        ))
        .child(div().id("profile-groups").overflow_x_scroll().child(grid))
        .child(
            h_flex()
                .justify_between()
                .font_family(mono.clone())
                .text_size(px(11.))
                .text_color(color(theme::TEXT_QUIET))
                .child(format!(
                    "scale {} → {}",
                    format_number(low),
                    format_number(high)
                )),
        )
        .when(folded, |panel| {
            panel.child(footnote(format!(
                "{} groups in all; the smaller ones are folded into Other.",
                thousands(table.distinct as u64)
            )))
        })
        .into_any_element()
}

fn key_label(key: &Key, mono: SharedString) -> impl IntoElement {
    let quiet = !matches!(key, Key::Value(v) if !v.is_empty());
    let text = match key {
        Key::Value(v) if v.is_empty() => "(empty)",
        key => key.text(),
    };
    div()
        .min_w(px(60.))
        .max_w(px(220.))
        .truncate()
        .font_family(mono)
        .when(quiet, |d| {
            d.italic().text_color(color(theme::TEXT_TERTIARY))
        })
        .child(text.to_string())
}

/// Counts per pair of values, shaded by size; `percent` shades and labels
/// each row by its own total.
pub fn cross_tab(
    table: &CrossTab,
    rows_name: &str,
    columns_name: &str,
    percent: bool,
    mono: SharedString,
) -> AnyElement {
    let largest = table
        .counts
        .iter()
        .flatten()
        .copied()
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let cell_text = |count: usize, row_total: usize| {
        if percent {
            format!("{:.0}%", share(count, row_total))
        } else {
            thousands(count as u64)
        }
    };
    let cell = |text: String, shade: f64, strong: bool| {
        div()
            .h(px(28.))
            .px(px(8.))
            .flex()
            .items_center()
            .justify_end()
            .rounded(px(4.))
            .bg(color(theme::ACCENT).opacity((shade * 0.75) as f32))
            .text_color(color(if strong || shade > 0.45 {
                theme::TEXT
            } else {
                theme::TEXT_SECONDARY
            }))
            .child(text)
    };
    let mut grid = div()
        .grid()
        .grid_cols(table.columns.len() as u16 + 2)
        .gap(px(3.))
        .text_size(px(12.))
        .font_family(mono.clone())
        .child(div())
        .children(table.columns.iter().map(|column| {
            div()
                .px(px(8.))
                .text_right()
                .truncate()
                .text_color(color(theme::TEXT_TERTIARY))
                .child(column.text().to_string())
        }))
        .child(
            div()
                .px(px(8.))
                .text_right()
                .text_color(color(theme::TEXT_QUIET))
                .child("total"),
        );
    for (r, key) in table.rows.iter().enumerate() {
        let row_total = table.row_totals[r];
        grid = grid
            .child(
                div()
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .child(key_label(key, mono.clone())),
            )
            .children(table.counts[r].iter().map(|count| {
                let shade = if percent {
                    *count as f64 / row_total.max(1) as f64
                } else {
                    *count as f64 / largest
                };
                cell(cell_text(*count, row_total), shade, false)
            }))
            .child(
                div()
                    .h(px(28.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .justify_end()
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(thousands(row_total as u64)),
            );
    }
    grid = grid
        .child(
            div()
                .text_color(color(theme::TEXT_QUIET))
                .font_family(mono.clone())
                .child("total"),
        )
        .children(table.column_totals.iter().map(|total| {
            div()
                .px(px(8.))
                .text_right()
                .text_color(color(theme::TEXT_TERTIARY))
                .child(thousands(*total as u64))
        }))
        .child(
            div()
                .px(px(8.))
                .text_right()
                .text_color(color(theme::TEXT_SECONDARY))
                .child(thousands(table.total as u64)),
        );
    let association = match table.cramers_v {
        Some(v) => format!("Cramér's V {v:.2} · {} association", strength(v)),
        None => "One of the columns has a single value".to_string(),
    };
    panel()
        .child(heading(
            format!("{rows_name} × {columns_name}"),
            association,
        ))
        .child(
            div()
                .id("profile-cross-tab")
                .overflow_x_scroll()
                .child(grid),
        )
        .into_any_element()
}

/// A measure aggregated per period.
pub fn series(series: &Series, measure: &str, mono: SharedString) -> AnyElement {
    let aggregate = series.aggregate.label();
    let points = series
        .points
        .iter()
        .map(|point| Bar {
            tip: match point.value {
                Some(value) => format!(
                    "{} · {aggregate} {} · {} rows",
                    point.label,
                    format_number(value),
                    thousands(point.n as u64)
                ),
                None => format!("{} · no values", point.label),
            },
            label: point.label.clone(),
            value: point.value.unwrap_or(0.0),
        })
        .collect();
    let negative = series
        .points
        .iter()
        .any(|p| p.value.is_some_and(|v| v < 0.0));
    panel()
        .child(heading(
            format!(
                "{aggregate}({measure}) per {}",
                series.granularity.label().to_lowercase()
            ),
            format!("{} periods", thousands(series.points.len() as u64)),
        ))
        .child(bars("profile-series", points, CHART_HEIGHT, false, mono))
        .when(negative, |panel| {
            panel.child(footnote("Negative values are drawn as empty bars."))
        })
        .into_any_element()
}

/// How a category splits within each period, as stacked bars.
pub fn breakdown(
    breakdown: &Breakdown,
    category: &str,
    percent: bool,
    mono: SharedString,
) -> AnyElement {
    let totals: Vec<usize> = breakdown.counts.iter().map(|c| c.iter().sum()).collect();
    let tallest = totals.iter().copied().max().unwrap_or(1).max(1) as f32;
    let tone = |index: usize, key: &Key| {
        if *key == Key::Other {
            color(PALETTE[PALETTE.len() - 1])
        } else {
            color(PALETTE[index % (PALETTE.len() - 1)])
        }
    };
    let few = breakdown.periods.len() <= 12;
    let gap = if breakdown.periods.len() > 60 { 0. } else { 2. };
    let columns =
        breakdown
            .counts
            .iter()
            .zip(&totals)
            .enumerate()
            .map(|(index, (counts, total))| {
                let height = if percent {
                    if *total > 0 { 1.0 } else { 0.0 }
                } else {
                    *total as f32 / tallest
                };
                let mut tip = format!(
                    "{} · {}",
                    breakdown.periods[index],
                    thousands(*total as u64)
                );
                for (key, count) in breakdown.categories.iter().zip(counts) {
                    if *count > 0 {
                        tip.push_str(&format!(
                            "\n{} {} · {:.0}%",
                            key.text(),
                            thousands(*count as u64),
                            share(*count, *total)
                        ));
                    }
                }
                div()
                    .id(("profile-stack", index))
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_end()
                    .hover(|s| s.bg(color(theme::ACTIVE)))
                    .child(
                        v_flex()
                            .w_full()
                            .h(relative(height))
                            .flex_col_reverse()
                            .children(
                                breakdown
                                    .categories
                                    .iter()
                                    .zip(counts)
                                    .enumerate()
                                    .filter(|(_, (_, count))| **count > 0)
                                    .map(|(c, (key, count))| {
                                        div()
                                            .w_full()
                                            .h(relative(*count as f32 / (*total).max(1) as f32))
                                            .bg(tone(c, key))
                                    }),
                            ),
                    )
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            });
    let axis: Vec<String> = if few {
        breakdown.periods.clone()
    } else {
        [breakdown.periods.first(), breakdown.periods.last()]
            .into_iter()
            .map(|p| p.cloned().unwrap_or_default())
            .collect()
    };
    let category_totals: Vec<usize> = (0..breakdown.categories.len())
        .map(|c| breakdown.counts.iter().map(|counts| counts[c]).sum())
        .collect();
    let all: usize = totals.iter().sum();
    panel()
        .child(heading(
            format!(
                "{category} per {}",
                breakdown.granularity.label().to_lowercase()
            ),
            if percent {
                "share of each period"
            } else {
                "rows"
            },
        ))
        .child(
            v_flex()
                .gap(px(6.))
                .child(
                    h_flex()
                        .relative()
                        .h(px(CHART_HEIGHT))
                        .items_end()
                        .gap(px(gap))
                        .children(columns)
                        .child(baseline()),
                )
                .child(axis_labels(axis, few, gap, mono)),
        )
        .child(legend(
            breakdown
                .categories
                .iter()
                .zip(category_totals)
                .enumerate()
                .map(|(c, (key, count))| {
                    (
                        key.text().to_string(),
                        format!("{:.0}%", share(count, all)),
                        tone(c, key),
                    )
                }),
        ))
        .into_any_element()
}

/// Two measures: how they move together.
pub fn correlation(found: &Correlation, x: &str, y: &str, mono: SharedString) -> AnyElement {
    let coefficient = |name: &'static str, value: Option<f64>| {
        v_flex()
            .gap(px(4.))
            .p(px(12.))
            .flex_1()
            .rounded(px(8.))
            .bg(color(theme::GROUND))
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(name),
            )
            .child(
                div()
                    .text_size(px(22.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .font_family(mono.clone())
                    .child(value.map_or("—".to_string(), |v| format!("{v:.3}"))),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(match value {
                        Some(v) if v.abs() >= 0.1 => format!(
                            "{} {}",
                            strength(v),
                            if v > 0.0 { "positive" } else { "negative" }
                        ),
                        Some(_) => "no relationship".to_string(),
                        None => "a column does not vary".to_string(),
                    }),
            )
    };
    let line = match (found.slope, found.intercept) {
        (Some(slope), Some(intercept)) => format!(
            "{y} ≈ {} · {x} {} {}",
            format_number(slope),
            if intercept < 0.0 { "−" } else { "+" },
            format_number(intercept.abs())
        ),
        _ => "No line: the first column does not vary".to_string(),
    };
    panel()
        .child(heading(
            format!("{x} vs {y}"),
            format!("{} rows with both values", thousands(found.n as u64)),
        ))
        .child(
            h_flex()
                .gap(px(10.))
                .child(coefficient("Pearson r (linear)", found.pearson))
                .child(coefficient("Spearman ρ (rank)", found.spearman))
                .child(coefficient(
                    "r² (variance explained)",
                    found.pearson.map(|r| r * r),
                )),
        )
        .child(
            h_flex()
                .gap_2()
                .text_size(px(12.5))
                .child(
                    div()
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child("Best line"),
                )
                .child(div().font_family(mono).child(line)),
        )
        .into_any_element()
}

/// Whole numbers with thousands separators; others with up to four
/// decimals (two above a thousand), trailing zeros dropped.
pub fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        let digits = thousands(value.abs() as u64);
        return if value < 0.0 {
            format!("-{digits}")
        } else {
            digits
        };
    }
    let decimals = if value.abs() >= 1000.0 { 2 } else { 4 };
    let text = format!("{value:.decimals$}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    match text.split_once('.') {
        Some((whole, fraction)) => {
            let whole_value: f64 = whole.parse().unwrap_or(0.0);
            let digits = thousands(whole_value.abs() as u64);
            let sign = if value < 0.0 { "-" } else { "" };
            format!("{sign}{digits}.{fraction}")
        }
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers_for_charts() {
        assert_eq!(format_number(1_000_000.0), "1,000,000");
        assert_eq!(format_number(-42.0), "-42");
        assert_eq!(format_number(50.5), "50.5");
        assert_eq!(format_number(0.123456), "0.1235");
        assert_eq!(format_number(12_345.678), "12,345.68");
        assert_eq!(format_number(-0.5), "-0.5");
    }

    #[test]
    fn labels_bins_by_value_or_range() {
        let bin = |start, end| Bin {
            start,
            end,
            count: 1,
        };
        assert_eq!(bin_label(&bin(3.0, 4.0), true), "3");
        assert_eq!(bin_label(&bin(2.5, 2.5), false), "2.5");
        assert_eq!(bin_label(&bin(0.0, 1.5), false), "0 – 1.5");
    }

    #[test]
    fn names_the_strength_of_an_association() {
        assert_eq!(strength(0.05), "negligible");
        assert_eq!(strength(-0.42), "moderate");
        assert_eq!(strength(0.95), "very strong");
    }
}
