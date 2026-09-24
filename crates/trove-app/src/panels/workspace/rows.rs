//! Row construction: turning the flat cell list into frozen justified
//! rows (grid), day-sectioned pages (timeline) or the frozen-shape
//! refills that keep scrolling stable across pagination.
//!
//! Every entry point here borrows the cell list. A row owns its cells, so a
//! listing is copied exactly as many times as it is *placed*; taking the list
//! by value would make the caller copy it once more on top of that, which for a
//! grid that pages by appending is a full deep copy of everything loaded, per
//! window.

use super::*;

// ============================ row construction ===============================

/// One timeline day header: the date/count label over a hairline rule.
pub(super) fn timeline_header(label: String, cx: &App) -> gpui::AnyElement {
    h_flex()
        .h(px(TIMELINE_HEADER_HEIGHT))
        .w_full()
        .items_center()
        .gap_2()
        .px(px(GRID_GAP))
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child(label),
        )
        .into_any_element()
}

/// One frozen row: the geometry it was laid out with, and cells it now owns.
fn row_of(height: f32, widths: Vec<f32>, cells: impl IntoIterator<Item = Cell>) -> Row {
    Row {
        height,
        widths: widths.into(),
        cells: cells.into_iter().collect(),
        header: None,
    }
}

/// One list-view row per asset: full width, no justification.
pub(super) fn list_rows(cells: &[Cell], content_width: f32) -> Vec<Row> {
    cells
        .iter()
        .map(|c| row_of(LIST_ROW_HEIGHT, vec![content_width], [c.clone()]))
        .collect()
}

/// Pair a cell list with DP row layouts into frozen [`Row`]s.
pub(super) fn materialize_rows(cells: &[Cell], layouts: &[RowLayout]) -> Vec<Row> {
    let mut cursor = 0;
    layouts
        .iter()
        .map(|layout| {
            let start = cursor;
            cursor = (cursor + layout.item_widths.len()).min(cells.len());
            row_of(
                layout.height,
                layout.item_widths.clone(),
                cells[start..cursor].iter().cloned(),
            )
        })
        .collect()
}

/// Nearest row at or before `from` that holds cells. Timeline headers are
/// cell-less, so navigation has to hop over them.
pub(super) fn prev_cell_row(rows: &[Row], from: usize) -> Option<usize> {
    (0..=from.min(rows.len().saturating_sub(1)))
        .rev()
        .find(|&r| !rows[r].cells.is_empty())
}

/// Nearest row at or after `from` that holds cells.
pub(super) fn next_cell_row(rows: &[Row], from: usize) -> Option<usize> {
    (from..rows.len()).find(|&r| !rows[r].cells.is_empty())
}

/// Group cells into day sections for the timeline view.
///
/// The newest day comes first; within a day the order the query returned is
/// kept. Each section is a header row plus its own justified rows, so a day
/// never straddles another day's header.
pub(super) fn timeline_rows(cells: &[Cell], content_width: f32, target: f32) -> Vec<Row> {
    // The day order is put in an index vector rather than by sorting the cells
    // themselves: the sort is a reorder of a listing that may hold every asset
    // the grid has loaded, and a row owns its cells once.
    let mut order: Vec<usize> = (0..cells.len()).collect();
    order.sort_by(|a, b| cells[*b].day.cmp(&cells[*a].day));

    let mut rows = Vec::new();
    let mut index = 0;
    while index < order.len() {
        let day = &cells[order[index]].day;
        let end = order[index..]
            .iter()
            .position(|&ix| &cells[ix].day != day)
            .map_or(order.len(), |offset| index + offset);
        let group = &order[index..end];
        let label = rust_i18n::t!(
            "workspace.timeline_day",
            date = day,
            count = group.len().to_string()
        )
        .to_string();
        rows.push(Row::section(label));
        let aspects: Vec<f32> = group.iter().map(|&ix| cells[ix].aspect()).collect();
        let layouts = justify_layout_with_target(&aspects, content_width, target);
        let mut cursor = 0;
        for layout in &layouts {
            let start = cursor;
            cursor = (cursor + layout.item_widths.len()).min(group.len());
            rows.push(row_of(
                layout.height,
                layout.item_widths.clone(),
                group[start..cursor].iter().map(|&ix| cells[ix].clone()),
            ));
        }
        index = end;
    }
    rows
}

/// Keep the rows already on screen and build only the cells that arrived after
/// them.
///
/// This is the pagination path: a window fetched from a frozen session is the
/// *tail* of the same listing, so every row that was laid out before still
/// holds the cells it holds, in the same order. Cloning those rows is a
/// refcount each (`Row`'s two lists are shared slices), which is what makes the
/// cost of appending a page proportional to the page rather than to everything
/// loaded.
///
/// The timeline view is deliberately not routed here — its day sections move
/// when the set does, so there is no frozen head to keep.
pub(super) fn appended_rows(
    head: &[Row],
    placed: usize,
    cells: &[Cell],
    list_mode: bool,
    content_width: f32,
    target: f32,
) -> Vec<Row> {
    let mut rows = head.to_vec();
    let tail = &cells[placed.min(cells.len())..];
    if list_mode {
        rows.extend(list_rows(tail, content_width));
        return rows;
    }
    let aspects: Vec<f32> = tail.iter().map(|c| c.aspect()).collect();
    let layouts = justify_layout_with_target(&aspects, content_width, target);
    rows.extend(materialize_rows(tail, &layouts));
    rows
}

/// Refill frozen row *shapes* (cells per row) with a new cell list.
///
/// Used when assets are added or removed: rows keep their positions, only
/// content and recomputed heights change. Cells that overflow the frozen
/// shapes (the set grew) become greedily-fitted tail rows.
pub(super) fn refill_rows(
    cells: &[Cell],
    counts: &[usize],
    content_width: f32,
    target: f32,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut cursor = 0;
    for count in counts {
        let chunk = &cells[cursor..(cursor + count).min(cells.len())];
        if chunk.is_empty() {
            break;
        }
        cursor += chunk.len();
        let aspects: Vec<f32> = chunk.iter().map(|c| c.aspect()).collect();
        let layout = fit_row(&aspects, content_width, target);
        rows.push(row_of(
            layout.height,
            layout.item_widths,
            chunk.iter().cloned(),
        ));
    }
    let rest = &cells[cursor..];
    if !rest.is_empty() {
        let aspects: Vec<f32> = rest.iter().map(|c| c.aspect()).collect();
        let layouts = justify_layout_with_target(&aspects, content_width, target);
        rows.extend(materialize_rows(rest, &layouts));
    }
    rows
}

/// Lay out a single row: uniform height, aspect-preserving widths, spanning
/// the container exactly (same formula as the DP's per-row scoring).
fn fit_row(aspects: &[f32], content_width: f32, target: f32) -> RowLayout {
    let aspects: Vec<f32> = aspects.iter().map(|a| a.max(MIN_ASPECT)).collect();
    let k = aspects.len() as f32;
    let natural: f32 = aspects.iter().sum::<f32>() * target;
    let content = (content_width - GRID_GAP * (k - 1.0)).max(1.0);
    let h_raw = target * content / natural.max(1e-3);
    // Raising a too-short row to MIN would push it past the container width;
    // keep the exact fit there, so MIN stays a soft bound and rows never
    // overflow.
    let height = if h_raw < MIN_ROW_HEIGHT {
        h_raw
    } else {
        h_raw.min(MAX_ROW_HEIGHT)
    };
    RowLayout {
        height,
        item_widths: aspects.iter().map(|a| a * height).collect(),
    }
}
