//! Welcome window: open or create a project folder, or reopen a recent one.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Icon, IconName, TitleBar, h_flex, v_flex};
use gpui_kit::{
    Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::assets::AppIcon;
use crate::project::Project;
use crate::theme::{self, color};

pub struct Welcome {
    recent: Vec<(String, PathBuf)>,
    error: Option<String>,
}

impl Welcome {
    pub fn new(_: &mut Window, _: &mut Context<Self>) -> Self {
        let recent = crate::recent::load()
            .into_iter()
            .map(|path| {
                let name = Project::open(&path)
                    .map(|p| p.name)
                    .unwrap_or_else(|_| path.display().to_string());
                (name, path)
            })
            .collect();
        Self {
            recent,
            error: None,
        }
    }

    fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Err(error) = crate::app::open_project(&path, cx) {
            self.error = Some(error);
            cx.notify();
        }
    }
}

impl Render for Welcome {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut recent = v_flex().gap(px(2.));
        for (index, (name, path)) in self.recent.iter().enumerate() {
            let target = path.clone();
            recent = recent.child(
                h_flex()
                    .id(("recent", index))
                    .px_3()
                    .py_2()
                    .gap_3()
                    .rounded(px(8.))
                    .hover(|s| s.bg(color(0x2e2e32)))
                    .child(
                        Icon::new(IconName::FolderClosed)
                            .size(px(16.))
                            .text_color(color(theme::ACCENT)),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(div().font_weight(FontWeight::MEDIUM).child(name.clone()))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(11.5))
                                    .text_color(color(theme::TEXT_TERTIARY))
                                    .child(SharedString::from(path.display().to_string())),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.open(target.clone(), cx))),
            );
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
                                    .child(Icon::new(AppIcon::Database).size(px(26.)).text_color(color(0x04140c))),
                            )
                            .child(div().text_size(px(22.)).font_weight(FontWeight::SEMIBOLD).child("dbadmin"))
                            .child(
                                div()
                                    .text_color(color(theme::TEXT_SECONDARY))
                                    .child("A project is a folder. Its connections and saved queries live in .dbadmin/ inside it."),
                            )
                            .child(div().h(px(8.)))
                            .child(
                                Button::new("open-folder")
                                    .primary()
                                    .label("Open Folder…")
                                    .on_click(|_, _, cx| crate::app::prompt_open_project(cx)),
                            )
                            .child(
                                Button::new("new-project")
                                    .label("New Project…")
                                    .on_click(|_, _, cx| crate::app::prompt_open_project(cx)),
                            )
                            .children(self.error.clone().map(|error| {
                                div().text_size(px(12.)).text_color(color(theme::RED)).child(error)
                            })),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .p_6()
                            .gap_2()
                            .child(
                                div()
                                    .px_3()
                                    .text_size(px(11.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(color(theme::TEXT_TERTIARY))
                                    .child("Recent projects"),
                            )
                            .child(div().id("recent-list").flex_1().overflow_y_scroll().child(recent)),
                    ),
            )
    }
}
