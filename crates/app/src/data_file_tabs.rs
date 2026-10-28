//! Data file tabs, the same in a project window and in the Data files
//! window: opening data files and Query files, moving between tabs and
//! closing them.

use std::path::PathBuf;

use gpui_kit::{App, AppContext as _, Context, Entity, Window};
use kuery_files::FileSource;

use crate::commands::{NextTab, OpenDataFile, PreviousTab, QueryDataFiles};
use crate::file_query_tab::{self, FileQueryTab};
use crate::file_tab::{self, FileTab, data_file_error, no_files_to_query};

/// A window of tabs, some of them data files.
pub(crate) trait DataFileTabs: Sized + 'static {
    type Tab;

    fn tabs(&self) -> &[Self::Tab];
    fn tabs_mut(&mut self) -> &mut Vec<Self::Tab>;
    /// The index of the tab in view.
    fn active(&self) -> usize;
    /// Shows the tab at `index` and gives it keyboard focus; with no tab
    /// there, the window gets the focus.
    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>);
    /// The data file `tab` shows, if it shows one.
    fn as_file(tab: &Self::Tab) -> Option<&Entity<FileTab>>;
    fn file_tab(file: Entity<FileTab>) -> Self::Tab;
    fn query_files_tab(query: Entity<FileQueryTab>) -> Self::Tab;

    /// Shows the first tab `matches` picks, or else the one `make` opens at
    /// the end. Returns the tab's index.
    fn focus_or_open(
        &mut self,
        matches: impl Fn(&Self::Tab, &App) -> bool,
        make: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) -> Self::Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let existing = self.tabs().iter().position(|tab| matches(tab, cx));
        let index = existing.unwrap_or_else(|| {
            let tab = make(self, window, cx);
            self.tabs_mut().push(tab);
            self.tabs().len() - 1
        });
        self.activate(index, window, cx);
        index
    }

    fn new_file_tab(
        &self,
        source: FileSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self::Tab {
        let file = cx.new(|cx| FileTab::new(source, window, cx));
        cx.observe(&file, |_, _, cx| cx.notify()).detach();
        Self::file_tab(file)
    }

    fn open_data_file(&mut self, _: &OpenDataFile, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open data file".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            if let Some(path) = paths.into_iter().next() {
                this.update_in(cx, |this, window, cx| this.open_data_path(path, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Opens the data file at `path` in a tab, or shows its tab if it is
    /// already open.
    fn open_data_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let source = cx
                .background_spawn(async move { file_tab::load_source(path) })
                .await;
            this.update_in(cx, |this, window, cx| match source {
                Ok(source) => {
                    let opened = source.clone();
                    this.focus_or_open(
                        |tab, cx| {
                            Self::as_file(tab).is_some_and(|file| file.read(cx).matches(&opened))
                        },
                        |this, window, cx| this.new_file_tab(source, window, cx),
                        window,
                        cx,
                    );
                }
                Err(error) => data_file_error(&error, window, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Opens a Query files tab on the data files with a preview, the one in
    /// view first.
    fn query_data_files(
        &mut self,
        _: &QueryDataFiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self
            .tabs()
            .get(self.active())
            .and_then(Self::as_file)
            .map(|active| active.read(cx).path().to_path_buf());
        let sources = file_query_tab::query_sources(
            self.tabs()
                .iter()
                .filter_map(Self::as_file)
                .map(|file| file.read(cx)),
            active.as_deref(),
        );
        if sources.is_empty() {
            no_files_to_query(window, cx);
            return;
        }
        let query = cx.new(|cx| FileQueryTab::new(sources, window, cx));
        cx.observe(&query, |_, _, cx| cx.notify()).detach();
        self.tabs_mut().push(Self::query_files_tab(query));
        self.activate(self.tabs().len() - 1, window, cx);
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = next(self.active(), self.tabs().len()) {
            self.activate(index, window, cx);
        }
    }

    fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = previous(self.active(), self.tabs().len()) {
            self.activate(index, window, cx);
        }
    }

    /// Closes the tab at `index` without asking, and shows the tab that
    /// was in view, or the one that takes its place.
    fn remove_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.tabs().len() {
            self.tabs_mut().remove(index);
        }
        let active = active_after_removal(self.active(), index, self.tabs().len());
        self.activate(active, window, cx);
    }
}

/// The tab after `active` among `len`, the first after the last.
fn next(active: usize, len: usize) -> Option<usize> {
    (len > 0).then(|| (active + 1) % len)
}

/// The tab before `active` among `len`, the last before the first.
fn previous(active: usize, len: usize) -> Option<usize> {
    (len > 0).then(|| (active + len - 1) % len)
}

/// The tab in view once the one at `removed` is gone and `len` are left:
/// the same tab, the one now in the removed tab's place, or the last.
fn active_after_removal(active: usize, removed: usize, len: usize) -> usize {
    if active >= len {
        len.saturating_sub(1)
    } else if active > removed {
        active - 1
    } else {
        active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_and_previous_wrap_around() {
        assert_eq!(next(0, 3), Some(1));
        assert_eq!(next(2, 3), Some(0));
        assert_eq!(previous(0, 3), Some(2));
        assert_eq!(previous(2, 3), Some(1));
        assert_eq!(next(0, 0), None);
        assert_eq!(previous(0, 0), None);
    }

    #[test]
    fn removing_a_tab_keeps_the_tab_in_view() {
        // Tabs a b c d, c in view; b closes: c moves to 1.
        assert_eq!(active_after_removal(2, 1, 3), 1);
        // d closes: c stays at 2.
        assert_eq!(active_after_removal(2, 3, 3), 2);
    }

    #[test]
    fn removing_the_tab_in_view_shows_the_next_one() {
        // Tabs a b c, b in view and closed: c takes its place.
        assert_eq!(active_after_removal(1, 1, 2), 1);
        // c in view and closed: b, now last.
        assert_eq!(active_after_removal(2, 2, 2), 1);
        // The only tab closed.
        assert_eq!(active_after_removal(0, 0, 0), 0);
    }
}
