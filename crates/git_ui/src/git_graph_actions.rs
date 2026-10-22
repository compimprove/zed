//! Context menu actions of the git graph for commits, branches, remote
//! branches, and tags, mirroring VS Code's Git Graph extension. Every action
//! runs plain `git` commands through [`Repository::run_git_command`].

use std::future::Future;

use anyhow::Result;
use git::{
    Oid,
    repository::{RemoteCommandOutput, find_local_branch},
};
use gpui::{
    Action as _, App, AsyncWindowContext, ClipboardItem, Entity, PromptLevel, SharedString,
    WeakEntity, Window,
};
use notifications::status_toast::StatusToast;
use project::git_store::{CommitDataState, Repository};
use ui::{ContextMenu, ContextMenuEntry, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{
    branch_picker::force_delete_prompt_for_branch_delete_error,
    commit_context_menu::{OpenCommitView, askpass_delegate},
    commit_view::CommitView,
    git_action_dialog::{GitActionDialog, GitActionField, GitActionValues},
    git_panel::show_error_toast,
};

macro_rules! git_args {
    ($($arg:expr),* $(,)?) => {
        vec![$($arg.to_string()),*]
    };
}

/// A ref shown in the git graph, classified by kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GraphRef {
    LocalBranch {
        name: SharedString,
        is_head: bool,
    },
    RemoteBranch {
        remote: SharedString,
        branch: SharedString,
    },
    Tag {
        name: SharedString,
    },
}

impl GraphRef {
    /// Classifies a ref from git's `%D` decoration format, returning `None`
    /// for a detached `HEAD` and a remote's `HEAD` pointer.
    pub(crate) fn from_decoration(
        decoration: &str,
        head_branch: Option<&str>,
        remote_names: &[SharedString],
    ) -> Option<Self> {
        if let Some(name) = decoration.strip_prefix("tag: ") {
            return Some(Self::Tag {
                name: name.to_string().into(),
            });
        }
        let name = decoration.strip_prefix("HEAD -> ").unwrap_or(decoration);
        if name.is_empty() || name == "HEAD" {
            return None;
        }
        let remote_branch = name
            .split_once('/')
            .filter(|(remote, _)| remote_names.iter().any(|known| known.as_ref() == *remote));
        if let Some((remote, branch)) = remote_branch {
            if branch == "HEAD" {
                return None;
            }
            return Some(Self::RemoteBranch {
                remote: remote.to_string().into(),
                branch: branch.to_string().into(),
            });
        }
        Some(Self::LocalBranch {
            name: name.to_string().into(),
            is_head: head_branch == Some(name),
        })
    }
}

/// The repository state the graph's context menu actions depend on.
#[derive(Clone)]
pub(crate) struct GraphActionContext {
    pub(crate) repository: WeakEntity<Repository>,
    pub(crate) workspace: WeakEntity<Workspace>,
    pub(crate) head_branch: Option<SharedString>,
    pub(crate) remotes: Vec<SharedString>,
    pub(crate) local_branches: Vec<SharedString>,
    /// Remote branches as `remote/branch`.
    pub(crate) remote_branches: Vec<SharedString>,
}

impl GraphActionContext {
    pub(crate) fn new(
        repository: &Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        remotes: Vec<SharedString>,
        cx: &App,
    ) -> Self {
        let snapshot = repository.read(cx).snapshot();
        let mut local_branches = Vec::new();
        let mut remote_branches = Vec::new();
        for branch in snapshot.branch_list.iter() {
            let name = SharedString::from(branch.name().to_string());
            if branch.is_remote() {
                remote_branches.push(name);
            } else {
                local_branches.push(name);
            }
        }
        Self {
            repository: repository.downgrade(),
            workspace,
            head_branch: snapshot
                .branch
                .as_ref()
                .map(|branch| branch.name().to_string().into()),
            remotes,
            local_branches,
            remote_branches,
        }
    }

    fn current_branch_label(&self) -> SharedString {
        self.head_branch
            .clone()
            .unwrap_or_else(|| SharedString::from("HEAD"))
    }

    fn has_local_branch(&self, name: &str) -> bool {
        self.local_branches.iter().any(|branch| branch == name)
    }

    /// The remotes that have a branch named `branch`.
    fn remotes_with_branch(&self, branch: &str) -> Vec<SharedString> {
        self.remotes
            .iter()
            .filter(|remote| {
                let remote_branch = format!("{remote}/{branch}");
                self.remote_branches
                    .iter()
                    .any(|name| name.as_ref() == remote_branch)
            })
            .cloned()
            .collect()
    }

    /// The remote to push `branch` to by default: its upstream remote, then
    /// `origin`, then the first remote.
    fn default_push_remote(&self, branch: Option<&str>, cx: &App) -> usize {
        let upstream_remote = branch.and_then(|branch| {
            let repository = self.repository.upgrade()?;
            let snapshot = repository.read(cx).snapshot();
            let branch = snapshot
                .branch_list
                .iter()
                .find(|candidate| !candidate.is_remote() && candidate.name() == branch)?;
            branch
                .upstream
                .as_ref()
                .and_then(|upstream| upstream.remote_name())
                .map(ToString::to_string)
        });
        upstream_remote
            .as_deref()
            .into_iter()
            .chain(["origin"])
            .find_map(|name| self.remotes.iter().position(|remote| remote == name))
            .unwrap_or(0)
    }
}

/// The commit a context menu was opened on.
#[derive(Clone)]
pub(crate) struct GraphCommit {
    pub(crate) sha: Oid,
    pub(crate) subject: Option<SharedString>,
    /// The parents with their subjects, when loaded.
    pub(crate) parents: Vec<(Oid, Option<SharedString>)>,
    /// Whether the commit can be dropped or reworded by rebasing the current branch.
    pub(crate) can_drop: bool,
}

impl GraphCommit {
    fn parent_options(&self) -> Vec<SharedString> {
        self.parents
            .iter()
            .map(|(sha, subject)| match subject {
                Some(subject) => format!("{}: {subject}", sha.display_short()).into(),
                None => sha.display_short().into(),
            })
            .collect()
    }

    fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

/// Runs the `git` commands of one action.
struct GitRunner {
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
}

impl GitRunner {
    async fn git(
        &self,
        args: Vec<String>,
        cx: &mut AsyncWindowContext,
    ) -> Result<RemoteCommandOutput> {
        let repository = self.repository.clone();
        let workspace = self.workspace.clone();
        let receiver = cx.update(|window, cx| {
            let askpass = askpass_delegate(
                format!("git {}", args.join(" ")).into(),
                workspace,
                window,
                cx,
            );
            repository.update(cx, |repository, cx| {
                repository.run_git_command(args, askpass, cx)
            })
        })?;
        receiver.await?
    }

    /// Shows a prompt and returns the index of the chosen answer.
    async fn prompt(
        &self,
        level: PromptLevel,
        message: String,
        detail: Option<String>,
        answers: &[&str],
        cx: &mut AsyncWindowContext,
    ) -> Result<usize> {
        let answer =
            cx.update(|window, cx| window.prompt(level, &message, detail.as_deref(), answers, cx))?;
        Ok(answer.await?)
    }

    /// Commits the changes a `--squash` merge staged, if it staged any.
    async fn commit_squash(&self, message: String, cx: &mut AsyncWindowContext) -> Result<()> {
        let has_staged_changes = self
            .git(git_args!["diff", "--cached", "--quiet"], cx)
            .await
            .is_err();
        if has_staged_changes {
            self.git(git_args!["commit", "-m", message], cx).await?;
        }
        Ok(())
    }

    /// Deletes `branch` on `remote`, removing the stale remote-tracking
    /// branch when the remote no longer has it.
    async fn delete_remote_branch(
        &self,
        remote: &str,
        branch: &str,
        cx: &mut AsyncWindowContext,
    ) -> Result<()> {
        match self
            .git(git_args!["push", remote, "--delete", branch], cx)
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if error.to_string().contains("remote ref does not exist") => {
                self.git(
                    git_args!["branch", "-d", "-r", format!("{remote}/{branch}")],
                    cx,
                )
                .await?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

/// Runs `action`, then shows its success message or its error.
fn run_action<F, Fut>(
    context: &GraphActionContext,
    operation: &'static str,
    window: &mut Window,
    cx: &mut App,
    action: F,
) where
    F: FnOnce(GitRunner, AsyncWindowContext) -> Fut + 'static,
    Fut: Future<Output = Result<Option<String>>> + 'static,
{
    let Some(repository) = context.repository.upgrade() else {
        return;
    };
    let workspace = context.workspace.clone();
    let runner = GitRunner {
        repository,
        workspace: workspace.clone(),
    };
    window
        .spawn(cx, async move |cx| {
            let result = action(runner, cx.clone()).await;
            cx.update(|_, cx| {
                let Some(workspace) = workspace.upgrade() else {
                    return;
                };
                match result {
                    Ok(Some(message)) => workspace.update(cx, |workspace, cx| {
                        let toast = StatusToast::new(message, cx, |this, _| {
                            this.icon(
                                Icon::new(IconName::Check)
                                    .size(IconSize::Small)
                                    .color(Color::Success),
                            )
                        });
                        workspace.toggle_status_toast(toast, cx);
                    }),
                    Ok(None) => {}
                    Err(error) => show_error_toast(workspace, operation, error, cx),
                }
            })
            .log_err();
        })
        .detach();
}

fn open_dialog(
    context: &GraphActionContext,
    title: impl Into<SharedString>,
    message: Option<String>,
    confirm_label: &'static str,
    fields: Vec<GitActionField>,
    on_confirm: impl FnOnce(GitActionValues, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let title = title.into();
    context
        .workspace
        .update(cx, |workspace, cx| {
            GitActionDialog::open(
                workspace,
                title,
                message.map(SharedString::from),
                confirm_label,
                fields,
                on_confirm,
                window,
                cx,
            );
        })
        .log_err();
}

fn copy_entry(menu: ContextMenu, label: &'static str, text: SharedString) -> ContextMenu {
    menu.entry(label, None, move |_, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
    })
}

/// A field picking a remote to push to, or none: a checkbox with one
/// remote, a select with several, nothing without remotes. Read it back with
/// [`chosen_remote`].
fn remote_choice_field(
    key: &'static str,
    remotes: &[SharedString],
    checkbox_label: impl Fn(&SharedString) -> String,
    select_label: &'static str,
    none_option: &'static str,
) -> Option<GitActionField> {
    match remotes {
        [] => None,
        [remote] => Some(GitActionField::checkbox(key, checkbox_label(remote), false)),
        remotes => Some(GitActionField::select(
            key,
            select_label,
            std::iter::once(SharedString::from(none_option))
                .chain(remotes.iter().cloned())
                .collect(),
            0,
        )),
    }
}

fn chosen_remote(
    values: &GitActionValues,
    key: &str,
    remotes: &[SharedString],
) -> Option<SharedString> {
    match remotes {
        [] => None,
        [remote] => values.checked(key).then(|| remote.clone()),
        remotes => values
            .selected(key)
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| remotes.get(index).cloned()),
    }
}

// Commit actions

/// Adds the actions for `commit` to `menu`.
pub(crate) fn add_commit_entries(
    menu: ContextMenu,
    commit: GraphCommit,
    context: GraphActionContext,
) -> ContextMenu {
    let sha = commit.sha;
    let sha_text = SharedString::from(sha.to_string());
    let current = context.current_branch_label();

    let menu = menu
        .entry("Add Tag…", None, {
            let context = context.clone();
            move |window, cx| add_tag(&context, sha, window, cx)
        })
        .entry("Create Branch…", None, {
            let context = context.clone();
            move |window, cx| create_branch(&context, sha, window, cx)
        })
        .separator()
        .entry("Checkout…", None, {
            let context = context.clone();
            move |window, cx| checkout_commit(&context, sha, window, cx)
        })
        .entry("Cherry Pick…", None, {
            let context = context.clone();
            let commit = commit.clone();
            move |window, cx| cherry_pick(&context, &commit, window, cx)
        })
        .entry("Revert…", None, {
            let context = context.clone();
            let commit = commit.clone();
            move |window, cx| revert(&context, &commit, window, cx)
        })
        .when(commit.can_drop, |menu| {
            menu.entry("Edit Commit Message…", None, {
                let context = context.clone();
                move |window, cx| reword_commit(&context, sha, window, cx)
            })
            .entry("Drop…", None, {
                let context = context.clone();
                move |window, cx| drop_commit(&context, sha, window, cx)
            })
        })
        .separator()
        .entry(format!("Merge into {current}…"), None, {
            let context = context.clone();
            move |window, cx| {
                merge(
                    &context,
                    MergeTarget::Commit,
                    sha.to_string().into(),
                    window,
                    cx,
                )
            }
        })
        .entry(format!("Rebase {current} on This Commit…"), None, {
            let context = context.clone();
            move |window, cx| {
                rebase(
                    &context,
                    MergeTarget::Commit,
                    sha.to_string().into(),
                    window,
                    cx,
                )
            }
        })
        .entry(format!("Reset {current} to This Commit…"), None, {
            let context = context.clone();
            move |window, cx| reset(&context, sha, window, cx)
        })
        .separator()
        .entry("View Diff", Some(OpenCommitView.boxed_clone()), {
            move |window, cx| {
                CommitView::open(
                    sha.to_string(),
                    context.repository.clone(),
                    context.workspace.clone(),
                    None,
                    None,
                    window,
                    cx,
                );
            }
        });
    let menu = copy_entry(menu, "Copy Commit Hash", sha_text);
    match commit.subject {
        Some(subject) => copy_entry(menu, "Copy Commit Subject", subject),
        // The subject is still loading.
        None => menu.item(ContextMenuEntry::new("Copy Commit Subject").disabled(true)),
    }
}

fn add_tag(context: &GraphActionContext, sha: Oid, window: &mut Window, cx: &mut App) {
    let remotes = context.remotes.clone();
    let mut fields = vec![
        GitActionField::required_text("name", "Name", ""),
        GitActionField::select(
            "type",
            "Type",
            vec!["Annotated".into(), "Lightweight".into()],
            0,
        ),
        GitActionField::optional_text("message", "Message", "Optional")
            .info("A message can only be added to an annotated tag."),
    ];
    fields.extend(remote_choice_field(
        "push",
        &remotes,
        |remote| format!("Push to remote {remote}"),
        "Push to remote",
        "Don't push",
    ));
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Add Tag to Commit {}", sha.display_short()),
        None,
        "Add Tag",
        fields,
        move |values, window, cx| {
            let name = values.text("name");
            let annotated = values.selected("type") == Some(0);
            let message = values.text("message");
            let push_remote = chosen_remote(&values, "push", &remotes);
            run_action(
                &context_for_action,
                "tag",
                window,
                cx,
                async move |git, mut cx| {
                    let tag_args = |force: bool| {
                        let mut args = git_args!["tag"];
                        if force {
                            args.push("-f".into());
                        }
                        if annotated {
                            args.extend(git_args!["-a", name, "-m", message]);
                        } else {
                            args.push(name.clone());
                        }
                        args.push(sha.to_string());
                        args
                    };
                    if let Err(error) = git.git(tag_args(false), &mut cx).await {
                        if !error.to_string().contains("already exists") {
                            return Err(error);
                        }
                        let answer = git
                            .prompt(
                                PromptLevel::Warning,
                                format!("A tag named \"{name}\" already exists."),
                                Some("Do you want to replace it?".into()),
                                &["Replace Existing Tag", "Cancel"],
                                &mut cx,
                            )
                            .await?;
                        if answer != 0 {
                            return Ok(None);
                        }
                        git.git(tag_args(true), &mut cx).await?;
                    }
                    if let Some(remote) = &push_remote {
                        git.git(
                            git_args!["push", remote, format!("refs/tags/{name}")],
                            &mut cx,
                        )
                        .await?;
                        return Ok(Some(format!("Added tag {name} and pushed it to {remote}")));
                    }
                    Ok(Some(format!("Added tag {name}")))
                },
            );
        },
        window,
        cx,
    );
}

fn create_branch(context: &GraphActionContext, sha: Oid, window: &mut Window, cx: &mut App) {
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Create Branch at Commit {}", sha.display_short()),
        None,
        "Create Branch",
        vec![
            GitActionField::required_text("name", "Name", ""),
            GitActionField::checkbox("checkout", "Check out", false),
        ],
        move |values, window, cx| {
            let name = values.text("name");
            let checkout = values.checked("checkout");
            run_action(
                &context_for_action,
                "branch",
                window,
                cx,
                async move |git, mut cx| {
                    let args = if checkout {
                        git_args!["checkout", "-b", name, sha]
                    } else {
                        git_args!["branch", name, sha]
                    };
                    if let Err(error) = git.git(args, &mut cx).await {
                        if !error.to_string().contains("already exists") {
                            return Err(error);
                        }
                        let answer = git
                            .prompt(
                                PromptLevel::Warning,
                                format!("A branch named \"{name}\" already exists."),
                                Some("Do you want to replace it?".into()),
                                &["Replace Existing Branch", "Cancel"],
                                &mut cx,
                            )
                            .await?;
                        if answer != 0 {
                            return Ok(None);
                        }
                        git.git(git_args!["branch", "-f", name, sha], &mut cx)
                            .await?;
                        if checkout {
                            git.git(git_args!["checkout", name], &mut cx).await?;
                        }
                    }
                    Ok(Some(format!("Created branch {name}")))
                },
            );
        },
        window,
        cx,
    );
}

fn checkout_commit(context: &GraphActionContext, sha: Oid, window: &mut Window, cx: &mut App) {
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Checkout Commit {}", sha.display_short()),
        Some("This will result in a 'detached HEAD' state.".into()),
        "Checkout",
        Vec::new(),
        move |_, window, cx| {
            run_action(
                &context_for_action,
                "checkout",
                window,
                cx,
                async move |git, mut cx| {
                    git.git(git_args!["checkout", sha], &mut cx).await?;
                    Ok(None)
                },
            );
        },
        window,
        cx,
    );
}

/// A select of the parent to diff a merge commit against, for merge commits.
fn merge_parent_field(commit: &GraphCommit) -> Option<GitActionField> {
    commit.is_merge().then(|| {
        GitActionField::select("parent", "Parent", commit.parent_options(), 0)
            .info("Choose the parent on the main branch, to cherry pick or revert the changes relative to it.")
    })
}

/// The `-m <parent>` arguments for the parent chosen in a merge commit's dialog.
fn merge_parent_args(commit: &GraphCommit, values: &GitActionValues) -> Vec<String> {
    if !commit.is_merge() {
        return Vec::new();
    }
    let parent = values.selected("parent").unwrap_or(0) + 1;
    git_args!["-m", parent]
}

fn cherry_pick(
    context: &GraphActionContext,
    commit: &GraphCommit,
    window: &mut Window,
    cx: &mut App,
) {
    let mut fields: Vec<GitActionField> = merge_parent_field(commit).into_iter().collect();
    fields.push(
        GitActionField::checkbox("record_origin", "Record Origin", false)
            .info("Record that this commit was the origin of the cherry pick by appending a line to the original commit message."),
    );
    fields.push(
        GitActionField::checkbox("no_commit", "No Commit", false)
            .info("Cherry picked changes will be staged but not committed, so that you can select and commit specific parts of this commit."),
    );
    let context_for_action = context.clone();
    let commit = commit.clone();
    open_dialog(
        context,
        format!("Cherry Pick Commit {}", commit.sha.display_short()),
        None,
        "Cherry Pick",
        fields,
        move |values, window, cx| {
            let mut args = git_args!["cherry-pick"];
            if values.checked("no_commit") {
                args.push("--no-commit".into());
            }
            if values.checked("record_origin") {
                args.push("-x".into());
            }
            args.extend(merge_parent_args(&commit, &values));
            args.push(commit.sha.to_string());
            run_action(
                &context_for_action,
                "cherry-pick",
                window,
                cx,
                async move |git, mut cx| {
                    git.git(args, &mut cx).await?;
                    Ok(None)
                },
            );
        },
        window,
        cx,
    );
}

fn revert(context: &GraphActionContext, commit: &GraphCommit, window: &mut Window, cx: &mut App) {
    let context_for_action = context.clone();
    let fields = merge_parent_field(commit).into_iter().collect();
    let commit = commit.clone();
    open_dialog(
        context,
        format!("Revert Commit {}", commit.sha.display_short()),
        None,
        "Revert",
        fields,
        move |values, window, cx| {
            let mut args = git_args!["revert", "--no-edit"];
            args.extend(merge_parent_args(&commit, &values));
            args.push(commit.sha.to_string());
            run_action(
                &context_for_action,
                "revert",
                window,
                cx,
                async move |git, mut cx| {
                    git.git(args, &mut cx).await?;
                    Ok(None)
                },
            );
        },
        window,
        cx,
    );
}

fn drop_commit(context: &GraphActionContext, sha: Oid, window: &mut Window, cx: &mut App) {
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Drop Commit {}", sha.display_short()),
        Some("The commit will be permanently removed from the current branch.".into()),
        "Drop",
        Vec::new(),
        move |_, window, cx| {
            run_action(
                &context_for_action,
                "rebase",
                window,
                cx,
                async move |git, mut cx| {
                    git.git(
                        git_args!["rebase", "--onto", format!("{sha}^"), sha],
                        &mut cx,
                    )
                    .await?;
                    Ok(None)
                },
            );
        },
        window,
        cx,
    );
}

/// Replaces the message of `sha` with an `amend!` commit that an
/// autosquash rebase folds into it, keeping its author, date and changes.
fn reword_commit(context: &GraphActionContext, sha: Oid, window: &mut Window, cx: &mut App) {
    let Some(repository) = context.repository.upgrade() else {
        return;
    };
    let commit_data = repository.update(cx, |repository, cx| {
        repository.fetch_commit_data(sha, true, cx).clone()
    });
    let context = context.clone();
    window
        .spawn(cx, async move |cx| {
            let message = match commit_data {
                CommitDataState::Loaded(data) => data.message.clone(),
                CommitDataState::Loading(Some(receiver)) => receiver.await?.message.clone(),
                CommitDataState::Loading(None) => anyhow::bail!("commit {sha} was not found"),
            };
            cx.update(|window, cx| {
                let context_for_action = context.clone();
                open_dialog(
                    &context,
                    format!("Edit Commit Message {}", sha.display_short()),
                    Some("The commits after it are rebased, so they get new hashes.".into()),
                    "Save",
                    vec![GitActionField::required_message(
                        "message",
                        "Message",
                        message.trim_end().to_string(),
                    )],
                    move |values, window, cx| {
                        let message = values.text("message");
                        run_action(
                            &context_for_action,
                            "rebase",
                            window,
                            cx,
                            async move |git, mut cx| {
                                git.git(
                                    git_args![
                                        "commit",
                                        "--no-verify",
                                        "--allow-empty",
                                        "--only",
                                        "-m",
                                        format!("amend! {sha}\n\n{message}")
                                    ],
                                    &mut cx,
                                )
                                .await?;

                                git.git(
                                    git_args![
                                        "-c",
                                        "sequence.editor=:",
                                        "rebase",
                                        "--interactive",
                                        "--autosquash",
                                        "--autostash",
                                        format!("{sha}^")
                                    ],
                                    &mut cx,
                                )
                                .await?;
                                Ok(Some("Commit message updated".into()))
                            },
                        );
                    },
                    window,
                    cx,
                );
            })
        })
        .detach_and_log_err(cx);
}

fn reset(context: &GraphActionContext, sha: Oid, window: &mut Window, cx: &mut App) {
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!(
            "Reset {} to Commit {}",
            context.current_branch_label(),
            sha.display_short()
        ),
        None,
        "Reset",
        vec![GitActionField::select(
            "mode",
            "Mode",
            vec![
                "Soft - Keep all changes, but reset head".into(),
                "Mixed - Keep working tree, but reset index".into(),
                "Hard - Discard all changes".into(),
            ],
            1,
        )],
        move |values, window, cx| {
            let mode = match values.selected("mode") {
                Some(0) => "--soft",
                Some(2) => "--hard",
                _ => "--mixed",
            };
            run_action(
                &context_for_action,
                "reset",
                window,
                cx,
                async move |git, mut cx| {
                    git.git(git_args!["reset", mode, sha], &mut cx).await?;
                    Ok(None)
                },
            );
        },
        window,
        cx,
    );
}

// Merge and rebase

#[derive(Clone, Copy)]
enum MergeTarget {
    Branch,
    RemoteBranch,
    Commit,
}

impl MergeTarget {
    fn noun(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::RemoteBranch => "remote-tracking branch",
            Self::Commit => "commit",
        }
    }
}

fn merge(
    context: &GraphActionContext,
    target: MergeTarget,
    name: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let current = context.current_branch_label();
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Merge into {current}"),
        Some(format!(
            "Merge {} {name} into {current} (the current branch).",
            target.noun()
        )),
        "Merge",
        vec![
            GitActionField::checkbox(
                "no_ff",
                "Create a new commit even if fast-forward is possible",
                true,
            ),
            GitActionField::checkbox("squash", "Squash Commits", false),
            GitActionField::checkbox("no_commit", "No Commit", false)
                .info("Changes will be staged but not committed, so that you can review and/or modify the result of the merge before committing."),
        ],
        move |values, window, cx| {
            let squash = values.checked("squash");
            let no_commit = values.checked("no_commit");
            let mut args = git_args!["merge", name];
            if squash {
                args.push("--squash".into());
            } else if values.checked("no_ff") {
                args.push("--no-ff".into());
            }
            if no_commit {
                args.push("--no-commit".into());
            } else {
                args.push("--no-edit".into());
            }
            run_action(
                &context_for_action,
                "merge",
                window,
                cx,
                async move |git, mut cx| {
                    git.git(args, &mut cx).await?;
                    if squash && !no_commit {
                        git.commit_squash(format!("Merge {} '{name}'", target.noun()), &mut cx)
                            .await?;
                    }
                    Ok(Some(format!("Merged {name} into {current}")))
                },
            );
        },
        window,
        cx,
    );
}

fn rebase(
    context: &GraphActionContext,
    target: MergeTarget,
    name: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let current = context.current_branch_label();
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Rebase {current}"),
        Some(format!(
            "Rebase {current} (the current branch) on {} {name}.",
            target.noun()
        )),
        "Rebase",
        vec![
            GitActionField::checkbox("ignore_date", "Ignore Date", true)
                .info("The committer date is used as the author date of the rebased commits."),
        ],
        move |values, window, cx| {
            let mut args = git_args!["rebase", name];
            if values.checked("ignore_date") {
                args.push("--ignore-date".into());
            }
            run_action(
                &context_for_action,
                "rebase",
                window,
                cx,
                async move |git, mut cx| {
                    git.git(args, &mut cx).await?;
                    Ok(Some(format!("Rebased {current} on {name}")))
                },
            );
        },
        window,
        cx,
    );
}

// Ref actions

/// Adds the actions for `graph_ref` to `menu`.
pub(crate) fn add_ref_entries(
    menu: ContextMenu,
    graph_ref: GraphRef,
    sha: Oid,
    context: GraphActionContext,
) -> ContextMenu {
    match graph_ref {
        GraphRef::LocalBranch { name, is_head } => {
            add_local_branch_entries(menu, name, is_head, context)
        }
        GraphRef::RemoteBranch { remote, branch } => {
            add_remote_branch_entries(menu, remote, branch, context)
        }
        GraphRef::Tag { name } => add_tag_entries(menu, name, sha, context),
    }
}

fn add_local_branch_entries(
    menu: ContextMenu,
    name: SharedString,
    is_head: bool,
    context: GraphActionContext,
) -> ContextMenu {
    let current = context.current_branch_label();
    let menu = menu
        .when(!is_head, |menu| {
            menu.entry("Checkout Branch", None, {
                let context = context.clone();
                let name = name.clone();
                move |window, cx| {
                    let name = name.clone();
                    run_action(
                        &context,
                        "checkout",
                        window,
                        cx,
                        async move |git, mut cx| {
                            git.git(git_args!["checkout", name], &mut cx).await?;
                            Ok(None)
                        },
                    );
                }
            })
        })
        .entry("Rename Branch…", None, {
            let context = context.clone();
            let name = name.clone();
            move |window, cx| rename_branch(&context, name.clone(), window, cx)
        })
        .when(!is_head, |menu| {
            menu.entry("Delete Branch…", None, {
                let context = context.clone();
                let name = name.clone();
                move |window, cx| delete_branch(&context, name.clone(), window, cx)
            })
            .entry(format!("Merge into {current}…"), None, {
                let context = context.clone();
                let name = name.clone();
                move |window, cx| merge(&context, MergeTarget::Branch, name.clone(), window, cx)
            })
            .entry(format!("Rebase {current} on Branch…"), None, {
                let context = context.clone();
                let name = name.clone();
                move |window, cx| rebase(&context, MergeTarget::Branch, name.clone(), window, cx)
            })
        })
        .when(!context.remotes.is_empty(), |menu| {
            menu.entry("Push Branch…", None, {
                let context = context.clone();
                let name = name.clone();
                move |window, cx| push_branch(&context, name.clone(), window, cx)
            })
        })
        .separator();
    copy_entry(menu, "Copy Branch Name", name)
}

fn rename_branch(
    context: &GraphActionContext,
    name: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Rename Branch {name}"),
        None,
        "Rename Branch",
        vec![GitActionField::required_text(
            "name",
            "New Name",
            name.clone(),
        )],
        move |values, window, cx| {
            let new_name = values.text("name");
            if new_name == name.as_ref() {
                return;
            }
            run_action(
                &context_for_action,
                "branch",
                window,
                cx,
                async move |git, mut cx| {
                    git.git(git_args!["branch", "-m", name, new_name], &mut cx)
                        .await?;
                    Ok(Some(format!("Renamed branch {name} to {new_name}")))
                },
            );
        },
        window,
        cx,
    );
}

fn delete_branch(
    context: &GraphActionContext,
    name: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let remotes = context.remotes_with_branch(&name);
    let mut fields = vec![GitActionField::checkbox("force", "Force Delete", false)];
    if !remotes.is_empty() {
        let remote_list = remotes
            .iter()
            .map(|remote| format!("\"{remote}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let label = if remotes.len() > 1 {
            "Delete this branch on the remotes"
        } else {
            "Delete this branch on the remote"
        };
        fields.push(
            GitActionField::checkbox("delete_on_remote", label, false)
                .info(format!("This branch is on the remote(s): {remote_list}")),
        );
    }
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Delete Branch {name}"),
        Some(format!(
            "Are you sure you want to delete the branch {name}?"
        )),
        "Delete",
        fields,
        move |values, window, cx| {
            let force = values.checked("force");
            let delete_on_remotes = if values.checked("delete_on_remote") {
                remotes
            } else {
                Vec::new()
            };
            run_action(
                &context_for_action,
                "branch",
                window,
                cx,
                async move |git, mut cx| {
                    let flag = if force { "-D" } else { "-d" };
                    if let Err(error) = git.git(git_args!["branch", flag, name], &mut cx).await {
                        let Some(prompt) = (!force)
                            .then(|| force_delete_prompt_for_branch_delete_error(&error, &name))
                            .flatten()
                        else {
                            return Err(error);
                        };
                        let answer = git
                            .prompt(
                                PromptLevel::Warning,
                                prompt,
                                None,
                                &["Force Delete", "Cancel"],
                                &mut cx,
                            )
                            .await?;
                        if answer != 0 {
                            return Ok(None);
                        }
                        git.git(git_args!["branch", "-D", name], &mut cx).await?;
                    }
                    for remote in &delete_on_remotes {
                        git.delete_remote_branch(remote, &name, &mut cx).await?;
                    }
                    Ok(Some(format!("Deleted branch {name}")))
                },
            );
        },
        window,
        cx,
    );
}

fn push_branch(
    context: &GraphActionContext,
    name: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let remotes = context.remotes.clone();
    let mut fields = Vec::new();
    if remotes.len() > 1 {
        fields.push(GitActionField::select(
            "remote",
            "Push to Remote",
            remotes.clone(),
            context.default_push_remote(Some(&name), cx),
        ));
    }
    fields.push(GitActionField::checkbox(
        "set_upstream",
        "Set Upstream",
        true,
    ));
    fields.push(GitActionField::select(
        "mode",
        "Push Mode",
        vec!["Normal".into(), "Force With Lease".into(), "Force".into()],
        0,
    ));
    let message = match remotes.as_slice() {
        [remote] => format!("Push the branch {name} to the remote {remote}."),
        _ => format!("Push the branch {name}."),
    };
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Push Branch {name}"),
        Some(message),
        "Push",
        fields,
        move |values, window, cx| {
            let Some(remote) = remotes.get(values.selected("remote").unwrap_or(0)).cloned() else {
                return;
            };
            let mut args = git_args!["push", remote, name];
            if values.checked("set_upstream") {
                args.push("--set-upstream".into());
            }
            match values.selected("mode") {
                Some(1) => args.push("--force-with-lease".into()),
                Some(2) => args.push("--force".into()),
                _ => {}
            }
            run_action(
                &context_for_action,
                "push",
                window,
                cx,
                async move |git, mut cx| {
                    git.git(args, &mut cx).await?;
                    Ok(Some(format!("Pushed {name} to {remote}")))
                },
            );
        },
        window,
        cx,
    );
}

fn add_remote_branch_entries(
    menu: ContextMenu,
    remote: SharedString,
    branch: SharedString,
    context: GraphActionContext,
) -> ContextMenu {
    let full_name = SharedString::from(format!("{remote}/{branch}"));
    let current = context.current_branch_label();
    let can_fetch_into_local =
        context.has_local_branch(&branch) && context.head_branch.as_ref() != Some(&branch);
    let menu = menu
        .entry("Checkout Branch…", None, {
            let context = context.clone();
            let remote = remote.clone();
            let branch = branch.clone();
            move |window, cx| {
                checkout_remote_branch(&context, remote.clone(), branch.clone(), window, cx)
            }
        })
        .entry("Delete Remote Branch…", None, {
            let context = context.clone();
            let remote = remote.clone();
            let branch = branch.clone();
            move |window, cx| {
                delete_remote_branch(&context, remote.clone(), branch.clone(), window, cx)
            }
        })
        .when(can_fetch_into_local, |menu| {
            menu.entry("Fetch into Local Branch…", None, {
                let context = context.clone();
                let remote = remote.clone();
                let branch = branch.clone();
                move |window, cx| {
                    fetch_into_local_branch(&context, remote.clone(), branch.clone(), window, cx)
                }
            })
        })
        .entry(format!("Merge into {current}…"), None, {
            let context = context.clone();
            let full_name = full_name.clone();
            move |window, cx| {
                merge(
                    &context,
                    MergeTarget::RemoteBranch,
                    full_name.clone(),
                    window,
                    cx,
                )
            }
        })
        .entry(format!("Pull into {current}…"), None, {
            move |window, cx| pull_branch(&context, remote.clone(), branch.clone(), window, cx)
        })
        .separator();
    copy_entry(menu, "Copy Branch Name", full_name)
}

fn checkout_remote_branch(
    context: &GraphActionContext,
    remote: SharedString,
    branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let context_for_action = context.clone();
    let local_branches = context.local_branches.clone();
    open_dialog(
        context,
        format!("Checkout Branch {remote}/{branch}"),
        Some(format!(
            "Enter the name of the new branch to create when checking out {remote}/{branch}."
        )),
        "Checkout Branch",
        vec![GitActionField::required_text(
            "name",
            "Name",
            branch.clone(),
        )],
        move |values, window, cx| {
            let name = values.text("name");
            let local_branches = local_branches
                .iter()
                .map(|local| local.as_ref())
                .collect::<Vec<_>>()
                .join("\n");
            run_action(
                &context_for_action,
                "checkout",
                window,
                cx,
                async move |git, mut cx| {
                    // On case-insensitive filesystems `Feat/x` is the existing
                    // `feat/x`, so use the real name of the local branch.
                    let ignore_case = git
                        .git(
                            git_args!["config", "--type=bool", "core.ignorecase"],
                            &mut cx,
                        )
                        .await
                        .is_ok_and(|output| output.stdout.trim() == "true");
                    let Some(name) =
                        find_local_branch(&local_branches, &name, ignore_case).map(str::to_owned)
                    else {
                        git.git(
                            git_args!["checkout", "-b", name, format!("{remote}/{branch}")],
                            &mut cx,
                        )
                        .await?;
                        return Ok(None);
                    };
                    let answer = git
                        .prompt(
                            PromptLevel::Info,
                            format!("A branch named \"{name}\" already exists."),
                            Some(format!(
                                "Do you want to check it out and pull the changes from {remote}/{branch}?"
                            )),
                            &["Checkout and Pull", "Cancel"],
                            &mut cx,
                        )
                        .await?;
                    if answer != 0 {
                        return Ok(None);
                    }
                    git.git(git_args!["checkout", name], &mut cx).await?;
                    git.git(git_args!["pull", remote, branch, "--no-edit"], &mut cx)
                        .await?;
                    Ok(None)
                },
            );
        },
        window,
        cx,
    );
}

fn delete_remote_branch(
    context: &GraphActionContext,
    remote: SharedString,
    branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Delete Remote Branch {remote}/{branch}"),
        Some(format!(
            "Are you sure you want to delete the remote branch {remote}/{branch}? This deletes it on {remote} for everyone."
        )),
        "Delete",
        Vec::new(),
        move |_, window, cx| {
            run_action(
                &context_for_action,
                "push",
                window,
                cx,
                async move |git, mut cx| {
                    git.delete_remote_branch(&remote, &branch, &mut cx).await?;
                    Ok(Some(format!("Deleted remote branch {remote}/{branch}")))
                },
            );
        },
        window,
        cx,
    );
}

fn fetch_into_local_branch(
    context: &GraphActionContext,
    remote: SharedString,
    branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Fetch into Local Branch {branch}"),
        Some(format!(
            "Fetch the remote branch {remote}/{branch} into the local branch {branch}."
        )),
        "Fetch",
        vec![
            GitActionField::checkbox("force", "Force Fetch", false)
                .info("Force the local branch to be reset to this remote branch."),
        ],
        move |values, window, cx| {
            let mut args = git_args!["fetch"];
            if values.checked("force") {
                args.push("-f".into());
            }
            args.extend(git_args![remote, format!("{branch}:{branch}")]);
            run_action(
                &context_for_action,
                "fetch",
                window,
                cx,
                async move |git, mut cx| {
                    git.git(args, &mut cx).await?;
                    Ok(Some(format!("Fetched {remote}/{branch} into {branch}")))
                },
            );
        },
        window,
        cx,
    );
}

fn pull_branch(
    context: &GraphActionContext,
    remote: SharedString,
    branch: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    let current = context.current_branch_label();
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Pull into {current}"),
        Some(format!(
            "Pull the remote branch {remote}/{branch} into {current} (the current branch). If a merge is required:"
        )),
        "Pull",
        vec![
            GitActionField::checkbox(
                "no_ff",
                "Create a new commit even if fast-forward is possible",
                false,
            ),
            GitActionField::checkbox("squash", "Squash Commits", false),
        ],
        move |values, window, cx| {
            let squash = values.checked("squash");
            let mut args = git_args!["pull", remote, branch];
            if squash {
                args.push("--squash".into());
            } else if values.checked("no_ff") {
                args.push("--no-ff".into());
            }
            args.push("--no-edit".into());
            run_action(
                &context_for_action,
                "pull",
                window,
                cx,
                async move |git, mut cx| {
                    git.git(args, &mut cx).await?;
                    if squash {
                        git.commit_squash(format!("Merge branch '{remote}/{branch}'"), &mut cx)
                            .await?;
                    }
                    Ok(Some(format!("Pulled {remote}/{branch} into {current}")))
                },
            );
        },
        window,
        cx,
    );
}

fn add_tag_entries(
    menu: ContextMenu,
    name: SharedString,
    sha: Oid,
    context: GraphActionContext,
) -> ContextMenu {
    let menu = menu
        .entry("View Details", None, {
            let context = context.clone();
            let name = name.clone();
            move |window, cx| view_tag_details(&context, name.clone(), window, cx)
        })
        .entry("Delete Tag…", None, {
            let context = context.clone();
            let name = name.clone();
            move |window, cx| delete_tag(&context, name.clone(), window, cx)
        })
        .when(!context.remotes.is_empty(), |menu| {
            menu.entry("Push Tag…", None, {
                let context = context.clone();
                let name = name.clone();
                move |window, cx| push_tag(&context, name.clone(), sha, window, cx)
            })
        })
        .separator();
    copy_entry(menu, "Copy Tag Name", name)
}

fn view_tag_details(
    context: &GraphActionContext,
    name: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    run_action(context, "tag", window, cx, async move |git, mut cx| {
        let output = git
            .git(
                git_args![
                    "for-each-ref",
                    format!("refs/tags/{name}"),
                    "--format=%(objecttype)%00%(*objectname)%(objectname)%00%(taggername) %(taggeremail)%00%(taggerdate:iso)%00%(contents)",
                ],
                &mut cx,
            )
            .await?;
        let mut parts = output.stdout.splitn(5, '\0');
        let object_type = parts.next().unwrap_or_default().trim();
        let detail = if object_type == "tag" {
            let commit = parts.next().unwrap_or_default();
            let tagger = parts.next().unwrap_or_default();
            let date = parts.next().unwrap_or_default();
            let message = parts.next().unwrap_or_default().trim();
            format!("Commit: {commit}\nTagger: {tagger}\nDate: {date}\n\n{message}")
        } else {
            "This is a lightweight tag, which has no details.".to_string()
        };
        git.prompt(
            PromptLevel::Info,
            format!("Tag {name}"),
            Some(detail),
            &["OK"],
            &mut cx,
        )
        .await?;
        Ok(None)
    });
}

fn delete_tag(context: &GraphActionContext, name: SharedString, window: &mut Window, cx: &mut App) {
    let remotes = context.remotes.clone();
    let fields = remote_choice_field(
        "remote",
        &remotes,
        |remote| format!("Also delete on remote {remote}"),
        "Also delete on remote",
        "Don't delete on any remote",
    )
    .into_iter()
    .collect();
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Delete Tag {name}"),
        Some(format!("Are you sure you want to delete the tag {name}?")),
        "Delete",
        fields,
        move |values, window, cx| {
            let remote = chosen_remote(&values, "remote", &remotes);
            run_action(
                &context_for_action,
                "tag",
                window,
                cx,
                async move |git, mut cx| {
                    if let Some(remote) = &remote {
                        git.git(
                            git_args!["push", remote, "--delete", format!("refs/tags/{name}")],
                            &mut cx,
                        )
                        .await?;
                    }
                    git.git(git_args!["tag", "-d", name], &mut cx).await?;
                    Ok(Some(match remote {
                        Some(remote) => format!("Deleted tag {name} locally and on {remote}"),
                        None => format!("Deleted tag {name}"),
                    }))
                },
            );
        },
        window,
        cx,
    );
}

fn push_tag(
    context: &GraphActionContext,
    name: SharedString,
    sha: Oid,
    window: &mut Window,
    cx: &mut App,
) {
    let remotes = context.remotes.clone();
    let (message, fields) = match remotes.as_slice() {
        [remote] => (
            format!("Push the tag {name} to the remote {remote}."),
            Vec::new(),
        ),
        _ => (
            format!("Push the tag {name}."),
            vec![GitActionField::select(
                "remote",
                "Push to Remote",
                remotes.clone(),
                context.default_push_remote(None, cx),
            )],
        ),
    };
    let context_for_action = context.clone();
    open_dialog(
        context,
        format!("Push Tag {name}"),
        Some(message),
        "Push",
        fields,
        move |values, window, cx| {
            let Some(remote) = remotes.get(values.selected("remote").unwrap_or(0)).cloned() else {
                return;
            };
            run_action(
                &context_for_action,
                "push",
                window,
                cx,
                async move |git, mut cx| {
                    let containing_branches = git
                        .git(
                            git_args!["branch", "-r", "--no-color", "--contains", sha],
                            &mut cx,
                        )
                        .await?;
                    let remote_prefix = format!("{remote}/");
                    let commit_on_remote = containing_branches
                        .stdout
                        .lines()
                        .any(|line| line.trim().starts_with(&remote_prefix));
                    if !commit_on_remote {
                        let answer = git
                            .prompt(
                                PromptLevel::Warning,
                                format!("Commit {} is not on {remote}.", sha.display_short()),
                                Some(format!(
                                    "No branch on {remote} contains the commit the tag {name} points at. Pushing the tag will also push this commit."
                                )),
                                &["Proceed to Push", "Cancel"],
                                &mut cx,
                            )
                            .await?;
                        if answer != 0 {
                            return Ok(None);
                        }
                    }
                    git.git(
                        git_args!["push", remote, format!("refs/tags/{name}")],
                        &mut cx,
                    )
                    .await?;
                    Ok(Some(format!("Pushed tag {name} to {remote}")))
                },
            );
        },
        window,
        cx,
    );
}

/// Whether `sha` can be dropped by rebasing the current branch: it and every
/// commit after it up to `head` have one parent, and only the next commit on
/// that path as a child.
pub(crate) fn can_drop_commit(
    sha: Oid,
    head: Oid,
    parents_of: impl Fn(Oid) -> Option<Vec<Oid>>,
    child_count: impl Fn(Oid) -> usize,
) -> bool {
    let mut current = head;
    loop {
        let Some(parents) = parents_of(current) else {
            return false;
        };
        if parents.len() != 1 || (current != head && child_count(current) != 1) {
            return false;
        }
        if current == sha {
            return true;
        }
        current = parents[0];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_graph_ref_from_decoration() {
        let remotes = [SharedString::from("origin")];
        assert_eq!(
            GraphRef::from_decoration("tag: v1.0", Some("main"), &remotes),
            Some(GraphRef::Tag {
                name: "v1.0".into()
            })
        );
        assert_eq!(
            GraphRef::from_decoration("HEAD -> main", Some("main"), &remotes),
            Some(GraphRef::LocalBranch {
                name: "main".into(),
                is_head: true,
            })
        );
        assert_eq!(
            GraphRef::from_decoration("fix/login", Some("main"), &remotes),
            Some(GraphRef::LocalBranch {
                name: "fix/login".into(),
                is_head: false,
            })
        );
        assert_eq!(
            GraphRef::from_decoration("origin/fix/login", Some("main"), &remotes),
            Some(GraphRef::RemoteBranch {
                remote: "origin".into(),
                branch: "fix/login".into(),
            })
        );
        assert_eq!(
            GraphRef::from_decoration("origin/HEAD", None, &remotes),
            None
        );
        assert_eq!(GraphRef::from_decoration("HEAD", None, &remotes), None);
    }

    #[test]
    fn test_can_drop_commit() {
        let oid = |n: u8| Oid::try_from(format!("{n:040}").as_str()).unwrap();
        // 4 is a merge of 3 and 5; 3 -> 2 -> 1; 6 is another child of 1.
        let parents = |sha: Oid| -> Option<Vec<Oid>> {
            Some(match sha {
                sha if sha == oid(4) => vec![oid(3), oid(5)],
                sha if sha == oid(3) => vec![oid(2)],
                sha if sha == oid(2) => vec![oid(1)],
                sha if sha == oid(6) => vec![oid(1)],
                sha if sha == oid(5) => vec![oid(1)],
                sha if sha == oid(1) => vec![],
                _ => return None,
            })
        };
        let children = |sha: Oid| {
            [2, 3, 4, 5, 6]
                .into_iter()
                .filter(|n| parents(oid(*n)).unwrap().contains(&sha))
                .count()
        };

        assert!(can_drop_commit(oid(2), oid(3), parents, children));
        assert!(can_drop_commit(oid(3), oid(3), parents, children));
        // The root commit has no parent to rebase onto.
        assert!(!can_drop_commit(oid(1), oid(2), parents, children));
        // A merge commit is on the path to HEAD.
        assert!(!can_drop_commit(oid(3), oid(4), parents, children));
        // The commit isn't an ancestor of HEAD.
        assert!(!can_drop_commit(oid(6), oid(3), parents, children));
    }
}
