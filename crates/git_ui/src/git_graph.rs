pub use crate::commit_context_menu::{CopyCommitSha, CopyCommitTag, OpenCommitView};
use crate::{
    commit_context_menu::{checkout_branch, git_context_menu_tasks, git_task_context},
    commit_file_view::{CommitFileView, open_current_file},
    commit_tooltip::CommitAvatar,
    commit_view::CommitView,
    git_graph_actions::{
        GraphActionContext, GraphCommit, GraphRef, add_commit_entries, add_ref_entries,
        can_drop_commit,
    },
    git_status_icon,
};
use collections::{BTreeMap, HashMap, IndexSet};
use editor::Editor;
use file_icons::FileIcons;
use git::{
    BuildCommitPermalinkParams, GitHostingProviderRegistry, GitRemote, Oid, ParsedGitRemote,
    parse_git_remote_url,
    repository::{InitialGraphCommitData, LogOrder, LogSource, RepoPath, SearchCommitArgs},
    status::{FileStatus, StatusCode, TrackedStatus},
};
use gpui::{
    AbsoluteLength, Action, Anchor, AnyElement, App, Bounds, ClickEvent, ClipboardItem,
    DefiniteLength, DismissEvent, DragMoveEvent, ElementId, Empty, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, Length, MouseButton, MouseDownEvent, PathBuilder, Pixels, Point,
    ScrollEvent, ScrollHandle, ScrollStrategy, SharedString, Subscription, Task, TextRun,
    TextStyleRefinement, UniformListScrollHandle, WeakEntity, Window, actions, anchored, deferred,
    point, prelude::*, px, uniform_list,
};
use language::line_diff;
use markdown::{Markdown, MarkdownElement};
use menu::{Cancel, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use picker::{Picker, PickerDelegate};
use project::{
    ProjectPath, TaskSourceKind,
    git_store::{
        CommitDataState, CommitDiff, CommitFile, GitGraphEvent, GitStore, GitStoreEvent,
        GraphDataResponse, Repository, RepositoryEvent, RepositoryId, RepositorySnapshot,
    },
};
use smallvec::{SmallVec, smallvec};
use std::{
    cell::Cell,
    ops::Range,
    rc::Rc,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use task::ResolvedTask;
use zed_actions::{
    buffer_search,
    search::{SelectNextMatch, SelectPreviousMatch, ToggleCaseSensitive},
};

use db::kvp::KeyValueStore;
use theme::AccentColors;
use time::{OffsetDateTime, UtcOffset, format_description::BorrowedFormatItem};
use ui::utils::{PathTreeEntry, build_path_tree};
use ui::{
    Checkbox, Chip, ColumnWidthConfig, CommonAnimationExt as _, ContextMenu, DiffStat, Disclosure,
    Divider, HeaderResizeInfo, HighlightedLabel, IndentGuideColors, ListItem, ListItemSpacing,
    RedistributableColumnsState, ScrollableHandle, Table, TableInteractionState,
    TableRenderContext, TableResizeBehavior, Tooltip, WithScrollbar, bind_redistributable_columns,
    prelude::*, redistribute_hidden_widths, render_redistributable_columns_resize_handles,
    render_table_header, table_row::TableRow,
};
use util::{ResultExt, debug_panic};
use workspace::{
    ItemNavHistory, ModalView, Workspace,
    item::{Item, ItemEvent, TabTooltipContent},
};

const COMMIT_CIRCLE_RADIUS: Pixels = px(4.5);
const COMMIT_CIRCLE_STROKE_WIDTH: Pixels = px(1.5);
const COMMIT_CIRCLE_HALO_WIDTH: Pixels = px(2.0);
const LANE_WIDTH: Pixels = px(18.0);
const LEFT_PADDING: Pixels = px(12.0);
const LINE_WIDTH: Pixels = px(2.0);
const REF_LABEL_MAX_CHARS: usize = 25;
const RESIZE_HANDLE_WIDTH: f32 = 8.0;
const COPIED_STATE_DURATION: Duration = Duration::from_secs(2);
const COMMIT_TAG_LIST_WIDTH_IN_REMS: Rems = rems(10.);
const TREE_INDENT: f32 = 20.0;
const TABLE_COLUMN_COUNT: usize = 4;
const ROW_VERTICAL_PADDING: Pixels = px(4.0);

struct CopiedState {
    copied_at: Option<Instant>,
}

impl CopiedState {
    fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { copied_at: None }
    }

    fn is_copied(&self) -> bool {
        self.copied_at
            .map(|t| t.elapsed() < COPIED_STATE_DURATION)
            .unwrap_or(false)
    }

    fn mark_copied(&mut self) {
        self.copied_at = Some(Instant::now());
    }
}

struct DraggedSplitHandle;

struct CommitTagPicker {
    picker: Entity<Picker<CommitTagPickerDelegate>>,
}

impl CommitTagPicker {
    fn new(tag_names: Vec<SharedString>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = CommitTagPickerDelegate {
            picker: cx.entity().downgrade(),
            tag_names,
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            Picker::nonsearchable_uniform_list(delegate, window, cx)
                .initial_width(COMMIT_TAG_LIST_WIDTH_IN_REMS)
        });
        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for CommitTagPicker {}
impl ModalView for CommitTagPicker {}

impl Focusable for CommitTagPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for CommitTagPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

struct CommitTagPickerDelegate {
    picker: WeakEntity<CommitTagPicker>,
    tag_names: Vec<SharedString>,
    selected_index: usize,
}

impl PickerDelegate for CommitTagPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "commit-tag"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Copy Tag".into()
    }

    fn match_count(&self) -> usize {
        self.tag_names.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        _query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(tag_name) = self.tag_names.get(self.selected_index) {
            cx.write_to_clipboard(ClipboardItem::new_string(tag_name.to_string()));
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker
            .update(cx, |_this, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(self.tag_names.get(ix)?.clone())),
        )
    }
}

#[derive(Clone)]
struct ChangedFileEntry {
    status: FileStatus,
    file_name: SharedString,
    dir_path: SharedString,
    repo_path: RepoPath,
}

impl ChangedFileEntry {
    fn from_commit_file(file: &CommitFile, _cx: &App) -> Self {
        let file_name: SharedString = file
            .path
            .file_name()
            .map(|n| n.to_string())
            .unwrap_or_default()
            .into();
        let dir_path: SharedString = file
            .path
            .parent()
            .map(|p| p.as_unix_str().to_string())
            .unwrap_or_default()
            .into();

        let status_code = match (&file.old_text, &file.new_text) {
            (None, Some(_)) => StatusCode::Added,
            (Some(_), None) => StatusCode::Deleted,
            _ => StatusCode::Modified,
        };

        let status = FileStatus::Tracked(TrackedStatus {
            index_status: status_code,
            worktree_status: StatusCode::Unmodified,
        });

        Self {
            status,
            file_name,
            dir_path,
            repo_path: file.path.clone(),
        }
    }

    fn open_file_diff(
        &self,
        source: &ChangedFilesSource,
        repository: &WeakEntity<Repository>,
        workspace: &WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        match source {
            ChangedFilesSource::Commit(commit_sha) => CommitFileView::open(
                commit_sha.to_string(),
                repository.clone(),
                workspace.clone(),
                self.repo_path.clone(),
                window,
                cx,
            ),
            ChangedFilesSource::Comparison {
                base_sha,
                target_sha,
                merge_base,
            } => CommitFileView::open_range(
                base_sha.to_string(),
                target_sha.to_string(),
                *merge_base,
                repository.clone(),
                workspace.clone(),
                self.repo_path.clone(),
                window,
                cx,
            ),
        }
    }

    fn render(
        &self,
        ix: usize,
        depth: usize,
        directory_label: Option<SharedString>,
        source: ChangedFilesSource,
        repository: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        _cx: &App,
    ) -> AnyElement {
        let file_name = self.file_name.clone();
        let dir_path = self.dir_path.clone();

        ListItem::new(("changed-file", ix))
            .spacing(ListItemSpacing::Sparse)
            .indent_level(depth)
            .indent_step_size(px(TREE_INDENT))
            .start_slot(git_status_icon(self.status))
            .child(
                Label::new(file_name.clone())
                    .size(LabelSize::Small)
                    .truncate(),
            )
            .when_some(directory_label, |this, directory_label| {
                this.child(
                    Label::new(directory_label)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate_start(),
                )
            })
            .tooltip({
                let meta = if dir_path.is_empty() {
                    file_name
                } else {
                    format!("{}/{}", dir_path, file_name).into()
                };
                move |_, cx| Tooltip::with_meta("View Changes", None, meta.clone(), cx)
            })
            .when(!self.status.is_deleted(), |this| {
                let entry = self.clone();
                let repository = repository.clone();
                let workspace = workspace.clone();
                this.end_slot_on_hover(
                    IconButton::new(("open-current-file", ix), IconName::FileTextOutlined)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Open Current File"))
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            open_current_file(
                                &repository,
                                &entry.repo_path,
                                None,
                                &workspace,
                                window,
                                cx,
                            );
                        }),
                )
            })
            .on_click({
                let entry = self.clone();
                move |_, window, cx| {
                    entry.open_file_diff(&source, &repository, &workspace, window, cx);
                }
            })
            .into_any_element()
    }
}

/// Two commits compared in the details panel: the changes from `base` to
/// `target`, or from their merge base to `target` when `merge_base` is set.
struct GraphComparison {
    base: (Oid, SharedString),
    target: (Oid, SharedString),
    merge_base: bool,
    diff: Option<CommitDiff>,
    stats: Option<(usize, usize)>,
    error: Option<SharedString>,
    _task: Task<()>,
}

/// The "Compare with …" entries of a commit or ref context menu: `sha`,
/// labelled `label`, compared with HEAD and with the selected commit.
#[derive(Clone)]
struct CompareMenuEntries {
    sha: Oid,
    label: SharedString,
    head: Option<(Oid, SharedString)>,
    selected: Option<(Oid, SharedString)>,
}

impl CompareMenuEntries {
    fn add_entries(self, mut menu: ContextMenu, git_graph: WeakEntity<GitGraph>) -> ContextMenu {
        let base = (self.sha, self.label);
        let head = self.head.filter(|(sha, _)| *sha != self.sha);
        let selected = self
            .selected
            .filter(|(sha, _)| *sha != self.sha && head.as_ref().is_none_or(|head| head.0 != *sha));
        if head.is_none() && selected.is_none() {
            return menu;
        }
        menu = menu.separator();
        for (target, label) in [
            head.map(|head| (head.clone(), format!("Compare with {}", head.1))),
            selected.map(|selected| {
                (
                    selected.clone(),
                    format!("Compare with Selected Commit {}", selected.1),
                )
            }),
        ]
        .into_iter()
        .flatten()
        {
            let git_graph = git_graph.clone();
            let base = base.clone();
            menu = menu.entry(label, None, move |_, cx| {
                git_graph
                    .update(cx, |git_graph, cx| {
                        git_graph.compare(base.clone(), target.clone(), true, cx)
                    })
                    .ok();
            });
        }
        menu
    }
}

/// What the changed files in the details panel belong to.
#[derive(Clone)]
enum ChangedFilesSource {
    Commit(SharedString),
    Comparison {
        base_sha: Oid,
        target_sha: Oid,
        merge_base: bool,
    },
}

enum ChangedFileTreeEntry {
    Directory(ChangedFileDirectoryEntry),
    File(ChangedFileTreeStatusEntry),
}

struct ChangedFileTreeStatusEntry {
    entry: ChangedFileEntry,
    depth: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ChangedFilesViewMode {
    Flat,
    #[default]
    Tree,
}

impl ChangedFilesViewMode {
    fn toggled(self) -> Self {
        match self {
            Self::Flat => Self::Tree,
            Self::Tree => Self::Flat,
        }
    }

    fn is_tree(self) -> bool {
        matches!(self, Self::Tree)
    }
}

struct ChangedFileDirectoryEntry {
    path: RepoPath,
    name: SharedString,
    depth: usize,
    expanded: bool,
}

impl ChangedFileDirectoryEntry {
    fn render(&self, ix: usize, git_graph: WeakEntity<GitGraph>, cx: &App) -> AnyElement {
        let path = self.path.clone();
        let expanded = self.expanded;
        let folder_icon = FileIcons::get_folder_icon(expanded, path.as_std_path(), cx)
            .map(|icon| {
                Icon::from_path(icon)
                    .size(IconSize::Small)
                    .color(Color::Muted)
            })
            .unwrap_or_else(|| {
                let icon = if expanded {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                };
                Icon::new(icon).size(IconSize::Small).color(Color::Muted)
            });

        ListItem::new(("changed-file-dir", ix))
            .spacing(ListItemSpacing::Sparse)
            .indent_level(self.depth)
            .indent_step_size(px(TREE_INDENT))
            .start_slot(folder_icon)
            .child(
                Label::new(self.name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
            .tooltip({
                let name = self.name.clone();
                move |_, cx| Tooltip::with_meta("Toggle Folder", None, name.clone(), cx)
            })
            .on_click(move |_, _, cx| {
                git_graph
                    .update(cx, |git_graph, cx| {
                        git_graph
                            .changed_files_expanded_dirs
                            .insert(path.clone(), !expanded);
                        cx.notify();
                    })
                    .ok();
            })
            .into_any_element()
    }
}

fn build_changed_file_tree_entries(
    mut files: Vec<ChangedFileEntry>,
    expanded_dirs: &HashMap<RepoPath, bool>,
) -> Vec<ChangedFileTreeEntry> {
    files.sort_by(|a, b| a.repo_path.cmp(&b.repo_path));

    build_path_tree(
        files,
        |file| {
            file.repo_path
                .components()
                .map(|component| SharedString::from(component.to_string()))
                .collect()
        },
        |components| RepoPath::new(&components.join("/")).ok(),
        |path| *expanded_dirs.get(path).unwrap_or(&true),
    )
    .into_iter()
    .map(|entry| match entry {
        PathTreeEntry::Directory {
            path,
            name,
            depth,
            expanded,
        } => ChangedFileTreeEntry::Directory(ChangedFileDirectoryEntry {
            path,
            name,
            depth,
            expanded,
        }),
        PathTreeEntry::Item { item, depth } => {
            ChangedFileTreeEntry::File(ChangedFileTreeStatusEntry { entry: item, depth })
        }
    })
    .collect()
}

enum QueryState {
    Pending(SharedString),
    Confirmed((SharedString, Task<()>)),
    Empty,
}

impl QueryState {
    fn next_state(&mut self) {
        match self {
            Self::Confirmed((query, _)) => *self = Self::Pending(std::mem::take(query)),
            _ => {}
        };
    }
}

struct SearchState {
    case_sensitive: bool,
    editor: Entity<Editor>,
    state: QueryState,
    matches: IndexSet<Oid>,
    selected_index: Option<usize>,
}

struct SplitState {
    left_ratio: f32,
    visible_left_ratio: f32,
}

impl SplitState {
    fn new() -> Self {
        Self {
            left_ratio: 1.0,
            visible_left_ratio: 1.0,
        }
    }

    fn right_ratio(&self) -> f32 {
        1.0 - self.visible_left_ratio
    }

    fn on_drag_move(
        &mut self,
        drag_event: &DragMoveEvent<DraggedSplitHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let drag_position = drag_event.event.position;
        let bounds = drag_event.bounds;
        let bounds_width = bounds.right() - bounds.left();

        let min_ratio = 0.1;
        let max_ratio = 0.9;

        let new_ratio = (drag_position.x - bounds.left()) / bounds_width;
        self.visible_left_ratio = new_ratio.clamp(min_ratio, max_ratio);
    }

    fn commit_ratio(&mut self) {
        self.left_ratio = self.visible_left_ratio;
    }

    fn on_double_click(&mut self) {
        self.left_ratio = 1.0;
        self.visible_left_ratio = 1.0;
    }
}

actions!(
    git_graph,
    [
        /// Opens the Git Graph Tab.
        Open,
        /// Focuses the search field.
        FocusSearch,
        /// Focuses the next git graph tab stop.
        FocusNextTabStop,
        /// Focuses the previous git graph tab stop.
        FocusPreviousTabStop,
        /// Selects a commit half a page above the current selection.
        ScrollUp,
        /// Selects a commit half a page below the current selection.
        ScrollDown,
        /// Toggles the selected commit's changed files between flat and tree views.
        ToggleChangedFilesView,
    ]
);

/// Opens the Git Graph Tab at a specific commit.
#[derive(Clone, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
#[action(namespace = git_graph)]
pub struct OpenAtCommit {
    pub sha: String,
}

fn timestamp_format() -> &'static [BorrowedFormatItem<'static>] {
    static FORMAT: OnceLock<Vec<BorrowedFormatItem<'static>>> = OnceLock::new();
    FORMAT.get_or_init(|| {
        time::format_description::parse("[day] [month repr:short] [year] [hour]:[minute]")
            .unwrap_or_default()
    })
}

/// The Date column's compact format, e.g. `21.08 15:58`.
fn compact_timestamp_format() -> &'static [BorrowedFormatItem<'static>] {
    static FORMAT: OnceLock<Vec<BorrowedFormatItem<'static>>> = OnceLock::new();
    FORMAT.get_or_init(|| {
        time::format_description::parse("[day].[month] [hour]:[minute]").unwrap_or_default()
    })
}

fn format_timestamp(timestamp: i64) -> String {
    format_timestamp_with(timestamp, timestamp_format())
}

fn format_timestamp_with(timestamp: i64, format: &[BorrowedFormatItem<'_>]) -> String {
    let Ok(datetime) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return "Unknown".to_string();
    };

    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local_datetime = datetime.to_offset(local_offset);

    local_datetime.format(format).unwrap_or_default()
}

fn accent_colors_count(accents: &AccentColors) -> usize {
    accents.0.len()
}

#[derive(Copy, Clone, Debug)]
struct BranchColor(u8);

#[derive(Debug)]
enum LaneState {
    Empty,
    Active {
        child: Oid,
        parent: Oid,
        color: Option<BranchColor>,
        starting_row: usize,
        starting_col: usize,
        destination_column: Option<usize>,
        segments: SmallVec<[CommitLineSegment; 1]>,
    },
}

impl LaneState {
    fn to_commit_lines(
        &mut self,
        ending_row: usize,
        lane_column: usize,
        parent_column: usize,
        parent_color: BranchColor,
    ) -> Option<CommitLine> {
        let state = std::mem::replace(self, LaneState::Empty);

        match state {
            LaneState::Active {
                #[cfg_attr(not(test), allow(unused_variables))]
                parent,
                #[cfg_attr(not(test), allow(unused_variables))]
                child,
                color,
                starting_row,
                starting_col,
                destination_column,
                mut segments,
            } => {
                let final_destination = destination_column.unwrap_or(parent_column);
                let final_color = color.unwrap_or(parent_color);

                Some(CommitLine {
                    #[cfg(test)]
                    child,
                    #[cfg(test)]
                    parent,
                    child_column: starting_col,
                    full_interval: starting_row..ending_row,
                    color_idx: final_color.0 as usize,
                    segments: {
                        match segments.last_mut() {
                            Some(CommitLineSegment::Straight { to_row })
                                if *to_row == usize::MAX =>
                            {
                                if final_destination != lane_column {
                                    *to_row = ending_row - 1;

                                    let curved_line = CommitLineSegment::Curve {
                                        to_column: final_destination,
                                        on_row: ending_row,
                                        curve_kind: CurveKind::Checkout,
                                    };

                                    if *to_row == starting_row {
                                        let last_index = segments.len() - 1;
                                        segments[last_index] = curved_line;
                                    } else {
                                        segments.push(curved_line);
                                    }
                                } else {
                                    *to_row = ending_row;
                                }
                            }
                            Some(CommitLineSegment::Curve {
                                on_row,
                                to_column,
                                curve_kind,
                            }) if *on_row == usize::MAX => {
                                if *to_column == usize::MAX {
                                    *to_column = final_destination;
                                }
                                if matches!(curve_kind, CurveKind::Merge) {
                                    *on_row = starting_row + 1;
                                    if *on_row < ending_row {
                                        if *to_column != final_destination {
                                            segments.push(CommitLineSegment::Straight {
                                                to_row: ending_row - 1,
                                            });
                                            segments.push(CommitLineSegment::Curve {
                                                to_column: final_destination,
                                                on_row: ending_row,
                                                curve_kind: CurveKind::Checkout,
                                            });
                                        } else {
                                            segments.push(CommitLineSegment::Straight {
                                                to_row: ending_row,
                                            });
                                        }
                                    } else if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    }
                                } else {
                                    *on_row = ending_row;
                                    if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row,
                                        });
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    }
                                }
                            }
                            Some(CommitLineSegment::Curve {
                                on_row, to_column, ..
                            }) => {
                                if *on_row < ending_row {
                                    if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row - 1,
                                        });
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    } else {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row,
                                        });
                                    }
                                } else if *to_column != final_destination {
                                    segments.push(CommitLineSegment::Curve {
                                        to_column: final_destination,
                                        on_row: ending_row,
                                        curve_kind: CurveKind::Checkout,
                                    });
                                }
                            }
                            _ => {}
                        }

                        segments
                    },
                })
            }
            LaneState::Empty => None,
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            LaneState::Empty => true,
            LaneState::Active { .. } => false,
        }
    }
}

struct CommitEntry {
    data: Arc<InitialGraphCommitData>,
    lane: usize,
    color_idx: usize,
}

type ActiveLaneIdx = usize;

enum AllCommitCount {
    NotLoaded,
    Loading(usize),
    FullyLoaded(usize),
}

#[derive(Debug)]
enum CurveKind {
    Merge,
    Checkout,
}

#[derive(Debug)]
enum CommitLineSegment {
    Straight {
        to_row: usize,
    },
    Curve {
        to_column: usize,
        on_row: usize,
        curve_kind: CurveKind,
    },
}

#[derive(Debug)]
struct CommitLine {
    #[cfg(test)]
    child: Oid,
    #[cfg(test)]
    parent: Oid,
    child_column: usize,
    full_interval: Range<usize>,
    color_idx: usize,
    segments: SmallVec<[CommitLineSegment; 1]>,
}

impl CommitLine {
    fn get_first_visible_segment_idx(&self, first_visible_row: usize) -> Option<(usize, usize)> {
        if first_visible_row > self.full_interval.end {
            return None;
        } else if first_visible_row <= self.full_interval.start {
            return Some((0, self.child_column));
        }

        let mut current_column = self.child_column;

        for (idx, segment) in self.segments.iter().enumerate() {
            match segment {
                CommitLineSegment::Straight { to_row } => {
                    if *to_row >= first_visible_row {
                        return Some((idx, current_column));
                    }
                }
                CommitLineSegment::Curve {
                    to_column, on_row, ..
                } => {
                    if *on_row >= first_visible_row {
                        return Some((idx, current_column));
                    }
                    current_column = *to_column;
                }
            }
        }

        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CommitLineKey {
    child: Oid,
    parent: Oid,
}

struct GraphData {
    lane_states: SmallVec<[LaneState; 8]>,
    lane_colors: HashMap<ActiveLaneIdx, BranchColor>,
    parent_to_lanes: HashMap<Oid, SmallVec<[usize; 1]>>,
    next_color: BranchColor,
    accent_colors_count: usize,
    commits: Vec<Rc<CommitEntry>>,
    max_commit_count: AllCommitCount,
    max_lanes: usize,
    /// For each row, how many lanes its commit and the lines passing it use.
    row_lane_counts: Vec<usize>,
    lines: Vec<Rc<CommitLine>>,
    active_commit_lines: HashMap<CommitLineKey, usize>,
    active_commit_lines_by_parent: HashMap<Oid, SmallVec<[usize; 1]>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefPillKind {
    Branch,
    Tag,
    /// `HEAD` checked out at a commit rather than a branch.
    DetachedHead,
}

/// A branch or tag chip shown before a commit's description.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RefPill {
    kind: RefPillKind,
    /// The ref name without its `HEAD -> `, `tag: ` or remote prefix.
    name: SharedString,
    /// The remote this branch is on at this commit, e.g. `origin`.
    remote: Option<SharedString>,
    is_head: bool,
    /// The raw `%D` decoration, used to resolve the ref's context menu actions.
    decoration: SharedString,
}

impl RefPill {
    /// The name capped at `REF_LABEL_MAX_CHARS`, ending in an ellipsis when cut.
    fn display_name(&self) -> SharedString {
        if self.name.chars().count() <= REF_LABEL_MAX_CHARS {
            return self.name.clone();
        }
        let truncated = self
            .name
            .chars()
            .take(REF_LABEL_MAX_CHARS - 1)
            .collect::<String>();
        SharedString::from(format!("{truncated}…"))
    }

    /// The branch to check out for this chip: the local branch when there is
    /// one, otherwise the remote branch (e.g. `origin/feature`). `None` for
    /// tags and the already checked-out branch.
    fn checkout_branch(&self) -> Option<SharedString> {
        if self.kind != RefPillKind::Branch || self.is_head {
            return None;
        }
        GitGraph::ref_name_from_decoration(&self.decoration)
    }

    fn tooltip_text(&self) -> SharedString {
        match (&self.kind, &self.remote) {
            (RefPillKind::Tag, _) => SharedString::from(format!("Tag {}", self.name)),
            (RefPillKind::DetachedHead, _) => SharedString::from("Detached HEAD"),
            (RefPillKind::Branch, Some(remote)) => {
                SharedString::from(format!("{} ({remote}/{})", self.name, self.name))
            }
            (RefPillKind::Branch, None) => self.name.clone(),
        }
    }
}

impl GraphData {
    fn new(accent_colors_count: usize) -> Self {
        GraphData {
            lane_states: SmallVec::default(),
            lane_colors: HashMap::default(),
            parent_to_lanes: HashMap::default(),
            next_color: BranchColor(0),
            accent_colors_count,
            commits: Vec::default(),
            max_commit_count: AllCommitCount::NotLoaded,
            max_lanes: 0,
            row_lane_counts: Vec::default(),
            lines: Vec::default(),
            active_commit_lines: HashMap::default(),
            active_commit_lines_by_parent: HashMap::default(),
        }
    }

    fn clear(&mut self) {
        self.lane_states.clear();
        self.lane_colors.clear();
        self.parent_to_lanes.clear();
        self.commits.clear();
        self.lines.clear();
        self.active_commit_lines.clear();
        self.active_commit_lines_by_parent.clear();
        self.next_color = BranchColor(0);
        self.max_commit_count = AllCommitCount::NotLoaded;
        self.max_lanes = 0;
        self.row_lane_counts.clear();
    }

    /// The number of lanes up to and including the last active one.
    fn active_lane_count(&self) -> usize {
        self.lane_states
            .iter()
            .rposition(|state| !state.is_empty())
            .map_or(0, |idx| idx + 1)
    }

    /// The most lanes used by any of the given rows.
    fn max_lanes_in_rows(&self, rows: Range<usize>) -> usize {
        let end = rows.end.min(self.row_lane_counts.len());
        let start = rows.start.min(end);
        self.row_lane_counts[start..end]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
    }

    fn first_empty_lane_idx(&mut self) -> ActiveLaneIdx {
        self.lane_states
            .iter()
            .position(LaneState::is_empty)
            .unwrap_or_else(|| {
                self.lane_states.push(LaneState::Empty);
                self.lane_states.len() - 1
            })
    }

    fn get_lane_color(&mut self, lane_idx: ActiveLaneIdx) -> BranchColor {
        let accent_colors_count = self.accent_colors_count;
        *self.lane_colors.entry(lane_idx).or_insert_with(|| {
            let color_idx = self.next_color;
            self.next_color = BranchColor((self.next_color.0 + 1) % accent_colors_count as u8);
            color_idx
        })
    }

    fn add_commits(&mut self, commits: &[Arc<InitialGraphCommitData>]) {
        self.commits.reserve(commits.len());
        self.lines.reserve(commits.len() / 2);

        for commit in commits.iter() {
            let commit_row = self.commits.len();
            let lanes_before = self.active_lane_count();

            let commit_lane = self
                .parent_to_lanes
                .get(&commit.sha)
                .and_then(|lanes| lanes.iter().min().copied());

            let commit_lane = commit_lane.unwrap_or_else(|| self.first_empty_lane_idx());

            let commit_color = self.get_lane_color(commit_lane);

            if let Some(lanes) = self.parent_to_lanes.remove(&commit.sha) {
                for lane_column in lanes {
                    let state = &mut self.lane_states[lane_column];

                    if let LaneState::Active {
                        starting_row,
                        segments,
                        ..
                    } = state
                    {
                        if let Some(CommitLineSegment::Curve {
                            to_column,
                            curve_kind: CurveKind::Merge,
                            ..
                        }) = segments.first_mut()
                        {
                            let curve_row = *starting_row + 1;
                            let would_overlap =
                                if lane_column != commit_lane && curve_row < commit_row {
                                    self.commits[curve_row..commit_row]
                                        .iter()
                                        .any(|c| c.lane == commit_lane)
                                } else {
                                    false
                                };

                            if would_overlap {
                                *to_column = lane_column;
                            }
                        }
                    }

                    if let Some(commit_line) =
                        state.to_commit_lines(commit_row, lane_column, commit_lane, commit_color)
                    {
                        self.lines.push(Rc::new(commit_line));
                    }
                }
            }

            commit
                .parents
                .iter()
                .enumerate()
                .for_each(|(parent_idx, parent)| {
                    if parent_idx == 0 {
                        self.lane_states[commit_lane] = LaneState::Active {
                            parent: *parent,
                            child: commit.sha,
                            color: Some(commit_color),
                            starting_col: commit_lane,
                            starting_row: commit_row,
                            destination_column: None,
                            segments: smallvec![CommitLineSegment::Straight { to_row: usize::MAX }],
                        };

                        self.parent_to_lanes
                            .entry(*parent)
                            .or_default()
                            .push(commit_lane);
                    } else {
                        let new_lane = self.first_empty_lane_idx();

                        self.lane_states[new_lane] = LaneState::Active {
                            parent: *parent,
                            child: commit.sha,
                            color: None,
                            starting_col: commit_lane,
                            starting_row: commit_row,
                            destination_column: None,
                            segments: smallvec![CommitLineSegment::Curve {
                                to_column: usize::MAX,
                                on_row: usize::MAX,
                                curve_kind: CurveKind::Merge,
                            },],
                        };

                        self.parent_to_lanes
                            .entry(*parent)
                            .or_default()
                            .push(new_lane);
                    }
                });

            self.max_lanes = self.max_lanes.max(self.lane_states.len());
            self.row_lane_counts.push(
                lanes_before
                    .max(self.active_lane_count())
                    .max(commit_lane + 1),
            );

            self.commits.push(Rc::new(CommitEntry {
                data: commit.clone(),
                lane: commit_lane,
                color_idx: commit_color.0 as usize,
            }));
        }

        self.max_commit_count = AllCommitCount::Loading(self.commits.len());
    }
}

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<GitGraph>(cx);

    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
        workspace.register_action_renderer(|div, workspace, window, cx| {
            div.when_some(
                resolve_file_history_target(workspace, window, cx),
                |div, (repo_id, log_source)| {
                    let git_store = workspace.project().read(cx).git_store().clone();
                    let workspace = workspace.weak_handle();

                    div.on_action(move |_: &git::FileHistory, window, cx| {
                        let git_store = git_store.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                open_or_reuse_graph(
                                    workspace,
                                    repo_id,
                                    git_store,
                                    log_source.clone(),
                                    None,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                },
            )
            .when(
                workspace.project().read(cx).active_repository(cx).is_some(),
                |div| {
                    let workspace = workspace.weak_handle();

                    div.on_action({
                        let workspace = workspace.clone();
                        move |_: &Open, window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    let Some(repo) =
                                        workspace.project().read(cx).active_repository(cx)
                                    else {
                                        return;
                                    };
                                    let selected_repo_id = repo.read(cx).id;

                                    let git_store =
                                        workspace.project().read(cx).git_store().clone();
                                    open_or_reuse_graph(
                                        workspace,
                                        selected_repo_id,
                                        git_store,
                                        LogSource::All,
                                        None,
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        }
                    })
                    .on_action(move |action: &OpenAtCommit, window, cx| {
                        let sha = action.sha.clone();
                        workspace
                            .update(cx, |workspace, cx| {
                                let Some(repo) = workspace.project().read(cx).active_repository(cx)
                                else {
                                    return;
                                };
                                let selected_repo_id = repo.read(cx).id;

                                let git_store = workspace.project().read(cx).git_store().clone();
                                open_or_reuse_graph(
                                    workspace,
                                    selected_repo_id,
                                    git_store,
                                    LogSource::All,
                                    Some(sha),
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                },
            )
        });
    })
    .detach();
}

/// Resolves a `git::FileHistory` target from a known project path (used by
/// callers like `project_panel` that own a focused selection but cannot be
/// referenced from this module due to dependency direction).
pub fn resolve_file_history_target_from_project_path(
    workspace: &Workspace,
    project_path: &ProjectPath,
    cx: &App,
) -> Option<(RepositoryId, LogSource)> {
    let git_store = workspace.project().read(cx).git_store();
    let (repo, repo_path) = git_store
        .read(cx)
        .repository_and_path_for_project_path(project_path, cx)?;
    let log_source = if repo_path.is_empty() {
        LogSource::All
    } else {
        LogSource::Path(repo_path)
    };
    Some((repo.read(cx).id, log_source))
}

fn resolve_file_history_target(
    workspace: &Workspace,
    window: &Window,
    cx: &App,
) -> Option<(RepositoryId, LogSource)> {
    if let Some(panel) = workspace.panel::<crate::git_panel::GitPanel>(cx)
        && panel.read(cx).focus_handle(cx).contains_focused(window, cx)
        && let Some((repository, repo_path)) = panel.read(cx).selected_file_history_target()
    {
        return Some((repository.read(cx).id, LogSource::Path(repo_path)));
    }

    let editor = workspace.active_item_as::<Editor>(cx)?;

    let file = editor
        .read(cx)
        .file_at(editor.read(cx).selections.newest_anchor().head(), cx)?;
    let project_path = ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    };

    let git_store = workspace.project().read(cx).git_store();
    let (repo, repo_path) = git_store
        .read(cx)
        .repository_and_path_for_project_path(&project_path, cx)?;
    Some((repo.read(cx).id, LogSource::Path(repo_path)))
}

pub fn open_or_reuse_graph(
    workspace: &mut Workspace,
    repo_id: RepositoryId,
    git_store: Entity<GitStore>,
    log_source: LogSource,
    sha: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<GitGraph>(cx).find(|graph| {
        let graph = graph.read(cx);
        graph.repo_id == repo_id && graph.log_source == log_source
    });
    // Keep a single repository graph tab: a graph of another repository is
    // switched to this one instead of opening another tab.
    let other_repository_graph = (existing.is_none() && log_source == LogSource::All)
        .then(|| {
            workspace
                .items_of_type::<GitGraph>(cx)
                .find(|graph| graph.read(cx).log_source == LogSource::All)
        })
        .flatten();

    let git_graph = if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        existing
    } else if let Some(graph) = other_repository_graph {
        graph.update(cx, |graph, cx| {
            graph.clear_selected_commit(cx);
            graph.set_repo_id(repo_id, cx);
            // Load what the repository already has, as a new graph does, so a
            // commit can be selected right away.
            graph.fetch_initial_graph_data(cx);
        });
        workspace.activate_item(&graph, true, true, window, cx);
        graph
    } else {
        let workspace_handle = workspace.weak_handle();
        let git_graph = cx.new(|cx| {
            GitGraph::new(
                repo_id,
                git_store,
                workspace_handle,
                Some(log_source),
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        git_graph
    };

    if let Some(sha) = sha {
        cx.defer(move |cx| {
            git_graph.update(cx, |graph, cx| {
                graph.select_commit_by_sha(sha.as_str(), cx);
            });
        });
    }
}

fn lane_center_x(bounds: Bounds<Pixels>, lane: f32) -> Pixels {
    bounds.origin.x + LEFT_PADDING + lane * LANE_WIDTH + LANE_WIDTH / 2.0
}

fn to_row_center(
    to_row: usize,
    row_height: Pixels,
    scroll_offset: Pixels,
    bounds: Bounds<Pixels>,
) -> Pixels {
    bounds.origin.y + to_row as f32 * row_height + row_height / 2.0 - scroll_offset
}

fn fill_circle(
    center_x: Pixels,
    center_y: Pixels,
    radius: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let mut builder = PathBuilder::fill();

    // Start at the rightmost point of the circle
    builder.move_to(point(center_x + radius, center_y));

    // Draw the circle using two arc_to calls (top half, then bottom half)
    builder.arc_to(
        point(radius, radius),
        px(0.),
        false,
        true,
        point(center_x - radius, center_y),
    );
    builder.arc_to(
        point(radius, radius),
        px(0.),
        false,
        true,
        point(center_x + radius, center_y),
    );
    builder.close();

    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// Draws a commit node: a background-colored "halo" ring that masks any
/// connector lines passing through the lane, an outer ring in the branch
/// color, and a lighter center so the node reads as a clear, distinct point.
fn draw_commit_circle(
    center_x: Pixels,
    center_y: Pixels,
    color: Hsla,
    background: Hsla,
    window: &mut Window,
) {
    // Halo: clears space around the node so crossing lines don't touch it.
    fill_circle(
        center_x,
        center_y,
        COMMIT_CIRCLE_RADIUS + COMMIT_CIRCLE_HALO_WIDTH,
        background,
        window,
    );

    // Outer ring in the branch color.
    fill_circle(center_x, center_y, COMMIT_CIRCLE_RADIUS, color, window);

    // Inner fill makes the node read as a ring rather than a solid dot.
    fill_circle(
        center_x,
        center_y,
        (COMMIT_CIRCLE_RADIUS - COMMIT_CIRCLE_STROKE_WIDTH).max(px(0.)),
        background,
        window,
    );
}

fn compute_diff_stats(diff: &CommitDiff) -> (usize, usize) {
    diff.files.iter().fold((0, 0), |(added, removed), file| {
        let old_text = file.old_text.as_deref().unwrap_or("");
        let new_text = file.new_text.as_deref().unwrap_or("");
        let hunks = line_diff(old_text, new_text);
        hunks
            .iter()
            .fold((added, removed), |(a, r), (old_range, new_range)| {
                (
                    a + (new_range.end - new_range.start) as usize,
                    r + (old_range.end - old_range.start) as usize,
                )
            })
    })
}

struct GitGraphContextMenu {
    menu: Entity<ContextMenu>,
    position: Point<Pixels>,
    target_entry_index: Option<usize>,
    _subscription: Subscription,
}

#[derive(Clone)]
struct RefMenuEntry {
    graph_ref: Option<GraphRef>,
    sha: Oid,
    action_context: Option<GraphActionContext>,
    compare: CompareMenuEntries,
    git_tasks: Vec<(TaskSourceKind, ResolvedTask)>,
}

impl RefMenuEntry {
    fn add_entries(
        self,
        mut menu: ContextMenu,
        git_graph: &Entity<GitGraph>,
        window: &mut Window,
    ) -> ContextMenu {
        if let (Some(graph_ref), Some(action_context)) = (self.graph_ref, self.action_context) {
            menu = add_ref_entries(menu, graph_ref, self.sha, action_context);
        }
        menu = self.compare.add_entries(menu, git_graph.downgrade());
        add_custom_command_entries(menu, self.git_tasks, git_graph, window)
    }
}

/// Adds the "Custom Commands" section listing the `git-command` tasks, when
/// there are any.
fn add_custom_command_entries(
    mut menu: ContextMenu,
    git_tasks: Vec<(TaskSourceKind, ResolvedTask)>,
    git_graph: &Entity<GitGraph>,
    window: &mut Window,
) -> ContextMenu {
    if git_tasks.is_empty() {
        return menu;
    }

    menu = menu.separator().header("Custom Commands");
    for (task_source_kind, resolved_task) in git_tasks {
        let label = resolved_task.display_label().to_string();
        menu = menu.entry(
            label,
            None,
            window.handler_for(git_graph, move |this, window, cx| {
                this.schedule_git_task(task_source_kind.clone(), resolved_task.clone(), window, cx);
            }),
        );
    }
    menu
}

struct DetailPanelCommitMessage {
    sha: Oid,
    message: Entity<Markdown>,
    scroll_handle: ScrollHandle,
}

pub struct GitGraph {
    focus_handle: FocusHandle,
    search_state: SearchState,
    graph_data: GraphData,
    git_store: Entity<GitStore>,
    workspace: WeakEntity<Workspace>,
    context_menu: Option<GitGraphContextMenu>,
    table_interaction_state: Entity<TableInteractionState>,
    column_widths: Entity<RedistributableColumnsState>,
    /// Per-column visibility mask owned by the view (not the resize state) so columns can be
    /// hidden regardless of whether the table is resizable. `true` means the column is hidden.
    column_visibility: TableRow<bool>,
    selected_entry_idx: Option<usize>,
    hovered_entry_idx: Option<usize>,
    graph_canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    log_source: LogSource,
    log_order: LogOrder,
    selected_commit_diff: Option<CommitDiff>,
    selected_commit_diff_stats: Option<(usize, usize)>,
    _commit_diff_task: Option<Task<()>>,
    comparison: Option<GraphComparison>,
    selected_commit_message: Option<DetailPanelCommitMessage>,
    _selected_commit_message_task: Option<Task<()>>,
    commit_details_split_state: Entity<SplitState>,
    repo_id: RepositoryId,
    changed_files_scroll_handle: UniformListScrollHandle,
    changed_files_view_mode: ChangedFilesViewMode,
    commit_details_metadata_collapsed: bool,
    commit_details_message_collapsed: bool,
    changed_files_expanded_dirs: HashMap<RepoPath, bool>,
    pending_select_sha: Option<Oid>,
    nav_history: Option<ItemNavHistory>,
    /// Lanes the graph column is currently sized for; `0` refits on the next render.
    auto_graph_lanes: usize,
    /// The column widths last set by auto-sizing.
    auto_column_widths: Option<Vec<DefiniteLength>>,
    /// Set once the user resizes a column, which turns auto-sizing off.
    graph_width_manual: bool,
}

impl GitGraph {
    fn invalidate_state(&mut self, cx: &mut Context<Self>) {
        self.graph_data.clear();
        self.auto_graph_lanes = 0;
        self.search_state.matches.clear();
        self.search_state.selected_index = None;
        self.search_state.state.next_state();
        self.context_menu = None;
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    /// Computes the height of a single commit row in the git graph.
    ///
    /// The returned value is snapped to the nearest physical pixel. This is
    /// required so that the canvas's float math and the `uniform_list` layout
    /// (which snaps to device pixels) agree on row positions; otherwise rows
    /// drift apart as the user scrolls when `ui_font_size` is fractional.
    fn row_height(window: &Window, _cx: &App) -> Pixels {
        let rem_size = window.rem_size();
        let line_height = window.text_style().line_height_in_pixels(rem_size);
        let raw = line_height + ROW_VERTICAL_PADDING;
        let scale = window.scale_factor();

        (raw * scale).round() / scale
    }

    fn visible_row_count(&self, window: &Window, cx: &App) -> usize {
        let row_height = Self::row_height(window, cx);
        let viewport_height = self
            .table_interaction_state
            .read(cx)
            .scroll_handle
            .0
            .borrow()
            .last_item_size
            .map_or(window.viewport_size().height, |size| size.item.height);

        ((viewport_height / row_height).ceil() as usize).min(self.graph_data.commits.len())
    }

    fn graph_canvas_content_width(&self) -> Pixels {
        (LANE_WIDTH * self.graph_data.max_lanes.max(6) as f32) + LEFT_PADDING * 2.0
    }

    /// Returns the column fractions in display order:
    /// `[graph, description, date, author, commit]`.
    /// For path history there is no graph column, so its fraction is 0.
    fn preview_column_fractions(&self, window: &Window, cx: &App) -> [f32; 5] {
        let fractions = self
            .column_widths
            .read(cx)
            .rendered_fractions(Some(&self.column_visibility), window.rem_size());

        // Hidden columns occupy no space in the layout, so report them as zero here even though
        // the shared redistribution helper preserves their stored width for when they return.
        let value = |idx: usize| {
            if self.column_visibility.get(idx).copied().unwrap_or(false) {
                0.0
            } else {
                fractions[idx]
            }
        };

        let is_path_history = matches!(self.log_source, LogSource::Path(_));
        let graph_fraction = if is_path_history { 0.0 } else { value(0) };
        let offset = if is_path_history { 0 } else { 1 };

        [
            graph_fraction,
            value(offset),
            value(offset + 1),
            value(offset + 2),
            value(offset + 3),
        ]
    }

    fn table_column_width_config(&self, window: &Window, cx: &App) -> ColumnWidthConfig {
        // With fixed pixel widths, Description takes whatever the others leave.
        let offset = if matches!(self.log_source, LogSource::Path(_)) {
            0
        } else {
            1
        };
        let state = self.column_widths.read(cx);
        if state.fill_column() == Some(offset) {
            let widths = state.widths_to_render();
            let table_widths = &widths.as_slice()[offset..];
            if table_widths[1..]
                .iter()
                .all(|width| matches!(width, Length::Definite(DefiniteLength::Absolute(_))))
            {
                return ColumnWidthConfig::explicit_lengths(table_widths.to_vec());
            }
        }

        let [_, description, date, author, commit] = self.preview_column_fractions(window, cx);
        let table_total = description + date + author + commit;

        let widths = if table_total > 0.0 {
            vec![
                DefiniteLength::Fraction(description / table_total),
                DefiniteLength::Fraction(date / table_total),
                DefiniteLength::Fraction(author / table_total),
                DefiniteLength::Fraction(commit / table_total),
            ]
        } else {
            vec![DefiniteLength::Fraction(0.25); 4]
        };

        ColumnWidthConfig::explicit(widths)
    }

    /// Sizes the columns: the graph column fits the lanes used by `rows`, the
    /// Date, Author and Commit columns get fixed widths, and Description takes
    /// the rest. The widths are in pixels, so only Description changes when
    /// the container resizes. The graph column only grows while scrolling, so
    /// the other columns don't shift back and forth, and it refits after a
    /// reload or a jump to a commit (`auto_graph_lanes == 0`). Stops once the
    /// user resizes a column.
    fn fit_graph_column_to_rows(
        &mut self,
        rows: Range<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        const MIN_LANES: usize = 3;
        const MAX_GRAPH_FRACTION: f32 = 0.4;
        // Fits a date like "21.08 15:58".
        const DATE_COLUMN_WIDTH: Rems = rems(6.);
        // Fits the avatar and the start of the author name.
        const AUTHOR_COLUMN_WIDTH: Rems = rems(7.5);
        // Fits a short SHA.
        const COMMIT_COLUMN_WIDTH: Rems = rems(5.5);
        const DESCRIPTION_COLUMN: usize = 1;

        let hidden = self.column_visibility.as_slice();
        if self.graph_width_manual
            || hidden.len() != 5
            || hidden[0]
            || hidden[DESCRIPTION_COLUMN]
            || matches!(self.log_source, LogSource::Path(_))
        {
            return;
        }
        let column_widths = self.column_widths.read(cx);
        let container_width = column_widths.cached_container_width();
        if container_width <= px(0.) {
            return;
        }
        let current = column_widths.preview_widths().as_slice().to_vec();
        if self
            .auto_column_widths
            .as_ref()
            .is_some_and(|auto_widths| *auto_widths != current)
        {
            self.graph_width_manual = true;
            return;
        }

        let lanes = self
            .graph_data
            .max_lanes_in_rows(rows)
            .max(MIN_LANES)
            .max(self.auto_graph_lanes);
        self.auto_graph_lanes = lanes;

        // The cap is the only part that depends on the container width.
        let rem_size = window.rem_size();
        let graph_width = (LANE_WIDTH * lanes as f32 + LEFT_PADDING * 2.0)
            .min(container_width * MAX_GRAPH_FRACTION);
        let pixels = |width: Pixels| DefiniteLength::Absolute(AbsoluteLength::Pixels(width));
        let widths = vec![
            pixels(graph_width),
            current[DESCRIPTION_COLUMN],
            pixels(DATE_COLUMN_WIDTH.to_pixels(rem_size)),
            pixels(AUTHOR_COLUMN_WIDTH.to_pixels(rem_size)),
            pixels(COMMIT_COLUMN_WIDTH.to_pixels(rem_size)),
        ];
        if self.auto_column_widths.as_ref() == Some(&widths) {
            return;
        }

        let cols = widths.len();
        self.column_widths.update(cx, |state, _| {
            state.set_widths(TableRow::from_vec(widths.clone(), cols));
            state.set_fill_column(Some(DESCRIPTION_COLUMN));
        });
        self.auto_column_widths = Some(widths);
        cx.notify();
    }

    /// The graph column's width in pixels, when it has a fixed width.
    fn graph_column_pixel_width(&self, window: &Window, cx: &App) -> Option<Pixels> {
        if matches!(self.log_source, LogSource::Path(_)) {
            return None;
        }
        let state = self.column_widths.read(cx);
        match state.preview_widths().as_slice().first()? {
            DefiniteLength::Absolute(width) => Some(width.to_pixels(window.rem_size())),
            DefiniteLength::Fraction(_) => None,
        }
    }

    fn graph_viewport_width(&self, window: &Window, cx: &App) -> Pixels {
        if let Some(width) = self.graph_column_pixel_width(window, cx) {
            return width;
        }
        let container = self.column_widths.read(cx).cached_container_width();
        let graph_fraction = self.preview_column_fractions(window, cx)[0];
        if container > px(0.) && graph_fraction > 0.0 {
            container * graph_fraction
        } else {
            self.graph_canvas_content_width()
        }
    }

    pub fn new(
        repo_id: RepositoryId,
        git_store: Entity<GitStore>,
        workspace: WeakEntity<Workspace>,
        log_source: Option<LogSource>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        cx.on_focus(&focus_handle, window, |_, _, cx| cx.notify())
            .detach();
        // Like focusing an editor, focusing the graph makes its repository the
        // active one, so git actions such as fetch and push apply to it.
        cx.on_focus_in(&focus_handle, window, |this: &mut Self, _, cx| {
            this.activate_repository(cx);
        })
        .detach();

        let accent_colors = cx.theme().accents();
        let graph = GraphData::new(accent_colors_count(accent_colors));
        let log_source = log_source.unwrap_or_default();
        let log_order = LogOrder::default();

        cx.subscribe(&git_store, |this, _, event, cx| match event {
            GitStoreEvent::RepositoryUpdated(updated_repo_id, repo_event, _) => {
                if this.repo_id == *updated_repo_id {
                    if let Some(repository) = this.get_repository(cx) {
                        this.on_repository_event(repository, repo_event, cx);
                    }
                }
            }
            _ => {}
        })
        .detach();

        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search commits…", window, cx);
            editor
        });

        let table_interaction_state = cx.new(|cx| {
            let mut state = TableInteractionState::new(cx);
            state.focus_handle = state.focus_handle.tab_index(1).tab_stop(true);
            state
        });

        let column_widths = if matches!(log_source, LogSource::Path(_)) {
            cx.new(|_cx| {
                RedistributableColumnsState::new(
                    4,
                    vec![
                        DefiniteLength::Fraction(0.64),
                        DefiniteLength::Fraction(0.14),
                        DefiniteLength::Fraction(0.12),
                        DefiniteLength::Fraction(0.10),
                    ],
                    vec![TableResizeBehavior::Resizable; 4],
                )
            })
        } else {
            cx.new(|_cx| {
                RedistributableColumnsState::new(
                    5,
                    vec![
                        DefiniteLength::Fraction(0.30),
                        DefiniteLength::Fraction(0.37),
                        DefiniteLength::Fraction(0.13),
                        DefiniteLength::Fraction(0.11),
                        DefiniteLength::Fraction(0.09),
                    ],
                    vec![TableResizeBehavior::Resizable; 5],
                )
            })
        };
        // Re-fit the columns as soon as the table's width changes.
        cx.observe(&column_widths, |_, _, cx| cx.notify()).detach();
        let column_count = if matches!(log_source, LogSource::Path(_)) {
            TABLE_COLUMN_COUNT
        } else {
            TABLE_COLUMN_COUNT + 1
        };
        // The "Commit" (SHA) column is last and hidden by default.
        let column_visibility = TableRow::from_vec(
            (0..column_count)
                .map(|idx| idx == column_count - 1)
                .collect(),
            column_count,
        );
        let mut row_height = Self::row_height(window, cx);

        cx.observe_global_in::<settings::SettingsStore>(window, move |this, window, cx| {
            let new_row_height = Self::row_height(window, cx);
            if new_row_height != row_height {
                // The `uniform_list` powering the table caches the item size
                // from its last layout; invalidate it so it re-measures with
                // the new row height on the next frame.
                this.table_interaction_state.update(cx, |state, _cx| {
                    state.scroll_handle.0.borrow_mut().last_item_size = None;
                });
                row_height = new_row_height;
                cx.notify();
            }
        })
        .detach();

        let mut this = GitGraph {
            focus_handle,
            git_store,
            search_state: SearchState {
                case_sensitive: false,
                editor: search_editor,
                matches: IndexSet::default(),
                selected_index: None,
                state: QueryState::Empty,
            },
            workspace,
            graph_data: graph,
            _commit_diff_task: None,
            comparison: None,
            context_menu: None,
            table_interaction_state,
            column_widths,
            column_visibility,
            selected_entry_idx: None,
            hovered_entry_idx: None,
            graph_canvas_bounds: Rc::new(Cell::new(None)),
            selected_commit_diff: None,
            selected_commit_diff_stats: None,
            selected_commit_message: None,
            _selected_commit_message_task: None,
            log_source,
            log_order,
            commit_details_split_state: cx.new(|_cx| SplitState::new()),
            repo_id,
            changed_files_scroll_handle: UniformListScrollHandle::new(),
            changed_files_view_mode: ChangedFilesViewMode::default(),
            commit_details_metadata_collapsed: read_commit_details_collapsed(
                COMMIT_DETAILS_METADATA_COLLAPSED_KEY,
                cx,
            ),
            commit_details_message_collapsed: read_commit_details_collapsed(
                COMMIT_DETAILS_MESSAGE_COLLAPSED_KEY,
                cx,
            ),
            changed_files_expanded_dirs: HashMap::default(),
            pending_select_sha: None,
            auto_graph_lanes: 0,
            auto_column_widths: None,
            graph_width_manual: false,
            nav_history: None,
        };

        this.fetch_initial_graph_data(cx);
        this
    }

    fn on_repository_event(
        &mut self,
        repository: Entity<Repository>,
        event: &RepositoryEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            RepositoryEvent::GraphEvent((source, order), event)
                if source == &self.log_source && order == &self.log_order =>
            {
                match event {
                    GitGraphEvent::FullyLoaded => {
                        if let Some(pending_sha_index) =
                            self.pending_select_sha.take().and_then(|oid| {
                                repository
                                    .read(cx)
                                    .get_graph_data(source.clone(), *order)
                                    .and_then(|data| data.commit_oid_to_index.get(&oid).copied())
                            })
                        {
                            self.select_entry(pending_sha_index, ScrollStrategy::Nearest, cx);
                        }
                        let count = match self.graph_data.max_commit_count {
                            AllCommitCount::FullyLoaded(count) | AllCommitCount::Loading(count) => {
                                count
                            }
                            AllCommitCount::NotLoaded => 0,
                        };
                        self.graph_data.max_commit_count = AllCommitCount::FullyLoaded(count);
                        cx.notify();
                    }
                    GitGraphEvent::LoadingError => {
                        cx.notify();
                    }
                    GitGraphEvent::CountUpdated(commit_count) => {
                        let old_count = self.graph_data.commits.len();

                        if let Some(pending_selection_index) =
                            repository.update(cx, |repository, cx| {
                                let GraphDataResponse {
                                    commits,
                                    is_loading,
                                    error: _,
                                } = repository.graph_data(
                                    source.clone(),
                                    *order,
                                    old_count..*commit_count,
                                    cx,
                                );
                                self.graph_data.add_commits(commits);

                                let pending_sha_index = self.pending_select_sha.and_then(|oid| {
                                    repository.get_graph_data(source.clone(), *order).and_then(
                                        |data| data.commit_oid_to_index.get(&oid).copied(),
                                    )
                                });

                                if !is_loading && pending_sha_index.is_none() {
                                    self.pending_select_sha.take();
                                }

                                pending_sha_index
                            })
                        {
                            self.select_entry(pending_selection_index, ScrollStrategy::Nearest, cx);
                            self.pending_select_sha.take();
                        }

                        cx.notify();
                    }
                }
            }
            RepositoryEvent::TagsChanged => {
                self.pending_select_sha = None;
                self.invalidate_state(cx);
            }
            RepositoryEvent::HeadChanged | RepositoryEvent::BranchListChanged => {
                // Only invalidate if we scanned atleast once,
                // meaning we are not inside the initial repo loading state
                // NOTE: this fixes an loading performance regression
                if repository.read(cx).scan_id > 1 {
                    self.pending_select_sha = None;
                    self.invalidate_state(cx);
                }
            }
            RepositoryEvent::StashEntriesChanged if self.log_source == LogSource::All => {
                // Stash entries initial's scan id is 2, so we don't want to invalidate the graph before that
                if repository.read(cx).scan_id > 2 {
                    self.pending_select_sha = None;
                    self.invalidate_state(cx);
                }
            }
            RepositoryEvent::GraphEvent(_, _) => {}
            _ => {}
        }
    }

    fn fetch_initial_graph_data(&mut self, cx: &mut App) {
        if let Some(repository) = self.get_repository(cx) {
            repository.update(cx, |repository, cx| {
                let commits = repository
                    .graph_data(self.log_source.clone(), self.log_order, 0..usize::MAX, cx)
                    .commits;
                self.graph_data.add_commits(commits);
            });
        }
    }

    fn get_repository(&self, cx: &App) -> Option<Entity<Repository>> {
        let git_store = self.git_store.read(cx);
        git_store.repositories().get(&self.repo_id).cloned()
    }

    /// Checks whether a ref name from git's `%D` decoration
    ///  format refers to the currently checked-out branch.
    fn is_head_ref(ref_name: &str, head_branch_name: &Option<SharedString>) -> bool {
        head_branch_name.as_ref().is_some_and(|head| {
            ref_name == head.as_ref() || ref_name.strip_prefix("HEAD -> ") == Some(head.as_ref())
        })
    }

    /// Extracts a ref name (branch, remote ref, or tag) from a decoration in
    /// git's `%D` format, returning `None` for a detached `HEAD`.
    fn ref_name_from_decoration(decoration: &str) -> Option<SharedString> {
        let name = decoration
            .strip_prefix("tag: ")
            .or_else(|| decoration.strip_prefix("HEAD -> "))
            .unwrap_or(decoration);
        if name.is_empty() || name == "HEAD" {
            return None;
        }
        Some(SharedString::from(name.to_string()))
    }

    /// Groups a commit's `%D` decorations into the chips shown in the
    /// description column: the checked-out branch first, then other local
    /// branches, then remote-only branches, then tags. A remote branch that
    /// points at the same commit as a local branch of the same name is folded
    /// into that local branch's chip as its remote badge.
    fn ref_pills(
        ref_names: &[SharedString],
        head_branch_name: &Option<SharedString>,
        remote_names: &[SharedString],
    ) -> Vec<RefPill> {
        let mut local_branches = Vec::new();
        let mut remote_branches = Vec::new();
        let mut tags = Vec::new();
        let mut detached_head = None;

        for decoration in ref_names {
            if decoration == "HEAD" {
                detached_head = Some(RefPill {
                    kind: RefPillKind::DetachedHead,
                    name: decoration.clone(),
                    remote: None,
                    is_head: true,
                    decoration: decoration.clone(),
                });
                continue;
            }
            let Some(name) = Self::ref_name_from_decoration(decoration) else {
                continue;
            };
            if decoration.starts_with("tag: ") {
                tags.push(RefPill {
                    kind: RefPillKind::Tag,
                    name,
                    remote: None,
                    is_head: false,
                    decoration: decoration.clone(),
                });
                continue;
            }

            let remote_branch = name
                .split_once('/')
                .filter(|(remote, _)| remote_names.iter().any(|known| known.as_ref() == *remote));
            if let Some((remote, branch)) = remote_branch {
                if branch != "HEAD" {
                    remote_branches.push((
                        SharedString::from(remote.to_string()),
                        SharedString::from(branch.to_string()),
                        decoration.clone(),
                    ));
                }
                continue;
            }

            local_branches.push(RefPill {
                kind: RefPillKind::Branch,
                is_head: Self::is_head_ref(decoration, head_branch_name),
                name,
                remote: None,
                decoration: decoration.clone(),
            });
        }

        let mut remote_only_branches = Vec::new();
        for (remote, branch, decoration) in remote_branches {
            match local_branches
                .iter_mut()
                .find(|pill| pill.remote.is_none() && pill.name == branch)
            {
                Some(local) => local.remote = Some(remote),
                None => remote_only_branches.push(RefPill {
                    kind: RefPillKind::Branch,
                    name: branch,
                    remote: Some(remote),
                    is_head: false,
                    decoration,
                }),
            }
        }

        local_branches.sort_by_key(|pill| !pill.is_head);
        let is_head_commit =
            detached_head.is_some() || local_branches.iter().any(|pill| pill.is_head);
        for tag in &mut tags {
            tag.is_head = is_head_commit;
        }

        detached_head
            .into_iter()
            .chain(local_branches)
            .chain(remote_only_branches)
            .chain(tags)
            .collect()
    }

    /// The checked-out branch and the known remote names, which `ref_pills`
    /// needs to group a commit's refs into chips.
    fn ref_pill_context(
        snapshot: Option<&RepositorySnapshot>,
    ) -> (Option<SharedString>, Vec<SharedString>) {
        let head_branch_name: Option<SharedString> = snapshot.and_then(|snapshot| {
            snapshot
                .branch
                .as_ref()
                .map(|branch| SharedString::from(branch.name().to_string()))
        });
        let mut remote_names: Vec<SharedString> = Vec::new();
        if let Some(snapshot) = snapshot {
            let known_remotes = snapshot
                .branch_list
                .iter()
                .filter_map(|branch| branch.remote_name())
                .chain(snapshot.remote_origin_url.as_ref().map(|_| "origin"))
                .chain(snapshot.remote_upstream_url.as_ref().map(|_| "upstream"));
            for remote in known_remotes {
                if !remote_names.iter().any(|known| known.as_ref() == remote) {
                    remote_names.push(SharedString::from(remote.to_string()));
                }
            }
        }
        (head_branch_name, remote_names)
    }

    /// Splits `pills` into the ones that fit within `available_width` and the
    /// hidden remainder, reserving room for the "+N" chip when anything is
    /// hidden. The first pill is always shown since its label is length-capped.
    fn fit_ref_pills(
        pills: Vec<RefPill>,
        available_width: Pixels,
        pill_width: impl Fn(&RefPill) -> Pixels,
        count_chip_width: Pixels,
        gap: Pixels,
    ) -> (Vec<RefPill>, Vec<RefPill>) {
        let widths = pills.iter().map(&pill_width).collect::<Vec<_>>();
        let mut used = px(0.);
        let mut visible_count = 0;
        for (idx, width) in widths.iter().enumerate() {
            let gap_before = if idx == 0 { px(0.) } else { gap };
            let has_more = idx + 1 < widths.len();
            let reserve = if has_more {
                gap + count_chip_width
            } else {
                px(0.)
            };
            if idx > 0 && used + gap_before + *width + reserve > available_width {
                break;
            }
            used += gap_before + *width;
            visible_count += 1;
        }

        let mut visible = pills;
        let hidden = visible.split_off(visible_count);
        (visible, hidden)
    }

    /// Renders a ref chip for the commit at `commit_idx`. Chips get a
    /// right-click handler that opens a ref-specific context menu, so that
    /// custom commands can be resolved against the clicked ref.
    fn render_ref_chip(
        &self,
        name: SharedString,
        accent_color: gpui::Hsla,
        is_head: bool,
        commit_idx: usize,
        background: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tooltip_text = name.clone();
        let chip_id = SharedString::from(format!("git-graph-ref-chip-{commit_idx}-{name}"));

        div()
            .id(chip_id)
            .min_w_0()
            .overflow_hidden()
            .child(
                Chip::new(name.clone())
                    .label_size(LabelSize::Small)
                    .truncate()
                    .map(|chip| {
                        if is_head {
                            chip.icon(IconName::Check)
                                .bg_color(background.blend(accent_color.opacity(0.25)))
                                .border_color(accent_color.opacity(0.5))
                        } else {
                            chip.icon(IconName::GitBranch)
                                .icon_color(Color::Custom(accent_color))
                                .bg_color(background.blend(accent_color.opacity(0.08)))
                                .border_color(accent_color.opacity(0.25))
                        }
                    }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();

                        this.deploy_ref_context_menu(
                            event.position,
                            commit_idx,
                            name.clone(),
                            window,
                            cx,
                        );
                    }
                }),
            )
            .tooltip(move |_window, cx| Tooltip::simple(tooltip_text.clone(), cx))
            .into_any_element()
    }

    /// Renders a branch or tag chip for the description column. The checked-out
    /// branch (and tags on its commit) get an accent icon well, and a branch
    /// that also exists on a remote shows that remote as a trailing badge.
    fn render_ref_pill(
        &self,
        pill: &RefPill,
        accent_color: Hsla,
        commit_idx: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let head_color = colors.text_accent;
        let is_head_branch = pill.is_head && pill.kind != RefPillKind::Tag;
        let border_color = if is_head_branch {
            head_color.opacity(0.7)
        } else {
            colors.border_variant
        };
        let background = if is_head_branch {
            colors.editor_background.blend(head_color.opacity(0.15))
        } else {
            colors.element_background
        };
        let icon = match pill.kind {
            RefPillKind::Branch => IconName::GitBranch,
            RefPillKind::Tag => IconName::Tag,
            RefPillKind::DetachedHead => IconName::GitCommit,
        };
        let icon_color = if pill.is_head {
            Color::Custom(gpui::white())
        } else {
            Color::Custom(accent_color)
        };
        let tooltip_text = pill.tooltip_text();
        let decoration = pill.decoration.clone();
        let kind = pill.kind;
        let checkout_branch_name = pill.checkout_branch();
        let chip_id = SharedString::from(format!(
            "git-graph-ref-pill-{commit_idx}-{}",
            pill.decoration
        ));

        h_flex()
            .id(chip_id)
            .flex_none()
            .items_center()
            .border_1()
            .rounded_sm()
            .border_color(border_color)
            .bg(background)
            .overflow_hidden()
            .child(
                h_flex()
                    .px_1()
                    .self_stretch()
                    .items_center()
                    .when(pill.is_head, |this| this.bg(head_color))
                    .child(Icon::new(icon).size(IconSize::XSmall).color(icon_color)),
            )
            .child(
                div().px_1().child(
                    Label::new(pill.display_name())
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .when(pill.is_head, |label| {
                            label.weight(gpui::FontWeight::SEMIBOLD)
                        }),
                ),
            )
            .when_some(pill.remote.clone(), |this, remote| {
                // The remote badge stands for the remote branch, so it opens
                // that branch's menu rather than the local branch's.
                let remote_decoration = SharedString::from(format!("{remote}/{}", pill.name));
                this.child(
                    div()
                        .id("remote")
                        .px_1()
                        .border_l_1()
                        .border_color(border_color)
                        .child(
                            Label::new(remote)
                                .size(LabelSize::XSmall)
                                .italic()
                                .color(Color::Muted),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                this.deploy_ref_context_menu(
                                    event.position,
                                    commit_idx,
                                    remote_decoration.clone(),
                                    window,
                                    cx,
                                );
                            }),
                        ),
                )
            })
            .tooltip(move |_window, cx| Tooltip::simple(tooltip_text.clone(), cx))
            .when_some(checkout_branch_name, |this, branch_name| {
                this.on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    if event.click_count() < 2 {
                        return;
                    }
                    cx.stop_propagation();
                    if let Some(repository) = this.get_repository(cx) {
                        checkout_branch(repository, branch_name.to_string(), window, cx);
                    }
                }))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    // A detached HEAD is the commit itself, so it gets the
                    // commit's menu.
                    if kind == RefPillKind::DetachedHead {
                        this.deploy_entry_context_menu(
                            event.position,
                            commit_idx,
                            None,
                            window,
                            cx,
                        );
                        return;
                    }
                    this.deploy_ref_context_menu(
                        event.position,
                        commit_idx,
                        decoration.clone(),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
    }

    /// Estimates the rendered width of a `render_ref_pill` chip so the row can
    /// decide which chips fit before the description.
    fn ref_pill_width(pill: &RefPill, window: &Window, cx: &App) -> Pixels {
        let buffer_font = theme::theme_settings(cx).buffer_font(cx).clone();
        let text_width = |text: &str, size: TextSize| {
            let font_size = size.rems(cx).to_pixels(window.rem_size());
            window
                .text_system()
                .layout_line(
                    text,
                    font_size,
                    &[TextRun {
                        len: text.len(),
                        font: buffer_font.clone(),
                        color: gpui::black(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                )
                .width
        };
        let padding = rems(0.25).to_pixels(window.rem_size());
        let icon_width = IconSize::XSmall.rems().to_pixels(window.rem_size());
        let border = px(1.);

        let mut width = border * 2.
            + (icon_width + padding * 2.)
            + (text_width(&pill.display_name(), TextSize::Small) + padding * 2.);
        if let Some(remote) = &pill.remote {
            width += border + text_width(remote, TextSize::XSmall) + padding * 2.;
        }
        width
    }

    fn render_ref_count_chip(
        &self,
        hidden_refs: Vec<SharedString>,
        accent_color: gpui::Hsla,
        commit_idx: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hidden_refs_text = hidden_refs
            .iter()
            .map(|ref_name| format!("- {ref_name}"))
            .collect::<Vec<_>>()
            .join("\n");
        let chip_id = SharedString::from(format!("git-graph-ref-count-chip-{commit_idx}"));

        div()
            .id(chip_id)
            .flex_none()
            .cursor_pointer()
            .child(
                Chip::new(format!("+{}", hidden_refs.len()))
                    .label_size(LabelSize::Small)
                    .label_color(Color::Muted)
                    .border_color(accent_color.opacity(0.25)),
            )
            .tooltip(move |_window, cx| Tooltip::simple(hidden_refs_text.clone(), cx))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();

                    this.deploy_hidden_refs_context_menu(
                        event.position,
                        commit_idx,
                        hidden_refs.clone(),
                        window,
                        cx,
                    );
                }),
            )
            .into_any_element()
    }

    fn render_table_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Vec<AnyElement>> {
        self.fit_graph_column_to_rows(range.clone(), window, cx);
        let repository = self.get_repository(cx);

        let row_height = Self::row_height(window, cx);

        let remote = repository.as_ref().and_then(|repository| {
            let remote_url = repository.read(cx).default_remote_url()?;
            let provider_registry = GitHostingProviderRegistry::default_global(cx);
            let (provider, parsed) = parse_git_remote_url(provider_registry, &remote_url)?;
            Some(GitRemote {
                host: provider,
                owner: parsed.owner.into(),
                repo: parsed.repo.into(),
            })
        });

        let snapshot = repository
            .as_ref()
            .map(|repository| repository.read(cx).snapshot());
        let (head_branch_name, remote_names) = Self::ref_pill_context(snapshot.as_ref());
        let accent_colors = cx.theme().accents().clone();
        let rem_size = window.rem_size();
        let pill_gap = rems(0.25).to_pixels(rem_size);
        let count_chip_width = rems(2.).to_pixels(rem_size);
        let description_width = self.column_widths.read(cx).cached_container_width()
            * self.preview_column_fractions(window, cx)[1]
            - rems(1.).to_pixels(rem_size);

        // We fetch data outside the visible viewport to avoid loading entries when
        // users scroll through the git graph
        if let Some(repository) = repository.as_ref() {
            const FETCH_RANGE: usize = 100;
            repository.update(cx, |repository, cx| {
                self.graph_data.commits[range.start.saturating_sub(FETCH_RANGE)
                    ..(range.end + FETCH_RANGE)
                        .min(self.graph_data.commits.len().saturating_sub(1))]
                    .iter()
                    .for_each(|commit| {
                        repository.fetch_commit_data(commit.data.sha, false, cx);
                    });
            });
        }

        range
            .map(|idx| {
                let Some((commit, repository)) =
                    self.graph_data.commits.get(idx).zip(repository.as_ref())
                else {
                    return (0..4)
                        .map(|_| div().h(row_height).into_any_element())
                        .collect();
                };

                let data = repository.update(cx, |repository, cx| {
                    repository
                        .fetch_commit_data(commit.data.sha, false, cx)
                        .clone()
                });

                let short_sha = commit.data.sha.display_short();
                let mut formatted_time = String::new();
                let subject: SharedString;
                let author_name: Option<SharedString>;
                let author_email: Option<SharedString>;

                if let CommitDataState::Loaded(ref data) = data {
                    subject = data.subject.clone();
                    author_name = (!data.author_name.is_empty()).then(|| data.author_name.clone());
                    author_email =
                        (!data.author_email.is_empty()).then(|| data.author_email.clone());
                    formatted_time =
                        format_timestamp_with(data.commit_timestamp, compact_timestamp_format());
                } else {
                    subject = "Loading…".into();
                    author_name = None;
                    author_email = None;
                }

                let pills =
                    Self::ref_pills(&commit.data.ref_names, &head_branch_name, &remote_names);
                let (visible_pills, hidden_pills) = if description_width > px(0.) {
                    Self::fit_ref_pills(
                        pills,
                        description_width,
                        |pill| Self::ref_pill_width(pill, window, cx),
                        count_chip_width,
                        pill_gap,
                    )
                } else {
                    (pills, Vec::new())
                };
                let accent_color = accent_colors.color_for_index(commit.color_idx as u32);
                let pill_elements = visible_pills
                    .iter()
                    .map(|pill| self.render_ref_pill(pill, accent_color, idx, cx))
                    .collect::<Vec<_>>();
                let count_chip = (!hidden_pills.is_empty()).then(|| {
                    self.render_ref_count_chip(
                        hidden_pills
                            .iter()
                            .map(|pill| pill.decoration.clone())
                            .collect(),
                        accent_color,
                        idx,
                        cx,
                    )
                });

                let is_selected = self.selected_entry_idx == Some(idx);
                let is_matched = self.search_state.matches.contains(&commit.data.sha);
                let column_label = |label: SharedString| {
                    Label::new(label)
                        .when(!is_selected, |c| c.color(Color::Muted))
                        .truncate()
                        .into_any_element()
                };

                let subject_label = if is_matched {
                    let query = match &self.search_state.state {
                        QueryState::Confirmed((query, _)) => Some(query.clone()),
                        _ => None,
                    };
                    let highlight_ranges = query
                        .and_then(|q| {
                            let ranges = if self.search_state.case_sensitive {
                                subject
                                    .match_indices(q.as_str())
                                    .map(|(start, matched)| start..start + matched.len())
                                    .collect::<Vec<_>>()
                            } else {
                                let q = q.to_lowercase();
                                let subject_lower = subject.to_lowercase();

                                subject_lower
                                    .match_indices(&q)
                                    .filter_map(|(start, matched)| {
                                        let end = start + matched.len();
                                        subject.is_char_boundary(start).then_some(()).and_then(
                                            |_| subject.is_char_boundary(end).then_some(start..end),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            };

                            (!ranges.is_empty()).then_some(ranges)
                        })
                        .unwrap_or_default();
                    HighlightedLabel::from_ranges(subject, highlight_ranges)
                        .when(!is_selected, |c| c.color(Color::Muted))
                        .truncate()
                        .into_any_element()
                } else {
                    column_label(subject)
                };

                vec![
                    div()
                        .id(ElementId::NamedInteger("commit-subject".into(), idx as u64))
                        .overflow_hidden()
                        .child(
                            h_flex()
                                .gap_1()
                                .overflow_hidden()
                                .children(pill_elements)
                                .children(count_chip)
                                .child(div().min_w_0().flex_1().ml_1().child(subject_label)),
                        )
                        .into_any_element(),
                    column_label(formatted_time.into()),
                    {
                        let avatar_sha: SharedString = commit.data.sha.to_string().into();
                        let avatar = CommitAvatar::new(&avatar_sha, author_email, remote.as_ref())
                            .size(px(16.))
                            .render(window, cx);
                        h_flex()
                            .id(ElementId::NamedInteger("commit-author".into(), idx as u64))
                            .h(row_height)
                            .items_center()
                            .gap_1p5()
                            .overflow_hidden()
                            .child(avatar)
                            .when_some(author_name, |this, author_name| {
                                this.child(column_label(author_name))
                            })
                            .into_any_element()
                    },
                    column_label(short_sha.into()),
                ]
            })
            .collect()
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.clear_selected_commit(cx);
    }

    fn clear_selected_commit(&mut self, cx: &mut Context<Self>) {
        self.comparison = None;
        self.selected_entry_idx = None;
        self.selected_commit_diff = None;
        self.selected_commit_diff_stats = None;
        self.changed_files_expanded_dirs.clear();
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    fn select_first(&mut self, _: &SelectFirst, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_entry(0, ScrollStrategy::Nearest, cx);
    }

    fn select_prev(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected_entry_idx) = &self.selected_entry_idx {
            self.select_entry(
                selected_entry_idx.saturating_sub(1),
                ScrollStrategy::Nearest,
                cx,
            );
        } else {
            self.select_first(&SelectFirst, window, cx);
        }
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected_entry_idx) = &self.selected_entry_idx {
            self.select_entry(
                selected_entry_idx
                    .saturating_add(1)
                    .min(self.graph_data.commits.len().saturating_sub(1)),
                ScrollStrategy::Nearest,
                cx,
            );
        } else {
            self.select_prev(&SelectPrevious, window, cx);
        }
    }

    fn select_last(&mut self, _: &SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        self.select_entry(
            self.graph_data.commits.len().saturating_sub(1),
            ScrollStrategy::Nearest,
            cx,
        );
    }

    fn scroll_up(&mut self, _: &ScrollUp, window: &mut Window, cx: &mut Context<Self>) {
        let step = (self.visible_row_count(window, cx) / 2).max(1);
        let target_idx = self.selected_entry_idx.unwrap_or(0).saturating_sub(step);

        self.select_entry(target_idx, ScrollStrategy::Nearest, cx);
    }

    fn scroll_down(&mut self, _: &ScrollDown, window: &mut Window, cx: &mut Context<Self>) {
        let Some(last_entry_idx) = self.graph_data.commits.len().checked_sub(1) else {
            return;
        };

        let step = (self.visible_row_count(window, cx) / 2).max(1);
        let target_idx = self
            .selected_entry_idx
            .unwrap_or(0)
            .saturating_add(step)
            .min(last_entry_idx);

        self.select_entry(target_idx, ScrollStrategy::Nearest, cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.open_selected_commit_view(window, cx);
    }

    fn toggle_changed_files_view(
        &mut self,
        _: &ToggleChangedFilesView,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.changed_files_view_mode = self.changed_files_view_mode.toggled();
        self.changed_files_scroll_handle
            .scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn search(&mut self, query: SharedString, cx: &mut Context<Self>) {
        let Some(repo) = self.get_repository(cx) else {
            return;
        };

        self.search_state.matches.clear();
        self.search_state.selected_index = None;
        self.search_state.editor.update(cx, |editor, _cx| {
            editor.set_text_style_refinement(Default::default());
        });

        if query.as_str().is_empty() {
            self.search_state.state = QueryState::Empty;
            cx.notify();
            return;
        }

        let (request_tx, request_rx) = async_channel::unbounded::<Oid>();

        repo.update(cx, |repo, cx| {
            repo.search_commits(
                self.log_source.clone(),
                SearchCommitArgs {
                    query: query.clone(),
                    case_sensitive: self.search_state.case_sensitive,
                },
                request_tx,
                cx,
            );
        });

        let search_task = cx.spawn(async move |this, cx| {
            while let Ok(first_oid) = request_rx.recv().await {
                let mut pending_oids = vec![first_oid];
                while let Ok(oid) = request_rx.try_recv() {
                    pending_oids.push(oid);
                }

                this.update(cx, |this, cx| {
                    if this.search_state.selected_index.is_none() {
                        this.search_state.selected_index = Some(0);
                        this.select_commit_by_sha(first_oid, cx);
                    }

                    this.search_state.matches.extend(pending_oids);
                    cx.notify();
                })
                .ok();
            }

            this.update(cx, |this, cx| {
                if this.search_state.matches.is_empty() {
                    this.search_state.editor.update(cx, |editor, cx| {
                        editor.set_text_style_refinement(TextStyleRefinement {
                            color: Some(Color::Error.color(cx)),
                            ..Default::default()
                        });
                    });
                }
            })
            .ok();
        });

        self.search_state.state = QueryState::Confirmed((query, search_task));
        cx.emit(ItemEvent::Edit);
    }

    fn confirm_search(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let query = self.search_state.editor.read(cx).text(cx).into();
        self.search(query, cx);
    }

    fn activate_search_editor_if_focused(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_state.editor.update(cx, |editor, cx| {
            if editor.is_focused(window) {
                editor.select_all(&Default::default(), window, cx);
                editor.show_cursor(cx);
            }
        });
    }

    fn focus_next_tab_stop(
        &mut self,
        _: &FocusNextTabStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_next(cx);
        self.activate_search_editor_if_focused(window, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn focus_previous_tab_stop(
        &mut self,
        _: &FocusPreviousTabStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus_prev(cx);
        self.activate_search_editor_if_focused(window, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn select_entry(
        &mut self,
        idx: usize,
        scroll_strategy: ScrollStrategy,
        cx: &mut Context<Self>,
    ) {
        if self.selected_entry_idx == Some(idx) || idx >= self.graph_data.commits.len() {
            debug_assert!(
                idx < self.graph_data.commits.len(),
                "attempted to select out of bounds index: {idx}, commits.len: {}",
                self.graph_data.commits.len()
            );
            return;
        }

        self.selected_entry_idx = Some(idx);
        self.comparison = None;
        self.selected_commit_diff = None;
        self.selected_commit_diff_stats = None;
        self.changed_files_expanded_dirs.clear();
        self.changed_files_scroll_handle
            .scroll_to_item(0, ScrollStrategy::Top);
        self.table_interaction_state.update(cx, |state, cx| {
            state.scroll_handle.scroll_to_item(idx, scroll_strategy);
            cx.notify();
        });

        let Some(commit) = self.graph_data.commits.get(idx) else {
            return;
        };

        let Some(repository) = self.get_repository(cx) else {
            return;
        };

        let commit_message_handle = commit.data.sha;
        let diff_handle = commit.data.sha.to_string();

        self.load_selected_commit_message(cx, &commit_message_handle, &repository);

        let diff_task =
            repository.update(cx, |repo, cx| repo.load_commit_diff(diff_handle, false, cx));

        self._commit_diff_task = Some(cx.spawn(async move |this, cx| {
            if let Ok(diff) = diff_task.await {
                this.update(cx, |this, cx| {
                    let stats = compute_diff_stats(&diff);
                    this.selected_commit_diff = Some(diff);
                    this.selected_commit_diff_stats = Some(stats);
                    cx.notify();
                })
                .ok();
            }
        }));

        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    fn load_selected_commit_message(
        &mut self,
        cx: &mut Context<'_, Self>,
        sha: &Oid,
        repository: &Entity<Repository>,
    ) {
        if self
            .selected_commit_message
            .as_ref()
            .is_some_and(|old| old.sha == *sha)
        {
            return;
        }

        self._selected_commit_message_task = None;
        match repository.update(cx, |repo, cx| {
            repo.fetch_commit_data(*sha, true, cx).clone()
        }) {
            CommitDataState::Loaded(commit_data) => {
                self.set_selected_commit_message(cx, commit_data.sha, commit_data.message.clone());
            }
            CommitDataState::Loading(Some(receiver)) => {
                self._selected_commit_message_task = Some(cx.spawn(async move |this, cx| {
                    if let Ok(commit_data) = receiver.await {
                        this.update(cx, |this, cx| {
                            this.set_selected_commit_message(
                                cx,
                                commit_data.sha,
                                commit_data.message.clone(),
                            );
                        })
                        .log_err();
                    }
                }))
            }
            _ => {
                debug_panic!(
                    "Fetched commit data asynchronously, but was not given a listener or cached commit data."
                );
            }
        };
    }

    fn set_selected_commit_message(
        &mut self,
        cx: &mut Context<'_, GitGraph>,
        sha: Oid,
        message: SharedString,
    ) {
        let languages = self
            .workspace
            .read_with(cx, |workspace, cx| {
                workspace.project().read(cx).languages().clone()
            })
            .log_err();
        self.selected_commit_message = Some(DetailPanelCommitMessage {
            sha,
            message: cx.new(|cx| Markdown::new(message, languages, None, cx)),
            scroll_handle: ScrollHandle::new(),
        });
        self._selected_commit_message_task = None;
        cx.notify();
    }

    fn select_previous_match(&mut self, cx: &mut Context<Self>) {
        if self.search_state.matches.is_empty() {
            return;
        }

        let mut prev_selection = self.search_state.selected_index.unwrap_or_default();

        if prev_selection == 0 {
            prev_selection = self.search_state.matches.len() - 1;
        } else {
            prev_selection -= 1;
        }

        let Some(&oid) = self.search_state.matches.get_index(prev_selection) else {
            return;
        };

        self.search_state.selected_index = Some(prev_selection);
        self.select_commit_by_sha(oid, cx);
    }

    fn select_next_match(&mut self, cx: &mut Context<Self>) {
        if self.search_state.matches.is_empty() {
            return;
        }

        let mut next_selection = self
            .search_state
            .selected_index
            .map(|index| index + 1)
            .unwrap_or_default();

        if next_selection >= self.search_state.matches.len() {
            next_selection = 0;
        }

        let Some(&oid) = self.search_state.matches.get_index(next_selection) else {
            return;
        };

        self.search_state.selected_index = Some(next_selection);
        self.select_commit_by_sha(oid, cx);
    }

    pub fn set_repo_id(&mut self, repo_id: RepositoryId, cx: &mut Context<Self>) {
        if repo_id != self.repo_id
            && self
                .git_store
                .read(cx)
                .repositories()
                .contains_key(&repo_id)
        {
            self.repo_id = repo_id;
            self.invalidate_state(cx);
        }
    }

    pub fn select_commit_by_sha(&mut self, sha: impl TryInto<Oid>, cx: &mut Context<Self>) {
        fn inner(this: &mut GitGraph, oid: Oid, cx: &mut Context<GitGraph>) {
            this.auto_graph_lanes = 0;
            let Some(selected_repository) = this.get_repository(cx) else {
                return;
            };

            let Some(index) = selected_repository
                .read(cx)
                .get_graph_data(this.log_source.clone(), this.log_order)
                .and_then(|data| data.commit_oid_to_index.get(&oid))
                .copied()
                .filter(|index| *index < this.graph_data.commits.len())
            else {
                this.pending_select_sha = Some(oid);
                return;
            };

            this.pending_select_sha = None;
            this.select_entry(index, ScrollStrategy::Center, cx);
        }

        if let Ok(oid) = sha.try_into() {
            inner(self, oid, cx);
        }
    }

    fn open_selected_commit_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selected_entry_index) = self.selected_entry_idx else {
            return;
        };

        self.open_commit_view(selected_entry_index, window, cx);
    }

    fn open_commit_view(
        &mut self,
        entry_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(commit_entry) = self.graph_data.commits.get(entry_index) else {
            return;
        };

        let Some(repository) = self.get_repository(cx) else {
            return;
        };

        CommitView::open(
            commit_entry.data.sha.to_string(),
            repository.downgrade(),
            self.workspace.clone(),
            None,
            None,
            window,
            cx,
        );
    }

    fn copy_commit_sha(&mut self, entry_index: usize, cx: &mut Context<Self>) {
        let Some(commit) = self.graph_data.commits.get(entry_index) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(commit.data.sha.to_string()));
    }

    fn copy_selected_commit_sha(
        &mut self,
        _: &CopyCommitSha,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected_entry_index) = self.selected_entry_idx else {
            return;
        };
        self.copy_commit_sha(selected_entry_index, cx);
    }

    fn copy_commit_tag(&mut self, entry_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit) = self.graph_data.commits.get(entry_index) else {
            return;
        };

        let tag_names = commit
            .data
            .tag_names()
            .into_iter()
            .map(|tag_name| SharedString::from(tag_name.to_string()))
            .collect::<Vec<_>>();

        match tag_names.as_slice() {
            [] => {}
            [tag_name] => cx.write_to_clipboard(ClipboardItem::new_string(tag_name.to_string())),
            _ => {
                self.workspace
                    .update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, |window, cx| {
                            CommitTagPicker::new(tag_names, window, cx)
                        });
                    })
                    .ok();
            }
        }
    }

    fn copy_selected_commit_tag(
        &mut self,
        _: &CopyCommitTag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selected_entry_index) = self.selected_entry_idx else {
            return;
        };
        self.copy_commit_tag(selected_entry_index, window, cx);
    }

    fn schedule_git_task(
        &mut self,
        task_source_kind: TaskSourceKind,
        resolved_task: ResolvedTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.schedule_resolved_task(
                    task_source_kind,
                    resolved_task,
                    false,
                    window,
                    cx,
                );
            })
            .ok();
    }

    fn deploy_ref_context_menu(
        &mut self,
        position: Point<Pixels>,
        index: usize,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(commit) = self.graph_data.commits.get(index) else {
            return;
        };

        let ref_entry = self.ref_menu_entry(commit.data.sha, &name, cx);
        let git_graph = cx.entity();
        let context_menu = ContextMenu::build(window, cx, move |menu, window, _| {
            menu.spacing(ListItemSpacing::Sparse)
                .header(format!("Ref {name}"))
                .map(|menu| ref_entry.add_entries(menu, &git_graph, window))
        });

        self.set_context_menu(context_menu, position, Some(index), window, cx);
    }

    /// Builds a context menu listing the refs that aren't shown inline in the
    /// row. Each ref is a submenu exposing that ref's actions, so users can
    /// still act on refs hidden behind the "+N" badge.
    fn deploy_hidden_refs_context_menu(
        &mut self,
        position: Point<Pixels>,
        index: usize,
        hidden_refs: Vec<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(commit) = self.graph_data.commits.get(index) else {
            return;
        };

        let ref_entries = hidden_refs
            .into_iter()
            .map(|name| (self.ref_menu_entry(commit.data.sha, &name, cx), name))
            .collect::<Vec<_>>();

        let git_graph = cx.entity();
        let context_menu = ContextMenu::build(window, cx, move |mut menu, _window, _| {
            menu = menu.spacing(ListItemSpacing::Sparse).header("Refs");
            for (ref_entry, name) in ref_entries {
                let git_graph = git_graph.clone();
                menu = menu.submenu(name, move |submenu, window, _cx| {
                    ref_entry.clone().add_entries(
                        submenu.spacing(ListItemSpacing::Sparse),
                        &git_graph,
                        window,
                    )
                });
            }
            menu
        });

        self.set_context_menu(context_menu, position, Some(index), window, cx);
    }

    /// Resolves the data needed to populate a single ref's context-menu entries
    /// (its kind and any custom git command tasks). Kept separate so the
    /// same entries can be rendered both as a standalone menu and as a submenu.
    fn ref_menu_entry(&self, sha: Oid, name: &SharedString, cx: &App) -> RefMenuEntry {
        let ref_name = Self::ref_name_from_decoration(name);
        let repository = self.get_repository(cx);
        let snapshot = repository
            .as_ref()
            .map(|repository| repository.read(cx).snapshot());
        let (head_branch_name, remote_names) = Self::ref_pill_context(snapshot.as_ref());
        let graph_ref = GraphRef::from_decoration(name, head_branch_name.as_deref(), &remote_names);
        let git_tasks = git_context_menu_tasks(
            git_task_context(
                &repository.as_ref().map(|repository| repository.downgrade()),
                sha,
                ref_name.as_deref(),
                cx,
            ),
            &self.workspace,
            cx,
        );
        let compare_label = ref_name.unwrap_or_else(|| name.clone());
        RefMenuEntry {
            graph_ref,
            sha,
            action_context: self.graph_action_context(cx),
            compare: self.compare_menu_entries(sha, compare_label, cx),
            git_tasks,
        }
    }

    fn compare_menu_entries(&self, sha: Oid, label: SharedString, cx: &App) -> CompareMenuEntries {
        let head = self.get_repository(cx).and_then(|repository| {
            let snapshot = repository.read(cx).snapshot();
            let head_sha = Oid::try_from(snapshot.head_commit.as_ref()?.sha.as_ref()).ok()?;
            let head_label = snapshot
                .branch
                .as_ref()
                .map(|branch| SharedString::from(branch.name().to_string()))
                .unwrap_or_else(|| "HEAD".into());
            Some((head_sha, head_label))
        });
        let selected = self
            .selected_entry_idx
            .and_then(|index| self.graph_data.commits.get(index))
            .map(|commit| {
                (
                    commit.data.sha,
                    SharedString::from(commit.data.sha.display_short()),
                )
            });
        CompareMenuEntries {
            sha,
            label,
            head,
            selected,
        }
    }

    /// Shows the changes from `base` to `target` in the details panel.
    pub(crate) fn compare(
        &mut self,
        base: (Oid, SharedString),
        target: (Oid, SharedString),
        merge_base: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.get_repository(cx) else {
            return;
        };
        let diff_task = repository.update(cx, |repository, cx| {
            repository.load_range_diff(base.0.to_string(), target.0.to_string(), merge_base, cx)
        });
        self.changed_files_expanded_dirs.clear();
        self.changed_files_scroll_handle
            .scroll_to_item(0, ScrollStrategy::Top);
        self.comparison = Some(GraphComparison {
            base,
            target,
            merge_base,
            diff: None,
            stats: None,
            error: None,
            _task: cx.spawn(async move |this, cx| {
                let result = diff_task.await;
                this.update(cx, |this, cx| {
                    if let Some(comparison) = this.comparison.as_mut() {
                        match result {
                            Ok(diff) => {
                                comparison.stats = Some(compute_diff_stats(&diff));
                                comparison.diff = Some(diff);
                            }
                            Err(error) => comparison.error = Some(error.to_string().into()),
                        }
                    }
                    cx.notify();
                })
                .ok();
            }),
        });
        cx.notify();
    }

    fn graph_action_context(&self, cx: &App) -> Option<GraphActionContext> {
        let repository = self.get_repository(cx)?;
        let snapshot = repository.read(cx).snapshot();
        let (_, remote_names) = Self::ref_pill_context(Some(&snapshot));
        Some(GraphActionContext::new(
            &repository,
            self.workspace.clone(),
            remote_names,
            cx,
        ))
    }

    /// The commit at `index` with what its context menu actions need.
    fn graph_commit(&self, index: usize, cx: &mut Context<Self>) -> Option<GraphCommit> {
        let commit = self.graph_data.commits.get(index)?;
        let repository = self.get_repository(cx)?;
        let subject_of = |sha: Oid, cx: &mut Context<Self>| match repository
            .update(cx, |repository, cx| {
                repository.fetch_commit_data(sha, false, cx).clone()
            }) {
            CommitDataState::Loaded(data) => Some(data.subject.clone()),
            CommitDataState::Loading(_) => None,
        };
        let sha = commit.data.sha;
        let parents = commit.data.parents.clone();
        let subject = subject_of(sha, cx);
        let parents = parents
            .into_iter()
            .map(|parent| (parent, subject_of(parent, cx)))
            .collect();

        let head = repository
            .read(cx)
            .snapshot()
            .head_commit
            .as_ref()
            .and_then(|commit| Oid::try_from(commit.sha.as_ref()).ok());
        let can_drop = head.is_some_and(|head| {
            let commits = &self.graph_data.commits;
            let index_of = |sha: Oid| commits.iter().position(|commit| commit.data.sha == sha);
            can_drop_commit(
                sha,
                head,
                |sha| {
                    index_of(sha).map(|index| commits[index].data.parents.iter().copied().collect())
                },
                |sha| {
                    commits
                        .iter()
                        .filter(|commit| commit.data.parents.contains(&sha))
                        .count()
                },
            )
        });

        Some(GraphCommit {
            sha,
            subject,
            parents,
            can_drop,
        })
    }

    fn deploy_entry_context_menu(
        &mut self,
        position: Point<Pixels>,
        index: usize,
        ref_name: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(commit) = self.graph_commit(index, cx) else {
            return;
        };
        let Some(action_context) = self.graph_action_context(cx) else {
            return;
        };
        let repository = self
            .get_repository(cx)
            .map(|repository| repository.downgrade());
        let git_tasks = git_context_menu_tasks(
            git_task_context(&repository, commit.sha, ref_name.as_deref(), cx),
            &self.workspace,
            cx,
        );
        let header = format!("Commit {}", commit.sha.display_short());
        let compare = self.compare_menu_entries(commit.sha, commit.sha.display_short().into(), cx);
        let focus_handle = self.focus_handle.clone();
        let git_graph = cx.entity();
        let context_menu = ContextMenu::build(window, cx, move |menu, window, _| {
            let menu = add_commit_entries(
                menu.context(focus_handle)
                    .spacing(ListItemSpacing::Sparse)
                    .header(header),
                commit,
                action_context,
            );
            let menu = compare.add_entries(menu, git_graph.downgrade());
            add_custom_command_entries(menu, git_tasks, &git_graph, window)
        });
        self.set_context_menu(context_menu, position, Some(index), window, cx);
    }

    fn set_context_menu(
        &mut self,
        context_menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        target_entry_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&context_menu.focus_handle(cx), cx);

        let subscription = cx.subscribe_in(
            &context_menu,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                if this.context_menu.as_ref().is_some_and(|context_menu| {
                    context_menu
                        .menu
                        .focus_handle(cx)
                        .contains_focused(window, cx)
                }) {
                    cx.focus_self(window);
                }
                this.context_menu.take();
                cx.notify();
            },
        );
        self.context_menu = Some(GitGraphContextMenu {
            menu: context_menu,
            position,
            target_entry_index,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn toggle_column_visibility(&mut self, col_idx: usize, cx: &mut Context<Self>) {
        if let Some(slot) = self.column_visibility.as_mut_slice().get_mut(col_idx) {
            *slot = !*slot;
            // Column visibility is persisted per item, so schedule a workspace serialization.
            cx.emit(ItemEvent::Edit);
        }
    }

    fn deploy_header_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_path_history = matches!(self.log_source, LogSource::Path(_));
        let columns: &[&str] = if is_path_history {
            &["Description", "Date", "Author", "Commit"]
        } else {
            &["Graph", "Description", "Date", "Author", "Commit"]
        };

        let filter = self.column_visibility.clone();
        let visible_count = filter
            .as_slice()
            .iter()
            .filter(|filtered| !**filtered)
            .count();

        let focus_handle = self.focus_handle.clone();
        let git_graph = cx.entity();
        let context_menu = ContextMenu::build(window, cx, |mut context_menu, _window, _cx| {
            context_menu = context_menu.context(focus_handle).header("Columns");
            for (col_idx, label) in columns.iter().enumerate() {
                let is_visible = !filter.get(col_idx).copied().unwrap_or(false);
                // Disable hiding the last remaining visible column.
                let can_toggle = !is_visible || visible_count > 1;
                let git_graph = git_graph.clone();
                context_menu = context_menu.toggleable_entry_disabled_when(
                    label.to_string(),
                    is_visible,
                    !can_toggle,
                    IconPosition::End,
                    None,
                    move |_window, cx| {
                        git_graph.update(cx, |this, cx| {
                            this.toggle_column_visibility(col_idx, cx);
                            cx.notify();
                        });
                    },
                );
            }
            context_menu
        });

        self.set_context_menu(context_menu, position, None, window, cx);
    }

    /// Makes the graph's repository the active one, which git actions such as
    /// fetch and push apply to.
    fn activate_repository(&self, cx: &mut App) {
        if let Some(repository) = self.get_repository(cx) {
            repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
        }
    }

    /// Fetch, pull and push buttons for the graph's repository.
    fn render_remote_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let buttons: [(&'static str, IconName, Color, &'static str, Box<dyn Action>); 5] = [
            (
                "git-graph-fetch",
                IconName::ArrowCircle,
                Color::Default,
                "Fetch",
                Box::new(git::Fetch),
            ),
            (
                "git-graph-pull",
                IconName::ArrowDown,
                Color::Default,
                "Pull",
                Box::new(git::Pull),
            ),
            (
                "git-graph-pull-rebase",
                IconName::ArrowDownRight,
                Color::Default,
                "Pull (Rebase)",
                Box::new(git::PullRebase),
            ),
            (
                "git-graph-push",
                IconName::ArrowUp,
                Color::Default,
                "Push",
                Box::new(git::Push),
            ),
            (
                "git-graph-force-push",
                IconName::ArrowUp,
                Color::Warning,
                "Force Push",
                Box::new(git::ForcePush),
            ),
        ];
        h_flex()
            .gap_1()
            .children(buttons.into_iter().map(|(id, icon, color, label, action)| {
                let focus_handle = self.focus_handle.clone();
                let tooltip_action = action.boxed_clone();
                IconButton::new(id, icon)
                    .size(ButtonSize::Large)
                    .width(rems_from_px(32.))
                    .icon_size(IconSize::Medium)
                    .icon_color(color)
                    .tooltip(move |_, cx| {
                        Tooltip::for_action_in(label, &*tooltip_action, &focus_handle, cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_repository(cx);
                        window.dispatch_action(action.boxed_clone(), cx);
                    }))
            }))
    }

    fn render_search_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let color = cx.theme().colors();
        let query_focus_handle = self
            .search_state
            .editor
            .focus_handle(cx)
            .tab_index(1)
            .tab_stop(true);

        h_flex()
            .key_context("GitGraphSearchBar")
            .tab_index(1)
            .tab_group()
            .tab_stop(false)
            .w_full()
            .p_1p5()
            .gap_1p5()
            .border_b_1()
            .border_color(color.border_variant)
            .child(
                h_flex()
                    .h_8()
                    .flex_1()
                    .min_w_0()
                    .max_w(px(420.))
                    .px_1p5()
                    .gap_1()
                    .track_focus(&query_focus_handle)
                    .border_1()
                    .border_color(color.border_variant)
                    .rounded_md()
                    .bg(color.toolbar_background)
                    .on_action(cx.listener(Self::confirm_search))
                    .child(self.search_state.editor.clone())
                    .child({
                        let focus_handle = query_focus_handle.clone();
                        IconButton::new("git-graph-search-case-sensitive", IconName::CaseSensitive)
                            .shape(ui::IconButtonShape::Square)
                            .toggle_state(self.search_state.case_sensitive)
                            .on_click({
                                let focus_handle = query_focus_handle.clone();
                                move |_, window, cx| {
                                    if !focus_handle.is_focused(window) {
                                        window.focus(&focus_handle, cx);
                                    }
                                    window.dispatch_action(ToggleCaseSensitive.boxed_clone(), cx);
                                }
                            })
                            .tooltip(move |_window, cx| {
                                Tooltip::for_action_in(
                                    "Match Case Sensitivity",
                                    &ToggleCaseSensitive,
                                    &focus_handle,
                                    cx,
                                )
                            })
                    }),
            )
            .child(
                h_flex()
                    .min_w_64()
                    .gap_1()
                    .child({
                        let focus_handle = self.focus_handle.clone();
                        IconButton::new("git-graph-search-prev", IconName::ChevronLeft)
                            .shape(ui::IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .tooltip(move |_, cx| {
                                Tooltip::for_action_in(
                                    "Select Previous Match",
                                    &SelectPreviousMatch,
                                    &focus_handle,
                                    cx,
                                )
                            })
                            .map(|this| {
                                if self.search_state.matches.is_empty() {
                                    this.disabled(true)
                                } else {
                                    this.disabled(false).on_click(cx.listener(|this, _, _, cx| {
                                        this.select_previous_match(cx);
                                    }))
                                }
                            })
                    })
                    .child({
                        let focus_handle = self.focus_handle.clone();
                        IconButton::new("git-graph-search-next", IconName::ChevronRight)
                            .shape(ui::IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .tooltip(move |_, cx| {
                                Tooltip::for_action_in(
                                    "Select Next Match",
                                    &SelectNextMatch,
                                    &focus_handle,
                                    cx,
                                )
                            })
                            .map(|this| {
                                if self.search_state.matches.is_empty() {
                                    this.disabled(true)
                                } else {
                                    this.disabled(false).on_click(cx.listener(|this, _, _, cx| {
                                        this.select_next_match(cx);
                                    }))
                                }
                            })
                    })
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Label::new(format!(
                                    "{}/{}",
                                    self.search_state
                                        .selected_index
                                        .map(|index| index + 1)
                                        .unwrap_or(0),
                                    self.search_state.matches.len()
                                ))
                                .size(LabelSize::Small)
                                .when(self.search_state.matches.is_empty(), |this| {
                                    this.color(Color::Disabled)
                                }),
                            )
                            .when(
                                matches!(
                                    &self.search_state.state,
                                    QueryState::Confirmed((_, task)) if !task.is_ready()
                                ),
                                |this| {
                                    this.child(
                                        Icon::new(IconName::ArrowCircle)
                                            .color(Color::Accent)
                                            .size(IconSize::Small)
                                            .with_rotate_animation(2)
                                            .into_any_element(),
                                    )
                                },
                            ),
                    ),
            )
            .child(div().flex_1())
            .child(self.render_remote_buttons(cx))
    }

    fn render_loading_spinner(&self, cx: &App) -> AnyElement {
        let rems = TextSize::Large.rems(cx);
        Icon::new(IconName::LoadCircle)
            .size(IconSize::Custom(rems))
            .color(Color::Accent)
            .with_rotate_animation(3)
            .into_any_element()
    }

    fn render_commit_detail_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if let Some(comparison) = &self.comparison {
            return self.render_comparison_panel(comparison, window, cx);
        }
        let Some(selected_idx) = self.selected_entry_idx else {
            return Empty.into_any_element();
        };

        let Some(commit_entry) = self.graph_data.commits.get(selected_idx) else {
            return Empty.into_any_element();
        };

        let Some(repository) = self.get_repository(cx) else {
            return Empty.into_any_element();
        };

        let data = repository.update(cx, |repository, cx| {
            repository
                .fetch_commit_data(commit_entry.data.sha, false, cx)
                .clone()
        });

        let full_sha: SharedString = commit_entry.data.sha.to_string().into();
        let short_sha: SharedString = full_sha.chars().take(7).collect::<String>().into();
        let ref_names = commit_entry.data.ref_names.clone();

        let head_branch_name: Option<SharedString> = repository
            .read(cx)
            .snapshot()
            .branch
            .as_ref()
            .map(|branch| SharedString::from(branch.name().to_string()));

        let accent_colors = cx.theme().accents();
        let accent_color = accent_colors
            .0
            .get(commit_entry.color_idx)
            .copied()
            .unwrap_or_else(|| accent_colors.0.first().copied().unwrap_or_default());

        let (author_name, author_email, commit_timestamp) = match &data {
            CommitDataState::Loaded(data) => (
                data.author_name.clone(),
                data.author_email.clone(),
                Some(data.commit_timestamp),
            ),
            CommitDataState::Loading(_) => ("Loading…".into(), "".into(), None),
        };

        let date_string = commit_timestamp.map(format_timestamp).unwrap_or_default();

        let remote = repository.update(cx, |repo, cx| {
            let remote_url = repo.default_remote_url()?;
            let provider_registry = GitHostingProviderRegistry::default_global(cx);
            let (provider, parsed) = parse_git_remote_url(provider_registry, &remote_url)?;
            Some(GitRemote {
                host: provider,
                owner: parsed.owner.into(),
                repo: parsed.repo.into(),
            })
        });

        let avatar = {
            let author_email_for_avatar = if author_email.is_empty() {
                None
            } else {
                Some(author_email.clone())
            };

            CommitAvatar::new(&full_sha, author_email_for_avatar, remote.as_ref())
                .size(px(32.))
                .render(window, cx)
        };

        v_flex()
            .min_w(px(300.))
            .h_full()
            .bg(cx.theme().colors().editor_background)
            .flex_basis(DefiniteLength::Fraction(
                self.commit_details_split_state.read(cx).right_ratio(),
            ))
            .child(
                v_flex()
                    .relative()
                    .w_full()
                    .p_2()
                    .gap_2()
                    .child(
                        div().absolute().top_2().right_2().child(
                            IconButton::new("close-detail", IconName::Close)
                                .icon_size(IconSize::Small)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.selected_entry_idx = None;
                                    this.selected_commit_diff = None;
                                    this.selected_commit_diff_stats = None;
                                    this.selected_commit_message = None;
                                    this._selected_commit_message_task = None;
                                    this.changed_files_expanded_dirs.clear();
                                    this._commit_diff_task = None;
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        div().absolute().top_2().left_2().child(
                            Disclosure::new(
                                "toggle-commit-metadata",
                                !self.commit_details_metadata_collapsed,
                            )
                            .tooltip({
                                let label = if self.commit_details_metadata_collapsed {
                                    "Show Commit Details"
                                } else {
                                    "Hide Commit Details"
                                };
                                move |_window, cx: &mut App| Tooltip::simple(label, cx)
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.commit_details_metadata_collapsed =
                                    !this.commit_details_metadata_collapsed;
                                write_commit_details_collapsed(
                                    COMMIT_DETAILS_METADATA_COLLAPSED_KEY,
                                    this.commit_details_metadata_collapsed,
                                    cx,
                                );
                                cx.notify();
                            })),
                        ),
                    )
                    .map(|this| {
                        if self.commit_details_metadata_collapsed {
                            return this.child(
                                h_flex()
                                    .w_full()
                                    .px_6()
                                    .gap_1()
                                    .justify_center()
                                    .overflow_hidden()
                                    .child(
                                        Label::new(author_name).size(LabelSize::Small).truncate(),
                                    )
                                    .child(
                                        Label::new(short_sha)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            );
                        }
                        this.child(
                            v_flex()
                                .py_1()
                                .w_full()
                                .items_center()
                                .child(avatar)
                                .child(Label::new(author_name).mt_1p5())
                                .child(
                                    Label::new(date_string)
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                ),
                        )
                        .children((!ref_names.is_empty()).then(|| {
                            h_flex().gap_1().flex_wrap().justify_center().children(
                                ref_names.iter().map(|name| {
                                    let is_head =
                                        Self::is_head_ref(name.as_ref(), &head_branch_name);
                                    self.render_ref_chip(
                                        name.clone(),
                                        accent_color,
                                        is_head,
                                        selected_idx,
                                        cx.theme().colors().editor_background,
                                        cx,
                                    )
                                }),
                            )
                        }))
                        .child(
                            v_flex()
                                .ml_neg_1()
                                .gap_1p5()
                                .when(!author_email.is_empty(), |this| {
                                    let copied_state: Entity<CopiedState> = window.use_keyed_state(
                                        "author-email-copy",
                                        cx,
                                        CopiedState::new,
                                    );
                                    let is_copied = copied_state.read(cx).is_copied();

                                    let (icon, icon_color, tooltip_label) = if is_copied {
                                        (IconName::Check, Color::Success, "Email Copied!")
                                    } else {
                                        (IconName::Envelope, Color::Muted, "Copy Email")
                                    };

                                    let copy_email = author_email.clone();
                                    let author_email_for_tooltip = author_email.clone();

                                    this.child(
                                        Button::new("author-email-copy", author_email.clone())
                                            .start_icon(
                                                Icon::new(icon)
                                                    .size(IconSize::Small)
                                                    .color(icon_color),
                                            )
                                            .label_size(LabelSize::Small)
                                            .truncate(true)
                                            .color(Color::Muted)
                                            .tooltip(move |_, cx| {
                                                Tooltip::with_meta(
                                                    tooltip_label,
                                                    None,
                                                    author_email_for_tooltip.clone(),
                                                    cx,
                                                )
                                            })
                                            .on_click(move |_, _, cx| {
                                                copied_state.update(cx, |state, _cx| {
                                                    state.mark_copied();
                                                });
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    copy_email.to_string(),
                                                ));
                                                let state_id = copied_state.entity_id();
                                                cx.spawn(async move |cx| {
                                                    cx.background_executor()
                                                        .timer(COPIED_STATE_DURATION)
                                                        .await;
                                                    cx.update(|cx| {
                                                        cx.notify(state_id);
                                                    })
                                                })
                                                .detach();
                                            }),
                                    )
                                })
                                .child({
                                    let copy_sha = full_sha.clone();
                                    let copied_state: Entity<CopiedState> =
                                        window.use_keyed_state("sha-copy", cx, CopiedState::new);
                                    let is_copied = copied_state.read(cx).is_copied();

                                    let (icon, icon_color, tooltip_label) = if is_copied {
                                        (IconName::Check, Color::Success, "Commit SHA Copied!")
                                    } else {
                                        (IconName::Hash, Color::Muted, "Copy Commit SHA")
                                    };

                                    Button::new("sha-button", &full_sha)
                                        .start_icon(
                                            Icon::new(icon).size(IconSize::Small).color(icon_color),
                                        )
                                        .label_size(LabelSize::Small)
                                        .truncate(true)
                                        .color(Color::Muted)
                                        .tooltip({
                                            let full_sha = full_sha.clone();
                                            move |_, cx| {
                                                Tooltip::with_meta(
                                                    tooltip_label,
                                                    None,
                                                    full_sha.clone(),
                                                    cx,
                                                )
                                            }
                                        })
                                        .on_click(move |_, _, cx| {
                                            copied_state.update(cx, |state, _cx| {
                                                state.mark_copied();
                                            });
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_sha.to_string(),
                                            ));
                                            let state_id = copied_state.entity_id();
                                            cx.spawn(async move |cx| {
                                                cx.background_executor()
                                                    .timer(COPIED_STATE_DURATION)
                                                    .await;
                                                cx.update(|cx| {
                                                    cx.notify(state_id);
                                                })
                                            })
                                            .detach();
                                        })
                                })
                                .when_some(remote.clone(), |this, remote| {
                                    let provider_name = remote.host.name();
                                    let icon =
                                        ui::git_hosting_provider_icon(provider_name.as_str());
                                    let parsed_remote = ParsedGitRemote {
                                        owner: remote.owner.as_ref().into(),
                                        repo: remote.repo.as_ref().into(),
                                    };
                                    let params = BuildCommitPermalinkParams {
                                        sha: full_sha.as_ref(),
                                    };
                                    let url = remote
                                        .host
                                        .build_commit_permalink(&parsed_remote, params)
                                        .to_string();

                                    this.child(
                                        Button::new(
                                            "view-on-provider",
                                            format!("View on {}", provider_name),
                                        )
                                        .start_icon(
                                            Icon::new(icon)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .label_size(LabelSize::Small)
                                        .truncate(true)
                                        .color(Color::Muted)
                                        .on_click(
                                            move |_, _, cx| {
                                                cx.open_url(&url);
                                            },
                                        ),
                                    )
                                }),
                        )
                    }),
            )
            .child(Divider::horizontal())
            .child(self.render_commit_message(window, cx))
            .child(Divider::horizontal())
            .child(self.render_changed_files(
                self.selected_commit_diff.as_ref(),
                self.selected_commit_diff_stats,
                ChangedFilesSource::Commit(full_sha.clone()),
                &repository,
                window,
                cx,
            ))
            .child(Divider::horizontal())
            .child(
                h_flex().p_1p5().w_full().child(
                    Button::new("view-commit", "View Commit")
                        .full_width()
                        .start_icon(
                            Icon::new(IconName::GitCommit)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .style(ButtonStyle::OutlinedGhost)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_selected_commit_view(window, cx);
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_comparison_panel(
        &self,
        comparison: &GraphComparison,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(repository) = self.get_repository(cx) else {
            return Empty.into_any_element();
        };
        let ref_label = |(sha, label): &(Oid, SharedString)| {
            let sha = sha.to_string();
            div()
                .id(SharedString::from(format!("compare-{sha}")))
                .min_w_0()
                .tooltip(Tooltip::text(sha))
                .child(Label::new(label.clone()).size(LabelSize::Small).truncate())
        };

        v_flex()
            .min_w(px(300.))
            .h_full()
            .bg(cx.theme().colors().editor_background)
            .flex_basis(DefiniteLength::Fraction(
                self.commit_details_split_state.read(cx).right_ratio(),
            ))
            .child(
                v_flex()
                    .w_full()
                    .p_2()
                    .gap_2()
                    .child(
                        h_flex()
                            .w_full()
                            .justify_between()
                            .child(
                                Label::new("Compare")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        IconButton::new("swap-comparison", IconName::ChevronUpDown)
                                            .icon_size(IconSize::Small)
                                            .tooltip(Tooltip::text("Swap Sides"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                if let Some(comparison) = &this.comparison {
                                                    let base = comparison.target.clone();
                                                    let target = comparison.base.clone();
                                                    let merge_base = comparison.merge_base;
                                                    this.compare(base, target, merge_base, cx);
                                                }
                                            })),
                                    )
                                    .child(
                                        IconButton::new("close-comparison", IconName::Close)
                                            .icon_size(IconSize::Small)
                                            .tooltip(Tooltip::text("Close Comparison"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.comparison = None;
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap_1()
                            .child(ref_label(&comparison.base))
                            .child(
                                Icon::new(IconName::ArrowRight)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(ref_label(&comparison.target)),
                    )
                    .child(
                        Checkbox::new(
                            "comparison-merge-base",
                            ToggleState::from(comparison.merge_base),
                        )
                        .label("Only changes since the common ancestor")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(
                            |this, state: &ToggleState, _, cx| {
                                if let Some(comparison) = &this.comparison {
                                    let base = comparison.base.clone();
                                    let target = comparison.target.clone();
                                    this.compare(base, target, state.selected(), cx);
                                }
                            },
                        )),
                    )
                    .when_some(comparison.error.clone(), |this, error| {
                        this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                    }),
            )
            .child(Divider::horizontal())
            .child(self.render_changed_files(
                comparison.diff.as_ref(),
                comparison.stats,
                ChangedFilesSource::Comparison {
                    base_sha: comparison.base.0,
                    target_sha: comparison.target.0,
                    merge_base: comparison.merge_base,
                },
                &repository,
                window,
                cx,
            ))
            .into_any_element()
    }

    /// The "N Changed Files" header and the list of changed files of a
    /// commit or comparison.
    fn render_changed_files(
        &self,
        diff: Option<&CommitDiff>,
        stats: Option<(usize, usize)>,
        source: ChangedFilesSource,
        repository: &Entity<Repository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let changed_files_count = diff.map(|diff| diff.files.len()).unwrap_or(0);

        let (total_lines_added, total_lines_removed) = stats.unwrap_or((0, 0));

        let changed_file_entries: Vec<ChangedFileEntry> = diff
            .map(|diff| {
                let mut files = diff.files.iter().collect::<Vec<_>>();
                if !self.changed_files_view_mode.is_tree() {
                    files.sort_by_key(|file| file.status());
                }
                files
                    .into_iter()
                    .map(|file| ChangedFileEntry::from_commit_file(file, cx))
                    .collect()
            })
            .unwrap_or_default();
        let changed_file_entries = Rc::new(changed_file_entries);
        let tree_entries: Rc<Vec<ChangedFileTreeEntry>> = if self.changed_files_view_mode.is_tree()
        {
            Rc::new(build_changed_file_tree_entries(
                changed_file_entries.as_ref().clone(),
                &self.changed_files_expanded_dirs,
            ))
        } else {
            Rc::default()
        };

        let is_tree_view = self.changed_files_view_mode.is_tree();
        let view_toggle = IconButton::new("toggle-changed-files-view", IconName::ListTree)
            .icon_size(IconSize::Small)
            .toggle_state(self.changed_files_view_mode.is_tree())
            .tooltip({
                let tooltip = if is_tree_view {
                    "Show Flat View"
                } else {
                    "Show Tree View"
                };
                move |_, cx| Tooltip::for_action(tooltip, &ToggleChangedFilesView, cx)
            })
            .on_click(cx.listener(|this, _, _window, cx| {
                this.changed_files_view_mode = this.changed_files_view_mode.toggled();
                this.changed_files_scroll_handle
                    .scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            }));

        v_flex()
            .min_w_0()
            .flex_1()
            .overflow_hidden()
            .child(
                h_flex()
                    .p_2()
                    .pr_3()
                    .pb_1()
                    .gap_1()
                    .w_full()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(format!(
                                    "{} Changed {}",
                                    changed_files_count,
                                    if changed_files_count == 1 {
                                        "File"
                                    } else {
                                        "Files"
                                    }
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                            .child(Divider::vertical())
                            .child(view_toggle),
                    )
                    .child(DiffStat::new(
                        "commit-diff-stat",
                        total_lines_added,
                        total_lines_removed,
                    )),
            )
            .child(
                div()
                    .id("changed-files-container")
                    .flex_1()
                    .min_h_0()
                    .child({
                        let flat_entries = changed_file_entries;

                        let entry_count = if is_tree_view {
                            tree_entries.len()
                        } else {
                            flat_entries.len()
                        };
                        let repository = repository.downgrade();
                        let workspace = self.workspace.clone();
                        let git_graph = cx.weak_entity();
                        let indent_tree_entries = tree_entries.clone();

                        uniform_list(
                            "changed-files-list",
                            entry_count,
                            move |range, _window, cx| {
                                range
                                    .map(|ix| {
                                        if is_tree_view {
                                            match &tree_entries[ix] {
                                                ChangedFileTreeEntry::Directory(entry) => {
                                                    entry.render(ix, git_graph.clone(), cx)
                                                }
                                                ChangedFileTreeEntry::File(entry) => {
                                                    entry.entry.render(
                                                        ix,
                                                        entry.depth,
                                                        None,
                                                        source.clone(),
                                                        repository.clone(),
                                                        workspace.clone(),
                                                        cx,
                                                    )
                                                }
                                            }
                                        } else {
                                            let directory_label =
                                                (!flat_entries[ix].dir_path.is_empty())
                                                    .then(|| flat_entries[ix].dir_path.clone());
                                            flat_entries[ix].render(
                                                ix,
                                                0,
                                                directory_label,
                                                source.clone(),
                                                repository.clone(),
                                                workspace.clone(),
                                                cx,
                                            )
                                        }
                                    })
                                    .collect()
                            },
                        )
                        .when(is_tree_view, |list| {
                            list.with_decoration(
                                ui::indent_guides(px(TREE_INDENT), IndentGuideColors::panel(cx))
                                    .with_left_offset(
                                        ui::LIST_ITEM_INDENT_GUIDE_LEFT_OFFSET - px(2.),
                                    )
                                    .with_compute_indents_fn(
                                        cx.entity(),
                                        move |_, range, _window, _cx| {
                                            range
                                                .map(|ix| match indent_tree_entries.get(ix) {
                                                    Some(ChangedFileTreeEntry::Directory(
                                                        entry,
                                                    )) => entry.depth,
                                                    Some(ChangedFileTreeEntry::File(entry)) => {
                                                        entry.depth
                                                    }
                                                    None => 0,
                                                })
                                                .collect()
                                        },
                                    ),
                            )
                        })
                        .size_full()
                        .track_scroll(&self.changed_files_scroll_handle)
                    })
                    .vertical_scrollbar_for(&self.changed_files_scroll_handle, window, cx),
            )
            .into_any_element()
    }

    fn render_graph_canvas(&self, window: &Window, cx: &mut Context<GitGraph>) -> impl IntoElement {
        let row_height = Self::row_height(window, cx);
        let visible_row_count = self.visible_row_count(window, cx);
        let table_state = self.table_interaction_state.read(cx);
        let viewport_height = table_state
            .scroll_handle
            .0
            .borrow()
            .last_item_size
            .map(|size| size.item.height)
            .unwrap_or(window.viewport_size().height);
        let loaded_commit_count = self.graph_data.commits.len();

        let content_height = row_height * loaded_commit_count;
        let max_scroll = (content_height - viewport_height).max(px(0.));
        let scroll_offset_y = (-table_state.scroll_offset().y).clamp(px(0.), max_scroll);

        let first_visible_row = (scroll_offset_y / row_height).floor() as usize;
        let vertical_scroll_offset = scroll_offset_y - (first_visible_row as f32 * row_height);

        let canvas_viewport_width = self.graph_viewport_width(window, cx).max(px(0.));
        let graph_width = self.graph_canvas_content_width().max(canvas_viewport_width);
        let last_visible_row = first_visible_row + visible_row_count + 1;

        let viewport_range = first_visible_row.min(loaded_commit_count.saturating_sub(1))
            ..(last_visible_row).min(loaded_commit_count);
        let rows = self.graph_data.commits[viewport_range.clone()].to_vec();
        let commit_lines: Vec<_> = self
            .graph_data
            .lines
            .iter()
            .filter(|line| {
                line.full_interval.start <= viewport_range.end
                    && line.full_interval.end >= viewport_range.start
            })
            .cloned()
            .collect();

        let mut lines: BTreeMap<usize, Vec<_>> = BTreeMap::new();

        let hovered_entry_idx = self.hovered_entry_idx;
        let selected_entry_idx = self.selected_entry_idx;
        let context_menu_target_index = self
            .context_menu
            .as_ref()
            .and_then(|menu| menu.target_entry_index);
        let is_focused = self.focus_handle.is_focused(window);
        let graph_canvas_bounds = self.graph_canvas_bounds.clone();

        gpui::canvas(
            move |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, _: (), window: &mut Window, cx: &mut App| {
                graph_canvas_bounds.set(Some(bounds));

                window.paint_layer(bounds, |window| {
                    let theme = cx.theme();
                    let accent_colors = theme.accents();
                    let background = theme.colors().editor_background;

                    let hover_bg = theme.colors().element_hover.opacity(0.6);
                    let selected_bg = if is_focused {
                        theme.colors().element_selected
                    } else {
                        theme.colors().element_hover
                    };

                    for visible_row_idx in 0..rows.len() {
                        let absolute_row_idx = first_visible_row + visible_row_idx;
                        let is_hovered = hovered_entry_idx == Some(absolute_row_idx);
                        let is_selected = selected_entry_idx == Some(absolute_row_idx);
                        let is_context_menu_target =
                            context_menu_target_index == Some(absolute_row_idx);

                        if is_hovered || is_selected || is_context_menu_target {
                            let row_y = bounds.origin.y + visible_row_idx as f32 * row_height
                                - vertical_scroll_offset;
                            let row_bounds = Bounds::new(
                                point(bounds.origin.x, row_y),
                                gpui::Size {
                                    width: bounds.size.width,
                                    height: row_height,
                                },
                            );

                            let bg_color = if is_selected || is_context_menu_target {
                                selected_bg
                            } else {
                                hover_bg
                            };
                            window.paint_quad(gpui::fill(row_bounds, bg_color));
                        }
                    }

                    for line in commit_lines {
                        let Some((start_segment_idx, start_column)) =
                            line.get_first_visible_segment_idx(first_visible_row)
                        else {
                            continue;
                        };

                        let line_x = lane_center_x(bounds, start_column as f32);

                        let start_row = line.full_interval.start as i32 - first_visible_row as i32;

                        let from_y =
                            bounds.origin.y + start_row as f32 * row_height + row_height / 2.0
                                - vertical_scroll_offset
                                + COMMIT_CIRCLE_RADIUS;

                        let mut current_row = from_y;
                        let mut current_column = line_x;

                        let mut builder = PathBuilder::stroke(LINE_WIDTH);
                        builder.move_to(point(line_x, from_y));

                        let segments = &line.segments[start_segment_idx..];
                        let desired_curve_height = row_height / 3.0;
                        let desired_curve_width = LANE_WIDTH / 3.0;

                        for (segment_idx, segment) in segments.iter().enumerate() {
                            let is_last = segment_idx + 1 == segments.len();

                            match segment {
                                CommitLineSegment::Straight { to_row } => {
                                    let mut dest_row = to_row_center(
                                        to_row - first_visible_row,
                                        row_height,
                                        vertical_scroll_offset,
                                        bounds,
                                    );
                                    if is_last {
                                        dest_row -= COMMIT_CIRCLE_RADIUS;
                                    }

                                    let dest_point = point(current_column, dest_row);

                                    current_row = dest_point.y;
                                    builder.line_to(dest_point);
                                    builder.move_to(dest_point);
                                }
                                CommitLineSegment::Curve {
                                    to_column,
                                    on_row,
                                    curve_kind,
                                } => {
                                    let mut to_column = lane_center_x(bounds, *to_column as f32);

                                    let mut to_row = to_row_center(
                                        *on_row - first_visible_row,
                                        row_height,
                                        vertical_scroll_offset,
                                        bounds,
                                    );

                                    // This means that this branch was a checkout
                                    let going_right = to_column > current_column;
                                    let column_shift = if going_right {
                                        COMMIT_CIRCLE_RADIUS + COMMIT_CIRCLE_STROKE_WIDTH
                                    } else {
                                        -COMMIT_CIRCLE_RADIUS - COMMIT_CIRCLE_STROKE_WIDTH
                                    };

                                    match curve_kind {
                                        CurveKind::Checkout => {
                                            if is_last {
                                                to_column -= column_shift;
                                            }

                                            let available_curve_width =
                                                (to_column - current_column).abs();
                                            let available_curve_height =
                                                (to_row - current_row).abs();
                                            let curve_width =
                                                desired_curve_width.min(available_curve_width);
                                            let curve_height =
                                                desired_curve_height.min(available_curve_height);
                                            let signed_curve_width = if going_right {
                                                curve_width
                                            } else {
                                                -curve_width
                                            };
                                            let curve_start =
                                                point(current_column, to_row - curve_height);
                                            let curve_end =
                                                point(current_column + signed_curve_width, to_row);
                                            let curve_control = point(current_column, to_row);

                                            builder.move_to(point(current_column, current_row));
                                            builder.line_to(curve_start);
                                            builder.move_to(curve_start);
                                            builder.curve_to(curve_end, curve_control);
                                            builder.move_to(curve_end);
                                            builder.line_to(point(to_column, to_row));
                                        }
                                        CurveKind::Merge => {
                                            if is_last {
                                                to_row -= COMMIT_CIRCLE_RADIUS;
                                            }

                                            let merge_start = point(
                                                current_column + column_shift,
                                                current_row - COMMIT_CIRCLE_RADIUS,
                                            );
                                            let available_curve_width =
                                                (to_column - merge_start.x).abs();
                                            let available_curve_height =
                                                (to_row - merge_start.y).abs();
                                            let curve_width =
                                                desired_curve_width.min(available_curve_width);
                                            let curve_height =
                                                desired_curve_height.min(available_curve_height);
                                            let signed_curve_width = if going_right {
                                                curve_width
                                            } else {
                                                -curve_width
                                            };
                                            let curve_start = point(
                                                to_column - signed_curve_width,
                                                merge_start.y,
                                            );
                                            let curve_end =
                                                point(to_column, merge_start.y + curve_height);
                                            let curve_control = point(to_column, merge_start.y);

                                            builder.move_to(merge_start);
                                            builder.line_to(curve_start);
                                            builder.move_to(curve_start);
                                            builder.curve_to(curve_end, curve_control);
                                            builder.move_to(curve_end);
                                            builder.line_to(point(to_column, to_row));
                                        }
                                    }
                                    current_row = to_row;
                                    current_column = to_column;
                                    builder.move_to(point(current_column, current_row));
                                }
                            }
                        }

                        builder.close();
                        lines.entry(line.color_idx).or_default().push(builder);
                    }

                    for (color_idx, builders) in lines {
                        let line_color = accent_colors.color_for_index(color_idx as u32);

                        for builder in builders {
                            if let Ok(path) = builder.build() {
                                // we paint each color on it's own layer to stop overlapping lines
                                // of different colors changing the color of a line
                                window.paint_layer(bounds, |window| {
                                    window.paint_path(path, line_color);
                                });
                            }
                        }
                    }

                    // Commit nodes are painted last so they sit on top of the
                    // connector lines, keeping each commit a clear, distinct point.
                    for (row_idx, row) in rows.into_iter().enumerate() {
                        let absolute_row_idx = first_visible_row + row_idx;
                        let row_color = accent_colors.color_for_index(row.color_idx as u32);
                        let row_y_center =
                            bounds.origin.y + row_idx as f32 * row_height + row_height / 2.0
                                - vertical_scroll_offset;

                        let commit_x = lane_center_x(bounds, row.lane as f32);

                        // Match the node halo to the row's effective background so
                        // the ring blends seamlessly on hovered/selected rows.
                        let node_background = if selected_entry_idx == Some(absolute_row_idx)
                            || context_menu_target_index == Some(absolute_row_idx)
                        {
                            background.blend(selected_bg)
                        } else if hovered_entry_idx == Some(absolute_row_idx) {
                            background.blend(hover_bg)
                        } else {
                            background
                        };

                        draw_commit_circle(
                            commit_x,
                            row_y_center,
                            row_color,
                            node_background,
                            window,
                        );
                    }
                })
            },
        )
        .w(graph_width)
        .h_full()
    }

    fn row_at_position(
        &self,
        position_y: Pixels,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<usize> {
        let canvas_bounds = self.graph_canvas_bounds.get()?;
        let table_state = self.table_interaction_state.read(cx);
        let scroll_offset_y = -table_state.scroll_offset().y;

        let local_y = position_y - canvas_bounds.origin.y;

        if local_y >= px(0.) && local_y < canvas_bounds.size.height {
            let absolute_y = local_y + scroll_offset_y;
            let row_height = Self::row_height(window, cx);
            let absolute_row = (absolute_y / row_height).floor() as usize;

            if absolute_row < self.graph_data.commits.len() {
                return Some(absolute_row);
            }
        }

        None
    }

    fn handle_graph_mouse_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.row_at_position(event.position.y, window, cx) {
            if self.hovered_entry_idx != Some(row) {
                self.hovered_entry_idx = Some(row);
                cx.notify();
            }
        } else if self.hovered_entry_idx.is_some() {
            self.hovered_entry_idx = None;
            cx.notify();
        }
    }

    fn handle_entry_click(
        &mut self,
        entry_idx: usize,
        event: &ClickEvent,
        scroll_strategy: ScrollStrategy,
        focus_handle: Option<&FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Right-clicks open the context menu, not the details panel.
        if event.is_right_click() {
            return;
        }

        if let Some(focus_handle) = focus_handle {
            focus_handle.focus(window, cx);
        }

        self.select_entry(entry_idx, scroll_strategy, cx);

        if event.click_count() >= 2 {
            self.open_commit_view(entry_idx, window, cx);
        }
    }

    fn handle_graph_click(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.row_at_position(event.position().y, window, cx) {
            self.handle_entry_click(row, event, ScrollStrategy::Nearest, None, window, cx);
        }
    }

    fn handle_entry_secondary_mouse_down(
        &mut self,
        entry_idx: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.deploy_entry_context_menu(event.position, entry_idx, None, window, cx);
        cx.stop_propagation();
    }

    fn handle_graph_secondary_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.row_at_position(event.position.y, window, cx) else {
            return;
        };

        self.handle_entry_secondary_mouse_down(row, event, window, cx);
    }

    fn handle_graph_scroll(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = window.line_height();
        let delta = event.delta.pixel_delta(line_height);

        let table_state = self.table_interaction_state.read(cx);
        let current_offset = table_state.scroll_offset();

        let viewport_height = table_state.scroll_handle.viewport().size.height;

        let commit_count = match self.graph_data.max_commit_count {
            AllCommitCount::Loading(count) => count,
            AllCommitCount::FullyLoaded(count) => count,
            AllCommitCount::NotLoaded => self.graph_data.commits.len(),
        };
        let content_height = Self::row_height(window, cx) * commit_count;
        let max_vertical_scroll = (viewport_height - content_height).min(px(0.));

        let new_y = (current_offset.y + delta.y).clamp(max_vertical_scroll, px(0.));
        let new_offset = Point::new(current_offset.x, new_y);

        if new_offset != current_offset {
            table_state.set_scroll_offset(new_offset);
            cx.notify();
        }
    }

    fn commit_count_and_loading_state(&mut self, cx: &mut Context<Self>) -> (usize, bool) {
        match self.graph_data.max_commit_count {
            AllCommitCount::FullyLoaded(count) => (count, false),
            AllCommitCount::Loading(count) => {
                let is_loading = self
                    .get_repository(cx)
                    .map(|repository| {
                        repository.update(cx, |repository, cx| {
                            repository
                                .graph_data(self.log_source.clone(), self.log_order, 0..0, cx)
                                .is_loading
                        })
                    })
                    .unwrap_or(false);

                (count, is_loading)
            }
            AllCommitCount::NotLoaded => {
                let (commit_count, is_loading) = if let Some(repository) = self.get_repository(cx) {
                    repository.update(cx, |repository, cx| {
                        // Start loading the graph data if we haven't started already
                        let GraphDataResponse {
                            commits,
                            is_loading,
                            error: _,
                        } = repository.graph_data(
                            self.log_source.clone(),
                            self.log_order,
                            0..usize::MAX,
                            cx,
                        );
                        self.graph_data.add_commits(commits);
                        (commits.len(), is_loading)
                    })
                } else {
                    (0, false)
                };

                (commit_count, is_loading)
            }
        }
    }

    fn render_commit_view_resize_handle(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("commit-view-split-resize-container")
            .relative()
            .h_full()
            .flex_shrink_0()
            .w(px(1.))
            .bg(cx.theme().colors().border_variant)
            .child(
                div()
                    .id("commit-view-split-resize-handle")
                    .absolute()
                    .left(px(-RESIZE_HANDLE_WIDTH / 2.0))
                    .w(px(RESIZE_HANDLE_WIDTH))
                    .h_full()
                    .cursor_col_resize()
                    .block_mouse_except_scroll()
                    .on_click(cx.listener(|this, event: &ClickEvent, _window, cx| {
                        if event.click_count() >= 2 {
                            this.commit_details_split_state.update(cx, |state, _| {
                                state.on_double_click();
                            });
                        }
                        cx.stop_propagation();
                    }))
                    .on_drag(DraggedSplitHandle, |_, _, _, cx| cx.new(|_| gpui::Empty)),
            )
            .into_any_element()
    }

    fn render_commit_message(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(DetailPanelCommitMessage {
            message,
            scroll_handle,
            ..
        }) = self.selected_commit_message.as_ref()
        else {
            return Empty.into_any_element();
        };

        let message_style = editor::hover_markdown_style(window, cx);
        let rem_size = window.rem_size();
        let line_height = message_style
            .base_text_style
            .line_height_in_pixels(rem_size);

        let collapsed = self.commit_details_message_collapsed;
        // At most 20% of the window height, but always a few lines.
        let message_max_height = (window.viewport_size().height * 0.2).max(line_height * 3.);

        div()
            // Using grid over flexbox because the structure of this side
            // panel prvents taffy from calculating a concrete width correctly,
            // which causes problems with text reflow when using flexbox.
            // grid, on the other hand, doesn't appear to give taffy the same
            // problems.
            .w_full()
            .py_2()
            .pl_2()
            .grid()
            .grid_cols(1)
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Disclosure::new("toggle-commit-message", !collapsed).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.commit_details_message_collapsed =
                                    !this.commit_details_message_collapsed;
                                write_commit_details_collapsed(
                                    COMMIT_DETAILS_MESSAGE_COLLAPSED_KEY,
                                    this.commit_details_message_collapsed,
                                    cx,
                                );
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        Label::new("Message")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .when(!collapsed, |this| {
                this.child(
                    div()
                        .relative()
                        .w_full()
                        .child(
                            div()
                                .id("commit-message")
                                .text_sm()
                                .w_full()
                                .max_h(message_max_height)
                                .overflow_y_scroll()
                                .track_scroll(scroll_handle)
                                .child(MarkdownElement::new(message.clone(), message_style)),
                        )
                        .vertical_scrollbar_for(scroll_handle, window, cx),
                )
            })
            .into_any_element()
    }
}

impl Render for GitGraph {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // This happens when we changed branches, we should refresh our search as well
        if let QueryState::Pending(query) = &mut self.search_state.state {
            let query = std::mem::take(query);
            self.search_state.state = QueryState::Empty;
            self.search(query, cx);
        }
        let (commit_count, is_loading) = self.commit_count_and_loading_state(cx);

        let error = self.get_repository(cx).and_then(|repo| {
            repo.read(cx)
                .get_graph_data(self.log_source.clone(), self.log_order)
                .and_then(|data| data.error.clone())
        });

        let content = if commit_count == 0 {
            let message = if let Some(error) = &error {
                format!("Error loading: {}", error)
            } else if is_loading {
                "Loading".to_string()
            } else {
                "No commits found".to_string()
            };
            let label = Label::new(message)
                .color(Color::Muted)
                .size(LabelSize::Large);

            h_flex()
                .size_full()
                .gap_1()
                .justify_center()
                .child(label)
                .when(is_loading && error.is_none(), |this| {
                    this.child(self.render_loading_spinner(cx))
                })
        } else {
            let is_path_history = matches!(self.log_source, LogSource::Path(_));
            let header_resize_info =
                HeaderResizeInfo::from_redistributable(&self.column_widths, cx);

            let column_filter = self.column_visibility.clone();

            // The graph column (index 0) only exists in the non-path-history layout and is
            // rendered as a separate canvas outside the table.
            let graph_visible =
                is_path_history || !column_filter.get(0usize).copied().unwrap_or(false);

            let table_offset = if is_path_history { 0 } else { 1 };
            let table_filter = column_filter
                .as_slice()
                .get(table_offset..table_offset + TABLE_COLUMN_COUNT)
                .map(|slice| TableRow::from_vec(slice.to_vec(), TABLE_COLUMN_COUNT))
                .unwrap_or_else(|| TableRow::from_element(false, TABLE_COLUMN_COUNT));
            let header_widths = redistribute_hidden_widths(
                &self.column_widths.read(cx).widths_to_render(),
                Some(&column_filter),
            );
            let header_context = TableRenderContext::for_column_widths(Some(header_widths), true)
                .with_column_filter(Some(column_filter));

            let [
                graph_fraction,
                description_fraction,
                date_fraction,
                author_fraction,
                commit_fraction,
            ] = self.preview_column_fractions(window, cx);
            let table_fraction =
                description_fraction + date_fraction + author_fraction + commit_fraction;
            let table_width_config = self.table_column_width_config(window, cx);

            let table_collapsed = table_fraction <= f32::EPSILON;
            let graph_content_width = self.graph_canvas_content_width();
            let graph_pixel_width = self
                .graph_column_pixel_width(window, cx)
                .filter(|_| !table_collapsed);

            h_flex()
                .size_full()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .size_full()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                        this.deploy_header_context_menu(event.position, window, cx);
                                        cx.stop_propagation();
                                    }),
                                )
                                .child(render_table_header(
                                    if !is_path_history {
                                        TableRow::from_vec(
                                            vec![
                                                Label::new("Graph")
                                                    .color(Color::Muted)
                                                    .truncate()
                                                    .into_any_element(),
                                                Label::new("Description")
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new("Date")
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new("Author")
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new("Commit")
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                            ],
                                            5,
                                        )
                                    } else {
                                        TableRow::from_vec(
                                            vec![
                                                Label::new("Description")
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new("Date")
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new("Author")
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                                Label::new("Commit")
                                                    .color(Color::Muted)
                                                    .into_any_element(),
                                            ],
                                            4,
                                        )
                                    },
                                    header_context,
                                    Some(header_resize_info),
                                    Some(self.column_widths.entity_id()),
                                    cx,
                                )),
                        )
                        .child({
                            let row_height = Self::row_height(window, cx);
                            let selected_entry_idx = self.selected_entry_idx;
                            let hovered_entry_idx = self.hovered_entry_idx;
                            let context_menu_target_index = self
                                .context_menu
                                .as_ref()
                                .and_then(|menu| menu.target_entry_index);
                            let weak_self = cx.weak_entity();
                            let focus_handle = self.focus_handle.clone();
                            let table_focus_handle =
                                self.table_interaction_state.read(cx).focus_handle.clone();

                            let graph_canvas = div()
                                .id("graph-canvas")
                                .size_full()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .size_full()
                                        .child(self.render_graph_canvas(window, cx)),
                                );

                            let commits_table = Table::new(4)
                                .interactable(&self.table_interaction_state)
                                .hide_row_borders()
                                .hide_row_hover()
                                .width_config(table_width_config)
                                .column_filter(table_filter)
                                .map_row(move |(index, row), window, cx| {
                                    let is_selected = selected_entry_idx == Some(index);
                                    let is_hovered = hovered_entry_idx == Some(index);
                                    let is_context_menu_target =
                                        context_menu_target_index == Some(index);
                                    let table_focus_handle = table_focus_handle.clone();
                                    let is_focused = focus_handle.is_focused(window)
                                        || table_focus_handle.is_focused(window);
                                    let weak = weak_self.clone();
                                    let weak_for_hover = weak.clone();
                                    let weak_for_context_menu = weak.clone();

                                    let hover_bg = cx.theme().colors().element_hover.opacity(0.6);
                                    let selected_bg = if is_focused {
                                        cx.theme().colors().element_selected
                                    } else {
                                        cx.theme().colors().element_hover
                                    };

                                    row.h(row_height)
                                        .cursor_pointer()
                                        .when(is_selected || is_context_menu_target, |row| {
                                            row.bg(selected_bg)
                                        })
                                        .when(
                                            is_hovered && !is_selected && !is_context_menu_target,
                                            |row| row.bg(hover_bg),
                                        )
                                        .on_hover(move |&is_hovered, _, cx| {
                                            weak_for_hover
                                                .update(cx, |this, cx| {
                                                    if is_hovered {
                                                        if this.hovered_entry_idx != Some(index) {
                                                            this.hovered_entry_idx = Some(index);
                                                            cx.notify();
                                                        }
                                                    } else if this.hovered_entry_idx == Some(index)
                                                    {
                                                        this.hovered_entry_idx = None;
                                                        cx.notify();
                                                    }
                                                })
                                                .ok();
                                        })
                                        .on_click(move |event, window, cx| {
                                            weak.update(cx, |this, cx| {
                                                this.handle_entry_click(
                                                    index,
                                                    event,
                                                    ScrollStrategy::Center,
                                                    Some(&table_focus_handle),
                                                    window,
                                                    cx,
                                                );
                                            })
                                            .ok();
                                        })
                                        .on_mouse_down(
                                            MouseButton::Right,
                                            move |event: &MouseDownEvent, window, cx| {
                                                weak_for_context_menu
                                                    .update(cx, |this, cx| {
                                                        this.handle_entry_secondary_mouse_down(
                                                            index, event, window, cx,
                                                        );
                                                    })
                                                    .ok();
                                            },
                                        )
                                        .into_any_element()
                                })
                                .uniform_list(
                                    "git-graph-commits",
                                    commit_count,
                                    cx.processor(Self::render_table_rows),
                                );

                            bind_redistributable_columns(
                                div()
                                    .relative()
                                    .flex_1()
                                    .w_full()
                                    .overflow_hidden()
                                    .child(
                                        h_flex()
                                            .size_full()
                                            .when(!is_path_history && graph_visible, |this| {
                                                this.child(
                                                    h_flex()
                                                        .id("graph-column")
                                                        .map(|this| {
                                                            if table_collapsed {
                                                                this.w(graph_content_width)
                                                            } else if let Some(width) =
                                                                graph_pixel_width
                                                            {
                                                                this.w(width).flex_none()
                                                            } else {
                                                                this.w(DefiniteLength::Fraction(
                                                                    graph_fraction,
                                                                ))
                                                            }
                                                        })
                                                        .h_full()
                                                        .min_w_0()
                                                        .overflow_hidden()
                                                        .cursor_pointer()
                                                        .on_scroll(
                                                            cx.listener(Self::handle_graph_scroll),
                                                        )
                                                        .on_mouse_move(cx.listener(
                                                            Self::handle_graph_mouse_move,
                                                        ))
                                                        .on_click(
                                                            cx.listener(Self::handle_graph_click),
                                                        )
                                                        .on_mouse_down(
                                                            MouseButton::Right,
                                                            cx.listener(
                                                                Self::handle_graph_secondary_mouse_down,
                                                            ),
                                                        )
                                                        .on_hover(cx.listener(
                                                            |this, &is_hovered: &bool, _, cx| {
                                                                if !is_hovered
                                                                    && this
                                                                        .hovered_entry_idx
                                                                        .is_some()
                                                                {
                                                                    this.hovered_entry_idx = None;
                                                                    cx.notify();
                                                                }
                                                            },
                                                        ))
                                                        .child(
                                                            div()
                                                                .relative()
                                                                .flex_1()
                                                                .h_full()
                                                                .min_w_0()
                                                                .overflow_hidden()
                                                                .child(graph_canvas),
                                                        ),
                                                )
                                            })
                                            .child(
                                                div()
                                                    .tab_index(2)
                                                    .tab_group()
                                                    .tab_stop(false)
                                                    .map(|this| {
                                                        if table_collapsed
                                                            || graph_pixel_width.is_some()
                                                        {
                                                            this.flex_1()
                                                        } else {
                                                            this.w(DefiniteLength::Fraction(
                                                                table_fraction,
                                                            ))
                                                        }
                                                    })
                                                    .h_full()
                                                    .min_w_0()
                                                    .child(commits_table),
                                            ),
                                    )
                                    .child(render_redistributable_columns_resize_handles(
                                        &self.column_widths,
                                        Some(&self.column_visibility),
                                        window,
                                        cx,
                                    )),
                                self.column_widths.clone(),
                                Some(self.column_visibility.clone()),
                            )
                        }),
                )
                .on_drag_move::<DraggedSplitHandle>(cx.listener(|this, event, window, cx| {
                    this.commit_details_split_state.update(cx, |state, cx| {
                        state.on_drag_move(event, window, cx);
                    });
                }))
                .on_drop::<DraggedSplitHandle>(cx.listener(|this, _event, _window, cx| {
                    this.commit_details_split_state.update(cx, |state, _cx| {
                        state.commit_ratio();
                    });
                }))
                .when(
                    self.selected_entry_idx.is_some() || self.comparison.is_some(),
                    |this| {
                    this.child(self.render_commit_view_resize_handle(window, cx))
                        .child(self.render_commit_detail_panel(window, cx))
                })
        };

        div()
            .key_context("GitGraph")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(|this, _: &OpenCommitView, window, cx| {
                this.open_selected_commit_view(window, cx);
            }))
            .on_action(cx.listener(Self::copy_selected_commit_sha))
            .on_action(cx.listener(Self::copy_selected_commit_tag))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.search_state
                    .editor
                    .update(cx, |editor, cx| editor.focus_handle(cx).focus(window, cx));
                this.activate_search_editor_if_focused(window, cx);
            }))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::scroll_up))
            .on_action(cx.listener(Self::scroll_down))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::toggle_changed_files_view))
            .on_action(cx.listener(Self::focus_next_tab_stop))
            .on_action(cx.listener(Self::focus_previous_tab_stop))
            .on_action(cx.listener(|this, _: &SelectNextMatch, _window, cx| {
                this.select_next_match(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousMatch, _window, cx| {
                this.select_previous_match(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleCaseSensitive, _window, cx| {
                this.search_state.case_sensitive = !this.search_state.case_sensitive;
                this.search_state.state.next_state();
                cx.emit(ItemEvent::Edit);
                cx.notify();
            }))
            .child(
                v_flex()
                    .size_full()
                    .child(self.render_search_bar(cx))
                    .child(div().flex_1().child(content)),
            )
            .children(self.context_menu.as_ref().map(|context_menu| {
                deferred(
                    anchored()
                        .position(context_menu.position)
                        .anchor(Anchor::TopLeft)
                        .child(context_menu.menu.clone()),
                )
                .with_priority(1)
            }))
            .on_action(cx.listener(|_, _: &buffer_search::Deploy, window, cx| {
                window.dispatch_action(Box::new(FocusSearch), cx);
                cx.stop_propagation();
            }))
    }
}

impl EventEmitter<ItemEvent> for GitGraph {}

impl Focusable for GitGraph {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_state.editor.read(cx).focus_handle(cx)
    }
}

impl Item for GitGraph {
    type Event = ItemEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitGraph))
    }

    fn tab_tooltip_content(&self, cx: &App) -> Option<TabTooltipContent> {
        let repo_name = self.get_repository(cx).and_then(|repo| {
            repo.read(cx)
                .work_directory_abs_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        });
        let path_history_path = match &self.log_source {
            LogSource::Path(path) => Some(path.as_unix_str().to_string()),
            _ => None,
        };

        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            move |_, _| {
                v_flex()
                    .child(Label::new(if path_history_path.is_some() {
                        "Path History"
                    } else {
                        "Git Graph"
                    }))
                    .when_some(path_history_path.clone(), |this, path| {
                        this.child(Label::new(path).color(Color::Muted).size(LabelSize::Small))
                    })
                    .when_some(repo_name.clone(), |this, name| {
                        this.child(Label::new(name).color(Color::Muted).size(LabelSize::Small))
                    })
                    .into_any_element()
            }
        }))))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        if let LogSource::Path(path) = &self.log_source {
            return path
                .as_ref()
                .file_name()
                .map(|name| SharedString::from(name.to_string()))
                .unwrap_or_else(|| SharedString::from(path.as_unix_str().to_string()));
        }

        self.get_repository(cx)
            .and_then(|repo| {
                repo.read(cx)
                    .work_directory_abs_path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .map_or_else(|| "Git Graph".into(), |name| SharedString::from(name))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn deactivated(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(nav_history) = self.nav_history.as_mut() {
            nav_history.push::<()>(None, None, cx);
        }
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.nav_history = Some(nav_history);
    }
}

impl workspace::SerializableItem for GitGraph {
    fn serialized_item_kind() -> &'static str {
        "GitGraph"
    }

    fn cleanup(
        workspace_id: workspace::WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<gpui::Result<()>> {
        workspace::delete_unloaded_items(
            alive_items,
            workspace_id,
            "git_graphs",
            &persistence::GitGraphsDb::global(cx),
            cx,
        )
    }

    fn deserialize(
        project: Entity<project::Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<gpui::Result<Entity<Self>>> {
        let db = persistence::GitGraphsDb::global(cx);
        let Some((
            repo_work_path,
            log_source_type,
            log_source_value,
            log_order,
            selected_sha,
            search_query,
            search_case_sensitive,
            hidden_columns,
        )) = db.get_git_graph(item_id, workspace_id).ok().flatten()
        else {
            return Task::ready(Err(anyhow::anyhow!("No git graph to deserialize")));
        };

        let state = persistence::SerializedGitGraphState {
            log_source_type,
            log_source_value,
            log_order,
            selected_sha,
            search_query,
            search_case_sensitive,
            hidden_columns,
        };

        let window_handle = window.window_handle();
        let project = project.read(cx);
        let git_store = project.git_store().clone();
        let wait = project.wait_for_initial_scan(cx);

        cx.spawn(async move |cx| {
            wait.await;

            cx.update_window(window_handle, |_, window, cx| {
                let path = repo_work_path.as_path();

                let repositories = git_store.read(cx).repositories();
                let repo_id = repositories.iter().find_map(|(&repo_id, repo)| {
                    if repo.read(cx).snapshot().work_directory_abs_path.as_ref() == path {
                        Some(repo_id)
                    } else {
                        None
                    }
                });

                let Some(repo_id) = repo_id else {
                    return Err(anyhow::anyhow!("Repository not found for path: {:?}", path));
                };

                let log_source = persistence::deserialize_log_source(&state);
                let log_order = persistence::deserialize_log_order(&state);

                let git_graph = cx.new(|cx| {
                    let mut graph =
                        GitGraph::new(repo_id, git_store, workspace, Some(log_source), window, cx);
                    graph.log_order = log_order;

                    if let Some(sha) = &state.selected_sha {
                        graph.select_commit_by_sha(sha.as_str(), cx);
                    }

                    graph
                });

                git_graph.update(cx, |graph, cx| {
                    if let Some(bits) = state.hidden_columns {
                        let cols = graph.column_visibility.cols();
                        let mask = persistence::deserialize_hidden_columns(bits, cols);
                        // Never restore an all-hidden mask (e.g. from corrupt data); the UI
                        // guarantees at least one column stays visible.
                        if mask.iter().any(|is_hidden| !is_hidden) {
                            graph.column_visibility = TableRow::from_vec(mask, cols);
                        }
                    }

                    graph.search_state.case_sensitive =
                        state.search_case_sensitive.unwrap_or(false);

                    if let Some(query) = &state.search_query
                        && !query.is_empty()
                    {
                        graph
                            .search_state
                            .editor
                            .update(cx, |editor, cx| editor.set_text(query.as_str(), window, cx));
                        graph.search(query.clone().into(), cx);
                    }
                });

                Ok(git_graph)
            })?
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<gpui::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let repo = self.get_repository(cx)?;
        let repo_working_path = repo
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .to_string_lossy()
            .to_string();

        let selected_sha = self
            .selected_entry_idx
            .and_then(|idx| self.graph_data.commits.get(idx))
            .map(|commit| commit.data.sha.to_string());

        let search_query = self.search_state.editor.read(cx).text(cx);
        let search_query = if search_query.is_empty() {
            None
        } else {
            Some(search_query)
        };

        let log_source_type = Some(persistence::serialize_log_source_type(&self.log_source));
        let log_source_value = persistence::serialize_log_source_value(&self.log_source);
        let log_order = Some(persistence::serialize_log_order(&self.log_order));
        let search_case_sensitive = Some(self.search_state.case_sensitive);
        let hidden_columns = Some(persistence::serialize_hidden_columns(
            self.column_visibility.as_slice(),
        ));

        let db = persistence::GitGraphsDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_git_graph(
                item_id,
                workspace_id,
                repo_working_path,
                log_source_type,
                log_source_value,
                log_order,
                selected_sha,
                search_query,
                search_case_sensitive,
                hidden_columns,
            )
            .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        match event {
            ItemEvent::UpdateTab | ItemEvent::Edit => true,
            _ => false,
        }
    }
}

const COMMIT_DETAILS_METADATA_COLLAPSED_KEY: &str = "git_graph_commit_details_metadata_collapsed";
const COMMIT_DETAILS_MESSAGE_COLLAPSED_KEY: &str = "git_graph_commit_details_message_collapsed";

/// Commit details sections stay collapsed across git graphs and restarts.
fn read_commit_details_collapsed(key: &str, cx: &App) -> bool {
    KeyValueStore::global(cx)
        .read_kvp(key)
        .log_err()
        .is_some_and(|value| value.is_some())
}

fn write_commit_details_collapsed(key: &'static str, collapsed: bool, cx: &mut App) {
    let kvp = KeyValueStore::global(cx);
    db::write_and_log(cx, move || async move {
        if collapsed {
            kvp.write_kvp(key.into(), "1".into()).await
        } else {
            kvp.delete_kvp(key.into()).await
        }
    });
}

mod persistence {
    use std::{path::PathBuf, str::FromStr};

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use git::{
        Oid,
        repository::{LogOrder, LogSource, RepoPath},
    };
    use workspace::WorkspaceDb;

    pub struct GitGraphsDb(ThreadSafeConnection);

    impl Domain for GitGraphsDb {
        const NAME: &str = stringify!(GitGraphsDb);

        const MIGRATIONS: &[&str] = &[
            sql!(
                CREATE TABLE git_graphs (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,
                    is_open INTEGER DEFAULT FALSE,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN repo_working_path TEXT;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN log_source_type TEXT;
                ALTER TABLE git_graphs ADD COLUMN log_source_value TEXT;
                ALTER TABLE git_graphs ADD COLUMN log_order TEXT;
                ALTER TABLE git_graphs ADD COLUMN selected_sha TEXT;
                ALTER TABLE git_graphs ADD COLUMN search_query TEXT;
                ALTER TABLE git_graphs ADD COLUMN search_case_sensitive INTEGER;
            ),
            sql!(
                ALTER TABLE git_graphs ADD COLUMN hidden_columns INTEGER;
            ),
        ];
    }

    db::static_connection!(GitGraphsDb, [WorkspaceDb]);

    pub const LOG_SOURCE_ALL: i32 = 0;
    pub const LOG_SOURCE_BRANCH: i32 = 1;
    pub const LOG_SOURCE_SHA: i32 = 2;
    pub const LOG_SOURCE_PATH: i32 = 3;

    pub const LOG_ORDER_DATE: i32 = 0;
    pub const LOG_ORDER_TOPO: i32 = 1;
    pub const LOG_ORDER_AUTHOR_DATE: i32 = 2;
    pub const LOG_ORDER_REVERSE: i32 = 3;

    pub fn serialize_log_source_type(log_source: &LogSource) -> i32 {
        match log_source {
            LogSource::All => LOG_SOURCE_ALL,
            LogSource::Branch(_) => LOG_SOURCE_BRANCH,
            LogSource::Sha(_) => LOG_SOURCE_SHA,
            LogSource::Path(_) => LOG_SOURCE_PATH,
        }
    }

    pub fn serialize_log_source_value(log_source: &LogSource) -> Option<String> {
        match log_source {
            LogSource::All => None,
            LogSource::Branch(branch) => Some(branch.to_string()),
            LogSource::Sha(oid) => Some(oid.to_string()),
            LogSource::Path(path) => Some(path.as_unix_str().to_string()),
        }
    }

    pub fn serialize_log_order(log_order: &LogOrder) -> i32 {
        match log_order {
            LogOrder::DateOrder => LOG_ORDER_DATE,
            LogOrder::TopoOrder => LOG_ORDER_TOPO,
            LogOrder::AuthorDateOrder => LOG_ORDER_AUTHOR_DATE,
            LogOrder::ReverseChronological => LOG_ORDER_REVERSE,
        }
    }

    pub fn deserialize_log_source(state: &SerializedGitGraphState) -> LogSource {
        match state.log_source_type {
            Some(LOG_SOURCE_ALL) => LogSource::All,
            Some(LOG_SOURCE_BRANCH) => state
                .log_source_value
                .as_ref()
                .map(|v| LogSource::Branch(v.clone().into()))
                .unwrap_or_default(),
            Some(LOG_SOURCE_SHA) => state
                .log_source_value
                .as_ref()
                .and_then(|v| Oid::from_str(v).ok())
                .map(LogSource::Sha)
                .unwrap_or_default(),
            Some(LOG_SOURCE_PATH) => state
                .log_source_value
                .as_ref()
                .and_then(|v| RepoPath::new(v).ok())
                .map(LogSource::Path)
                .unwrap_or_default(),
            None | Some(_) => LogSource::default(),
        }
    }

    pub fn deserialize_log_order(state: &SerializedGitGraphState) -> LogOrder {
        match state.log_order {
            Some(LOG_ORDER_DATE) => LogOrder::DateOrder,
            Some(LOG_ORDER_TOPO) => LogOrder::TopoOrder,
            Some(LOG_ORDER_AUTHOR_DATE) => LogOrder::AuthorDateOrder,
            Some(LOG_ORDER_REVERSE) => LogOrder::ReverseChronological,
            _ => LogOrder::default(),
        }
    }

    /// Packs the per-column visibility mask into a bitmask (bit `i` set means column `i` is
    /// hidden), so it fits in a single integer database column regardless of column count.
    pub fn serialize_hidden_columns(hidden: &[bool]) -> i32 {
        hidden.iter().enumerate().fold(
            0,
            |bits, (idx, &is_hidden)| {
                if is_hidden { bits | (1 << idx) } else { bits }
            },
        )
    }

    /// Inverse of [`serialize_hidden_columns`]. Bits beyond `cols` are ignored, and missing
    /// bits default to visible, so a mask saved with a different column count degrades safely.
    pub fn deserialize_hidden_columns(bits: i32, cols: usize) -> Vec<bool> {
        (0..cols).map(|idx| bits & (1 << idx) != 0).collect()
    }

    #[derive(Debug, Default, Clone)]
    pub struct SerializedGitGraphState {
        pub log_source_type: Option<i32>,
        pub log_source_value: Option<String>,
        pub log_order: Option<i32>,
        pub selected_sha: Option<String>,
        pub search_query: Option<String>,
        pub search_case_sensitive: Option<bool>,
        pub hidden_columns: Option<i32>,
    }

    impl GitGraphsDb {
        query! {
            pub async fn save_git_graph(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId,
                repo_working_path: String,
                log_source_type: Option<i32>,
                log_source_value: Option<String>,
                log_order: Option<i32>,
                selected_sha: Option<String>,
                search_query: Option<String>,
                search_case_sensitive: Option<bool>,
                hidden_columns: Option<i32>
            ) -> Result<()> {
                INSERT OR REPLACE INTO git_graphs(
                    item_id, workspace_id, repo_working_path,
                    log_source_type, log_source_value, log_order,
                    selected_sha, search_query, search_case_sensitive,
                    hidden_columns
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            }
        }

        query! {
            pub fn get_git_graph(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId
            ) -> Result<Option<(
                PathBuf,
                Option<i32>,
                Option<String>,
                Option<i32>,
                Option<String>,
                Option<String>,
                Option<bool>,
                Option<i32>
            )>> {
                SELECT
                    repo_working_path,
                    log_source_type,
                    log_source_value,
                    log_order,
                    selected_sha,
                    search_query,
                    search_case_sensitive,
                    hidden_columns
                FROM git_graphs
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl GitGraph {
    pub fn search_for_test(&mut self, query: SharedString, cx: &mut Context<Self>) {
        self.search(query, cx);
    }

    pub fn search_matches_for_test(&self) -> Vec<Oid> {
        self.search_state.matches.iter().copied().collect()
    }

    pub fn initial_commit_data_for_test(&self) -> Vec<Arc<InitialGraphCommitData>> {
        self.graph_data
            .commits
            .iter()
            .map(|commit| commit.data.clone())
            .collect()
    }

    pub fn commit_count_and_loading_state_for_test(
        &mut self,
        cx: &mut Context<Self>,
    ) -> (usize, bool) {
        self.commit_count_and_loading_state(cx)
    }

    pub fn log_source_for_test(&self) -> &LogSource {
        &self.log_source
    }
}

/// Generates a random commit DAG suitable for testing git graph rendering.
///
/// The commits are ordered newest-first (like git log output), so:
/// - Index 0 = most recent commit (HEAD)
/// - Last index = oldest commit (root, has no parents)
/// - Parents of commit at index I must have index > I
///
/// When `adversarial` is true, generates complex topologies with many branches
/// and octopus merges. Otherwise generates more realistic linear histories
/// with occasional branches.
#[cfg(any(test, feature = "test-support"))]
pub fn generate_random_commit_dag(
    rng: &mut rand::rngs::StdRng,
    num_commits: usize,
    adversarial: bool,
) -> Vec<Arc<InitialGraphCommitData>> {
    use rand::Rng as _;

    if num_commits == 0 {
        return Vec::new();
    }

    let mut commits: Vec<Arc<InitialGraphCommitData>> = Vec::with_capacity(num_commits);
    let oids: Vec<Oid> = (0..num_commits).map(|_| Oid::random(rng)).collect();

    for i in 0..num_commits {
        let sha = oids[i];

        let parents = if i == num_commits - 1 {
            smallvec![]
        } else {
            generate_parents_from_oids(rng, &oids, i, num_commits, adversarial)
        };

        let ref_names = if i == 0 {
            vec!["HEAD".into(), "main".into()]
        } else if adversarial && rng.random_bool(0.1) {
            vec![format!("branch-{i}").into()]
        } else {
            Vec::new()
        };

        commits.push(Arc::new(InitialGraphCommitData {
            sha,
            parents,
            ref_names,
        }));
    }

    commits
}

#[cfg(any(test, feature = "test-support"))]
fn generate_parents_from_oids(
    rng: &mut rand::rngs::StdRng,
    oids: &[Oid],
    current_idx: usize,
    num_commits: usize,
    adversarial: bool,
) -> SmallVec<[Oid; 1]> {
    use rand::{Rng as _, seq::SliceRandom as _};

    let remaining = num_commits - current_idx - 1;
    if remaining == 0 {
        return smallvec![];
    }

    if adversarial {
        let merge_chance = 0.4;
        let octopus_chance = 0.15;

        if remaining >= 3 && rng.random_bool(octopus_chance) {
            let num_parents = rng.random_range(3..=remaining.min(5));
            let mut parent_indices: Vec<usize> = (current_idx + 1..num_commits).collect();
            parent_indices.shuffle(rng);
            parent_indices
                .into_iter()
                .take(num_parents)
                .map(|idx| oids[idx])
                .collect()
        } else if remaining >= 2 && rng.random_bool(merge_chance) {
            let mut parent_indices: Vec<usize> = (current_idx + 1..num_commits).collect();
            parent_indices.shuffle(rng);
            parent_indices
                .into_iter()
                .take(2)
                .map(|idx| oids[idx])
                .collect()
        } else {
            let parent_idx = rng.random_range(current_idx + 1..num_commits);
            smallvec![oids[parent_idx]]
        }
    } else {
        let merge_chance = 0.15;
        let skip_chance = 0.1;

        if remaining >= 2 && rng.random_bool(merge_chance) {
            let first_parent = current_idx + 1;
            let second_parent = rng.random_range(current_idx + 2..num_commits);
            smallvec![oids[first_parent], oids[second_parent]]
        } else if rng.random_bool(skip_chance) && remaining >= 2 {
            let skip = rng.random_range(1..remaining.min(3));
            smallvec![oids[current_idx + 1 + skip]]
        } else {
            smallvec![oids[current_idx + 1]]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result, bail};
    use collections::{HashMap, HashSet};
    use fs::FakeFs;
    use git::Oid;
    use git::repository::{CommitData, InitialGraphCommitData};
    use gpui::{TestAppContext, UpdateGlobal};
    use project::git_store::{GitStoreEvent, RepositoryEvent};
    use project::{
        GIT_COMMAND_TASK_TAG, Project, TaskSourceKind, task_store::TaskSettingsLocation,
    };
    use rand::prelude::*;
    use serde_json::json;
    use settings::{SettingsStore, ThemeSettingsContent};
    use smallvec::{SmallVec, smallvec};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            crate::init(cx);
        });
    }

    fn build_oid_to_row_map(graph: &GraphData) -> HashMap<Oid, usize> {
        graph
            .commits
            .iter()
            .enumerate()
            .map(|(idx, entry)| (entry.data.sha, idx))
            .collect()
    }

    fn verify_commit_order(
        graph: &GraphData,
        commits: &[Arc<InitialGraphCommitData>],
    ) -> Result<()> {
        if graph.commits.len() != commits.len() {
            bail!(
                "Commit count mismatch: graph has {} commits, expected {}",
                graph.commits.len(),
                commits.len()
            );
        }

        for (idx, (graph_commit, expected_commit)) in
            graph.commits.iter().zip(commits.iter()).enumerate()
        {
            if graph_commit.data.sha != expected_commit.sha {
                bail!(
                    "Commit order mismatch at index {}: graph has {:?}, expected {:?}",
                    idx,
                    graph_commit.data.sha,
                    expected_commit.sha
                );
            }
        }

        Ok(())
    }

    fn verify_line_endpoints(graph: &GraphData, oid_to_row: &HashMap<Oid, usize>) -> Result<()> {
        for line in &graph.lines {
            let child_row = *oid_to_row
                .get(&line.child)
                .context("Line references non-existent child commit")?;

            let parent_row = *oid_to_row
                .get(&line.parent)
                .context("Line references non-existent parent commit")?;

            if child_row >= parent_row {
                bail!(
                    "child_row ({}) must be < parent_row ({})",
                    child_row,
                    parent_row
                );
            }

            if line.full_interval.start != child_row {
                bail!(
                    "full_interval.start ({}) != child_row ({})",
                    line.full_interval.start,
                    child_row
                );
            }

            if line.full_interval.end != parent_row {
                bail!(
                    "full_interval.end ({}) != parent_row ({})",
                    line.full_interval.end,
                    parent_row
                );
            }

            if let Some(last_segment) = line.segments.last() {
                let segment_end_row = match last_segment {
                    CommitLineSegment::Straight { to_row } => *to_row,
                    CommitLineSegment::Curve { on_row, .. } => *on_row,
                };

                if segment_end_row != line.full_interval.end {
                    bail!(
                        "last segment ends at row {} but full_interval.end is {}",
                        segment_end_row,
                        line.full_interval.end
                    );
                }
            }
        }

        Ok(())
    }

    fn verify_column_correctness(
        graph: &GraphData,
        oid_to_row: &HashMap<Oid, usize>,
    ) -> Result<()> {
        for line in &graph.lines {
            let child_row = *oid_to_row
                .get(&line.child)
                .context("Line references non-existent child commit")?;

            let parent_row = *oid_to_row
                .get(&line.parent)
                .context("Line references non-existent parent commit")?;

            let child_lane = graph.commits[child_row].lane;
            if line.child_column != child_lane {
                bail!(
                    "child_column ({}) != child's lane ({})",
                    line.child_column,
                    child_lane
                );
            }

            let mut current_column = line.child_column;
            for segment in &line.segments {
                if let CommitLineSegment::Curve { to_column, .. } = segment {
                    current_column = *to_column;
                }
            }

            let parent_lane = graph.commits[parent_row].lane;
            if current_column != parent_lane {
                bail!(
                    "ending column ({}) != parent's lane ({})",
                    current_column,
                    parent_lane
                );
            }
        }

        Ok(())
    }

    fn verify_segment_continuity(graph: &GraphData) -> Result<()> {
        for line in &graph.lines {
            if line.segments.is_empty() {
                bail!("Line has no segments");
            }

            let mut current_row = line.full_interval.start;

            for (idx, segment) in line.segments.iter().enumerate() {
                let segment_end_row = match segment {
                    CommitLineSegment::Straight { to_row } => *to_row,
                    CommitLineSegment::Curve { on_row, .. } => *on_row,
                };

                if segment_end_row < current_row {
                    bail!(
                        "segment {} ends at row {} which is before current row {}",
                        idx,
                        segment_end_row,
                        current_row
                    );
                }

                current_row = segment_end_row;
            }
        }

        Ok(())
    }

    fn verify_line_overlaps(graph: &GraphData) -> Result<()> {
        for line in &graph.lines {
            let child_row = line.full_interval.start;

            let mut current_column = line.child_column;
            let mut current_row = child_row;

            for segment in &line.segments {
                match segment {
                    CommitLineSegment::Straight { to_row } => {
                        for row in (current_row + 1)..*to_row {
                            if row < graph.commits.len() {
                                let commit_at_row = &graph.commits[row];
                                if commit_at_row.lane == current_column {
                                    bail!(
                                        "straight segment from row {} to {} in column {} passes through commit {:?} at row {}",
                                        current_row,
                                        to_row,
                                        current_column,
                                        commit_at_row.data.sha,
                                        row
                                    );
                                }
                            }
                        }
                        current_row = *to_row;
                    }
                    CommitLineSegment::Curve {
                        to_column, on_row, ..
                    } => {
                        current_column = *to_column;
                        current_row = *on_row;
                    }
                }
            }
        }

        Ok(())
    }

    fn verify_keep_shared_parents_on_leftmost_lane(graph: &GraphData) -> Result<()> {
        let mut active_lane_parents: Vec<Option<Oid>> = Vec::new();
        let mut parent_to_lanes: HashMap<Oid, SmallVec<[usize; 1]>> = HashMap::default();

        for (row, entry) in graph.commits.iter().enumerate() {
            let pending_lanes = parent_to_lanes.remove(&entry.data.sha).unwrap_or_default();

            if pending_lanes.len() > 1
                && let Some(expected_lane) = pending_lanes.iter().copied().min()
                && entry.lane != expected_lane
            {
                bail!(
                    "commit {:?} at row {} uses lane {}, but shared parent should use leftmost pending lane {} from {:?}",
                    entry.data.sha,
                    row,
                    entry.lane,
                    expected_lane,
                    pending_lanes
                );
            }

            for lane in pending_lanes {
                let Some(active_lane_parent) = active_lane_parents.get_mut(lane) else {
                    bail!(
                        "commit {:?} at row {} was pending on missing lane {}",
                        entry.data.sha,
                        row,
                        lane
                    );
                };

                if *active_lane_parent != Some(entry.data.sha) {
                    bail!(
                        "commit {:?} at row {} was pending on lane {}, but that lane points to {:?}",
                        entry.data.sha,
                        row,
                        lane,
                        active_lane_parent
                    );
                }

                *active_lane_parent = None;
            }

            for (parent_index, parent) in entry.data.parents.iter().enumerate() {
                let lane = if parent_index == 0 {
                    entry.lane
                } else if let Some(empty_lane) =
                    active_lane_parents.iter().position(Option::is_none)
                {
                    empty_lane
                } else {
                    active_lane_parents.push(None);
                    active_lane_parents.len() - 1
                };

                if lane >= active_lane_parents.len() {
                    active_lane_parents.resize(lane + 1, None);
                }

                active_lane_parents[lane] = Some(*parent);
                parent_to_lanes.entry(*parent).or_default().push(lane);
            }
        }

        Ok(())
    }

    fn verify_coverage(graph: &GraphData) -> Result<()> {
        let mut expected_edges: HashSet<(Oid, Oid)> = HashSet::default();
        for entry in &graph.commits {
            for parent in &entry.data.parents {
                expected_edges.insert((entry.data.sha, *parent));
            }
        }

        let mut found_edges: HashSet<(Oid, Oid)> = HashSet::default();
        for line in &graph.lines {
            let edge = (line.child, line.parent);

            if !found_edges.insert(edge) {
                bail!(
                    "Duplicate line found for edge {:?} -> {:?}",
                    line.child,
                    line.parent
                );
            }

            if !expected_edges.contains(&edge) {
                bail!(
                    "Orphan line found: {:?} -> {:?} is not in the commit graph",
                    line.child,
                    line.parent
                );
            }
        }

        for (child, parent) in &expected_edges {
            if !found_edges.contains(&(*child, *parent)) {
                bail!("Missing line for edge {:?} -> {:?}", child, parent);
            }
        }

        assert_eq!(
            expected_edges.symmetric_difference(&found_edges).count(),
            0,
            "The symmetric difference should be zero"
        );

        Ok(())
    }

    fn verify_merge_line_optimality(
        graph: &GraphData,
        oid_to_row: &HashMap<Oid, usize>,
    ) -> Result<()> {
        for line in &graph.lines {
            let first_segment = line.segments.first();
            let is_merge_line = matches!(
                first_segment,
                Some(CommitLineSegment::Curve {
                    curve_kind: CurveKind::Merge,
                    ..
                })
            );

            if !is_merge_line {
                continue;
            }

            let child_row = *oid_to_row
                .get(&line.child)
                .context("Line references non-existent child commit")?;

            let parent_row = *oid_to_row
                .get(&line.parent)
                .context("Line references non-existent parent commit")?;

            let parent_lane = graph.commits[parent_row].lane;

            let Some(CommitLineSegment::Curve { to_column, .. }) = first_segment else {
                continue;
            };

            let curves_directly_to_parent = *to_column == parent_lane;

            if !curves_directly_to_parent {
                continue;
            }

            let curve_row = child_row + 1;
            let has_commits_in_path = graph.commits[curve_row..parent_row]
                .iter()
                .any(|c| c.lane == parent_lane);

            if has_commits_in_path {
                bail!(
                    "Merge line from {:?} to {:?} curves directly to parent lane {} but there are commits in that lane between rows {} and {}",
                    line.child,
                    line.parent,
                    parent_lane,
                    curve_row,
                    parent_row
                );
            }

            let curve_ends_at_parent = curve_row == parent_row;

            if curve_ends_at_parent {
                if line.segments.len() != 1 {
                    bail!(
                        "Merge line from {:?} to {:?} curves directly to parent (curve_row == parent_row), but has {} segments instead of 1 [MergeCurve]",
                        line.child,
                        line.parent,
                        line.segments.len()
                    );
                }
            } else {
                if line.segments.len() != 2 {
                    bail!(
                        "Merge line from {:?} to {:?} curves directly to parent lane without overlap, but has {} segments instead of 2 [MergeCurve, Straight]",
                        line.child,
                        line.parent,
                        line.segments.len()
                    );
                }

                let is_straight_segment = matches!(
                    line.segments.get(1),
                    Some(CommitLineSegment::Straight { .. })
                );

                if !is_straight_segment {
                    bail!(
                        "Merge line from {:?} to {:?} curves directly to parent lane without overlap, but second segment is not a Straight segment",
                        line.child,
                        line.parent
                    );
                }
            }
        }

        Ok(())
    }

    fn verify_all_invariants(
        graph: &GraphData,
        commits: &[Arc<InitialGraphCommitData>],
    ) -> Result<()> {
        let oid_to_row = build_oid_to_row_map(graph);

        verify_commit_order(graph, commits).context("commit order")?;
        verify_line_endpoints(graph, &oid_to_row).context("line endpoints")?;
        verify_column_correctness(graph, &oid_to_row).context("column correctness")?;
        verify_segment_continuity(graph).context("segment continuity")?;
        verify_merge_line_optimality(graph, &oid_to_row).context("merge line optimality")?;
        verify_keep_shared_parents_on_leftmost_lane(graph)
            .context("keep shared parents on leftmost lane")?;
        verify_coverage(graph).context("coverage")?;
        verify_line_overlaps(graph).context("line overlaps")?;
        Ok(())
    }

    #[test]
    fn test_git_graph_merge_commits() {
        let mut rng = StdRng::seed_from_u64(42);

        let oid1 = Oid::random(&mut rng);
        let oid2 = Oid::random(&mut rng);
        let oid3 = Oid::random(&mut rng);
        let oid4 = Oid::random(&mut rng);

        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: oid1,
                parents: smallvec![oid2, oid3],
                ref_names: vec!["HEAD".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid2,
                parents: smallvec![oid4],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid3,
                parents: smallvec![oid4],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid4,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!("Graph invariant violation for merge commits:\n{}", error);
        }
    }

    #[test]
    fn test_git_graph_linear_commits() {
        let mut rng = StdRng::seed_from_u64(42);

        let oid1 = Oid::random(&mut rng);
        let oid2 = Oid::random(&mut rng);
        let oid3 = Oid::random(&mut rng);

        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: oid1,
                parents: smallvec![oid2],
                ref_names: vec!["HEAD".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid2,
                parents: smallvec![oid3],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: oid3,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!("Graph invariant violation for linear commits:\n{}", error);
        }
    }

    #[test]
    fn test_git_graph_random_commits() {
        for seed in 0..100 {
            let mut rng = StdRng::seed_from_u64(seed);

            let adversarial = rng.random_bool(0.2);
            let num_commits = if adversarial {
                rng.random_range(10..100)
            } else {
                rng.random_range(5..50)
            };

            let commits = generate_random_commit_dag(&mut rng, num_commits, adversarial);

            assert_eq!(
                num_commits,
                commits.len(),
                "seed={}: Generate random commit dag didn't generate the correct amount of commits",
                seed
            );

            let mut graph_data = GraphData::new(8);
            graph_data.add_commits(&commits);

            if let Err(error) = verify_all_invariants(&graph_data, &commits) {
                panic!(
                    "Graph invariant violation (seed={}, adversarial={}, num_commits={}):\n{:#}",
                    seed, adversarial, num_commits, error
                );
            }
        }
    }

    // The full integration test has less iterations because it's significantly slower
    // than the random commit test
    #[gpui::test(iterations = 10)]
    async fn test_git_graph_random_integration(mut rng: StdRng, cx: &mut TestAppContext) {
        init_test(cx);

        let adversarial = rng.random_bool(0.2);
        let num_commits = if adversarial {
            rng.random_range(10..100)
        } else {
            rng.random_range(5..50)
        };

        let commits = generate_random_commit_dag(&mut rng, num_commits, adversarial);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(LogSource::default(), LogOrder::default(), 0..usize::MAX, cx);
        });
        cx.run_until_parked();

        let graph_commits: Vec<Arc<InitialGraphCommitData>> = repository.update(cx, |repo, cx| {
            repo.graph_data(LogSource::default(), LogOrder::default(), 0..usize::MAX, cx)
                .commits
                .to_vec()
        });

        let mut graph_data = GraphData::new(8);
        graph_data.add_commits(&graph_commits);

        if let Err(error) = verify_all_invariants(&graph_data, &commits) {
            panic!(
                "Graph invariant violation (adversarial={}, num_commits={}):\n{:#}",
                adversarial, num_commits, error
            );
        }
    }

    #[gpui::test]
    async fn test_empty_nested_repository_graph_stops_loading(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                "repo_a": {
                    ".git": {},
                    "file_a.txt": "content",
                },
                "repo_b": {
                    ".git": {},
                    "file_b.txt": "content",
                },
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            assert_eq!(project.repositories(cx).len(), 2);
            project
                .active_repository(cx)
                .expect("should have an active repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let (commit_count, is_loading) = git_graph.update(cx, |graph, cx| {
            graph.commit_count_and_loading_state_for_test(cx)
        });

        assert_eq!(commit_count, 0);
        assert!(!is_loading, "empty graph data should stop loading");
    }

    #[gpui::test]
    async fn test_initial_graph_data_not_cleared_on_initial_loading(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [], cx).await;
        let observed_repository_events = Arc::new(Mutex::new(Vec::new()));
        project.update(cx, |project, cx| {
            let observed_repository_events = observed_repository_events.clone();
            cx.subscribe(project.git_store(), move |_, _, event, _| {
                if let GitStoreEvent::RepositoryUpdated(_, repository_event, true) = event {
                    observed_repository_events
                        .lock()
                        .expect("repository event mutex should be available")
                        .push(repository_event.clone());
                }
            })
            .detach();
        });
        project
            .update(cx, |project, cx| {
                project.create_worktree("/project", true, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(LogSource::default(), LogOrder::default(), 0..usize::MAX, cx);
        });

        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        let observed_repository_events = observed_repository_events
            .lock()
            .expect("repository event mutex should be available");
        assert!(
            observed_repository_events
                .iter()
                .any(|event| matches!(event, RepositoryEvent::HeadChanged)),
            "initial repository scan should emit HeadChanged"
        );
        let commit_count_after = repository.read_with(cx, |repo, _| {
            repo.get_graph_data(LogSource::default(), LogOrder::default())
                .map(|data| data.commit_data.len())
                .unwrap()
        });
        assert_eq!(
            commits.len(),
            commit_count_after,
            "initial_graph_data should remain populated after events emitted by initial repository scan"
        );
    }

    #[gpui::test]
    async fn test_initial_graph_data_propagates_error(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        fs.set_graph_error(
            Path::new("/project/.git"),
            Some("fatal: bad default revision 'HEAD'".to_string()),
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        repository.update(cx, |repo, cx| {
            repo.graph_data(LogSource::default(), LogOrder::default(), 0..usize::MAX, cx);
        });

        cx.run_until_parked();

        let error = repository.read_with(cx, |repo, _| {
            repo.get_graph_data(LogSource::default(), LogOrder::default())
                .and_then(|data| data.error.clone())
        });

        assert!(
            error.is_some(),
            "graph data should contain an error after initial_graph_data fails"
        );
        let error_message = error.unwrap();
        assert!(
            error_message.contains("bad default revision"),
            "error should contain the git error message, got: {}",
            error_message
        );
    }

    #[gpui::test]
    async fn test_focusing_graph_activates_its_repository(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(Path::new("/project_a"), json!({ ".git": {}, "a.txt": "a" }))
            .await;
        fs.insert_tree(Path::new("/project_b"), json!({ ".git": {}, "b.txt": "b" }))
            .await;
        let project = Project::test(
            fs.clone(),
            [Path::new("/project_a"), Path::new("/project_b")],
            cx,
        )
        .await;
        cx.run_until_parked();

        let repository_at = |path: &str, cx: &mut TestAppContext| {
            project.read_with(cx, |project, cx| {
                project
                    .repositories(cx)
                    .values()
                    .find(|repository| {
                        repository.read(cx).work_directory_abs_path.as_ref() == Path::new(path)
                    })
                    .cloned()
                    .expect("repository should exist")
            })
        };
        let repository_a = repository_at("/project_a", cx);
        let repository_b = repository_at("/project_b", cx);
        repository_b.update(cx, |repository, cx| repository.set_as_active_repository(cx));
        cx.run_until_parked();

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        });
        let workspace_weak = workspace.downgrade();
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository_a.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |git_graph, window, cx| {
            window.focus(&git_graph.focus_handle, cx);
        });
        cx.run_until_parked();

        let active_repository = project.read_with(&*cx, |project, cx| {
            project
                .active_repository(cx)
                .map(|repository| repository.read(cx).id)
        });
        assert_eq!(
            active_repository,
            Some(repository_a.read_with(&*cx, |repository, _| repository.id))
        );
    }

    #[gpui::test]
    async fn test_graph_data_repopulated_from_cache_after_repo_switch(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project_a"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        fs.insert_tree(
            Path::new("/project_b"),
            json!({
                ".git": {},
                "other.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project_a/.git"), commits.clone());

        let project = Project::test(
            fs.clone(),
            [Path::new("/project_a"), Path::new("/project_b")],
            cx,
        )
        .await;
        cx.run_until_parked();

        let (first_repository, second_repository) = project.read_with(cx, |project, cx| {
            let mut first_repository = None;
            let mut second_repository = None;

            for repository in project.repositories(cx).values() {
                let work_directory_abs_path = &repository.read(cx).work_directory_abs_path;
                if work_directory_abs_path.as_ref() == Path::new("/project_a") {
                    first_repository = Some(repository.clone());
                } else if work_directory_abs_path.as_ref() == Path::new("/project_b") {
                    second_repository = Some(repository.clone());
                }
            }

            (
                first_repository.expect("should have repository for /project_a"),
                second_repository.expect("should have repository for /project_b"),
            )
        });
        first_repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
        cx.run_until_parked();

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                first_repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Verify initial graph data is loaded
        let initial_commit_count =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert!(
            initial_commit_count > 0,
            "graph data should have been loaded, got 0 commits"
        );

        git_graph.update(cx, |graph, cx| {
            graph.set_repo_id(second_repository.read(cx).id, cx)
        });
        cx.run_until_parked();

        let commit_count_after_clear =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            commit_count_after_clear, 0,
            "graph_data should be cleared after switching away"
        );

        git_graph.update(cx, |graph, cx| {
            graph.set_repo_id(first_repository.read(cx).id, cx)
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        // Verify graph data is reloaded from repository cache on switch back
        let reloaded_commit_count =
            git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            reloaded_commit_count,
            commits.len(),
            "graph data should be reloaded after switching back"
        );
    }

    #[gpui::test]
    async fn test_file_history_action_uses_git_panel_and_editor_sources(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new(util::path!("/project")),
            json!({
                ".git": {},
                "tracked1.txt": "tracked 1",
                "tracked2.txt": "tracked 2",
            }),
        )
        .await;
        fs.set_status_for_repo(
            Path::new(util::path!("/project/.git")),
            &[
                ("tracked1.txt", StatusCode::Modified.worktree()),
                ("tracked2.txt", StatusCode::Modified.worktree()),
            ],
        );

        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: Oid::from_bytes(&[1; 20]).unwrap(),
            parents: smallvec![],
            ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
        })];
        fs.set_graph_commits(Path::new(util::path!("/project/.git")), commits);

        let project = Project::test(fs.clone(), [Path::new(util::path!("/project"))], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have active repository")
        });
        let tracked1_repo_path = RepoPath::new(&"tracked1.txt").unwrap();
        let tracked2_repo_path = RepoPath::new(&"tracked2.txt").unwrap();
        let tracked1 = repository
            .read_with(cx, |repository, cx| {
                repository.repo_path_to_project_path(&tracked1_repo_path, cx)
            })
            .expect("tracked1 should resolve to project path");
        let tracked2 = repository
            .read_with(cx, |repository, cx| {
                repository.repo_path_to_project_path(&tracked2_repo_path, cx)
            })
            .expect("tracked2 should resolve to project path");

        let workspace_window = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = workspace_window
            .read_with(cx, |multi, _| multi.workspace().clone())
            .expect("workspace should exist");

        let (weak_workspace, async_window_cx) = workspace_window
            .update(cx, |multi, window, cx| {
                (multi.workspace().downgrade(), window.to_async(cx))
            })
            .expect("window should be available");
        cx.background_executor.allow_parking();
        let git_panel = cx
            .foreground_executor()
            .clone()
            .block_test(crate::git_panel::GitPanel::load(
                weak_workspace,
                async_window_cx,
            ))
            .expect("git panel should load");
        cx.background_executor.forbid_parking();

        workspace_window
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace();
                workspace.update(cx, |workspace, cx| {
                    workspace.add_panel(git_panel.clone(), window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();

        workspace_window
            .update(cx, |_, window, cx| {
                git_panel.update(cx, |panel, cx| {
                    panel.select_entry_by_path(tracked1.clone(), window, cx);
                });
                git_panel.update(cx, |panel, cx| {
                    panel.focus_handle(cx).focus(window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();
        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 1);
            assert_eq!(
                graphs[0].read(cx).log_source,
                LogSource::Path(tracked1_repo_path.clone())
            );
        });

        workspace_window
            .update(cx, |_, window, cx| {
                git_panel.update(cx, |panel, cx| {
                    panel.select_entry_by_path(tracked1.clone(), window, cx);
                });
                git_panel.update(cx, |panel, cx| {
                    panel.focus_handle(cx).focus(window, cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();
        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 1);
            assert_eq!(
                graphs[0].read(cx).log_source,
                LogSource::Path(tracked1_repo_path.clone())
            );
        });

        let tracked1_buffer = project
            .update(cx, |project, cx| project.open_buffer(tracked1.clone(), cx))
            .await
            .expect("tracked1 buffer should open");
        let tracked2_buffer = project
            .update(cx, |project, cx| project.open_buffer(tracked2.clone(), cx))
            .await
            .expect("tracked2 buffer should open");
        workspace_window
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace();
                let multibuffer = cx.new(|cx| {
                    let mut multibuffer = editor::MultiBuffer::new(language::Capability::ReadWrite);
                    multibuffer.set_excerpts_for_buffer(
                        tracked1_buffer.clone(),
                        [Default::default()..tracked1_buffer.read(cx).max_point()],
                        0,
                        cx,
                    );
                    multibuffer.set_excerpts_for_buffer(
                        tracked2_buffer.clone(),
                        [Default::default()..tracked2_buffer.read(cx).max_point()],
                        0,
                        cx,
                    );
                    multibuffer
                });
                let editor = cx.new(|cx| {
                    Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx)
                });
                workspace.update(cx, |workspace, cx| {
                    workspace.add_item_to_active_pane(
                        Box::new(editor.clone()),
                        None,
                        true,
                        window,
                        cx,
                    );
                });
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    let second_excerpt_point = snapshot
                        .range_for_buffer(tracked2_buffer.read(cx).remote_id())
                        .expect("tracked2 excerpt should exist")
                        .start;
                    let anchor = snapshot.anchor_before(second_excerpt_point);
                    editor.change_selections(
                        editor::SelectionEffects::no_scroll(),
                        window,
                        cx,
                        |selections| {
                            selections.select_anchor_ranges([anchor..anchor]);
                        },
                    );
                    window.focus(&editor.focus_handle(cx), cx);
                });
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace_window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(git::FileHistory), cx);
            })
            .expect("workspace window should be available");
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 2);
            let latest = graphs
                .into_iter()
                .max_by_key(|graph| graph.entity_id())
                .expect("expected a git graph");
            assert_eq!(
                latest.read(cx).log_source,
                LogSource::Path(tracked2_repo_path)
            );
        });
    }

    #[gpui::test]
    async fn test_file_history_action_resolves_through_project_diff(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new(util::path!("/project")),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(util::path!("/project/.git")),
            &[("file.txt", "tracked".to_owned())],
        );

        let project = Project::test(fs.clone(), [Path::new(util::path!("/project"))], cx).await;
        cx.run_until_parked();

        let tracked_repo_path = RepoPath::new(&"file.txt").unwrap();

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());

        // A project diff is a `ProjectDiff` item wrapping a `SplittableEditor`, not
        // an `Editor` itself, so resolving the file-history target must go through
        // `act_as` rather than a direct downcast of the active item.
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(crate::project_diff::Diff), cx);
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<crate::ProjectDiff>(cx)
                .expect("project diff should be the active item");
        });

        cx.update(|window, cx| {
            window.dispatch_action(Box::new(git::FileHistory), cx);
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(
                graphs.len(),
                1,
                "dispatching FileHistory from a project diff should open a git graph"
            );
            assert_eq!(
                graphs[0].read(cx).log_source,
                LogSource::Path(tracked_repo_path)
            );
        });
    }

    #[gpui::test]
    fn test_serialized_state_roundtrip(_cx: &mut TestAppContext) {
        use persistence::SerializedGitGraphState;

        let path = RepoPath::new(&"src/main.rs").unwrap();
        let sha = Oid::from_bytes(&[0xab; 20]).unwrap();

        let state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_PATH),
            log_source_value: Some("src/main.rs".to_string()),
            log_order: Some(persistence::LOG_ORDER_TOPO),
            selected_sha: Some(sha.to_string()),
            search_query: Some("fix bug".to_string()),
            search_case_sensitive: Some(true),
            hidden_columns: None,
        };

        assert_eq!(
            persistence::deserialize_log_source(&state),
            LogSource::Path(path)
        );
        assert!(matches!(
            persistence::deserialize_log_order(&state),
            LogOrder::TopoOrder
        ));
        assert_eq!(
            state.selected_sha.as_deref(),
            Some(sha.to_string()).as_deref()
        );
        assert_eq!(state.search_query.as_deref(), Some("fix bug"));
        assert_eq!(state.search_case_sensitive, Some(true));

        let all_state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_ALL),
            log_source_value: None,
            log_order: Some(persistence::LOG_ORDER_DATE),
            selected_sha: None,
            search_query: None,
            search_case_sensitive: None,
            hidden_columns: None,
        };
        assert_eq!(
            persistence::deserialize_log_source(&all_state),
            LogSource::All
        );
        assert!(matches!(
            persistence::deserialize_log_order(&all_state),
            LogOrder::DateOrder
        ));

        let branch_state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_BRANCH),
            log_source_value: Some("refs/heads/main".to_string()),
            ..Default::default()
        };
        assert_eq!(
            persistence::deserialize_log_source(&branch_state),
            LogSource::Branch("refs/heads/main".into())
        );

        let sha_state = SerializedGitGraphState {
            log_source_type: Some(persistence::LOG_SOURCE_SHA),
            log_source_value: Some(sha.to_string()),
            ..Default::default()
        };
        assert_eq!(
            persistence::deserialize_log_source(&sha_state),
            LogSource::Sha(sha)
        );

        let empty_state = SerializedGitGraphState::default();
        assert_eq!(
            persistence::deserialize_log_source(&empty_state),
            LogSource::All
        );
        assert!(matches!(
            persistence::deserialize_log_order(&empty_state),
            LogOrder::DateOrder
        ));
    }

    #[gpui::test]
    fn test_hidden_columns_bitmask_roundtrip(_cx: &mut TestAppContext) {
        let mask = [false, true, false, true, false];
        let bits = persistence::serialize_hidden_columns(&mask);
        assert_eq!(
            persistence::deserialize_hidden_columns(bits, mask.len()),
            mask.to_vec()
        );

        assert_eq!(persistence::serialize_hidden_columns(&[false; 5]), 0);
        assert_eq!(
            persistence::deserialize_hidden_columns(0, 4),
            vec![false; 4]
        );

        // A mask saved with more columns than we restore with is truncated safely, and one
        // saved with fewer columns defaults the extra columns to visible.
        let bits = persistence::serialize_hidden_columns(&[true, false, true, false, true]);
        assert_eq!(
            persistence::deserialize_hidden_columns(bits, 4),
            vec![true, false, true, false]
        );
        assert_eq!(
            persistence::deserialize_hidden_columns(bits, 6),
            vec![true, false, true, false, true, false]
        );
    }

    #[gpui::test]
    async fn test_git_graph_state_persists_across_serialization_roundtrip(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(99);
        let commits = generate_random_commit_dag(&mut rng, 20, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak.clone(),
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let commit_count = git_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert!(commit_count > 0, "graph should have loaded commits, got 0");

        let target_sha = commits[5].sha;
        git_graph.update(cx, |graph, _| {
            graph.selected_entry_idx = Some(5);
        });

        let selected_sha = git_graph.read_with(&*cx, |graph, _| {
            graph
                .selected_entry_idx
                .and_then(|idx| graph.graph_data.commits.get(idx))
                .map(|c| c.data.sha.to_string())
        });
        assert_eq!(selected_sha, Some(target_sha.to_string()));

        let item_id = workspace::ItemId::from(999_u64);
        let workspace_db = cx.read(|cx| workspace::WorkspaceDb::global(cx));
        let workspace_id = workspace_db
            .next_id()
            .await
            .expect("should create workspace id");
        let db = cx.read(|cx| persistence::GitGraphsDb::global(cx));
        // Hide the "Date" column (index 2 in the non-path-history layout).
        let hidden_columns =
            persistence::serialize_hidden_columns(&[false, false, true, false, false]);
        db.save_git_graph(
            item_id,
            workspace_id,
            "/project".to_string(),
            Some(persistence::LOG_SOURCE_ALL),
            None,
            Some(persistence::LOG_ORDER_DATE),
            selected_sha.clone(),
            Some("some query".to_string()),
            Some(true),
            Some(hidden_columns),
        )
        .await
        .expect("save should succeed");

        let restored_graph = cx
            .update(|window, cx| {
                <GitGraph as workspace::SerializableItem>::deserialize(
                    project.clone(),
                    workspace_weak,
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await
            .expect("deserialization should succeed");
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| restored_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let restored_commit_count =
            restored_graph.read_with(&*cx, |graph, _| graph.graph_data.commits.len());
        assert_eq!(
            restored_commit_count, commit_count,
            "restored graph should have the same number of commits"
        );

        restored_graph.read_with(&*cx, |graph, _| {
            assert_eq!(
                graph.log_source,
                LogSource::All,
                "log_source should be restored"
            );

            let restored_selected_sha = graph
                .selected_entry_idx
                .and_then(|idx| graph.graph_data.commits.get(idx))
                .map(|c| c.data.sha.to_string());
            assert_eq!(
                restored_selected_sha, selected_sha,
                "selected commit should be restored via pending_select_sha"
            );

            assert_eq!(
                graph.search_state.case_sensitive, true,
                "search case sensitivity should be restored"
            );

            assert_eq!(
                graph.column_visibility.as_slice(),
                &[false, false, true, false, false],
                "hidden columns should be restored"
            );
        });

        restored_graph.read_with(&*cx, |graph, cx| {
            let editor_text = graph.search_state.editor.read(cx).text(cx);
            assert_eq!(
                editor_text, "some query",
                "search query text should be restored in editor"
            );
        });
    }

    #[gpui::test]
    async fn test_git_graph_search_matches_commit_hash_prefix(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let first_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let target_sha = Oid::from_bytes(&[2; 20]).unwrap();
        let third_sha = Oid::from_bytes(&[3; 20]).unwrap();
        let commits = vec![
            Arc::new(InitialGraphCommitData {
                sha: first_sha,
                parents: smallvec![target_sha],
                ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
            }),
            Arc::new(InitialGraphCommitData {
                sha: target_sha,
                parents: smallvec![third_sha],
                ref_names: vec![],
            }),
            Arc::new(InitialGraphCommitData {
                sha: third_sha,
                parents: smallvec![],
                ref_names: vec![],
            }),
        ];
        fs.set_graph_commits(Path::new("/project/.git"), commits);
        fs.set_commit_data(
            Path::new("/project/.git"),
            [
                (
                    CommitData {
                        sha: first_sha,
                        parents: smallvec![target_sha],
                        author_name: "Author".into(),
                        author_email: "author@example.com".into(),
                        commit_timestamp: 1,
                        subject: "Add feature".into(),
                        message: "Add feature".into(),
                    },
                    false,
                ),
                (
                    CommitData {
                        sha: target_sha,
                        parents: smallvec![third_sha],
                        author_name: "Author".into(),
                        author_email: "author@example.com".into(),
                        commit_timestamp: 2,
                        subject: "Fix branch loading".into(),
                        message: "Fix branch loading".into(),
                    },
                    false,
                ),
                (
                    CommitData {
                        sha: third_sha,
                        parents: smallvec![],
                        author_name: "Author".into(),
                        author_email: "author@example.com".into(),
                        commit_timestamp: 3,
                        subject: "Update docs".into(),
                        message: "Update docs".into(),
                    },
                    false,
                ),
            ],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| {
            graph.search_for_test("0202020".into(), cx);
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.search_matches_for_test(), vec![target_sha]);
            let selected_sha = graph
                .selected_entry_idx
                .and_then(|idx| graph.graph_data.commits.get(idx))
                .map(|commit| commit.data.sha);
            assert_eq!(selected_sha, Some(target_sha));
        });

        git_graph.update(cx, |graph, cx| {
            graph.search_for_test("docs".into(), cx);
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.search_matches_for_test(), vec![third_sha]);
        });
    }

    #[gpui::test]
    async fn test_graph_data_reloaded_after_stash_change(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let initial_head = Oid::from_bytes(&[1; 20]).unwrap();
        let initial_stash = Oid::from_bytes(&[2; 20]).unwrap();
        let updated_head = Oid::from_bytes(&[3; 20]).unwrap();
        let updated_stash = Oid::from_bytes(&[4; 20]).unwrap();

        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: initial_head,
                    parents: smallvec![initial_stash],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: initial_stash,
                    parents: smallvec![],
                    ref_names: vec!["refs/stash".into()],
                }),
            ],
        );
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.stash_entries = git::stash::GitStash {
                entries: vec![git::stash::StashEntry {
                    index: 0,
                    oid: initial_stash,
                    message: "initial stash".to_string(),
                    branch: Some("main".to_string()),
                    timestamp: 1,
                }]
                .into(),
            };
        })
        .unwrap();

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let initial_shas = git_graph.read_with(&*cx, |graph, _| {
            graph
                .graph_data
                .commits
                .iter()
                .map(|commit| commit.data.sha)
                .collect::<Vec<_>>()
        });
        assert_eq!(initial_shas, vec![initial_head, initial_stash]);

        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: updated_head,
                    parents: smallvec![updated_stash],
                    ref_names: vec!["HEAD".into(), "refs/heads/main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: updated_stash,
                    parents: smallvec![],
                    ref_names: vec!["refs/stash".into()],
                }),
            ],
        );
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.stash_entries = git::stash::GitStash {
                entries: vec![git::stash::StashEntry {
                    index: 0,
                    oid: updated_stash,
                    message: "updated stash".to_string(),
                    branch: Some("main".to_string()),
                    timestamp: 1,
                }]
                .into(),
            };
        })
        .unwrap();

        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let reloaded_shas = git_graph.read_with(&*cx, |graph, _| {
            graph
                .graph_data
                .commits
                .iter()
                .map(|commit| commit.data.sha)
                .collect::<Vec<_>>()
        });
        assert_eq!(reloaded_shas, vec![updated_head, updated_stash]);
    }

    #[gpui::test]
    async fn test_graph_data_reloaded_after_tag_created(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let head = Oid::from_bytes(&[1; 20]).unwrap();
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: head,
                parents: smallvec![],
                ref_names: vec!["HEAD -> main".into()],
            })],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let ref_names_of_head = |cx: &mut gpui::VisualTestContext| {
            git_graph.read_with(&*cx, |graph, _| {
                graph
                    .graph_data
                    .commits
                    .iter()
                    .map(|commit| commit.data.ref_names.clone())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            ref_names_of_head(cx),
            vec![vec![SharedString::from("HEAD -> main")]]
        );

        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: head,
                parents: smallvec![],
                ref_names: vec!["HEAD -> main".into(), "tag: v1.0.0".into()],
            })],
        );
        repository
            .update(cx, |repository, _| {
                repository.create_tag("v1.0.0".to_string(), head.to_string())
            })
            .await
            .unwrap()
            .unwrap();
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        assert_eq!(
            ref_names_of_head(cx),
            vec![vec![
                SharedString::from("HEAD -> main"),
                SharedString::from("tag: v1.0.0"),
            ]]
        );
    }

    #[gpui::test]
    async fn test_git_graph_row_at_position_rounding(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits.clone());

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            assert!(
                graph.graph_data.commits.len() >= 10,
                "graph should load dummy commits"
            );

            let row_height = GitGraph::row_height(window, cx);
            let origin_y = px(100.0);
            graph.graph_canvas_bounds.set(Some(Bounds {
                origin: point(px(0.0), origin_y),
                size: gpui::size(px(100.0), row_height * 50.0),
            }));

            // Scroll down by half a row so the row under a position near the
            // top of the canvas is row 1 rather than row 0.
            let scroll_offset = row_height * 0.75;
            graph.table_interaction_state.update(cx, |state, _| {
                state.set_scroll_offset(point(px(0.0), -scroll_offset))
            });
            let pos_y = origin_y + row_height * 0.5;
            let absolute_calc_row = graph.row_at_position(pos_y, window, cx);

            assert_eq!(
                absolute_calc_row,
                Some(1),
                "Row calculation should yield absolute row exactly"
            );
        });
    }

    #[gpui::test]
    async fn test_focus_handle_focuses_search_editor(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            window.focus(&graph.focus_handle(cx), cx);
            assert!(
                graph
                    .search_state
                    .editor
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window),
                "focusing the git graph item should focus the search editor"
            );
        });
    }

    #[gpui::test]
    async fn test_row_height_matches_uniform_list_item_height(cx: &mut TestAppContext) {
        init_test(cx);

        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    *settings.theme = ThemeSettingsContent {
                        ui_font_size: Some(12.7.into()),
                        ..Default::default()
                    }
                });
            })
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(99);
        let commits = generate_random_commit_dag(&mut rng, 20, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            let commit_count = graph.graph_data.commits.len();
            assert!(
                commit_count > 0,
                "need at least one commit to measure item height"
            );

            let table_state = graph.table_interaction_state.read(cx);
            let item_size = table_state.scroll_handle.0.borrow().last_item_size.expect(
                "uniform_list should have populated last_item_size after draw(); \
                     the table has not been laid out",
            );

            let measured_item_height = item_size.contents.height / commit_count as f32;
            let computed_row_height = GitGraph::row_height(window, cx);

            assert_eq!(
                computed_row_height, measured_item_height,
                "GitGraph::row_height ({}) must exactly match the height that \
                 uniform_list measured for each table row ({}). \
                 A mismatch means the canvas and table rows will drift when scrolling.",
                computed_row_height, measured_item_height,
            );
        });
    }

    #[gpui::test]
    async fn test_copy_selected_commit_tag_with_one_tag_copies_to_clipboard(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec![
                SharedString::from("HEAD -> main"),
                SharedString::from("origin/main"),
                SharedString::from("tag: v1.0.0"),
            ],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            assert_eq!(graph.graph_data.commits.len(), 1);
            graph.selected_entry_idx = Some(0);
            graph.copy_selected_commit_tag(&CopyCommitTag, window, cx);
        });

        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("v1.0.0".to_string())
        );
    }

    #[gpui::test]
    async fn test_copy_selected_commit_tag_with_multiple_tags_opens_picker_and_copies_selected_tag(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec![
                SharedString::from("HEAD -> main"),
                SharedString::from("origin/main"),
                SharedString::from("tag: v1.0.0"),
                SharedString::from("tag: v1.1.0"),
            ],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            assert_eq!(graph.graph_data.commits.len(), 1);
            graph.selected_entry_idx = Some(0);
            graph.copy_selected_commit_tag(&CopyCommitTag, window, cx);
        });

        // Ensure that nothing has been copied at this point
        assert_eq!(cx.read_from_clipboard().and_then(|item| item.text()), None);

        let picker = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommitTagPicker>(cx)
                .expect("commit tag picker is not open")
                .read(cx)
                .picker
                .clone()
        });

        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.selected_index, 0);
            assert_eq!(
                picker.delegate.tag_names,
                [SharedString::from("v1.0.0"), SharedString::from("v1.1.0")]
            );
        });

        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();

        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("v1.0.0".to_string())
        );
    }

    #[gpui::test]
    async fn test_open_at_commit_reuses_loaded_graph(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let first_sha = Oid::from_bytes(&[1; 20]).expect("valid commit SHA");
        let second_sha = Oid::from_bytes(&[2; 20]).expect("valid commit SHA");
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: second_sha,
                    parents: smallvec![first_sha],
                    ref_names: vec!["HEAD -> main".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: first_sha,
                    parents: smallvec![],
                    ref_names: Vec::new(),
                }),
            ],
        );
        fs.set_commit_data(
            Path::new("/project/.git"),
            [first_sha, second_sha].map(|sha| {
                (
                    CommitData {
                        sha,
                        parents: smallvec![],
                        author_name: "Author".into(),
                        author_email: "author@example.com".into(),
                        commit_timestamp: 1_700_000_000,
                        subject: "Commit subject".into(),
                        message: "Commit message".into(),
                    },
                    false,
                )
            }),
        );

        let project = Project::test(fs, [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace.downgrade(),
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| {
            graph.select_commit_by_sha(first_sha, cx);
        });
        cx.run_until_parked();
        git_graph.update(cx, |graph, cx| {
            graph.select_commit_by_sha(second_sha, cx);
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            open_or_reuse_graph(
                workspace,
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                LogSource::All,
                Some(first_sha.to_string()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(1));
        });
    }

    #[gpui::test]
    async fn test_opening_another_repository_reuses_graph_tab(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/a"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;
        fs.insert_tree(
            Path::new("/b"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let commit = |byte: u8, parent: Option<u8>, ref_names: Vec<&str>| {
            Arc::new(InitialGraphCommitData {
                sha: Oid::from_bytes(&[byte; 20]).expect("valid commit SHA"),
                parents: parent
                    .map(|parent| Oid::from_bytes(&[parent; 20]).expect("valid commit SHA"))
                    .into_iter()
                    .collect(),
                ref_names: ref_names.into_iter().map(Into::into).collect(),
            })
        };
        fs.set_graph_commits(
            Path::new("/a/.git"),
            vec![
                commit(2, Some(1), vec!["HEAD -> main"]),
                commit(1, None, vec![]),
            ],
        );
        fs.set_graph_commits(
            Path::new("/b/.git"),
            vec![
                commit(13, Some(12), vec!["HEAD -> main"]),
                commit(12, Some(11), vec![]),
                commit(11, None, vec![]),
            ],
        );

        let project = Project::test(fs, [Path::new("/a"), Path::new("/b")], cx).await;
        cx.run_until_parked();

        let repo_id = |path: &str, cx: &mut TestAppContext| {
            project.read_with(cx, |project, cx| {
                project
                    .git_store()
                    .read(cx)
                    .repositories()
                    .values()
                    .find(|repository| {
                        repository.read(cx).work_directory_abs_path.as_ref() == Path::new(path)
                    })
                    .map(|repository| repository.read(cx).id)
                    .expect("repository should exist")
            })
        };
        let repo_a = repo_id("/a", cx);
        let repo_b = repo_id("/b", cx);

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let git_store = project.read_with(&*cx, |project, _| project.git_store().clone());
        let open = |repo_id, sha: Option<String>, cx: &mut gpui::VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                open_or_reuse_graph(
                    workspace,
                    repo_id,
                    git_store.clone(),
                    LogSource::All,
                    sha,
                    window,
                    cx,
                );
            });
            cx.run_until_parked();
        };

        // Load repository B's graph, so its commits are cached, then switch the
        // tab to A and back to B, selecting a commit of B. Selecting it used to
        // index the graph before its rows were reloaded and panic.
        open(repo_b, None, cx);
        open(repo_a, None, cx);
        let oldest_b_commit = Oid::from_bytes(&[11; 20]).expect("valid commit SHA");
        open(repo_b, Some(oldest_b_commit.to_string()), cx);

        workspace.read_with(&*cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 1, "a single repository graph tab");
            let graph = graphs[0].read(cx);
            assert_eq!(graph.repo_id, repo_b);
            assert_eq!(graph.selected_entry_idx, Some(2));
            assert_eq!(graph.graph_data.commits.len(), 3);
        });

        // Switching repositories clears the previous repository's selection.
        open(repo_a, None, cx);
        workspace.read_with(&*cx, |workspace, cx| {
            let graphs = workspace.items_of_type::<GitGraph>(cx).collect::<Vec<_>>();
            assert_eq!(graphs.len(), 1);
            let graph = graphs[0].read(cx);
            assert_eq!(graph.repo_id, repo_a);
            assert_eq!(graph.selected_entry_idx, None);
        });
    }

    #[gpui::test]
    async fn test_graph_columns_keep_pixel_widths_when_the_container_resizes(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;
        let commit = |byte: u8, parent: Option<u8>| {
            Arc::new(InitialGraphCommitData {
                sha: Oid::from_bytes(&[byte; 20]).expect("valid commit SHA"),
                parents: parent
                    .map(|parent| Oid::from_bytes(&[parent; 20]).expect("valid commit SHA"))
                    .into_iter()
                    .collect(),
                ref_names: Vec::new(),
            })
        };
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![commit(2, Some(1)), commit(1, None)],
        );

        let project = Project::test(fs, [Path::new("/project")], cx).await;
        cx.run_until_parked();
        let repo_id = project.read_with(cx, |project, cx| {
            project
                .git_store()
                .read(cx)
                .repositories()
                .values()
                .next()
                .map(|repository| repository.read(cx).id)
                .expect("repository should exist")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let git_store = project.read_with(&*cx, |project, _| project.git_store().clone());
        workspace.update_in(cx, |workspace, window, cx| {
            open_or_reuse_graph(
                workspace,
                repo_id,
                git_store,
                LogSource::All,
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        let graph = workspace.read_with(&*cx, |workspace, cx| {
            workspace
                .items_of_type::<GitGraph>(cx)
                .next()
                .expect("graph tab should be open")
        });

        graph.update_in(cx, |graph, window, cx| {
            fn fit(
                graph: &mut GitGraph,
                container_width: Pixels,
                window: &mut Window,
                cx: &mut gpui::Context<GitGraph>,
            ) -> Vec<DefiniteLength> {
                graph.column_widths.update(cx, |state, _| {
                    state.set_cached_container_width(container_width);
                });
                graph.fit_graph_column_to_rows(0..2, window, cx);
                graph
                    .column_widths
                    .read(cx)
                    .preview_widths()
                    .as_slice()
                    .to_vec()
            }
            let is_pixels = |width: &DefiniteLength| {
                matches!(width, DefiniteLength::Absolute(AbsoluteLength::Pixels(_)))
            };

            // The graph fits the minimum of three lanes; Date, Author and
            // Commit are fixed; Description takes the rest.
            let wide = fit(graph, px(1200.), window, cx);
            let graph_width = LANE_WIDTH * 3. + LEFT_PADDING * 2.;
            assert!(wide[0] == DefiniteLength::Absolute(AbsoluteLength::Pixels(graph_width)));
            assert!(wide[2..].iter().all(is_pixels));
            assert_eq!(graph.column_widths.read(cx).fill_column(), Some(1));
            assert!(!graph.graph_width_manual);

            // Closing a sidebar doesn't change any column but Description.
            let narrower = fit(graph, px(900.), window, cx);
            assert!(narrower == wide);
            assert_eq!(graph.graph_viewport_width(window, cx), graph_width);
            let fractions = graph.preview_column_fractions(window, cx);
            let total: f32 = fractions.iter().sum();
            assert!((total - 1.0).abs() < 1e-4, "fractions sum to {total}");

            // In a very narrow pane, the graph is capped at 40% of the width.
            let narrow = fit(graph, px(100.), window, cx);
            assert!(narrow[0] == DefiniteLength::Absolute(AbsoluteLength::Pixels(px(40.))));
            assert!(narrow[2..] == wide[2..]);

            // A column the user resized keeps its width.
            let mut resized = narrow;
            resized[0] = DefiniteLength::Absolute(AbsoluteLength::Pixels(px(150.)));
            graph.column_widths.update(cx, |state, _| {
                state.set_widths(TableRow::from_vec(resized.clone(), 5));
            });
            let after_resize = fit(graph, px(1200.), window, cx);
            assert!(after_resize == resized);
            assert!(graph.graph_width_manual);
        });
    }

    #[gpui::test]
    async fn test_go_back_from_commit_view_returns_to_git_graph(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let first_sha = Oid::from_bytes(&[1; 20]).expect("valid commit SHA");
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: first_sha,
                parents: smallvec![],
                ref_names: vec!["HEAD -> main".into()],
            })],
        );
        fs.set_commit_data(
            Path::new("/project/.git"),
            [(
                CommitData {
                    sha: first_sha,
                    parents: smallvec![],
                    author_name: "Author".into(),
                    author_email: "author@example.com".into(),
                    commit_timestamp: 1_700_000_000,
                    subject: "Commit subject".into(),
                    message: "Commit message".into(),
                },
                false,
            )],
        );

        let project = Project::test(fs, [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());

        // Open a file first, so there's something in nav history before the Git Graph tab.
        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(Path::new("/project/file.txt"), cx)
            })
            .await
            .expect("file should open");
        let buffer_editor = cx.new_window_entity(|window, cx| {
            Editor::for_buffer(buffer, Some(project.clone()), window, cx)
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(
                Box::new(buffer_editor.clone()),
                None,
                true,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace.downgrade(),
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| {
            graph.select_commit_by_sha(first_sha, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            graph.open_selected_commit_view(window, cx);
        });
        cx.run_until_parked();

        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());

        pane.read_with(cx, |pane, _cx| {
            assert!(
                pane.active_item()
                    .and_then(|item| item.downcast::<CommitView>())
                    .is_some(),
                "expected the commit diff view to be active after opening a commit"
            );
        });

        pane.update_in(cx, |pane, window, cx| {
            pane.navigate_backward(&Default::default(), window, cx);
        });
        cx.run_until_parked();

        pane.read_with(cx, |pane, _cx| {
            let active_git_graph = pane
                .active_item()
                .and_then(|item| item.downcast::<GitGraph>());
            assert_eq!(
                active_git_graph,
                Some(git_graph.clone()),
                "Go Back from the commit diff view should return to the Git Graph view"
            );
        });
    }

    #[gpui::test]
    async fn test_git_graph_navigation(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            serde_json::json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let mut rng = StdRng::seed_from_u64(42);
        let commits = generate_random_commit_dag(&mut rng, 10, false);
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });

        let workspace = multi_workspace.read_with(&*cx, |multi, _| multi.workspace().clone());
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            graph.focus_handle(cx).focus(window, cx);
        });
        cx.run_until_parked();

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| multi_workspace.clone().into_any_element(),
        );
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            graph.focus_handle(cx).focus(window, cx);
        });
        cx.run_until_parked();

        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.graph_data.commits.len(), 10);
        });
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, None);
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        let scroll_step = git_graph.update_in(cx, |graph, window, cx| {
            (graph.visible_row_count(window, cx) / 2).max(1)
        });

        cx.dispatch_action(ScrollDown);
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(scroll_step));
        });

        cx.dispatch_action(ScrollUp);
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_next(&menu::SelectNext, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(1));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_prev(&menu::SelectPrevious, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_last(&menu::SelectLast, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(9));
        });

        cx.dispatch_action(ScrollDown);
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(9));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_next(&menu::SelectNext, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(9));
        });

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_prev(&menu::SelectPrevious, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(8));
        });

        git_graph.update(cx, |graph, cx| {
            graph.selected_entry_idx = None;
            cx.notify();
        });
        cx.run_until_parked();
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_prev(&menu::SelectPrevious, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });

        git_graph.update(cx, |graph, cx| {
            graph.selected_entry_idx = None;
            cx.notify();
        });
        cx.run_until_parked();
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_next(&menu::SelectNext, window, cx);
        });
        cx.run_until_parked();
        git_graph.read_with(&*cx, |graph, _| {
            assert_eq!(graph.selected_entry_idx, Some(0));
        });
    }

    #[gpui::test]
    async fn test_global_git_command_task_runs_from_context_menu(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let commit_sha = Oid::try_from("abcdef1234567890abcdef1234567890abcdef12")
            .expect("commit SHA should be valid");
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: commit_sha,
                parents: SmallVec::new(),
                ref_names: Vec::new(),
            })],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("project should have an active repository")
        });
        let task_inventory = project.read_with(cx, |project, cx| {
            project
                .task_store()
                .read(cx)
                .task_inventory()
                .cloned()
                .expect("project should have a task inventory")
        });

        task_inventory.update(cx, |inventory, _| {
            inventory
                .update_file_based_tasks(
                    TaskSettingsLocation::Global(Path::new("/tasks.json")),
                    Some(
                        &serde_json::to_string(&json!([
                            // Tagged global task that should be scheduled from the Git graph context menu.
                            {
                                "label": "Git Show $ZED_GIT_SHA_SHORT",
                                "command": "git",
                                "args": ["show", "$ZED_GIT_SHA"],
                                "cwd": "$ZED_GIT_REPOSITORY_PATH",
                                "env": {
                                    "REPOSITORY": "$ZED_GIT_REPOSITORY_NAME",
                                },
                                "tags": [GIT_COMMAND_TASK_TAG],
                            },
                            // Untagged task that should not appear in the Git graph context menu.
                            {
                                "label": "Git Status",
                                "command": "git",
                                "args": ["status"],
                            },
                            // Tagged task that still should not appear because Git graph task contexts
                            // do not provide editor-specific variables.
                            {
                                "label": "Print File $ZED_FILE",
                                "command": "echo",
                                "args": ["$ZED_FILE"],
                                "tags": [GIT_COMMAND_TASK_TAG],
                            },
                        ]))
                        .expect("tasks JSON should serialize"),
                    ),
                )
                .expect("tasks should parse");
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        });
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |git_graph, window, cx| {
            assert_eq!(git_graph.graph_data.commits.len(), 1);
            git_graph.deploy_entry_context_menu(point(px(20.), px(20.)), 0, None, window, cx);
        });
        cx.run_until_parked();

        let context_menu = git_graph.read_with(&*cx, |git_graph, _| {
            git_graph
                .context_menu
                .as_ref()
                .expect("context menu should be open")
                .menu
                .clone()
        });
        context_menu.update_in(cx, |context_menu, window, cx| {
            context_menu
                .select_last(window, cx)
                .expect("custom Git task should be selectable");
            context_menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();

        let (task_source_kind, resolved_task) = task_inventory.read_with(&*cx, |inventory, _| {
            inventory
                .last_scheduled_task(None)
                .expect("custom Git task should be scheduled")
        });

        assert!(
            matches!(task_source_kind, TaskSourceKind::AbsPath { .. }),
            "scheduled task should come from global tasks"
        );
        assert_eq!(resolved_task.resolved_label, "Git Show abcdef1");
        assert_eq!(resolved_task.resolved.command, Some("git".to_string()));
        assert_eq!(
            resolved_task.resolved.args,
            vec![
                "show".to_string(),
                "abcdef1234567890abcdef1234567890abcdef12".to_string(),
            ]
        );
        assert_eq!(
            resolved_task.resolved.cwd,
            Some(Path::new("/project").to_path_buf())
        );
        assert_eq!(
            resolved_task.resolved.env.get("REPOSITORY"),
            Some(&"project".to_string())
        );
    }

    #[gpui::test]
    async fn test_global_git_command_task_runs_from_ref_context_menu(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let commit_sha = Oid::try_from("abcdef1234567890abcdef1234567890abcdef12")
            .expect("commit SHA should be valid");
        fs.set_graph_commits(
            Path::new("/project/.git"),
            vec![Arc::new(InitialGraphCommitData {
                sha: commit_sha,
                parents: SmallVec::new(),
                ref_names: vec!["HEAD -> feature-x".into()],
            })],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("project should have an active repository")
        });
        let task_inventory = project.read_with(cx, |project, cx| {
            project
                .task_store()
                .read(cx)
                .task_inventory()
                .cloned()
                .expect("project should have a task inventory")
        });

        task_inventory.update(cx, |inventory, _| {
            inventory
                .update_file_based_tasks(
                    TaskSettingsLocation::Global(Path::new("/tasks.json")),
                    Some(
                        &serde_json::to_string(&json!([
                            {
                                "label": "Check out $ZED_GIT_REF",
                                "command": "git",
                                "args": ["checkout", "$ZED_GIT_REF"],
                                "cwd": "$ZED_GIT_REPOSITORY_PATH",
                                "tags": [GIT_COMMAND_TASK_TAG],
                            },
                        ]))
                        .expect("tasks JSON should serialize"),
                    ),
                )
                .expect("tasks should parse");
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        });
        let workspace_weak = workspace.downgrade();

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |git_graph, window, cx| {
            assert_eq!(git_graph.graph_data.commits.len(), 1);
            git_graph.deploy_entry_context_menu(
                point(px(20.), px(20.)),
                0,
                Some("feature-x".into()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let context_menu = git_graph.read_with(&*cx, |git_graph, _| {
            git_graph
                .context_menu
                .as_ref()
                .expect("context menu should be open")
                .menu
                .clone()
        });
        context_menu.update_in(cx, |context_menu, window, cx| {
            context_menu
                .select_last(window, cx)
                .expect("custom Git task should be selectable");
            context_menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();

        let (_task_source_kind, resolved_task) = task_inventory.read_with(&*cx, |inventory, _| {
            inventory
                .last_scheduled_task(None)
                .expect("custom Git task should be scheduled")
        });

        assert_eq!(resolved_task.resolved_label, "Check out feature-x");
        assert_eq!(
            resolved_task.resolved.args,
            vec!["checkout".to_string(), "feature-x".to_string()]
        );
    }

    #[gpui::test]
    async fn test_context_menu_actions_run_git_commands(cx: &mut TestAppContext) {
        use crate::git_action_dialog::GitActionDialog;
        use gpui::VisualTestContext;

        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;

        let dot_git = Path::new("/project/.git");
        let head_sha = Oid::try_from("2222222222222222222222222222222222222222").unwrap();
        let base_sha = Oid::try_from("1111111111111111111111111111111111111111").unwrap();
        fs.set_graph_commits(
            dot_git,
            vec![
                Arc::new(InitialGraphCommitData {
                    sha: head_sha,
                    parents: smallvec![base_sha],
                    ref_names: vec!["HEAD -> main".into(), "tag: v1".into()],
                }),
                Arc::new(InitialGraphCommitData {
                    sha: base_sha,
                    parents: SmallVec::new(),
                    ref_names: vec!["feature".into(), "origin/feature".into()],
                }),
            ],
        );
        fs.insert_branches(dot_git, &["main", "feature", "origin/feature"]);
        fs.with_git_state(dot_git, true, |state| {
            state.refs.insert("HEAD".into(), head_sha.to_string());
        })
        .unwrap();
        fs.set_commit_data(
            dot_git,
            [(
                git::repository::CommitData {
                    sha: head_sha,
                    parents: smallvec![base_sha],
                    author_name: "Author".into(),
                    author_email: "author@example.com".into(),
                    commit_timestamp: 0,
                    subject: "Old subject".into(),
                    message: "Old subject\n".into(),
                },
                false,
            )],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("project should have an active repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(&*cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        });
        let workspace_weak = workspace.downgrade();
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(git_graph.clone()), None, true, window, cx);
        });
        cx.run_until_parked();

        let open_menu = |row: usize, decoration: Option<&str>, cx: &mut VisualTestContext| {
            git_graph.update_in(cx, |git_graph, window, cx| match decoration {
                Some(decoration) => git_graph.deploy_ref_context_menu(
                    point(px(20.), px(20.)),
                    row,
                    decoration.to_string().into(),
                    window,
                    cx,
                ),
                None => git_graph.deploy_entry_context_menu(
                    point(px(20.), px(20.)),
                    row,
                    None,
                    window,
                    cx,
                ),
            });
            cx.run_until_parked();
            git_graph.read_with(&*cx, |git_graph, _| {
                git_graph
                    .context_menu
                    .as_ref()
                    .expect("context menu should be open")
                    .menu
                    .clone()
            })
        };
        let labels = |menu: &Entity<ContextMenu>, cx: &mut VisualTestContext| {
            menu.read_with(&*cx, |menu, _| {
                menu.entry_labels()
                    .into_iter()
                    .map(|label| label.to_string())
                    .collect::<Vec<_>>()
            })
        };
        let click = |menu: &Entity<ContextMenu>, label: &str, cx: &mut VisualTestContext| {
            menu.update_in(cx, |menu, window, cx| {
                assert!(
                    menu.confirm_entry_with_label(label, window, cx),
                    "menu should have an entry {label:?}"
                );
            });
            cx.run_until_parked();
            workspace
                .read_with(&*cx, |workspace, cx| {
                    workspace.active_modal::<GitActionDialog>(cx)
                })
                .expect("a dialog should be open")
        };
        let confirm = |dialog: Entity<GitActionDialog>, cx: &mut VisualTestContext| {
            dialog.update_in(cx, |dialog, window, cx| dialog.confirm_for_test(window, cx));
            cx.run_until_parked();
            fs.with_git_state(dot_git, false, |state| {
                std::mem::take(&mut state.git_commands)
            })
            .unwrap()
        };
        let args = |args: &[&str]| args.iter().map(ToString::to_string).collect::<Vec<_>>();

        let menu = open_menu(0, Some("tag: v1"), cx);
        assert_eq!(
            labels(&menu, cx),
            ["View Details", "Delete Tag…", "Push Tag…", "Copy Tag Name"]
        );
        let dialog = click(&menu, "Delete Tag…", cx);
        dialog.update(cx, |dialog, _| {
            assert_eq!(dialog.title().as_ref(), "Delete Tag v1");
            dialog.set_checked("remote", true);
        });
        assert_eq!(
            confirm(dialog, cx),
            [
                args(&["push", "origin", "--delete", "refs/tags/v1"]),
                args(&["tag", "-d", "v1"]),
            ]
        );

        let menu = open_menu(1, Some("feature"), cx);
        assert_eq!(
            labels(&menu, cx),
            [
                "Checkout Branch",
                "Rename Branch…",
                "Delete Branch…",
                "Merge into main…",
                "Rebase main on Branch…",
                "Push Branch…",
                "Copy Branch Name",
                "Compare with main",
            ]
        );
        let dialog = click(&menu, "Delete Branch…", cx);
        dialog.update(cx, |dialog, _| dialog.set_checked("delete_on_remote", true));
        assert_eq!(
            confirm(dialog, cx),
            [
                args(&["branch", "-d", "feature"]),
                args(&["push", "origin", "--delete", "feature"]),
            ]
        );

        let menu = open_menu(0, Some("HEAD -> main"), cx);
        assert_eq!(
            labels(&menu, cx),
            ["Rename Branch…", "Push Branch…", "Copy Branch Name"]
        );

        let menu = open_menu(1, Some("origin/feature"), cx);
        assert_eq!(
            labels(&menu, cx),
            [
                "Checkout Branch…",
                "Delete Remote Branch…",
                "Fetch into Local Branch…",
                "Merge into main…",
                "Pull into main…",
                "Copy Branch Name",
                "Compare with main",
            ]
        );
        let dialog = click(&menu, "Merge into main…", cx);
        dialog.update(cx, |dialog, _| dialog.set_checked("squash", true));
        assert_eq!(
            confirm(dialog, cx),
            [
                args(&["merge", "origin/feature", "--squash", "--no-edit"]),
                args(&["diff", "--cached", "--quiet"]),
            ]
        );

        let menu = open_menu(0, None, cx);
        assert_eq!(
            labels(&menu, cx),
            [
                "Add Tag…",
                "Create Branch…",
                "Checkout…",
                "Cherry Pick…",
                "Revert…",
                "Edit Commit Message…",
                "Drop…",
                "Merge into main…",
                "Rebase main on This Commit…",
                "Reset main to This Commit…",
                "View Diff",
                "Copy Commit Hash",
                "Copy Commit Subject",
            ]
        );
        let dialog = click(&menu, "Cherry Pick…", cx);
        dialog.update(cx, |dialog, _| dialog.set_checked("record_origin", true));
        assert_eq!(
            confirm(dialog, cx),
            [args(&["cherry-pick", "-x", &head_sha.to_string()])]
        );

        let menu = open_menu(0, None, cx);
        let dialog = click(&menu, "Edit Commit Message…", cx);
        dialog.update_in(cx, |dialog, window, cx| {
            assert_eq!(dialog.title().as_ref(), "Edit Commit Message 2222222");
            dialog.set_text("message", "New subject\n\nNew body", window, cx);
        });
        assert_eq!(
            confirm(dialog, cx),
            [
                args(&[
                    "commit",
                    "--no-verify",
                    "--allow-empty",
                    "--only",
                    "-m",
                    &format!("amend! {head_sha}\n\nNew subject\n\nNew body"),
                ]),
                args(&[
                    "-c",
                    "sequence.editor=:",
                    "rebase",
                    "--interactive",
                    "--autosquash",
                    "--autostash",
                    &format!("{head_sha}^"),
                ]),
            ]
        );

        let menu = open_menu(1, None, cx);
        assert!(!labels(&menu, cx).contains(&"Drop…".to_string()));
        let dialog = click(&menu, "Reset main to This Commit…", cx);
        dialog.update(cx, |dialog, _| dialog.set_selected("mode", 2));
        assert_eq!(
            confirm(dialog, cx),
            [args(&["reset", "--hard", &base_sha.to_string()])]
        );

        let menu = open_menu(1, None, cx);
        let dialog = click(&menu, "Add Tag…", cx);
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.set_text("name", "v2", window, cx);
            dialog.set_text("message", "Release", window, cx);
            dialog.set_checked("push", true);
        });
        assert_eq!(
            confirm(dialog, cx),
            [
                args(&["tag", "-a", "v2", "-m", "Release", &base_sha.to_string()]),
                args(&["push", "origin", "refs/tags/v2"]),
            ]
        );

        fs.set_commit_diff(
            dot_git,
            &format!("{base_sha}...{head_sha}"),
            vec![(
                RepoPath::new("file.txt").unwrap(),
                Some("old".into()),
                Some("new".into()),
            )],
        );
        fs.set_commit_diff(dot_git, &format!("{base_sha}..{head_sha}"), Vec::new());
        let menu = open_menu(1, None, cx);
        assert!(labels(&menu, cx).contains(&"Compare with main".to_string()));
        menu.update_in(cx, |menu, window, cx| {
            assert!(menu.confirm_entry_with_label("Compare with main", window, cx));
        });
        cx.run_until_parked();
        let compared_files = |cx: &mut VisualTestContext| {
            git_graph.read_with(&*cx, |git_graph, _| {
                let comparison = git_graph.comparison.as_ref().expect("should be comparing");
                (
                    comparison.base.clone(),
                    comparison.target.clone(),
                    comparison.merge_base,
                    comparison
                        .diff
                        .as_ref()
                        .expect("comparison should be loaded")
                        .files
                        .iter()
                        .map(|file| file.path.as_unix_str().to_string())
                        .collect::<Vec<_>>(),
                )
            })
        };
        assert_eq!(
            compared_files(cx),
            (
                (base_sha, SharedString::from(base_sha.display_short())),
                (head_sha, SharedString::from("main")),
                true,
                vec!["file.txt".to_string()],
            )
        );

        git_graph.update(cx, |git_graph, cx| {
            let comparison = git_graph.comparison.as_ref().unwrap();
            let (base, target) = (comparison.base.clone(), comparison.target.clone());
            git_graph.compare(base, target, false, cx);
        });
        cx.run_until_parked();
        assert_eq!(compared_files(cx).3, Vec::<String>::new());

        git_graph.update(cx, |git_graph, cx| {
            git_graph.select_entry(0, ScrollStrategy::Nearest, cx)
        });
        git_graph.read_with(&*cx, |git_graph, _| assert!(git_graph.comparison.is_none()));
    }

    #[test]
    fn test_ref_name_from_decoration() {
        assert_eq!(
            GitGraph::ref_name_from_decoration("HEAD -> main"),
            Some("main".into())
        );
        assert_eq!(
            GitGraph::ref_name_from_decoration("main"),
            Some("main".into())
        );
        assert_eq!(
            GitGraph::ref_name_from_decoration("origin/main"),
            Some("origin/main".into())
        );
        assert_eq!(
            GitGraph::ref_name_from_decoration("tag: v1.0"),
            Some("v1.0".into())
        );
        assert_eq!(GitGraph::ref_name_from_decoration("HEAD"), None);
    }

    fn pill_summary(pills: &[RefPill]) -> Vec<(RefPillKind, &str, Option<&str>, bool)> {
        pills
            .iter()
            .map(|pill| {
                (
                    pill.kind,
                    pill.name.as_ref(),
                    pill.remote.as_ref().map(|remote| remote.as_ref()),
                    pill.is_head,
                )
            })
            .collect()
    }

    #[test]
    fn test_ref_pills_order_and_remote_merge() {
        let ref_names: Vec<SharedString> = vec![
            "tag: v2.0".into(),
            "origin/staging".into(),
            "staging".into(),
            "origin/feature/only-remote".into(),
            "HEAD -> fix/bug".into(),
            "origin/fix/bug".into(),
            "origin/HEAD".into(),
            "tag: v2.0-beta".into(),
        ];
        let pills = GitGraph::ref_pills(&ref_names, &Some("fix/bug".into()), &["origin".into()]);

        assert_eq!(
            pill_summary(&pills),
            vec![
                (RefPillKind::Branch, "fix/bug", Some("origin"), true),
                (RefPillKind::Branch, "staging", Some("origin"), false),
                (
                    RefPillKind::Branch,
                    "feature/only-remote",
                    Some("origin"),
                    false
                ),
                (RefPillKind::Tag, "v2.0", None, true),
                (RefPillKind::Tag, "v2.0-beta", None, true),
            ]
        );
        assert_eq!(pills[0].decoration.as_ref(), "HEAD -> fix/bug");
        assert_eq!(pills[2].decoration.as_ref(), "origin/feature/only-remote");
    }

    #[test]
    fn test_ref_pills_detached_head() {
        let ref_names: Vec<SharedString> = vec!["tag: v1.0".into(), "HEAD".into(), "main".into()];
        let pills = GitGraph::ref_pills(&ref_names, &None, &["origin".into()]);
        let summary: Vec<_> = pills
            .iter()
            .map(|pill| (pill.kind, pill.name.as_ref(), pill.is_head))
            .collect();
        assert_eq!(
            summary,
            vec![
                (RefPillKind::DetachedHead, "HEAD", true),
                (RefPillKind::Branch, "main", false),
                (RefPillKind::Tag, "v1.0", true),
            ]
        );
    }

    #[test]
    fn test_ref_pills_slash_branch_without_known_remote_is_local() {
        let ref_names: Vec<SharedString> = vec!["feat/hris-integration".into()];
        let pills = GitGraph::ref_pills(&ref_names, &None, &["origin".into()]);

        assert_eq!(
            pill_summary(&pills),
            vec![(RefPillKind::Branch, "feat/hris-integration", None, false)]
        );
    }

    #[test]
    fn test_ref_pill_display_name_is_capped() {
        let pill = |name: &str| RefPill {
            kind: RefPillKind::Branch,
            name: SharedString::from(name.to_string()),
            remote: None,
            is_head: false,
            decoration: SharedString::from(name.to_string()),
        };

        assert_eq!(pill("develop").display_name().as_ref(), "develop");
        let exact = "a".repeat(REF_LABEL_MAX_CHARS);
        assert_eq!(pill(&exact).display_name().as_ref(), exact.as_str());
        let capped = pill("fix/EM-2150/skip-disabled-hris-connections").display_name();
        assert_eq!(capped.chars().count(), REF_LABEL_MAX_CHARS);
        assert_eq!(capped.as_ref(), "fix/EM-2150/skip-disable…");
    }

    #[test]
    fn test_fit_ref_pills_reserves_room_for_count_chip() {
        let pills = ["one", "two", "three", "four"]
            .into_iter()
            .map(|name| RefPill {
                kind: RefPillKind::Tag,
                name: name.into(),
                remote: None,
                is_head: false,
                decoration: format!("tag: {name}").into(),
            })
            .collect::<Vec<_>>();
        let fit = |available: f32| {
            let (visible, hidden) =
                GitGraph::fit_ref_pills(pills.clone(), px(available), |_| px(50.), px(20.), px(5.));
            (visible.len(), hidden.len())
        };

        // Everything fits: 4 * 50 + 3 * 5 = 215.
        assert_eq!(fit(215.), (4, 0));
        // Two pills plus the "+N" chip: 50 + 5 + 50 + 5 + 20 = 130.
        assert_eq!(fit(130.), (2, 2));
        // The first pill is always shown, even when nothing fits.
        assert_eq!(fit(10.), (1, 3));
    }

    #[gpui::test]
    async fn test_commit_message_rendered_as_markdown(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec!["HEAD -> main".into()],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);
        fs.set_commit_data(
            Path::new("/project/.git"),
            [(
                CommitData {
                    sha: commit_sha,
                    parents: smallvec![],
                    author_name: "Author".into(),
                    author_email: "author@example.com".into(),
                    commit_timestamp: 1_700_000_000,
                    subject: "Fix crash".into(),
                    message: "Fix crash\n\nThis fixes a crash that occurred when...".into(),
                },
                false,
            )],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Select the commit to trigger loading the commit message
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();

        // Verify the commit message was loaded as markdown
        git_graph.read_with(&*cx, |graph, app| {
            let message = graph
                .selected_commit_message
                .as_ref()
                .expect("selected_commit_message should be Some");
            assert_eq!(message.sha, commit_sha);
            let source = message.message.read_with(app, |m, _| m.source().to_owned());
            assert!(source.contains("Fix crash"));
            assert!(source.contains("This fixes a crash"));
        });
    }

    #[gpui::test]
    async fn test_long_commit_message_is_constrained_to_scroll_viewport(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).expect("commit SHA should be valid");
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec!["HEAD -> main".into()],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);

        let message = (0..40)
            .map(|line_number| {
                format!(
                    "Line {line_number}: This commit message is long enough to require scrolling."
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        fs.set_commit_data(
            Path::new("/project/.git"),
            [(
                CommitData {
                    sha: commit_sha,
                    parents: smallvec![],
                    author_name: "Author".into(),
                    author_email: "author@example.com".into(),
                    commit_timestamp: 1_700_000_000,
                    subject: "Long commit message".into(),
                    message: message.into(),
                },
                false,
            )],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());
        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();

        git_graph.update(cx, |graph, cx| {
            graph.selected_commit_diff = Some(CommitDiff {
                files: vec![CommitFile {
                    path: RepoPath::new("file.txt").expect("repository path should be valid"),
                    old_text: Some("content".into()),
                    new_text: Some("updated content".into()),
                    is_binary: false,
                }],
                is_shallow_boundary: false,
            });
            graph.selected_commit_diff_stats = Some((1, 1));
            cx.notify();
        });

        cx.draw(
            point(px(0.), px(0.)),
            gpui::size(px(1200.), px(800.)),
            |_, _| git_graph.clone().into_any_element(),
        );
        cx.run_until_parked();

        let (message_scroll_handle, changed_files_scroll_handle) =
            git_graph.read_with(&*cx, |graph, _| {
                (
                    graph
                        .selected_commit_message
                        .as_ref()
                        .expect("selected commit message should be loaded")
                        .scroll_handle
                        .clone(),
                    graph.changed_files_scroll_handle.clone(),
                )
            });
        let maximum_message_height = git_graph.update_in(cx, |_, window, cx| {
            editor::hover_markdown_style(window, cx)
                .base_text_style
                .line_height_in_pixels(window.rem_size())
                * 12.
        });
        let message_bounds = message_scroll_handle.bounds();
        let changed_files_bounds = changed_files_scroll_handle.0.borrow().base_handle.bounds();

        assert!(
            message_bounds.size.height <= maximum_message_height,
            "commit message viewport height ({}) should not exceed its maximum ({})",
            message_bounds.size.height,
            maximum_message_height,
        );
        assert!(
            message_scroll_handle.max_offset().y > px(0.),
            "long commit message should be scrollable"
        );
        assert!(
            message_bounds.bottom() <= changed_files_bounds.top(),
            "commit message viewport {message_bounds:?} should not overlap changed files {changed_files_bounds:?}"
        );
    }

    #[gpui::test]
    async fn test_commit_message_not_reloaded_for_same_sha(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({ ".git": {}, "file.txt": "content" }),
        )
        .await;

        let commit_sha = Oid::from_bytes(&[1; 20]).unwrap();
        let commits = vec![Arc::new(InitialGraphCommitData {
            sha: commit_sha,
            parents: smallvec![],
            ref_names: vec!["HEAD -> main".into()],
        })];
        fs.set_graph_commits(Path::new("/project/.git"), commits);
        fs.set_commit_data(
            Path::new("/project/.git"),
            [(
                CommitData {
                    sha: commit_sha,
                    parents: smallvec![],
                    author_name: "Author".into(),
                    author_email: "author@example.com".into(),
                    commit_timestamp: 1_700_000_000,
                    subject: "Fix crash".into(),
                    message: "Fix crash\n\nBody text.".into(),
                },
                false,
            )],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        cx.run_until_parked();

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace_weak =
            multi_workspace.read_with(&*cx, |multi, _| multi.workspace().downgrade());

        let git_graph = cx.new_window_entity(|window, cx| {
            GitGraph::new(
                repository.read(cx).id,
                project.read(cx).git_store().clone(),
                workspace_weak,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // Select the commit to load the message
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();

        // Verify message is loaded
        let message_entity_id = git_graph.read_with(&*cx, |graph, _| {
            graph
                .selected_commit_message
                .as_ref()
                .map(|m| m.message.entity_id())
        });
        assert!(message_entity_id.is_some());

        // Select the same commit again to trigger the early-return logic
        git_graph.update_in(cx, |graph, window, cx| {
            graph.select_first(&menu::SelectFirst, window, cx);
        });
        cx.run_until_parked();

        // Verify the message entity is the same (not replaced)
        git_graph.read_with(&*cx, |graph, _| {
            let new_entity_id = graph
                .selected_commit_message
                .as_ref()
                .map(|m| m.message.entity_id());
            assert_eq!(message_entity_id, new_entity_id);
        });
    }
}
