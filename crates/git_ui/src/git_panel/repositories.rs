//! The "Repositories" section at the top of the git panel: every repository in
//! the project, grouped under its parent folder, with linked worktrees shown
//! alongside main checkouts. Clicking a repository makes it active and opens
//! its git graph.

use std::{path::Path, sync::Arc};

use gpui::{Action as _, Anchor, AnyElement, Entity, SharedString, Window};
use project::git_store::Repository;
use ui::{Disclosure, ListItem, ListItemSpacing, PopoverMenu, Tooltip, prelude::*};

use super::{GitPanel, section_resize};
use crate::{git_graph::open_or_reuse_graph, repository_selector::RepositorySelector};
use git::repository::LogSource;

/// A repository entry as needed for grouping.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepositoryPath {
    work_directory: Arc<Path>,
    is_worktree: bool,
}

/// Indices into the input slice, grouped by the repository's parent folder.
/// Folders holding linked worktrees come first, then the rest; both by name.
/// Within a folder, repositories are sorted by name.
fn group_repositories(repositories: &[RepositoryPath]) -> Vec<(Arc<Path>, Vec<usize>)> {
    let mut groups: Vec<(Arc<Path>, Vec<usize>)> = Vec::new();
    for (idx, repository) in repositories.iter().enumerate() {
        let parent: Arc<Path> = repository
            .work_directory
            .parent()
            .unwrap_or(&repository.work_directory)
            .into();
        match groups.iter_mut().find(|(dir, _)| *dir == parent) {
            Some((_, members)) => members.push(idx),
            None => groups.push((parent, vec![idx])),
        }
    }

    for (_, members) in &mut groups {
        members.sort_by_key(|idx| file_name(&repositories[*idx].work_directory));
    }
    groups.sort_by_key(|(dir, members)| {
        let has_worktrees = members.iter().any(|idx| repositories[*idx].is_worktree);
        (!has_worktrees, file_name(dir))
    });
    groups
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

impl GitPanel {
    pub(super) fn render_repositories_section(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let repositories = self
            .project
            .read(cx)
            .git_store()
            .read(cx)
            .repositories()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        if repositories.is_empty() {
            return None;
        }

        let paths = repositories
            .iter()
            .map(|repository| {
                let repository = repository.read(cx);
                RepositoryPath {
                    work_directory: repository.work_directory_abs_path.clone(),
                    is_worktree: repository.is_linked_worktree(),
                }
            })
            .collect::<Vec<_>>();
        let active_repository_id = self
            .active_repository
            .as_ref()
            .map(|repository| repository.read(cx).id);

        let collapsed = self.repositories_collapsed;
        let header = h_flex()
            .h(rems(1.75))
            .pl_1()
            .pr_1p5()
            .gap_1()
            .justify_between()
            .child(
                h_flex()
                    .id("repositories-header")
                    .gap_1()
                    .cursor_pointer()
                    .child(Disclosure::new("repositories-disclosure", !collapsed))
                    .child(Label::new("Repositories").weight(gpui::FontWeight::SEMIBOLD))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.repositories_collapsed = !this.repositories_collapsed;
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(self.render_repository_search_button())
                    .child(
                        IconButton::new("create-worktree", IconName::GitBranchPlus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::for_action_title(
                                "Worktrees",
                                &zed_actions::git::Worktree,
                            ))
                            .on_click(|_, window, cx| {
                                window
                                    .dispatch_action(zed_actions::git::Worktree.boxed_clone(), cx);
                            }),
                    ),
            );

        let mut rows: Vec<AnyElement> = Vec::new();
        if !collapsed {
            for (dir, members) in group_repositories(&paths) {
                let group_collapsed = self.collapsed_repository_groups.contains(&dir);
                let group_name = SharedString::from(file_name(&dir));
                let group_dir = dir.clone();
                rows.push(
                    ListItem::new(SharedString::from(format!(
                        "repository-group-{}",
                        dir.display()
                    )))
                    .spacing(ListItemSpacing::Sparse)
                    .start_slot(
                        h_flex()
                            .gap_1()
                            .child(Disclosure::new(
                                SharedString::from(format!(
                                    "repository-group-disclosure-{}",
                                    dir.display()
                                )),
                                !group_collapsed,
                            ))
                            .child(
                                Icon::new(if group_collapsed {
                                    IconName::Folder
                                } else {
                                    IconName::FolderOpen
                                })
                                .size(IconSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(Label::new(group_name).truncate())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed_repository_groups.remove(&group_dir) {
                            this.collapsed_repository_groups.insert(group_dir.clone());
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
                );

                if group_collapsed {
                    continue;
                }
                for idx in members {
                    rows.push(self.render_repository_row(
                        &repositories[idx],
                        &paths[idx],
                        active_repository_id == Some(repositories[idx].read(cx).id),
                        cx,
                    ));
                }
            }
        }

        Some(
            v_flex()
                .flex_none()
                .relative()
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .child(header)
                .when(!collapsed, |this| {
                    this.child(
                        v_flex()
                            .id("repositories-list")
                            .relative()
                            .map(|this| match self.repositories_list_height {
                                Some(height) => this.h(height),
                                None => {
                                    this.max_h(section_resize::DEFAULT_REPOSITORIES_LIST_HEIGHT)
                                }
                            })
                            .overflow_y_scroll()
                            .px_1()
                            .pb_1()
                            .child(section_resize::measure_bounds(
                                self.repositories_list_bounds.clone(),
                            ))
                            .children(rows),
                    )
                    .child(self.render_section_resize_handle(
                        section_resize::PanelSection::Repositories,
                        false,
                        cx,
                    ))
                })
                .into_any_element(),
        )
    }

    fn render_repository_row(
        &self,
        repository: &Entity<Repository>,
        path: &RepositoryPath,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = SharedString::from(file_name(&path.work_directory));
        let tooltip_path = SharedString::from(path.work_directory.display().to_string());
        let repository = repository.clone();

        ListItem::new(SharedString::from(format!(
            "repository-{}",
            path.work_directory.display()
        )))
        .spacing(ListItemSpacing::Sparse)
        .indent_level(1)
        .indent_step_size(px(16.))
        .toggle_state(is_active)
        .start_slot(
            Icon::new(if path.is_worktree {
                IconName::GitWorktree
            } else {
                IconName::Book
            })
            .size(IconSize::Small)
            .color(Color::Muted),
        )
        .child(Label::new(name).truncate())
        .when(is_active, |this| {
            this.end_slot(
                Icon::new(IconName::Check)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
        })
        .tooltip(Tooltip::text(tooltip_path))
        .on_click(cx.listener(move |this, _, window, cx| {
            repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
            let repository_id = repository.read(cx).id;
            // Select the checked-out commit so its details panel opens.
            let head_sha = repository
                .read(cx)
                .head_commit
                .as_ref()
                .map(|commit| commit.sha.to_string());
            this.workspace
                .update(cx, |workspace, cx| {
                    let git_store = workspace.project().read(cx).git_store().clone();
                    open_or_reuse_graph(
                        workspace,
                        repository_id,
                        git_store,
                        LogSource::All,
                        head_sha,
                        window,
                        cx,
                    );
                })
                .ok();
        }))
        .into_any_element()
    }

    /// Reuses the repository picker to search repositories by name.
    fn render_repository_search_button(&self) -> impl IntoElement {
        let project = self.project.clone();
        PopoverMenu::new("repository-search")
            .menu(move |window, cx| {
                let project = project.clone();
                Some(cx.new(|cx| RepositorySelector::new(project, rems(20.), window, cx)))
            })
            .trigger_with_tooltip(
                IconButton::new("repository-search-button", IconName::MagnifyingGlass)
                    .icon_size(IconSize::Small),
                Tooltip::text("Search Repositories"),
            )
            .anchor(Anchor::TopRight)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(path: &str, is_worktree: bool) -> RepositoryPath {
        RepositoryPath {
            work_directory: Path::new(path).into(),
            is_worktree,
        }
    }

    #[test]
    fn test_group_repositories_by_parent_folder() {
        let repositories = vec![
            repo("/src/chocofin/reward-stack", false),
            repo(
                "/src/chocofin/.worktrees/CCA-730/corporate-serverless-stack-nric",
                true,
            ),
            repo("/src/chocofin/.agents", false),
            repo("/src/chocofin/.worktrees/CCA-691/reward-stack", true),
            repo(
                "/src/chocofin/.worktrees/CCA-730/chocolate-serverless-stack",
                true,
            ),
        ];

        let groups = group_repositories(&repositories)
            .into_iter()
            .map(|(dir, members)| {
                (
                    file_name(&dir),
                    members
                        .into_iter()
                        .map(|idx| file_name(&repositories[idx].work_directory))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            groups,
            vec![
                ("CCA-691".to_string(), vec!["reward-stack".to_string()]),
                (
                    "CCA-730".to_string(),
                    vec![
                        "chocolate-serverless-stack".to_string(),
                        "corporate-serverless-stack-nric".to_string(),
                    ]
                ),
                (
                    "chocofin".to_string(),
                    vec![".agents".to_string(), "reward-stack".to_string()]
                ),
            ]
        );
    }
}
