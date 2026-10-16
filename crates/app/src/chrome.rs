//! Controls shared by the toolbars and bars around the content: icon buttons
//! with tooltips, the primary button, segmented controls, pills and key hints.
//! They are plain elements so every toolbar looks the same.

use gpui_kit::component::Icon;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Div, ElementId, FontWeight, InteractiveElement as _, ParentElement as _, SharedString,
    Stateful, StatefulInteractiveElement as _, Styled as _, div, px,
};

use crate::theme::{self, color};

/// A 32×30 icon button with a tooltip (which names its shortcut, if any).
/// Disabled ones are dimmed and ignore clicks: give them no handler.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    tooltip: impl Into<SharedString>,
    enabled: bool,
) -> Stateful<Div> {
    let tooltip: SharedString = tooltip.into();
    div()
        .id(id)
        .w(px(32.))
        .h(px(30.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .text_color(color(if enabled {
            theme::TEXT_SECONDARY
        } else {
            0x4a4a50
        }))
        .when(enabled, |button| {
            button.hover(|s| s.bg(color(theme::ACTIVE)).text_color(color(theme::TEXT)))
        })
        .child(icon.into().size(px(16.)))
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
}

/// The one strong button of a toolbar: accent fill, icon, label and a quiet
/// key hint.
pub fn primary_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    label: impl Into<SharedString>,
    hint: Option<&'static str>,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(30.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(7.))
        .pl(px(10.))
        .pr(px(12.))
        .rounded(px(7.))
        .bg(color(theme::ACCENT))
        .text_color(color(0xffffff))
        .font_weight(FontWeight::MEDIUM)
        .hover(|s| s.bg(color(0x4796ff)))
        .child(icon.into().size(px(12.)))
        .child(label.into())
        .children(hint.map(|hint| {
            div()
                .text_size(px(11.))
                .text_color(color(0xffffff).opacity(0.7))
                .child(hint)
        }))
}

/// A small text button for clusters like the transaction pill. `strong`
/// makes it the light, filled one (Commit, Save).
pub fn pill_button(id: impl Into<ElementId>, label: &'static str, strong: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(22.))
        .px(px(8.))
        .flex()
        .items_center()
        .rounded(px(5.))
        .text_size(px(12.))
        .when(strong, |b| {
            b.bg(color(theme::TEXT))
                .text_color(color(theme::GROUND))
                .font_weight(FontWeight::SEMIBOLD)
                .hover(|s| s.bg(color(0xffffff)))
        })
        .when(!strong, |b| {
            b.text_color(color(0xc7c7cc))
                .hover(|s| s.bg(color(theme::ACTIVE_STRONG)))
        })
        .child(label)
}

/// The rounded frame of a pill cluster (transaction, staged changes).
pub fn pill() -> Div {
    div()
        .h(px(30.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(8.))
        .pl(px(10.))
        .pr(px(4.))
        .rounded(px(7.))
        .bg(color(theme::CONTROL))
        .border_1()
        .border_color(color(theme::CONTROL_BORDER))
        .text_size(px(12.))
}

/// The frame of a segmented control.
pub fn segmented() -> Div {
    div()
        .h(px(30.))
        .flex_shrink_0()
        .flex()
        .p(px(2.))
        .rounded(px(7.))
        .bg(color(theme::CONTROL))
        .border_1()
        .border_color(color(theme::CONTROL_BORDER))
        .text_size(px(12.))
}

/// One option of a segmented control.
pub fn segment(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    label: impl Into<SharedString>,
    active: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(5.))
        .px(px(9.))
        .rounded(px(5.))
        .when(active, |s| {
            s.bg(color(theme::ACTIVE_STRONG))
                .text_color(color(theme::TEXT))
                .font_weight(FontWeight::MEDIUM)
        })
        .when(!active, |s| {
            s.text_color(color(theme::TEXT_TERTIARY))
                .hover(|s| s.text_color(color(theme::TEXT)))
        })
        .child(icon.into().size(px(12.)))
        .child(label.into())
}

/// A thin vertical rule between toolbar groups.
pub fn toolbar_divider() -> Div {
    div()
        .w(px(1.))
        .h(px(20.))
        .mx(px(4.))
        .flex_shrink_0()
        .bg(color(theme::CONTROL_BORDER))
}

/// A small dot, e.g. an environment or an open transaction.
pub fn dot(tone: u32) -> Div {
    div()
        .size(px(6.))
        .flex_shrink_0()
        .rounded_full()
        .bg(color(tone))
}

/// A key hint like `⌘K`.
pub fn kbd(keys: impl Into<SharedString>, mono: SharedString) -> Div {
    div()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(4.))
        .bg(color(theme::ACTIVE))
        .text_color(color(theme::TEXT_SECONDARY))
        .font_family(mono)
        .text_size(px(11.))
        .child(keys.into())
}
