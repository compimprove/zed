use crate::{
    commit_view::CommitView, create_tag_at_commit, git_panel::show_error_toast, picker_prompt,
};
use anyhow::anyhow;
use askpass::AskPassDelegate;
use git::Oid;
use git_ui_core::askpass_modal::AskPassModal;
use gpui::{Action, ClipboardItem, Entity, FocusHandle, SharedString, WeakEntity, Window, actions};
use notifications::status_toast::StatusToast;
use project::{GIT_COMMAND_TASK_TAG, git_store::Repository};

use task::{TaskContext, TaskVariables, VariableName};
use ui::{Color, ContextMenu, ContextMenuEntry, IconName, IconPosition, prelude::*};
use util::ResultExt as _;
use workspace::{Workspace, notifications::DetachAndPromptErr as _};

actions!(
    git_graph,
    [
        /// Copies the SHA of the selected commit to the clipboard.
        CopyCommitSha,
        /// Copies a tag from the selected commit to the clipboard.
        CopyCommitTag,
        /// Opens the commit view for the selected commit.
        OpenCommitView,
    ]
);

const COMMIT_TAG_LIST_WIDTH_IN_REMS: Rems = rems(10.);
pub(crate) const CUSTOM_GIT_COMMANDS_DOCS_SLUG: &str = "tasks#custom-git-commands";

pub(crate) struct CommitContextMenuData {
    pub(crate) sha: Oid,
    pub(crate) tag_names: Vec<SharedString>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitContextMenuSource {
    GitPanel,
}

pub(crate) fn commit_context_menu(
    commit: CommitContextMenuData,
    source: CommitContextMenuSource,
    ref_name: Option<SharedString>,
    focus_handle: FocusHandle,
    repository: Option<WeakEntity<Repository>>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let sha = commit.sha;
    let sha_short = sha.display_short();
    let git_tasks = git_context_menu_tasks(
        git_task_context(&repository, sha, ref_name.as_deref(), cx),
        &workspace,
        cx,
    );
    let header = match &ref_name {
        Some(ref_name) => format!("Ref {ref_name}"),
        None => format!("Commit {sha_short}"),
    };

    ContextMenu::build(window, cx, move |context_menu, _, _| {
        context_menu
            .context(focus_handle)
            .header(header)
            .entry("View Diff", Some(OpenCommitView.boxed_clone()), {
                let repository = repository.clone();
                let workspace = workspace.clone();
                move |window, cx| {
                    let Some(repository) = repository.clone() else {
                        return;
                    };
                    CommitView::open(
                        sha.to_string(),
                        repository,
                        workspace.clone(),
                        None,
                        None,
                        window,
                        cx,
                    );
                }
            })
            .entry(
                "Copy SHA",
                Some(CopyCommitSha.boxed_clone()),
                move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(sha.to_string()));
                },
            )
            .when_some(ref_name.clone(), |menu, ref_name| {
                menu.entry("Copy Ref Name", None, move |_window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(ref_name.to_string()));
                })
            })
            .when(ref_name.is_none(), |menu| {
                menu.map(|menu| {
                    let tag_names = commit.tag_names.clone();
                    let copy_tag_label = "Copy Tag";

                    match tag_names.as_slice() {
                        [] => menu.item(
                            ContextMenuEntry::new(copy_tag_label)
                                .action(CopyCommitTag.boxed_clone())
                                .disabled(true),
                        ),
                        [tag_name] => {
                            let tag_name = tag_name.clone();
                            let label = format!("{copy_tag_label}: {tag_name}");
                            menu.entry(
                                label,
                                Some(CopyCommitTag.boxed_clone()),
                                move |_window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        tag_name.to_string(),
                                    ));
                                },
                            )
                        }
                        _ => menu.submenu(copy_tag_label, move |menu, _window, _cx| {
                            let mut menu = menu.fixed_width(COMMIT_TAG_LIST_WIDTH_IN_REMS.into());

                            for tag_name in tag_names.clone() {
                                let tag_name_to_copy = tag_name.clone();
                                menu = menu.entry(tag_name, None, move |_window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        tag_name_to_copy.to_string(),
                                    ));
                                });
                            }
                            menu
                        }),
                    }
                })
            })
            .when(ref_name.is_none(), |menu| {
                let tag_names = commit.tag_names.clone();
                let push_tag_label = "Push Tag";
                let push_entry_handler = {
                    let repository = repository.clone();
                    let workspace = workspace.clone();
                    move |tag_name: SharedString| {
                        let repository = repository.clone();
                        let workspace = workspace.clone();
                        move |window: &mut Window, cx: &mut App| {
                            let Some(repository) =
                                repository.as_ref().and_then(WeakEntity::upgrade)
                            else {
                                return;
                            };
                            push_tag(tag_name.clone(), repository, workspace.clone(), window, cx);
                        }
                    }
                };

                match tag_names.as_slice() {
                    [] => menu.item(ContextMenuEntry::new(push_tag_label).disabled(true)),
                    [tag_name] => menu.entry(
                        format!("{push_tag_label}: {tag_name}"),
                        None,
                        push_entry_handler(tag_name.clone()),
                    ),
                    _ => menu.submenu(push_tag_label, move |menu, _window, _cx| {
                        let mut menu = menu.fixed_width(COMMIT_TAG_LIST_WIDTH_IN_REMS.into());
                        for tag_name in tag_names.clone() {
                            menu = menu.entry(tag_name.clone(), None, push_entry_handler(tag_name));
                        }
                        menu
                    }),
                }
            })
            .entry("Create Tag…", None, {
                let repository = repository.clone();
                let workspace = workspace.clone();
                move |window, cx| {
                    let Some(repository) = repository.as_ref().and_then(WeakEntity::upgrade) else {
                        return;
                    };
                    workspace
                        .update(cx, |workspace, cx| {
                            create_tag_at_commit(sha, false, repository, workspace, window, cx);
                        })
                        .log_err();
                }
            })
            .when(source == CommitContextMenuSource::GitPanel, |menu| {
                menu.entry("Show in Git Graph", None, move |window, cx| {
                    window.dispatch_action(
                        Box::new(crate::git_graph::OpenAtCommit {
                            sha: sha.to_string(),
                        }),
                        cx,
                    );
                })
            })
            .map(|mut menu| {
                menu = menu.separator().header("Custom Commands");

                if git_tasks.is_empty() {
                    return menu.item(
                        ContextMenuEntry::new("Learn More")
                            .icon(IconName::ArrowUpRight)
                            .icon_color(Color::Muted)
                            .icon_position(IconPosition::End)
                            .handler(|_window, cx| {
                                let docs_url =
                                    release_channel::docs_url(CUSTOM_GIT_COMMANDS_DOCS_SLUG, cx);
                                cx.open_url(&docs_url);
                            }),
                    );
                }

                for (task_source_kind, resolved_task) in git_tasks {
                    let label = resolved_task.display_label().to_string();
                    let workspace = workspace.clone();
                    menu = menu.entry(label, None, move |window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.schedule_resolved_task(
                                    task_source_kind.clone(),
                                    resolved_task.clone(),
                                    false,
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    });
                }

                menu
            })
    })
}

/// Switches to `branch_name`, creating a tracking branch for a remote branch.
pub(crate) fn checkout_branch(
    repository: Entity<Repository>,
    branch_name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let receiver = repository.update(cx, |repository, _| repository.change_branch(branch_name));
    cx.spawn(async move |_| receiver.await?)
        .detach_and_prompt_err("Failed to checkout branch", window, cx, |_, _, _| None);
}

/// Pushes `tag_name` to a remote, prompting for the remote when there is more
/// than one.
pub(crate) fn push_tag(
    tag_name: SharedString,
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace_entity) = workspace.upgrade() else {
        return;
    };
    if workspace_entity.read(cx).project().read(cx).is_via_collab() {
        show_error_toast(
            workspace_entity,
            "push",
            anyhow!("Pushing is not supported in collaborative projects"),
            cx,
        );
        return;
    }

    let remotes = repository.update(cx, |repository, _| repository.get_remotes(None, true));
    let askpass = askpass_delegate(
        format!("git push {tag_name}").into(),
        workspace.clone(),
        window,
        cx,
    );

    window
        .spawn(cx, async move |cx| {
            let remote_names = remotes
                .await??
                .into_iter()
                .map(|remote| remote.name)
                .collect::<Vec<_>>();
            if remote_names.is_empty() {
                return Err(anyhow!("No remote available to push the tag to"));
            }
            let selection = cx
                .update(|window, cx| {
                    picker_prompt::prompt(
                        "Pick which remote to push the tag to",
                        remote_names.clone(),
                        workspace.clone(),
                        window,
                        cx,
                    )
                })?
                .await;
            let Some(remote_name) = selection.and_then(|index| remote_names.get(index).cloned())
            else {
                return Ok(());
            };

            // The refspec is fully qualified so git never resolves it to a
            // branch of the same name.
            let tag_ref: SharedString = format!("refs/tags/{tag_name}").into();
            let push = repository.update(cx, |repository, cx| {
                repository.push(
                    tag_ref.clone(),
                    tag_ref,
                    remote_name.clone(),
                    None,
                    askpass,
                    cx,
                )
            });
            let push_result = push.await?;

            cx.update(|_, cx| {
                let Some(workspace) = workspace.upgrade() else {
                    return;
                };
                if let Err(error) = push_result {
                    show_error_toast(workspace, "push", error, cx);
                    return;
                }
                workspace.update(cx, |workspace, cx| {
                    let toast = StatusToast::new(
                        format!("Pushed tag {tag_name} to {remote_name}"),
                        cx,
                        |this, _| {
                            this.icon(
                                Icon::new(IconName::Tag)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                        },
                    );
                    workspace.toggle_status_toast(toast, cx);
                });
            })?;
            anyhow::Ok(())
        })
        .detach_and_prompt_err("Failed to push tag", window, cx, |_, _, _| None);
}

/// Answers git's credential prompts for `operation` with a password modal.
pub(crate) fn askpass_delegate(
    operation: SharedString,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> AskPassDelegate {
    let window_handle = window.window_handle();
    AskPassDelegate::new_with_cancellation(
        &mut cx.to_async(),
        move |prompt, tx, cancellation, cx| {
            window_handle
                .update(cx, |_, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.toggle_modal(window, cx, |window, cx| {
                                AskPassModal::new(
                                    operation.clone(),
                                    prompt.into(),
                                    tx,
                                    cancellation,
                                    window,
                                    cx,
                                )
                            });
                        })
                        .log_err();
                })
                .log_err();
        },
    )
}

pub(crate) fn git_task_context(
    repository: &Option<WeakEntity<Repository>>,
    commit_sha: git::Oid,
    ref_name: Option<&str>,
    cx: &App,
) -> Option<TaskContext> {
    let repository_path = repository
        .as_ref()?
        .upgrade()?
        .read(cx)
        .work_directory_abs_path
        .to_path_buf();
    let repository_name = repository_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string);
    let mut task_variables = TaskVariables::from_iter([
        (VariableName::GitSha, commit_sha.to_string()),
        (VariableName::GitShaShort, commit_sha.display_short()),
        (
            VariableName::GitRepositoryPath,
            repository_path.to_string_lossy().into_owned(),
        ),
    ]);

    if let Some(repository_name) = repository_name {
        task_variables.insert(VariableName::GitRepositoryName, repository_name);
    }
    if let Some(ref_name) = ref_name {
        task_variables.insert(VariableName::GitRef, ref_name.to_string());
    }

    Some(TaskContext {
        cwd: Some(repository_path),
        task_variables,
        ..TaskContext::default()
    })
}

pub(crate) fn git_context_menu_tasks(
    task_context: Option<TaskContext>,
    workspace: &WeakEntity<Workspace>,
    cx: &App,
) -> Vec<(project::TaskSourceKind, task::ResolvedTask)> {
    let Some(task_context) = task_context else {
        return Vec::new();
    };
    let Some(workspace) = workspace.upgrade() else {
        return Vec::new();
    };
    let project = workspace.read(cx).project().clone();
    let task_inventory = project.read_with(cx, |project, cx| {
        project.task_store().read(cx).task_inventory().cloned()
    });
    let Some(task_inventory) = task_inventory else {
        return Vec::new();
    };

    task_inventory
        .read(cx)
        .resolve_global_tasks_with_tag(GIT_COMMAND_TASK_TAG, &task_context)
}
