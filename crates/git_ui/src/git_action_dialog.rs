use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Window,
};
use menu::{Cancel, Confirm};
use ui::{Checkbox, ContextMenu, DropdownMenu, DropdownStyle, ToggleState, prelude::*};
use workspace::{ModalView, Workspace};

/// A field of a [`GitActionDialog`], identified by `key` when reading its
/// value back.
pub(crate) struct GitActionField {
    key: &'static str,
    label: SharedString,
    info: Option<SharedString>,
    kind: GitActionFieldKind,
}

enum GitActionFieldKind {
    Checkbox {
        checked: bool,
    },
    Text {
        initial_text: SharedString,
        placeholder: SharedString,
        required: bool,
        multi_line: bool,
    },
    Select {
        options: Vec<SharedString>,
        selected: usize,
    },
}

impl GitActionField {
    pub(crate) fn checkbox(
        key: &'static str,
        label: impl Into<SharedString>,
        checked: bool,
    ) -> Self {
        Self {
            key,
            label: label.into(),
            info: None,
            kind: GitActionFieldKind::Checkbox { checked },
        }
    }

    /// A text input that must not be empty to confirm the dialog.
    pub(crate) fn required_text(
        key: &'static str,
        label: impl Into<SharedString>,
        initial_text: impl Into<SharedString>,
    ) -> Self {
        Self {
            key,
            label: label.into(),
            info: None,
            kind: GitActionFieldKind::Text {
                initial_text: initial_text.into(),
                placeholder: SharedString::default(),
                required: true,
                multi_line: false,
            },
        }
    }

    /// A multi-line text input, such as a commit message, that must not be empty.
    pub(crate) fn required_message(
        key: &'static str,
        label: impl Into<SharedString>,
        initial_text: impl Into<SharedString>,
    ) -> Self {
        Self {
            key,
            label: label.into(),
            info: None,
            kind: GitActionFieldKind::Text {
                initial_text: initial_text.into(),
                placeholder: SharedString::default(),
                required: true,
                multi_line: true,
            },
        }
    }

    pub(crate) fn optional_text(
        key: &'static str,
        label: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
    ) -> Self {
        Self {
            key,
            label: label.into(),
            info: None,
            kind: GitActionFieldKind::Text {
                initial_text: SharedString::default(),
                placeholder: placeholder.into(),
                required: false,
                multi_line: false,
            },
        }
    }

    pub(crate) fn select(
        key: &'static str,
        label: impl Into<SharedString>,
        options: Vec<SharedString>,
        selected: usize,
    ) -> Self {
        Self {
            key,
            label: label.into(),
            info: None,
            kind: GitActionFieldKind::Select {
                selected: selected.min(options.len().saturating_sub(1)),
                options,
            },
        }
    }

    pub(crate) fn info(mut self, info: impl Into<SharedString>) -> Self {
        self.info = Some(info.into());
        self
    }
}

enum FieldState {
    Checkbox(bool),
    Text {
        editor: Entity<Editor>,
        required: bool,
    },
    Select {
        options: Vec<SharedString>,
        selected: usize,
    },
}

/// The values of a confirmed [`GitActionDialog`], read by field key.
pub(crate) struct GitActionValues {
    values: Vec<(&'static str, GitActionValue)>,
}

enum GitActionValue {
    Bool(bool),
    Text(String),
    Index(usize),
}

impl GitActionValues {
    fn value(&self, key: &str) -> Option<&GitActionValue> {
        self.values
            .iter()
            .find(|(field_key, _)| *field_key == key)
            .map(|(_, value)| value)
    }

    /// Whether the checkbox `key` is checked; `false` when the dialog has no
    /// such checkbox.
    pub(crate) fn checked(&self, key: &str) -> bool {
        matches!(self.value(key), Some(GitActionValue::Bool(true)))
    }

    /// The trimmed text of the input `key`; empty when there is no such input.
    pub(crate) fn text(&self, key: &str) -> String {
        match self.value(key) {
            Some(GitActionValue::Text(text)) => text.clone(),
            _ => String::new(),
        }
    }

    /// The selected option index of the select `key`.
    pub(crate) fn selected(&self, key: &str) -> Option<usize> {
        match self.value(key) {
            Some(GitActionValue::Index(index)) => Some(*index),
            _ => None,
        }
    }
}

type ConfirmHandler = Box<dyn FnOnce(GitActionValues, &mut Window, &mut App)>;

/// A confirmation dialog for a git action, with optional checkboxes, text
/// inputs, and selects, like the dialogs of VS Code's Git Graph extension.
pub(crate) struct GitActionDialog {
    title: SharedString,
    message: Option<SharedString>,
    confirm_label: SharedString,
    fields: Vec<(GitActionField, FieldState)>,
    focus_handle: FocusHandle,
    error: Option<SharedString>,
    on_confirm: Option<ConfirmHandler>,
}

impl GitActionDialog {
    /// Opens the dialog in `workspace`; `on_confirm` runs with the field
    /// values when the user confirms.
    pub(crate) fn open(
        workspace: &mut Workspace,
        title: impl Into<SharedString>,
        message: Option<SharedString>,
        confirm_label: impl Into<SharedString>,
        fields: Vec<GitActionField>,
        on_confirm: impl FnOnce(GitActionValues, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let title = title.into();
        let confirm_label = confirm_label.into();
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(
                title,
                message,
                confirm_label,
                fields,
                Box::new(on_confirm),
                window,
                cx,
            )
        });
    }

    fn new(
        title: SharedString,
        message: Option<SharedString>,
        confirm_label: SharedString,
        fields: Vec<GitActionField>,
        on_confirm: ConfirmHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fields = fields
            .into_iter()
            .map(|field| {
                let state = match &field.kind {
                    GitActionFieldKind::Checkbox { checked } => FieldState::Checkbox(*checked),
                    GitActionFieldKind::Text {
                        initial_text,
                        placeholder,
                        required,
                        multi_line,
                    } => {
                        let editor = cx.new(|cx| {
                            let mut editor = if *multi_line {
                                Editor::auto_height(4, 16, window, cx)
                            } else {
                                Editor::single_line(window, cx)
                            };
                            editor.set_placeholder_text(placeholder, window, cx);
                            editor.set_text(initial_text.clone(), window, cx);
                            editor.select_all(&Default::default(), window, cx);
                            editor
                        });
                        FieldState::Text {
                            editor,
                            required: *required,
                        }
                    }
                    GitActionFieldKind::Select { options, selected } => FieldState::Select {
                        options: options.clone(),
                        selected: *selected,
                    },
                };
                (field, state)
            })
            .collect();
        Self {
            title,
            message,
            confirm_label,
            fields,
            focus_handle: cx.focus_handle(),
            error: None,
            on_confirm: Some(on_confirm),
        }
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let mut values = Vec::with_capacity(self.fields.len());
        for (field, state) in &self.fields {
            let value = match state {
                FieldState::Checkbox(checked) => GitActionValue::Bool(*checked),
                FieldState::Text { editor, required } => {
                    let text = editor.read(cx).text(cx).trim().to_string();
                    if *required && text.is_empty() {
                        self.error = Some(format!("{} is required", field.label).into());
                        cx.notify();
                        return;
                    }
                    GitActionValue::Text(text)
                }
                FieldState::Select { selected, .. } => GitActionValue::Index(*selected),
            };
            values.push((field.key, value));
        }

        let Some(on_confirm) = self.on_confirm.take() else {
            return;
        };
        cx.emit(DismissEvent);
        on_confirm(GitActionValues { values }, window, cx);
    }

    fn render_field(
        &self,
        index: usize,
        field: &GitActionField,
        state: &FieldState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let control = match state {
            FieldState::Checkbox(checked) => Checkbox::new(
                SharedString::from(format!("git-action-checkbox-{index}")),
                ToggleState::from(*checked),
            )
            .label(field.label.clone())
            .on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                if let Some((_, FieldState::Checkbox(checked))) = this.fields.get_mut(index) {
                    *checked = state.selected();
                }
                cx.notify();
            }))
            .into_any_element(),
            FieldState::Text { editor, .. } => v_flex()
                .gap_1()
                .child(Label::new(field.label.clone()).size(LabelSize::Small))
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .border_1()
                        .rounded_sm()
                        .border_color(cx.theme().colors().border)
                        .bg(cx.theme().colors().editor_background)
                        .child(editor.clone()),
                )
                .into_any_element(),
            FieldState::Select { options, selected } => {
                let this = cx.entity().downgrade();
                let options = options.clone();
                let selected = *selected;
                let label = options.get(selected).cloned().unwrap_or_default();
                let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for (option_index, option) in options.iter().enumerate() {
                        let this = this.clone();
                        menu = menu.toggleable_entry(
                            option.clone(),
                            option_index == selected,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                this.update(cx, |this, cx| {
                                    if let Some((_, FieldState::Select { selected, .. })) =
                                        this.fields.get_mut(index)
                                    {
                                        *selected = option_index;
                                    }
                                    cx.notify();
                                })
                                .ok();
                            },
                        );
                    }
                    menu
                });
                v_flex()
                    .gap_1()
                    .child(Label::new(field.label.clone()).size(LabelSize::Small))
                    .child(
                        DropdownMenu::new(
                            SharedString::from(format!("git-action-select-{index}")),
                            label,
                            menu,
                        )
                        .style(DropdownStyle::Outlined)
                        .full_width(true),
                    )
                    .into_any_element()
            }
        };

        v_flex()
            .gap_0p5()
            .child(control)
            .when_some(field.info.clone(), |this, info| {
                this.child(Label::new(info).size(LabelSize::Small).color(Color::Muted))
            })
            .into_any_element()
    }

    #[cfg(test)]
    pub(crate) fn title(&self) -> &SharedString {
        &self.title
    }

    #[cfg(test)]
    pub(crate) fn set_checked(&mut self, key: &str, value: bool) {
        for (field, state) in &mut self.fields {
            if field.key == key
                && let FieldState::Checkbox(checked) = state
            {
                *checked = value;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_text(&mut self, key: &str, text: &str, window: &mut Window, cx: &mut App) {
        for (field, state) in &self.fields {
            if field.key == key
                && let FieldState::Text { editor, .. } = state
            {
                editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_selected(&mut self, key: &str, index: usize) {
        for (field, state) in &mut self.fields {
            if field.key == key
                && let FieldState::Select { selected, .. } = state
            {
                *selected = index;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn confirm_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm(&Confirm, window, cx);
    }
}

impl EventEmitter<DismissEvent> for GitActionDialog {}
impl ModalView for GitActionDialog {}

impl Focusable for GitActionDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.fields
            .iter()
            .find_map(|(_, state)| match state {
                FieldState::Text { editor, .. } => Some(editor.focus_handle(cx)),
                _ => None,
            })
            .unwrap_or_else(|| self.focus_handle.clone())
    }
}

impl Render for GitActionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fields = self
            .fields
            .iter()
            .enumerate()
            .map(|(index, (field, state))| self.render_field(index, field, state, window, cx))
            .collect::<Vec<_>>();

        v_flex()
            .key_context("GitActionDialog")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(rems(34.))
            .p_3()
            .gap_3()
            .child(Headline::new(self.title.clone()).size(HeadlineSize::Small))
            .when_some(self.message.clone(), |this, message| {
                this.child(Label::new(message).color(Color::Muted))
            })
            .when(!fields.is_empty(), |this| {
                this.child(v_flex().gap_2().children(fields))
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("git-action-cancel", "Cancel")
                            .color(Color::Muted)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.cancel(&Cancel, window, cx)),
                            ),
                    )
                    .child(
                        Button::new("git-action-confirm", self.confirm_label.clone())
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&Confirm, window, cx)
                            })),
                    ),
            )
    }
}
