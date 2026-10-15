//! Welcome window: open or create a project folder, or reopen a recent one.
//! Recent projects can be removed from the list (or all cleared), and
//! folders that no longer exist are shown as such so they can be removed.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Icon, IconName, Sizable as _, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::assets::AppIcon;
use crate::recent::{self, RecentProject};
use crate::theme::{self, color};

pub struct Welcome {
    recent: Vec<RecentProject>,
}

impl Welcome {
    pub fn new(_: &mut Window, _: &mut Context<Self>) -> Self {
        Self {
            recent: recent::entries(),
        }
    }

    /// Reads the list again after it changed somewhere else.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.recent = recent::entries();
        cx.notify();
    }

    fn open(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        crate::app::request_open(Some(path), Some(window.window_handle()), cx);
    }

    fn remove(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        recent::remove(&path);
        crate::app::recents_changed(cx);
    }

    fn render_row(
        &self,
        index: usize,
        project: &RecentProject,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let group = SharedString::from(format!("recent-{index}"));
        let path = project.path.clone();
        let available = project.available;
        let detail = if available {
            project.display_path()
        } else {
            format!("Folder not found · {}", project.display_path())
        };

        let action = |id: &'static str, icon: IconName, tooltip: &'static str| {
            Button::new((id, index))
                .ghost()
                .xsmall()
                .icon(icon)
                .tooltip(tooltip)
        };

        h_flex()
            .id(("recent", index))
            .group(group.clone())
            .px_3()
            .py_2()
            .gap_3()
            .rounded(px(8.))
            .when(available, |row| {
                let path = path.clone();
                row.hover(|s| s.bg(color(0x2e2e32))).on_click(
                    cx.listener(move |this, _, window, cx| this.open(path.clone(), window, cx)),
                )
            })
            .child(
                Icon::new(IconName::FolderClosed)
                    .size(px(16.))
                    .text_color(color(if available {
                        theme::ACCENT
                    } else {
                        theme::TEXT_TERTIARY
                    })),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .when(!available, |name| {
                                name.text_color(color(theme::TEXT_TERTIARY))
                            })
                            .child(project.name.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(color(if available {
                                theme::TEXT_TERTIARY
                            } else {
                                theme::AMBER
                            }))
                            .child(detail),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_shrink_0()
                    // Shown on hover, except for missing folders: removing
                    // them is the only thing left to do.
                    .when(available, |actions| {
                        actions.invisible().group_hover(group, |s| s.visible())
                    })
                    .when(available, |actions| {
                        let path = path.clone();
                        actions.child(
                            action("reveal-recent", IconName::FolderOpen, REVEAL_LABEL).on_click(
                                move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.reveal_path(&path);
                                },
                            ),
                        )
                    })
                    .child(
                        action(
                            "remove-recent",
                            IconName::Close,
                            "Remove from Recent Projects",
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.remove(path.clone(), cx);
                        })),
                    ),
            )
    }
}

const REVEAL_LABEL: &str = if cfg!(target_os = "macos") {
    "Reveal in Finder"
} else if cfg!(target_os = "windows") {
    "Show in Explorer"
} else {
    "Open Containing Folder"
};

impl Render for Welcome {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut recent = v_flex().gap(px(2.));
        for (index, project) in self.recent.iter().enumerate() {
            recent = recent.child(self.render_row(index, project, cx));
        }
        if self.recent.is_empty() {
            recent = recent.child(
                div()
                    .px_3()
                    .py_2()
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child("Projects you open appear here."),
            );
        }

        v_flex()
            .size_full()
            .bg(color(theme::GROUND))
            .text_color(color(theme::TEXT))
            .text_size(px(13.))
            .child(TitleBar::new())
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .w(px(300.))
                            .h_full()
                            .p_8()
                            .gap_3()
                            .border_r_1()
                            .border_color(color(theme::HAIRLINE))
                            .child(
                                div()
                                    .size(px(48.))
                                    .rounded(px(12.))
                                    .bg(color(0x1fa971))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(AppIcon::Database)
                                            .size(px(26.))
                                            .text_color(color(0x04140c)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(22.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Kuery"),
                            )
                            .child(div().text_color(color(theme::TEXT_SECONDARY)).child(
                                "A project is a folder. Its connections and saved queries live in .kuery/ inside it.",
                            ))
                            .child(div().h(px(8.)))
                            .child(
                                Button::new("open-folder")
                                    .primary()
                                    .label("Open Folder…")
                                    .on_click(|_, window, cx| {
                                        crate::app::request_open(
                                            None,
                                            Some(window.window_handle()),
                                            cx,
                                        )
                                    }),
                            )
                            .child(
                                Button::new("new-project")
                                    .label("New Project…")
                                    .on_click(|_, window, cx| {
                                        crate::app::request_open(
                                            None,
                                            Some(window.window_handle()),
                                            cx,
                                        )
                                    }),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .p_6()
                            .gap_2()
                            .child(
                                h_flex()
                                    .px_3()
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_size(px(11.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(color(theme::TEXT_TERTIARY))
                                            .child("Recent projects"),
                                    )
                                    .when(!self.recent.is_empty(), |header| {
                                        header.child(
                                            Button::new("clear-recent")
                                                .ghost()
                                                .xsmall()
                                                .label("Clear")
                                                .on_click(cx.listener(|_, _, _, cx| {
                                                    recent::clear();
                                                    crate::app::recents_changed(cx);
                                                })),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .id("recent-list")
                                    .flex_1()
                                    .overflow_y_scroll()
                                    .child(recent),
                            ),
                    ),
            )
    }
}
