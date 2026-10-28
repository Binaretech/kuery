//! The window for data files opened with no project window open (`kuery
//! orders.csv`): their tabs and Query files, without connections, saved
//! queries or history.

use std::path::PathBuf;

use gpui_kit::component::button::Button;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{Icon, IconName, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::assets::AppIcon;
use crate::commands::{CloseTab, OpenDataFile};
use crate::context_menus::{copy, item, reveal};
use crate::data_file_tabs::DataFileTabs;
use crate::file_query_tab::FileQueryTab;
use crate::file_tab::FileTab;
use crate::menus::CloseWindow;
use crate::theme::{self, color};

pub(crate) enum Tab {
    File(Entity<FileTab>),
    Query(Entity<FileQueryTab>),
}

impl Tab {
    fn title(&self, cx: &App) -> SharedString {
        match self {
            Tab::File(file) => file.read(cx).title(),
            Tab::Query(query) => query.read(cx).title(),
        }
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Tab::File(file) => file.read(cx).focus_handle(cx),
            Tab::Query(query) => query.read(cx).focus_handle(cx),
        }
    }
}

pub struct FileWindow {
    tabs: Vec<Tab>,
    active: usize,
    focus_handle: FocusHandle,
}

impl FileWindow {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.set_window_title("Data files");
        Self {
            tabs: Vec::new(),
            active: 0,
            focus_handle: cx.focus_handle(),
        }
    }

    /// The open data files and the index of the active one among them, to
    /// reopen after a restart. Query files tabs are not kept.
    pub fn session(&self, cx: &App) -> (Vec<PathBuf>, usize) {
        let files: Vec<(usize, PathBuf)> = self
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| match tab {
                Tab::File(file) => Some((index, file.read(cx).path().to_path_buf())),
                Tab::Query(_) => None,
            })
            .collect();
        let active = files
            .iter()
            .position(|(index, _)| *index == self.active)
            .unwrap_or(0);
        (files.into_iter().map(|(_, path)| path).collect(), active)
    }

    /// Reopens what [`Self::session`] saved, in order. Files that are gone
    /// are left out.
    pub fn restore(
        &mut self,
        files: Vec<PathBuf>,
        active: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let sources = cx
                .background_spawn(async move {
                    files
                        .into_iter()
                        .map(|path| {
                            crate::file_tab::load_source(path.clone()).map_err(|error| {
                                eprintln!("kuery: could not reopen {}: {error}", path.display())
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                let mut shown = None;
                for (index, source) in sources.into_iter().enumerate() {
                    let Ok(source) = source else { continue };
                    let tab = this.new_file_tab(source, window, cx);
                    this.tabs.push(tab);
                    if index <= active {
                        shown = Some(this.tabs.len() - 1);
                    }
                }
                if let Some(index) = shown {
                    this.activate(index, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Closes the tabs `close` picks. Like project windows close these
    /// tabs, without asking: a data file has nothing to save, and a Query
    /// files tab's SQL is not kept.
    fn close_tabs(
        &mut self,
        close: impl Fn(usize) -> bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for index in (0..self.tabs.len()).rev().filter(|index| close(*index)) {
            self.remove_tab(index, window, cx);
        }
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            window.dispatch_action(Box::new(CloseWindow), cx);
        } else {
            self.remove_tab(self.active, window, cx);
        }
    }

    fn render_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut strip = h_flex()
            .h(px(36.))
            .flex_shrink_0()
            .items_end()
            .gap(px(2.))
            .px_2()
            .bg(color(theme::TOOLBAR))
            .border_b_1()
            .border_color(color(theme::HAIRLINE));
        for (index, tab) in self.tabs.iter().enumerate() {
            let active = index == self.active;
            let icon = match tab {
                Tab::File(_) => AppIcon::Table,
                Tab::Query(_) => AppIcon::Code,
            };
            let tooltip: SharedString = match tab {
                Tab::File(file) => file.read(cx).path().display().to_string().into(),
                Tab::Query(_) => tab.title(cx),
            };
            let group = SharedString::from(format!("tab-{index}"));
            strip = strip.child(
                h_flex()
                    .id(("tab", index))
                    .group(group.clone())
                    .h(px(30.))
                    .max_w(px(240.))
                    .pl(px(12.))
                    .pr(px(8.))
                    .gap(px(8.))
                    .rounded_t(px(7.))
                    .text_size(px(12.5))
                    .when(active, |t| {
                        t.bg(color(theme::GROUND))
                            .border_1()
                            .border_b_0()
                            .border_color(color(theme::HAIRLINE))
                            .mb(px(-1.))
                            .text_color(color(theme::TEXT))
                    })
                    .when(!active, |t| {
                        t.text_color(color(theme::TEXT_TERTIARY))
                            .hover(|s| s.text_color(color(theme::TEXT)))
                    })
                    .child(Icon::new(icon).size(px(13.)).text_color(color(if active {
                        0x4da3ff
                    } else {
                        theme::TEXT_TERTIARY
                    })))
                    .child(div().truncate().child(tab.title(cx)))
                    .child(
                        div()
                            .id(("close-tab", index))
                            .size(px(18.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .text_color(color(theme::TEXT_TERTIARY))
                            .when(!active, |b| {
                                b.invisible().group_hover(group.clone(), |s| s.visible())
                            })
                            .hover(|s| s.bg(color(theme::ACTIVE)).text_color(color(theme::TEXT)))
                            .child(Icon::new(IconName::Close).size(px(11.)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.remove_tab(index, window, cx);
                            })),
                    )
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tooltip.clone())
                            .build(window, cx)
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.activate(index, window, cx)),
                    )
                    .context_menu({
                        let this = cx.entity().downgrade();
                        let count = self.tabs.len();
                        let file = match tab {
                            Tab::File(file) => Some(file.read(cx).path().to_path_buf()),
                            Tab::Query(_) => None,
                        };
                        move |menu, _, _| {
                            let mut menu = menu
                                .item(
                                    item("Close", &this, move |this, window, cx| {
                                        this.remove_tab(index, window, cx)
                                    })
                                    .icon(IconName::Close),
                                )
                                .item(
                                    item("Close Others", &this, move |this, window, cx| {
                                        this.close_tabs(|i| i != index, window, cx)
                                    })
                                    .disabled(count < 2),
                                )
                                .item(
                                    item(
                                        "Close Tabs to the Right",
                                        &this,
                                        move |this, window, cx| {
                                            this.close_tabs(|i| i > index, window, cx)
                                        },
                                    )
                                    .disabled(index + 1 >= count),
                                )
                                .item(item("Close All", &this, |this, window, cx| {
                                    this.close_tabs(|_| true, window, cx)
                                }));
                            if let Some(file) = &file {
                                menu = menu
                                    .separator()
                                    .item(reveal(file.clone()))
                                    .item(copy("Copy Path", file.display().to_string()));
                            }
                            menu
                        }
                    }),
            );
        }
        strip
    }

    fn render_content(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.tabs.get(self.active) {
            Some(Tab::File(file)) => file.clone().into_any_element(),
            Some(Tab::Query(query)) => query.clone().into_any_element(),
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .child("Explore a data file")
                .child(
                    Button::new("empty-open-file")
                        .label("Open Data File…")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_data_file(&OpenDataFile, window, cx)
                        })),
                )
                .into_any_element(),
        }
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = match self.tabs.get(self.active) {
            Some(Tab::File(file)) => Some(file.read(cx).status()),
            Some(Tab::Query(query)) => Some(query.read(cx).status().to_owned()),
            None => None,
        };
        h_flex()
            .h(px(26.))
            .flex_shrink_0()
            .px_3()
            .gap_4()
            .bg(color(theme::TOOLBAR))
            .border_t_1()
            .border_color(color(theme::HAIRLINE))
            .text_size(px(11.5))
            .text_color(color(theme::TEXT_TERTIARY))
            .child(div().flex_1().min_w_0().child("Data files · read only"))
            .children(status)
            .child(crate::updates::status_item(cx))
    }
}

impl DataFileTabs for FileWindow {
    type Tab = Tab;

    fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    fn tabs_mut(&mut self) -> &mut Vec<Tab> {
        &mut self.tabs
    }

    fn active(&self) -> usize {
        self.active
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active = index;
        let handle = match self.tabs.get(index) {
            Some(tab) => tab.focus_handle(cx),
            None => self.focus_handle.clone(),
        };
        window.focus(&handle, cx);
        cx.notify();
    }

    fn as_file(tab: &Tab) -> Option<&Entity<FileTab>> {
        match tab {
            Tab::File(file) => Some(file),
            Tab::Query(_) => None,
        }
    }

    fn file_tab(file: Entity<FileTab>) -> Tab {
        Tab::File(file)
    }

    fn query_files_tab(query: Entity<FileQueryTab>) -> Tab {
        Tab::Query(query)
    }
}

impl Focusable for FileWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FileWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep focus in the window, or its shortcuts would have nowhere to go.
        if window.focused(cx).is_none() {
            let handle = self.focus_handle.clone();
            window.defer(cx, move |window, cx| window.focus(&handle, cx));
        }
        v_flex()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::open_data_file))
            .on_action(cx.listener(Self::query_data_files))
            .bg(color(theme::GROUND))
            .text_color(color(theme::TEXT))
            .text_size(px(13.))
            .child(
                TitleBar::new().child(
                    h_flex()
                        .w_full()
                        .h_full()
                        .text_size(px(12.5))
                        .text_color(color(theme::TEXT_SECONDARY))
                        .child("Data files"),
                ),
            )
            .child(self.render_tab_strip(cx))
            .child(div().flex_1().min_h_0().child(self.render_content(cx)))
            .child(self.render_status_bar(cx))
    }
}
