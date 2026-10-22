//! Drag handles that resize the git panel's Repositories list and commit
//! editor. The changes list between them takes the remaining height.
//! Double-clicking a handle restores the section's default height.

use std::{cell::Cell, rc::Rc};

use editor::{EditorMode, SizingBehavior};
use gpui::{AnyElement, Bounds, ClickEvent, DragMoveEvent, Empty, Pixels, canvas};
use ui::prelude::*;

use super::{GitPanel, MAX_PANEL_EDITOR_LINES};

const SECTION_MIN_HEIGHT: Pixels = px(40.);
/// The Repositories list's height until the user resizes it; it shrinks to
/// fit when there are fewer repositories.
pub(super) const DEFAULT_REPOSITORIES_LIST_HEIGHT: Pixels = px(310.);
const HANDLE_HEIGHT: Pixels = px(6.);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PanelSection {
    Repositories,
    CommitEditor,
}

/// The drag payload for a section resize handle.
#[derive(Clone, Copy)]
pub(super) struct DraggedPanelSection(PanelSection);

/// Where a resize drag started, so moves can be applied as a delta.
#[derive(Clone, Copy)]
pub(super) struct SectionResize {
    section: PanelSection,
    start_y: Pixels,
    start_height: Pixels,
}

/// An invisible element that records the bounds of its absolutely positioned
/// parent, so a drag can start from the section's laid-out height.
pub(super) fn measure_bounds(bounds_cell: Rc<Cell<Option<Bounds<Pixels>>>>) -> impl IntoElement {
    canvas(
        move |bounds, _, _| bounds_cell.set(Some(bounds)),
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

impl GitPanel {
    /// A thin handle straddling the top (`at_top`) or bottom edge of its
    /// relatively positioned parent.
    pub(super) fn render_section_resize_handle(
        &self,
        section: PanelSection,
        at_top: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel = cx.entity().downgrade();
        div()
            .id(match section {
                PanelSection::Repositories => "repositories-resize-handle",
                PanelSection::CommitEditor => "commit-editor-resize-handle",
            })
            .absolute()
            .left_0()
            .w_full()
            .h(HANDLE_HEIGHT)
            .map(|this| {
                if at_top {
                    this.top(-HANDLE_HEIGHT / 2.)
                } else {
                    this.bottom(-HANDLE_HEIGHT / 2.)
                }
            })
            .cursor_row_resize()
            .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                if event.click_count() >= 2 {
                    this.reset_section_height(section, cx);
                }
                cx.stop_propagation();
            }))
            .on_drag(DraggedPanelSection(section), move |_, _, window, cx| {
                let start_y = window.mouse_position().y;
                panel
                    .update(cx, |this, cx| {
                        this.start_section_resize(section, start_y, cx)
                    })
                    .ok();
                cx.new(|_| Empty)
            })
            .into_any_element()
    }

    fn start_section_resize(
        &mut self,
        section: PanelSection,
        start_y: Pixels,
        cx: &mut Context<Self>,
    ) {
        let (height, bounds) = match section {
            PanelSection::Repositories => (
                self.repositories_list_height,
                &self.repositories_list_bounds,
            ),
            PanelSection::CommitEditor => (self.commit_editor_height, &self.commit_editor_bounds),
        };
        let Some(start_height) = height.or_else(|| bounds.get().map(|bounds| bounds.size.height))
        else {
            return;
        };
        self.section_resize = Some(SectionResize {
            section,
            start_y,
            start_height,
        });
        if section == PanelSection::CommitEditor && self.commit_editor_height.is_none() {
            self.commit_editor_height = Some(start_height);
            self.use_full_height_commit_editor(cx);
        }
    }

    /// Lets the commit editor fill a height set by its resize handle.
    pub(super) fn use_full_height_commit_editor(&mut self, cx: &mut Context<Self>) {
        self.commit_editor.update(cx, |editor, _| {
            editor.set_mode(EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: false,
                show_active_line_background: false,
                sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
            })
        });
    }

    pub(super) fn on_section_drag_move(
        &mut self,
        event: &DragMoveEvent<DraggedPanelSection>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(resize) = self.section_resize else {
            return;
        };
        if resize.section != event.drag(cx).0 {
            return;
        }
        let delta = event.event.position.y - resize.start_y;
        let max_height = (event.bounds.size.height * 0.7).max(SECTION_MIN_HEIGHT);
        match resize.section {
            PanelSection::Repositories => {
                self.repositories_list_height =
                    Some((resize.start_height + delta).clamp(SECTION_MIN_HEIGHT, max_height));
            }
            PanelSection::CommitEditor => {
                self.commit_editor_height =
                    Some((resize.start_height - delta).clamp(SECTION_MIN_HEIGHT, max_height));
            }
        }
        cx.notify();
    }

    fn reset_section_height(&mut self, section: PanelSection, cx: &mut Context<Self>) {
        match section {
            PanelSection::Repositories => self.repositories_list_height = None,
            PanelSection::CommitEditor => {
                self.commit_editor_height = None;
                self.commit_editor.update(cx, |editor, _| {
                    editor.set_mode(EditorMode::AutoHeight {
                        min_lines: MAX_PANEL_EDITOR_LINES,
                        max_lines: Some(MAX_PANEL_EDITOR_LINES),
                    })
                });
            }
        }
        self.serialize(cx);
        cx.notify();
    }
}
