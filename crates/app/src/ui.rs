//! Shared interface pieces: number and time formatting, titled groups, tags
//! and the segmented control.

use std::time::Duration;

use gpui_kit::component::h_flex;
use gpui_kit::{
    AnyElement, Div, FontWeight, IntoElement, ParentElement as _, SharedString, Styled as _, div,
    px,
};

use crate::theme::{self, color};

/// `1000000` → `1,000,000`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

pub fn format_duration(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{} ms", duration.as_millis())
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

pub fn format_bytes(bytes: u64) -> String {
    match bytes {
        0..1_024 => format!("{bytes} B"),
        1_024..1_048_576 => format!("{:.0} kB", bytes as f64 / 1_024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

/// Titled group with its content in a rounded box, as in System Settings.
pub fn group(
    title: impl Into<SharedString>,
    detail: Option<String>,
    body: impl IntoElement,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .px_1()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title.into()))
                .children(detail.map(|d| {
                    div()
                        .text_size(px(12.))
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child(d)
                })),
        )
        .child(
            div()
                .rounded(px(10.))
                .bg(color(0x1c1c1e))
                .border_1()
                .border_color(color(theme::HAIRLINE))
                .overflow_hidden()
                .child(body),
        )
}

/// Group row, separated from the next one by a hairline.
pub fn group_row() -> Div {
    h_flex()
        .min_h(px(40.))
        .px_3()
        .py_2()
        .gap_3()
        .border_b_1()
        .border_color(color(0x2c2c2e))
}

/// Small uppercase text tag, without a box.
pub fn tag(text: impl Into<SharedString>, tone: u32) -> Div {
    div()
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color(tone))
        .child(text.into())
}

pub fn muted(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .text_color(color(theme::TEXT_SECONDARY))
        .child(text.into())
}

/// Centered message for empty or loading states.
pub fn placeholder(text: impl Into<SharedString>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(color(theme::TEXT_TERTIARY))
        .child(text.into())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands_with_commas() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(286_720), "280 kB");
        assert_eq!(format_bytes(4_299_161), "4.1 MB");
    }
}
