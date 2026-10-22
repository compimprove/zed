//! One file of a commit in its own read-only tab: the file's content after
//! the commit, with every change of that commit shown in a unified or split
//! diff.

use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use editor::{
    DiffStyleControls, Direction, Editor, EditorEvent, EditorSettings, HiddenDiffHunkRenderer,
    MultiBuffer, SplittableEditor,
    actions::{GoToHunk, GoToPreviousHunk, OpenExcerpts, OpenExcerptsSplit},
};
use git::repository::RepoPath;
use gpui::{
    Action, App, AppContext as _, Context, Empty, Entity, EventEmitter, FocusHandle, Focusable,
    Font, IntoElement, Render, Task, WeakEntity, Window,
};
use language::{Bias, Buffer, HighlightedText, Point};
use multi_buffer::ToPoint as _;
use project::{
    ProjectPath,
    git_store::{CommitDiff, Repository},
};
use settings::Settings;
use std::{
    any::{Any, TypeId},
    sync::Arc,
};
use ui::{Divider, Tooltip, prelude::*};
use workspace::{
    Item, ItemHandle, ItemNavHistory, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace, item::ItemEvent, notifications::DetachAndPromptErr as _,
    searchable::SearchableItemHandle,
};

use crate::{
    commit_view::{CommitFileBuffer, build_commit_file_buffer},
    solo_diff_view::SoloDiffView,
};

pub struct CommitFileView {
    editor: Entity<SplittableEditor>,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    showing_full_file: bool,
    repository: WeakEntity<Repository>,
    repo_path: RepoPath,
    workspace: WeakEntity<Workspace>,
}

/// Opens the file as it is now in the project, with the language server,
/// optionally at a 0-based row.
pub(crate) fn open_current_file(
    repository: &WeakEntity<Repository>,
    repo_path: &RepoPath,
    row: Option<u32>,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(project_path) = repository
        .read_with(cx, |repository, cx| {
            repository.repo_path_to_project_path(repo_path, cx)
        })
        .ok()
        .flatten()
    else {
        return;
    };
    let Ok(open) = workspace.update(cx, |workspace, cx| {
        workspace.open_path(project_path, None, true, window, cx)
    }) else {
        return;
    };
    window
        .spawn(cx, async move |cx| -> Result<()> {
            let item = open.await?;
            if let (Some(row), Some(editor)) = (row, item.downcast::<Editor>()) {
                editor.update_in(cx, |editor, window, cx| {
                    let point = editor
                        .buffer()
                        .read(cx)
                        .snapshot(cx)
                        .clip_point(Point::new(row, 0), Bias::Left);
                    editor.go_to_singleton_buffer_point(point, window, cx);
                })?;
            }
            Ok(())
        })
        .detach_and_prompt_err("Failed to open file", window, cx, |_, _, _| None);
}

impl CommitFileView {
    pub(crate) fn open(
        commit_sha: String,
        repository: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        path: RepoPath,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(strong_repository) = repository.upgrade() else {
            return;
        };
        let commit_diff = strong_repository.update(cx, |repository, cx| {
            repository.load_commit_diff(commit_sha.clone(), false, cx)
        });
        Self::open_diff(
            commit_diff,
            commit_sha,
            repository,
            workspace,
            path,
            window,
            cx,
        );
    }

    /// Opens the file's changes from `base_sha` to `target_sha`, or from their
    /// merge base to `target_sha` when `merge_base` is set.
    pub(crate) fn open_range(
        base_sha: String,
        target_sha: String,
        merge_base: bool,
        repository: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        path: RepoPath,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(strong_repository) = repository.upgrade() else {
            return;
        };
        let range_diff = strong_repository.update(cx, |repository, cx| {
            repository.load_range_diff(base_sha, target_sha.clone(), merge_base, cx)
        });
        Self::open_diff(
            range_diff, target_sha, repository, workspace, path, window, cx,
        );
    }

    fn open_diff(
        commit_diff: Task<Result<CommitDiff>>,
        commit_sha: String,
        repository: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        path: RepoPath,
        window: &mut Window,
        cx: &mut App,
    ) {
        let weak_repository = repository.clone();
        let Some(repository) = repository.upgrade() else {
            return;
        };

        window
            .spawn(cx, async move |cx| -> Result<()> {
                let mut commit_diff = commit_diff.await?;
                let file_ix = commit_diff
                    .files
                    .iter()
                    .position(|file| file.path == path)
                    .context("file not changed in this commit")?;
                let file = commit_diff.files.swap_remove(file_ix);
                let project =
                    workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
                let language_registry =
                    project.read_with(cx, |project, _| project.languages().clone());
                let CommitFileBuffer {
                    buffer,
                    buffer_diff,
                    ..
                } = build_commit_file_buffer(
                    file,
                    &commit_sha,
                    &repository,
                    &project,
                    &language_registry,
                    cx,
                )
                .await?;

                workspace.update_in(cx, |workspace, window, cx| {
                    let workspace_entity = cx.entity();
                    let workspace_handle = cx.weak_entity();
                    let view = cx.new(|cx| {
                        let showing_full_file =
                            EditorSettings::get_global(cx).file_diff.show_full_file;
                        let multibuffer = cx.new(|cx| {
                            let mut multibuffer = SoloDiffView::build_multibuffer(
                                buffer.clone(),
                                buffer_diff.clone(),
                                showing_full_file,
                                cx,
                            );
                            multibuffer.set_all_diff_hunks_expanded(cx);
                            multibuffer
                        });
                        let editor = cx.new(|cx| {
                            let editor = SplittableEditor::new(
                                EditorSettings::get_global(cx).diff_view_style,
                                multibuffer,
                                project,
                                workspace_entity,
                                window,
                                cx,
                            );
                            editor
                                .set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
                            editor.rhs_editor().update(cx, |editor, cx| {
                                editor.set_read_only(true);
                                editor.set_allow_git_diff_scrollbar_markers(showing_full_file, cx);
                                let snapshot = editor.snapshot(window, cx);
                                editor.go_to_hunk_before_or_after_position(
                                    &snapshot,
                                    language::Point::zero(),
                                    Direction::Next,
                                    true,
                                    window,
                                    cx,
                                );
                            });
                            editor
                        });
                        Self {
                            editor,
                            buffer,
                            diff: buffer_diff,
                            showing_full_file,
                            repository: weak_repository,
                            repo_path: path,
                            workspace: workspace_handle,
                        }
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })
            })
            .detach_and_log_err(cx);
    }

    /// Opens the current project file at the cursor's line, instead of
    /// another copy of this commit's version of the file.
    fn open_current_file_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rhs_editor = self.editor.read(cx).rhs_editor().clone();
        let row = rhs_editor.update(cx, |editor, cx| {
            let head = editor.selections.newest_anchor().head();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            snapshot
                .point_to_buffer_point(head.to_point(&snapshot))
                .map(|(_, point)| point.row)
        });
        open_current_file(
            &self.repository,
            &self.repo_path,
            row,
            &self.workspace,
            window,
            cx,
        );
    }

    fn toggle_showing_full_file(&mut self, cx: &mut Context<Self>) {
        self.showing_full_file = !self.showing_full_file;
        SoloDiffView::show_excerpts(
            &self.editor,
            &self.buffer,
            &self.diff,
            self.showing_full_file,
            cx,
        );
        cx.notify();
    }
}

impl EventEmitter<EditorEvent> for CommitFileView {}

impl Focusable for CommitFileView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for CommitFileView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.buffer
            .read(cx)
            .file()
            .map(|file| SharedString::from(file.file_name(cx).to_string()))
            .unwrap_or_else(|| MultiBuffer::DEFAULT_TITLE.into())
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let file = self.buffer.read(cx).file()?;
        Some(file.full_path(cx).to_string_lossy().into_owned().into())
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.deactivated(window, cx);
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            // Hidden so the search bar doesn't add a second copy of this
            // view's toolbar controls.
            None
        } else {
            self.editor.act_as_type(type_id, cx)
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            })
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor.update(cx, |editor, cx| {
            editor
                .rhs_editor()
                .update(cx, |editor, cx| editor.navigate(data, window, cx))
        })
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        self.editor.breadcrumbs(cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.added_to_workspace(workspace, window, cx)
            })
        });
    }
}

impl Render for CommitFileView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .capture_action(cx.listener(|this, _: &OpenExcerpts, window, cx| {
                cx.stop_propagation();
                this.open_current_file_at_cursor(window, cx);
            }))
            .capture_action(cx.listener(|this, _: &OpenExcerptsSplit, window, cx| {
                cx.stop_propagation();
                this.open_current_file_at_cursor(window, cx);
            }))
            .child(self.editor.clone())
    }
}

/// The unified/split diff toggle for a [`CommitFileView`].
pub struct CommitFileViewToolbar {
    commit_file: Option<WeakEntity<CommitFileView>>,
}

impl CommitFileViewToolbar {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self { commit_file: None }
    }
}

impl CommitFileViewToolbar {
    /// Runs an editor action in the diff, focusing it first so the action
    /// reaches the focused side.
    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        let Some(commit_file) = self.commit_file.as_ref().and_then(|view| view.upgrade()) else {
            return;
        };
        commit_file.focus_handle(cx).focus(window, cx);
        let action = action.boxed_clone();
        cx.defer(move |cx| cx.dispatch_action(action.as_ref()));
    }
}

impl EventEmitter<ToolbarItemEvent> for CommitFileViewToolbar {}

impl ToolbarItemView for CommitFileViewToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.commit_file = active_pane_item
            .and_then(|item| item.act_as::<CommitFileView>(cx))
            .map(|entity| entity.downgrade());
        if self.commit_file.is_some() {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl Render for CommitFileViewToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(commit_file) = self.commit_file.as_ref().and_then(|view| view.upgrade()) else {
            return Empty.into_any_element();
        };
        let (editor, showing_full_file) = {
            let commit_file = commit_file.read(cx);
            (commit_file.editor.clone(), commit_file.showing_full_file)
        };
        let focus_handle = editor.focus_handle(cx);
        let (expand_icon, expand_tooltip) = if showing_full_file {
            (IconName::ChevronDownUp, "Show Changes Only")
        } else {
            (IconName::ChevronUpDown, "Show Full File")
        };

        h_flex()
            .pl_0p5()
            .gap_1()
            .child(
                IconButton::new("commit-file-toggle-excerpts", expand_icon)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text(expand_tooltip))
                    .on_click(move |_, _, cx| {
                        commit_file.update(cx, |commit_file, cx| {
                            commit_file.toggle_showing_full_file(cx)
                        });
                    }),
            )
            .child(DiffStyleControls::new(editor))
            .child(Divider::vertical())
            .child(
                IconButton::new("commit-file-previous-hunk", IconName::ArrowUp)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title_in(
                        "Go to Previous Change",
                        &GoToPreviousHunk,
                        &focus_handle,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_action(&GoToPreviousHunk, window, cx)
                    })),
            )
            .child(
                IconButton::new("commit-file-next-hunk", IconName::ArrowDown)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title_in(
                        "Go to Next Change",
                        &GoToHunk,
                        &focus_handle,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_action(&GoToHunk, window, cx)
                    })),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;

    const COMMIT_SHA: &str = "1111111111111111111111111111111111111111";

    /// A 30-line file before the commit; the commit changes lines 2 and 29.
    fn old_text() -> String {
        (1..=30).map(|line| format!("line {line}\n")).collect()
    }

    fn new_text() -> String {
        old_text()
            .replace("line 2\n", "LINE 2\n")
            .replace("line 29\n", "LINE 29\n")
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    async fn open_commit_file(
        cx: &mut TestAppContext,
    ) -> (
        Entity<CommitFileView>,
        Entity<Workspace>,
        &mut VisualTestContext,
    ) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "src": { "lib.rs": new_text() },
            }),
        )
        .await;
        fs.set_commit_diff(
            Path::new("/project/.git"),
            COMMIT_SHA,
            vec![(
                RepoPath::new("src/lib.rs").unwrap(),
                Some(old_text()),
                Some(new_text()),
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
        workspace.update_in(cx, |workspace, window, cx| {
            CommitFileView::open(
                COMMIT_SHA.to_string(),
                repository.downgrade(),
                workspace.weak_handle(),
                RepoPath::new("src/lib.rs").unwrap(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let view = workspace.read_with(&*cx, |workspace, cx| {
            workspace
                .active_item_as::<CommitFileView>(cx)
                .expect("the commit file view should be the active item")
        });
        (view, workspace, cx)
    }

    fn cursor_row(view: &Entity<CommitFileView>, cx: &mut VisualTestContext) -> u32 {
        view.update(cx, |view, cx| {
            let rhs_editor = view.editor.read(cx).rhs_editor().clone();
            rhs_editor.update(cx, |editor, cx| {
                let head = editor.selections.newest_anchor().head();
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                snapshot
                    .point_to_buffer_point(head.to_point(&snapshot))
                    .map(|(_, point)| point.row)
                    .expect("cursor should be in the file")
            })
        })
    }

    #[gpui::test]
    async fn test_opens_read_only_on_first_change(cx: &mut TestAppContext) {
        init_test(cx);
        let (view, _workspace, cx) = open_commit_file(cx).await;

        view.read_with(&*cx, |view, cx| {
            assert_eq!(view.tab_content_text(0, cx), "1111111 - lib.rs");
            assert!(view.editor.read(cx).rhs_editor().read(cx).read_only(cx));
        });
        assert_eq!(
            cursor_row(&view, cx),
            1,
            "cursor starts on the first change"
        );
    }

    #[gpui::test]
    async fn test_toggles_between_full_file_and_changes_only(cx: &mut TestAppContext) {
        init_test(cx);
        let (view, _workspace, cx) = open_commit_file(cx).await;

        let row_count = |view: &Entity<CommitFileView>, cx: &mut VisualTestContext| {
            view.read_with(&*cx, |view, cx| {
                let rhs_editor = view.editor.read(cx).rhs_editor().clone();
                let snapshot = rhs_editor.read(cx).buffer().read(cx).snapshot(cx);
                snapshot.max_point().row
            })
        };

        let initially_full = view.read_with(&*cx, |view, _| view.showing_full_file);
        let first_count = row_count(&view, cx);
        for _ in 0..2 {
            view.update(cx, |view, cx| view.toggle_showing_full_file(cx));
            cx.run_until_parked();
        }
        assert_eq!(
            view.read_with(&*cx, |view, _| view.showing_full_file),
            initially_full
        );
        assert_eq!(row_count(&view, cx), first_count);

        view.update(cx, |view, cx| view.toggle_showing_full_file(cx));
        cx.run_until_parked();
        let toggled_count = row_count(&view, cx);
        if initially_full {
            assert!(
                toggled_count < first_count,
                "changes only hides unchanged lines"
            );
        } else {
            assert!(toggled_count > first_count, "full file shows every line");
        }
    }

    #[gpui::test]
    async fn test_open_excerpts_opens_current_project_file(cx: &mut TestAppContext) {
        init_test(cx);
        let (view, workspace, cx) = open_commit_file(cx).await;

        view.update_in(cx, |view, window, cx| {
            view.focus_handle(cx).focus(window, cx);
        });
        cx.dispatch_action(OpenExcerpts);
        cx.run_until_parked();
        let editor = workspace.read_with(&*cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("the current file should open in an editor")
        });
        editor.read_with(&*cx, |editor, cx| {
            let buffer = editor
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("a single file");
            assert!(
                project::File::from_dyn(buffer.read(cx).file()).is_some(),
                "a project file, not the commit's version"
            );
            assert!(!editor.read_only(cx));
        });
        let cursor_row = editor.update(cx, |editor, cx| {
            editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head()
                .row
        });
        assert_eq!(cursor_row, 1, "opens at the cursor's line");
        workspace.read_with(&*cx, |workspace, cx| {
            assert_eq!(
                workspace.items_of_type::<CommitFileView>(cx).count(),
                1,
                "no second copy of the commit's version"
            );
        });
    }
}
