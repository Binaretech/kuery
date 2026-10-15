//! Design colors (dark graphite, blue accent) on top of gpui-component's dark
//! theme, with IBM Plex Sans for the interface and JetBrains Mono for the
//! editor and the table.

use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::{App, Hsla, px, rgb};

use crate::assets::{MONO_FAMILY, SANS_FAMILY};

pub const GROUND: u32 = 0x151517;
pub const SIDEBAR: u32 = 0x232326;
pub const TOOLBAR: u32 = 0x1e1e20;
pub const TITLE_BAR: u32 = 0x1b1b1d;
pub const HAIRLINE: u32 = 0x000000;
pub const ROW_EVEN: u32 = 0x1b1b1d;
pub const SELECTION: u32 = 0x0a5fd6;
pub const ACCENT: u32 = 0x2f86ff;
pub const TEXT: u32 = 0xf5f5f7;
pub const TEXT_SECONDARY: u32 = 0xaeaeb2;
pub const TEXT_TERTIARY: u32 = 0x8e8e93;
pub const READ_ONLY_BG: u32 = 0x15233a;
pub const READ_ONLY_FG: u32 = 0xb5d4ff;
pub const GREEN: u32 = 0x32d74b;
pub const AMBER: u32 = 0xffb340;
pub const RED: u32 = 0xff453a;

pub fn color(hex: u32) -> Hsla {
    rgb(hex).into()
}

pub fn apply(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let theme = Theme::global_mut(cx);
    theme.font_size = px(13.);
    theme.radius = px(6.);

    theme.background = color(GROUND);
    theme.foreground = color(TEXT);
    theme.muted_foreground = color(TEXT_SECONDARY);
    theme.border = color(HAIRLINE);
    theme.title_bar = color(TITLE_BAR);
    theme.title_bar_border = color(HAIRLINE);
    theme.sidebar = color(SIDEBAR);
    theme.sidebar_border = color(HAIRLINE);
    theme.status_bar = color(TOOLBAR);
    theme.primary = color(ACCENT);
    theme.primary_hover = color(0x4796ff);
    theme.primary_foreground = color(0xffffff);
    theme.ring = color(ACCENT);
    theme.selection = color(SELECTION).opacity(0.45);

    theme.table = color(GROUND);
    theme.table_even = color(ROW_EVEN);
    theme.table_head = color(TOOLBAR);
    theme.table_head_foreground = color(TEXT_SECONDARY);
    theme.table_hover = color(0x232327);
    theme.table_active = color(SELECTION);
    theme.table_active_border = color(SELECTION);
    theme.table_row_border = color(GROUND);

    // Fonts go through `Theme::update` so they also reach the base layer and
    // the default text styles.
    Theme::update(cx, |theme| {
        theme.font_family = SANS_FAMILY.into();
        theme.mono_font_family = MONO_FAMILY.into();
    });
}
