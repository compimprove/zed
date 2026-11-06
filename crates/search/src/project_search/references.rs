//! Shows the references found by `editor::FindAllReferences` in a project
//! search view, so they get the same results tree as a text search.

use std::{cmp::Reverse, collections::HashMap, ops::Range, sync::Arc};

use editor::{PathKey, ReferencesViewOpener, search_results_context_lines};
use gpui::{App, AppContext as _, Context, Entity, Window};
use language::Buffer;
use text::Point;
use workspace::{Workspace, item::ItemHandle as _};

use super::{ProjectSearch, ProjectSearchView, SearchCompletion, SearchPhase, SearchState};

pub(super) fn init(cx: &mut App) {
    cx.set_global(ReferencesViewOpener(Arc::new(open_references)));
}

fn open_references(
    workspace: &mut Workspace,
    locations: HashMap<Entity<Buffer>, Vec<Range<Point>>>,
    title: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let weak_workspace = workspace.weak_handle();
    let search = cx.new(|cx| {
        let mut search = ProjectSearch::new(project, weak_workspace.clone(), cx);
        search.references = true;
        search.last_search_query_text = Some(title.clone());
        search
    });
    let view = cx.new(|cx| {
        let mut view = ProjectSearchView::new(weak_workspace, search.clone(), window, cx, None);
        view.filters_enabled = false;
        view
    });
    search.update(cx, |search, cx| search.set_references(locations, cx));

    // Running the same lookup again replaces its tab rather than adding one.
    let pane = workspace.active_pane().clone();
    let existing = pane
        .read(cx)
        .items_of_type::<ProjectSearchView>()
        .find(|existing| {
            let search = existing.read(cx).entity.read(cx);
            search.references && search.last_search_query_text.as_deref() == Some(&title)
        });
    let destination_index = existing.and_then(|existing| {
        let index = pane.read(cx).index_for_item(&existing);
        pane.update(cx, |pane, cx| {
            pane.remove_item(existing.item_id(), false, false, window, cx)
        });
        index
    });
    workspace.add_item_to_active_pane(Box::new(view.clone()), destination_index, true, window, cx);
    view.update(cx, |view, cx| view.focus_results_editor(window, cx));
}

impl ProjectSearch {
    /// Shows the given locations as the search results.
    fn set_references(
        &mut self,
        locations: HashMap<Entity<Buffer>, Vec<Range<Point>>>,
        cx: &mut Context<Self>,
    ) {
        let mut locations = locations.into_iter().collect::<Vec<_>>();
        locations.sort_by_key(|(buffer, _)| buffer.read(cx).remote_id());
        let match_ranges = self.excerpts.update(cx, |excerpts, cx| {
            let mut match_ranges = Vec::new();
            for (buffer, mut ranges) in locations {
                ranges.sort_by_key(|range| (range.start, Reverse(range.end)));
                excerpts.set_excerpts_for_path(
                    PathKey::for_buffer(&buffer, cx),
                    buffer.clone(),
                    ranges.clone(),
                    search_results_context_lines(cx),
                    cx,
                );
                let snapshot = excerpts.snapshot(cx);
                let buffer_snapshot = buffer.read(cx).snapshot();
                match_ranges.extend(ranges.into_iter().filter_map(|range| {
                    let range = buffer_snapshot.anchor_range_inside(range);
                    Some(
                        snapshot.anchor_in_buffer(range.start)?
                            ..snapshot.anchor_in_buffer(range.end)?,
                    )
                }));
            }
            let snapshot = excerpts.snapshot(cx);
            match_ranges.sort_by(|a, b| a.start.cmp(&b.start, &snapshot));
            match_ranges
        });
        self.search_state = SearchState::Completed(if match_ranges.is_empty() {
            SearchCompletion::NoResults
        } else {
            SearchCompletion::Results {
                limit_reached: false,
            }
        });
        self.match_ranges = match_ranges;
        self.phase = SearchPhase::Confirmed;
        self.search_id += 1;
        cx.notify();
    }
}
