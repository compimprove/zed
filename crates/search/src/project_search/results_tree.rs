//! The file tree shown next to the project search results.

use std::{ops::Range, path::Path};

use collections::HashSet;
use editor::{
    Anchor, Editor, EditorEvent, EditorSettings, MultiBuffer, PathKey, SelectionEffects,
    scroll::Autoscroll,
};
use file_icons::FileIcons;
use gpui::{
    App, Context, DragMoveEvent, Empty, Entity, Focusable, ScrollStrategy, SharedString,
    Subscription, UniformListScrollHandle, Window, px, uniform_list,
};
use multi_buffer::{ExcerptRange, MultiBufferSnapshot, ToPoint as _};
use settings::Settings as _;
use text::{OffsetRangeExt as _, Point};
use ui::{
    HighlightedLabel, ListItem, ListItemSpacing, Tooltip,
    prelude::*,
    utils::{PathTreeEntry, build_path_tree},
};

use super::{HighlightKey, ProjectSearchView, ViewEvent};

const DEFAULT_WIDTH: Pixels = px(260.);
const MIN_WIDTH: Pixels = px(140.);
const INDENT: Pixels = px(12.);
/// How much of a matched line the tree shows around the match.
const PREVIEW_CONTEXT_CHARS: usize = 24;
const PREVIEW_MAX_CHARS: usize = 160;

/// A file with search results.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ResultFile {
    /// The file's path in the tree, which also identifies it.
    pub(super) path: SharedString,
    pub(super) components: Vec<SharedString>,
    pub(super) matches: Vec<ResultMatch>,
}

impl ResultFile {
    fn name(&self) -> SharedString {
        self.components.last().cloned().unwrap_or_default()
    }

    fn match_indices(&self) -> Range<usize> {
        let start = self.matches.first().map_or(0, |m| m.index);
        let end = self.matches.last().map_or(0, |m| m.index + 1);
        start..end
    }
}

/// One search match, with a preview of its line.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ResultMatch {
    /// The match's index in `ProjectSearch::match_ranges`.
    pub(super) index: usize,
    /// The match's zero-based row in its file.
    pub(super) row: u32,
    pub(super) preview: SharedString,
    pub(super) highlight: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ResultsTreeRow {
    Directory {
        path: SharedString,
        name: SharedString,
        depth: usize,
        expanded: bool,
    },
    File {
        file_ix: usize,
        depth: usize,
        expanded: bool,
    },
    Match {
        file_ix: usize,
        match_ix: usize,
        depth: usize,
    },
}

/// Groups the matches by file, in the order of the results.
///
/// The path of a file starts with its worktree's name when the project has
/// several worktrees.
pub(super) fn collect_result_files(
    match_ranges: &[Range<Anchor>],
    snapshot: &MultiBufferSnapshot,
    include_worktree_name: bool,
    cx: &App,
) -> Vec<ResultFile> {
    let mut files: Vec<ResultFile> = Vec::new();
    let mut current_buffer = None;
    for (index, range) in match_ranges.iter().enumerate() {
        let start = range.start.to_point(snapshot);
        let end = range.end.to_point(snapshot);
        let Some((buffer, buffer_start)) = snapshot.point_to_buffer_point(start) else {
            continue;
        };

        let buffer_id = buffer.remote_id();
        if current_buffer != Some(buffer_id) {
            current_buffer = Some(buffer_id);
            let components = file_components(buffer, include_worktree_name, cx);
            let path = SharedString::from(
                components
                    .iter()
                    .map(|component| component.as_ref())
                    .collect::<Vec<_>>()
                    .join("/"),
            );
            files.push(ResultFile {
                path,
                components,
                matches: Vec::new(),
            });
        }

        let row = buffer_start.row;
        let line_len = buffer.line_len(row);
        let line: String = buffer
            .text_for_range(Point::new(row, 0)..Point::new(row, line_len))
            .collect();
        let match_end = if end.row == start.row {
            buffer_start.column + (end.column - start.column)
        } else {
            line_len
        };
        let (preview, highlight) =
            line_preview(&line, buffer_start.column as usize..match_end as usize);
        if let Some(file) = files.last_mut() {
            file.matches.push(ResultMatch {
                index,
                row,
                preview,
                highlight,
            });
        }
    }
    files
}

fn file_components(
    buffer: &language::BufferSnapshot,
    include_worktree_name: bool,
    cx: &App,
) -> Vec<SharedString> {
    let Some(file) = buffer.file() else {
        return vec![SharedString::from("untitled")];
    };
    let mut components = Vec::new();
    if include_worktree_name && let Some(root) = file.full_path(cx).components().next() {
        components.push(SharedString::from(
            root.as_os_str().to_string_lossy().to_string(),
        ));
    }
    components.extend(
        file.path()
            .components()
            .map(|component| SharedString::from(component.to_string())),
    );
    if components.is_empty() {
        components.push(SharedString::from(file.file_name(cx).to_string()));
    }
    components
}

/// Returns the part of `line` to show for a match at the `highlight` byte
/// range, with leading whitespace and long text around the match left out,
/// and the match's range in it.
pub(super) fn line_preview(line: &str, highlight: Range<usize>) -> (SharedString, Range<usize>) {
    let clamp = |offset: usize| {
        let mut offset = offset.min(line.len());
        while !line.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    };
    let highlight_start = clamp(highlight.start);
    let highlight_end = clamp(highlight.end).max(highlight_start);

    let indent_end = line.len() - line.trim_start().len();
    let mut start = indent_end.min(highlight_start);
    let context_start = line[..highlight_start]
        .char_indices()
        .rev()
        .nth(PREVIEW_CONTEXT_CHARS - 1)
        .map(|(ix, _)| ix);
    let mut prefix = "";
    if let Some(context_start) = context_start
        && context_start > start
    {
        start = context_start;
        prefix = "…";
    }

    let end = line[start..]
        .char_indices()
        .nth(PREVIEW_MAX_CHARS)
        .map_or(line.len(), |(ix, _)| start + ix)
        .max(highlight_end);
    let preview = format!("{prefix}{}", &line[start..end]);
    let offset = prefix.len();
    let highlight = (highlight_start - start + offset)..(highlight_end - start + offset);
    (preview.into(), highlight)
}

/// Lays the files out as tree rows. Directories are expanded unless
/// collapsed, and files are collapsed unless expanded.
pub(super) fn tree_rows(
    files: &[ResultFile],
    collapsed_directories: &HashSet<SharedString>,
    expanded_files: &HashSet<SharedString>,
) -> Vec<ResultsTreeRow> {
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by(|a, b| files[*a].components.cmp(&files[*b].components));

    let mut rows = Vec::new();
    for entry in build_path_tree(
        order,
        |file_ix| files[*file_ix].components.clone(),
        |components| {
            Some(SharedString::from(
                components
                    .iter()
                    .map(|component| component.as_ref())
                    .collect::<Vec<_>>()
                    .join("/"),
            ))
        },
        |path| !collapsed_directories.contains(path),
    ) {
        match entry {
            PathTreeEntry::Directory {
                path,
                name,
                depth,
                expanded,
            } => rows.push(ResultsTreeRow::Directory {
                path,
                name,
                depth,
                expanded,
            }),
            PathTreeEntry::Item {
                item: file_ix,
                depth,
            } => {
                let expanded = expanded_files.contains(&files[file_ix].path);
                rows.push(ResultsTreeRow::File {
                    file_ix,
                    depth,
                    expanded,
                });
                if expanded {
                    rows.extend((0..files[file_ix].matches.len()).map(|match_ix| {
                        ResultsTreeRow::Match {
                            file_ix,
                            match_ix,
                            depth: depth + 1,
                        }
                    }));
                }
            }
        }
    }
    rows
}

/// Returns the row that shows the match at `match_index`: the match's own row
/// when its file is expanded, otherwise the file's row.
pub(super) fn row_for_match(
    rows: &[ResultsTreeRow],
    files: &[ResultFile],
    match_index: usize,
) -> Option<usize> {
    let file_ix = files
        .iter()
        .position(|file| file.match_indices().contains(&match_index))?;
    rows.iter()
        .position(|row| match row {
            ResultsTreeRow::Match {
                file_ix: row_file_ix,
                match_ix,
                ..
            } => {
                *row_file_ix == file_ix
                    && files[file_ix].matches.get(*match_ix).map(|m| m.index) == Some(match_index)
            }
            _ => false,
        })
        .or_else(|| {
            rows.iter().position(|row| {
                matches!(row, ResultsTreeRow::File { file_ix: row_file_ix, .. } if *row_file_ix == file_ix)
            })
        })
}

/// The tree's state in a project search view.
pub(super) struct ResultsTree {
    pub(super) visible: bool,
    width: Pixels,
    files: Vec<ResultFile>,
    rows: Vec<ResultsTreeRow>,
    /// The search id and match count the files were collected for.
    files_key: Option<(usize, usize)>,
    collapsed_directories: HashSet<SharedString>,
    expanded_files: HashSet<SharedString>,
    scroll_handle: UniformListScrollHandle,
    /// Set when the active match changes, to scroll its row into view.
    scroll_to_active: bool,
    /// The file whose results are shown instead of all results, after a
    /// click on it in the tree.
    pub(super) file_results: Option<FileResults>,
}

/// The results of one file, in their own editor over the same buffer.
pub(super) struct FileResults {
    pub(super) path: SharedString,
    pub(super) editor: Entity<Editor>,
    _subscriptions: Vec<Subscription>,
}

impl ResultsTree {
    pub(super) fn new(visible: bool) -> Self {
        Self {
            visible,
            width: DEFAULT_WIDTH,
            files: Vec::new(),
            rows: Vec::new(),
            files_key: None,
            collapsed_directories: HashSet::default(),
            expanded_files: HashSet::default(),
            scroll_handle: UniformListScrollHandle::new(),
            scroll_to_active: false,
            file_results: None,
        }
    }

    /// Makes the next render collect the files again, e.g. after an edit.
    pub(super) fn invalidate(&mut self) {
        self.files_key = None;
    }

    pub(super) fn reveal_active_match(&mut self) {
        self.scroll_to_active = true;
    }

    #[cfg(test)]
    pub(super) fn rows(&self) -> &[ResultsTreeRow] {
        &self.rows
    }

    #[cfg(test)]
    pub(super) fn files(&self) -> &[ResultFile] {
        &self.files
    }

    fn update_rows(&mut self) {
        self.rows = tree_rows(
            &self.files,
            &self.collapsed_directories,
            &self.expanded_files,
        );
    }
}

/// Drag payload of the tree's resize handle.
struct DraggedResultsTreeHandle;

impl Render for DraggedResultsTreeHandle {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl ProjectSearchView {
    /// Collects the result files again when the results changed.
    pub(super) fn sync_results_tree(&mut self, cx: &App) {
        let search = self.entity.read(cx);
        let key = (search.search_id, search.match_ranges.len());
        if self.results_tree.files_key == Some(key) {
            return;
        }
        let snapshot = search.excerpts.read(cx).snapshot(cx);
        let include_worktree_name = search.project.read(cx).visible_worktrees(cx).count() > 1;
        let files =
            collect_result_files(&search.match_ranges, &snapshot, include_worktree_name, cx);
        let tree = &mut self.results_tree;
        // A new search, or results that no longer include the file, end
        // showing one file.
        let new_search = tree
            .files_key
            .is_some_and(|(search_id, _)| search_id != key.0);
        if new_search
            || tree.file_results.as_ref().is_some_and(|file_results| {
                !files.iter().any(|file| file.path == file_results.path)
            })
        {
            tree.file_results = None;
        }
        tree.files_key = Some(key);
        if tree.files != files {
            tree.files = files;
            tree.update_rows();
        }
    }

    fn toggle_results_tree_directory(&mut self, path: SharedString, cx: &mut Context<Self>) {
        let tree = &mut self.results_tree;
        if !tree.collapsed_directories.remove(&path) {
            tree.collapsed_directories.insert(path);
        }
        tree.update_rows();
        cx.notify();
    }

    pub(super) fn toggle_results_tree_file(&mut self, path: SharedString, cx: &mut Context<Self>) {
        let tree = &mut self.results_tree;
        if !tree.expanded_files.remove(&path) {
            tree.expanded_files.insert(path);
        }
        tree.update_rows();
        cx.notify();
    }

    /// The editor the results are shown in: one file's, or all of them.
    pub(super) fn visible_results_editor(&self) -> &Entity<Editor> {
        self.results_tree
            .file_results
            .as_ref()
            .map_or(&self.results_editor, |file_results| &file_results.editor)
    }

    /// Shows only the results of the file at `path`.
    fn show_file_results(
        &mut self,
        path: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .results_tree
            .file_results
            .as_ref()
            .is_some_and(|file_results| file_results.path == path)
        {
            return;
        }
        let Some(file) = self
            .results_tree
            .files
            .iter()
            .find(|file| file.path == path)
        else {
            return;
        };
        let match_ranges = self.entity.read(cx).match_ranges.clone();
        let Some(first_match) = file.matches.first().and_then(|m| match_ranges.get(m.index)) else {
            return;
        };

        let excerpts = self.entity.read(cx).excerpts.clone();
        let snapshot = excerpts.read(cx).snapshot(cx);
        let Some((_, buffer_snapshot)) = snapshot.anchor_to_buffer_anchor(first_match.start) else {
            return;
        };
        let buffer_id = buffer_snapshot.remote_id();
        let Some(buffer) = excerpts.read(cx).buffer(buffer_id) else {
            return;
        };
        let excerpt_ranges = snapshot
            .excerpts_for_buffer(buffer_id)
            .map(|range| ExcerptRange {
                context: range.context.to_point(buffer_snapshot),
                primary: range.primary.to_point(buffer_snapshot),
            })
            .collect::<Vec<_>>();
        let buffer_snapshot = buffer_snapshot.clone();
        let capability = excerpts.read(cx).capability();
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(capability);
            multibuffer.set_excerpt_ranges_for_path(
                PathKey::for_buffer(&buffer, cx),
                buffer.clone(),
                &buffer_snapshot,
                excerpt_ranges,
                cx,
            );
            multibuffer
        });
        let project = self.entity.read(cx).project.clone();
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_multibuffer(multibuffer, Some(project), window, cx);
            editor.set_searchable(false);
            editor.set_in_project_search(true);
            editor.set_buffer_header_folding(false);
            editor
        });
        let subscriptions = vec![
            cx.on_focus(&editor.focus_handle(cx), window, |this, _, cx| {
                this.confirm_active_search(cx);
            }),
            cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.results_tree.invalidate();
                    cx.notify();
                }
                cx.emit(ViewEvent::EditorEvent(event.clone()));
            }),
        ];
        let had_focus = self
            .visible_results_editor()
            .focus_handle(cx)
            .contains_focused(window, cx);
        if had_focus {
            window.focus(&editor.focus_handle(cx), cx);
        }
        self.results_tree.file_results = Some(FileResults {
            path,
            editor,
            _subscriptions: subscriptions,
        });
        self.highlight_file_results(&match_ranges, self.active_match_index, cx);
        cx.notify();
    }

    /// Goes back to showing the results of all files.
    pub(super) fn show_all_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file_results) = self.results_tree.file_results.take() else {
            return;
        };
        let had_focus = file_results
            .editor
            .focus_handle(cx)
            .contains_focused(window, cx);
        if had_focus {
            window.focus(&self.results_editor.focus_handle(cx), cx);
        }
        cx.notify();
    }

    /// The ranges of the shown file's matches in its editor, with their
    /// indices in `match_ranges`.
    fn file_results_ranges(
        &self,
        match_ranges: &[Range<Anchor>],
        cx: &App,
    ) -> Vec<(usize, Range<Anchor>)> {
        let Some(file_results) = self.results_tree.file_results.as_ref() else {
            return Vec::new();
        };
        let Some(file) = self
            .results_tree
            .files
            .iter()
            .find(|file| file.path == file_results.path)
        else {
            return Vec::new();
        };
        let snapshot = self.results_editor.read(cx).buffer().read(cx).snapshot(cx);
        let file_snapshot = file_results.editor.read(cx).buffer().read(cx).snapshot(cx);
        file.matches
            .iter()
            .filter_map(|m| {
                let range = match_ranges.get(m.index)?;
                let (start, _) = snapshot.anchor_to_buffer_anchor(range.start)?;
                let (end, _) = snapshot.anchor_to_buffer_anchor(range.end)?;
                let range = file_snapshot.anchor_range_in_buffer(start..end)?;
                Some((m.index, range))
            })
            .collect()
    }

    pub(super) fn highlight_file_results(
        &self,
        match_ranges: &[Range<Anchor>],
        active_index: Option<usize>,
        cx: &mut App,
    ) {
        let Some(editor) = self
            .results_tree
            .file_results
            .as_ref()
            .map(|file_results| file_results.editor.clone())
        else {
            return;
        };
        let (indices, ranges): (Vec<usize>, Vec<Range<Anchor>>) = self
            .file_results_ranges(match_ranges, cx)
            .into_iter()
            .unzip();
        let active = active_index.and_then(|active| indices.iter().position(|ix| *ix == active));
        editor.update(cx, |editor, cx| {
            editor.highlight_background(
                HighlightKey::ProjectSearchView,
                &ranges,
                move |index, theme| {
                    if active == Some(*index) {
                        theme.colors().search_active_match_background
                    } else {
                        theme.colors().search_match_background
                    }
                },
                cx,
            );
        });
    }

    /// When one file is shown, shows the file of the match at `index` and
    /// selects the match in it.
    pub(super) fn select_match_in_file_results(
        &mut self,
        match_ranges: &[Range<Anchor>],
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.results_tree.file_results.is_none() {
            return;
        }
        self.sync_results_tree(cx);
        let Some(path) = self
            .results_tree
            .files
            .iter()
            .find(|file| file.match_indices().contains(&index))
            .map(|file| file.path.clone())
        else {
            return;
        };
        self.show_file_results(path, window, cx);
        let Some(range) = self
            .file_results_ranges(match_ranges, cx)
            .into_iter()
            .find_map(|(match_index, range)| (match_index == index).then_some(range))
        else {
            return;
        };
        let Some(editor) = self
            .results_tree
            .file_results
            .as_ref()
            .map(|file_results| file_results.editor.clone())
        else {
            return;
        };
        editor.update(cx, |editor, cx| {
            let range = editor.range_for_match(&range);
            let autoscroll = if EditorSettings::get_global(cx).search.center_on_match {
                Autoscroll::center()
            } else {
                Autoscroll::fit()
            };
            editor.unfold_ranges(std::slice::from_ref(&range), false, true, cx);
            editor.change_selections(SelectionEffects::scroll(autoscroll), window, cx, |s| {
                s.select_ranges([range])
            });
        });
        self.highlight_file_results(match_ranges, Some(index), cx);
    }

    /// Shows the file of the match at `index` on its own and selects the
    /// match, from a click in the tree.
    pub(super) fn activate_match_from_tree(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_results_tree(cx);
        let Some(path) = self
            .results_tree
            .files
            .iter()
            .find(|file| file.match_indices().contains(&index))
            .map(|file| file.path.clone())
        else {
            return;
        };
        self.show_file_results(path, window, cx);
        let match_ranges = self.entity.read(cx).match_ranges.clone();
        self.select_match_at(&match_ranges, index, window, cx);
        let editor = self.visible_results_editor().clone();
        window.focus(&editor.focus_handle(cx), cx);
    }

    pub(super) fn render_results_tree(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.sync_results_tree(cx);
        let active_row = self.active_match_index.and_then(|index| {
            row_for_match(&self.results_tree.rows, &self.results_tree.files, index)
        });
        if std::mem::take(&mut self.results_tree.scroll_to_active)
            && let Some(row) = active_row
        {
            self.results_tree
                .scroll_handle
                .scroll_to_item(row, ScrollStrategy::Nearest);
        }

        let row_count = self.results_tree.rows.len();
        let max_width = (window.viewport_size().width * 0.6).max(MIN_WIDTH);
        let width = self.results_tree.width.clamp(MIN_WIDTH, max_width);
        let view = cx.entity();

        h_flex()
            .h_full()
            .w(width)
            .flex_none()
            .child(
                div()
                    .id("project-search-results-tree")
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .py_1()
                    .bg(cx.theme().colors().panel_background)
                    .child(
                        uniform_list(
                            "project-search-results-tree-rows",
                            row_count,
                            cx.processor(move |this, range: Range<usize>, _window, cx| {
                                range
                                    .map(|ix| this.render_results_tree_row(ix, active_row, cx))
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .size_full()
                        .track_scroll(&self.results_tree.scroll_handle),
                    ),
            )
            .child(
                div()
                    .id("project-search-results-tree-resize-handle")
                    .h_full()
                    .w(px(4.))
                    .flex_none()
                    .cursor_col_resize()
                    .border_r_1()
                    .border_color(cx.theme().colors().border)
                    .hover(|this| this.bg(cx.theme().colors().border_focused))
                    .on_drag(DraggedResultsTreeHandle, |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| DraggedResultsTreeHandle)
                    }),
            )
            .on_drag_move(
                move |event: &DragMoveEvent<DraggedResultsTreeHandle>, _, cx| {
                    let width = event.event.position.x - event.bounds.left();
                    view.update(cx, |this, cx| {
                        this.results_tree.width = width.max(MIN_WIDTH);
                        cx.notify();
                    });
                },
            )
            .into_any_element()
    }

    fn render_results_tree_row(
        &self,
        ix: usize,
        active_row: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tree = &self.results_tree;
        let Some(row) = tree.rows.get(ix) else {
            return Empty.into_any_element();
        };
        let item = ListItem::new(("results-tree-row", ix))
            .spacing(ListItemSpacing::Sparse)
            .indent_step_size(INDENT)
            .toggle_state(active_row == Some(ix));

        match row {
            ResultsTreeRow::Directory {
                path,
                name,
                depth,
                expanded,
            } => {
                let expanded = *expanded;
                let icon = FileIcons::get_folder_icon(expanded, Path::new(path.as_ref()), cx)
                    .map(|icon| {
                        Icon::from_path(icon)
                            .size(IconSize::Small)
                            .color(Color::Muted)
                    })
                    .unwrap_or_else(|| {
                        Icon::new(if expanded {
                            IconName::FolderOpen
                        } else {
                            IconName::Folder
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted)
                    });
                let path = path.clone();
                item.indent_level(*depth)
                    .start_slot(icon)
                    .child(
                        Label::new(name.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_results_tree_directory(path.clone(), cx);
                    }))
                    .into_any_element()
            }
            ResultsTreeRow::File {
                file_ix,
                depth,
                expanded,
            } => {
                let Some(file) = tree.files.get(*file_ix) else {
                    return Empty.into_any_element();
                };
                let icon = FileIcons::get_icon(Path::new(file.path.as_ref()), cx)
                    .map(|icon| {
                        Icon::from_path(icon)
                            .size(IconSize::Small)
                            .color(Color::Muted)
                    })
                    .unwrap_or_else(|| {
                        Icon::new(IconName::File)
                            .size(IconSize::Small)
                            .color(Color::Muted)
                    });
                let first_match = file.matches.first().map(|m| m.index);
                let toggle_path = file.path.clone();
                let tooltip_path = file.path.clone();
                item.indent_level(*depth)
                    .toggle(*expanded)
                    .on_toggle(cx.listener(move |this, _, _, cx| {
                        this.toggle_results_tree_file(toggle_path.clone(), cx);
                    }))
                    .start_slot(icon)
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div().flex_1().min_w_0().child(
                                    Label::new(file.name()).size(LabelSize::Small).truncate(),
                                ),
                            )
                            .child(
                                Label::new(file.matches.len().to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .tooltip(Tooltip::text(tooltip_path))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(index) = first_match {
                            this.activate_match_from_tree(index, window, cx);
                        }
                    }))
                    .into_any_element()
            }
            ResultsTreeRow::Match {
                file_ix,
                match_ix,
                depth,
            } => {
                let Some(result) = tree
                    .files
                    .get(*file_ix)
                    .and_then(|file| file.matches.get(*match_ix))
                else {
                    return Empty.into_any_element();
                };
                let index = result.index;
                item.indent_level(*depth)
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap_2()
                            .child(
                                Label::new((result.row + 1).to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                div().flex_1().min_w_0().child(
                                    HighlightedLabel::from_ranges(
                                        result.preview.clone(),
                                        vec![result.highlight.clone()],
                                    )
                                    .size(LabelSize::Small)
                                    .truncate(),
                                ),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_match_from_tree(index, window, cx);
                    }))
                    .into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, indices: Range<usize>) -> ResultFile {
        ResultFile {
            path: path.to_string().into(),
            components: path
                .split('/')
                .map(|component| SharedString::from(component.to_string()))
                .collect(),
            matches: indices
                .map(|index| ResultMatch {
                    index,
                    row: index as u32,
                    preview: "match".into(),
                    highlight: 0..5,
                })
                .collect(),
        }
    }

    #[test]
    fn tree_rows_group_files_and_expand_matches() {
        let files = [
            file("src/b.rs", 0..2),
            file("src/a.rs", 2..3),
            file("README.md", 3..4),
        ];
        let expanded = HashSet::from_iter([SharedString::from("src/b.rs")]);
        let rows = tree_rows(&files, &HashSet::default(), &expanded);
        assert_eq!(
            rows,
            [
                ResultsTreeRow::Directory {
                    path: "src".into(),
                    name: "src".into(),
                    depth: 0,
                    expanded: true,
                },
                ResultsTreeRow::File {
                    file_ix: 1,
                    depth: 1,
                    expanded: false,
                },
                ResultsTreeRow::File {
                    file_ix: 0,
                    depth: 1,
                    expanded: true,
                },
                ResultsTreeRow::Match {
                    file_ix: 0,
                    match_ix: 0,
                    depth: 2,
                },
                ResultsTreeRow::Match {
                    file_ix: 0,
                    match_ix: 1,
                    depth: 2,
                },
                ResultsTreeRow::File {
                    file_ix: 2,
                    depth: 0,
                    expanded: false,
                },
            ]
        );

        // The active match is shown on its own row, or on its file's row
        // when the file is collapsed.
        assert_eq!(row_for_match(&rows, &files, 1), Some(4));
        assert_eq!(row_for_match(&rows, &files, 2), Some(1));
        assert_eq!(row_for_match(&rows, &files, 9), None);

        let collapsed = HashSet::from_iter([SharedString::from("src")]);
        let rows = tree_rows(&files, &collapsed, &expanded);
        assert_eq!(rows.len(), 2);
        assert_eq!(row_for_match(&rows, &files, 0), None);
    }

    #[test]
    fn line_preview_trims_indentation_and_long_prefixes() {
        assert_eq!(
            line_preview("    node-version: '22.x'", 9..20),
            ("node-version: '22.x'".into(), 5..16)
        );

        let line = format!("{}needle tail", "x".repeat(100));
        let (preview, highlight) = line_preview(&line, 100..106);
        assert!(preview.starts_with('…'));
        assert_eq!(&preview[highlight], "needle");
    }

    #[test]
    fn line_preview_handles_multibyte_text_and_out_of_range_matches() {
        let line = "  héllo wörld";
        let (preview, highlight) = line_preview(line, 3..12);
        assert_eq!(&preview[highlight], "éllo wö");

        // Offsets inside a character or past the end don't panic.
        let (preview, highlight) = line_preview(line, 4..400);
        assert_eq!(&preview[highlight], "éllo wörld");
        let (preview, highlight) = line_preview("", 0..3);
        assert_eq!((preview.as_ref(), highlight), ("", 0..0));
    }
}
