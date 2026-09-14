//! Row construction: turning the flat cell list into frozen justified
//! rows (grid), day-sectioned pages (timeline) or the frozen-shape
//! refills that keep scrolling stable across pagination.

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

/// Pair a cell list with DP row layouts into frozen [`Row`]s.
pub(super) fn materialize_rows(cells: Vec<Cell>, layouts: &[RowLayout]) -> Vec<Row> {
    let mut cells = cells.into_iter();
    layouts
        .iter()
        .map(|layout| Row {
            height: layout.height,
            widths: layout.item_widths.clone(),
            cells: (&mut cells).take(layout.item_widths.len()).collect(),
            header: None,
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
pub(super) fn timeline_rows(cells: Vec<Cell>, content_width: f32, target: f32) -> Vec<Row> {
    let mut cells = cells;
    cells.sort_by(|a, b| b.day.cmp(&a.day));

    let mut rows = Vec::new();
    // `drain` below removes each group in place, so the cursor stays put.
    let index = 0;
    while index < cells.len() {
        let day = cells[index].day.clone();
        let end = cells[index..]
            .iter()
            .position(|c| c.day != day)
            .map_or(cells.len(), |offset| index + offset);
        let group: Vec<Cell> = cells.drain(index..end).collect();
        let label = rust_i18n::t!(
            "workspace.timeline_day",
            date = day,
            count = group.len().to_string()
        )
        .to_string();
        rows.push(Row::section(label));
        let aspects: Vec<f32> = group.iter().map(|c| c.aspect()).collect();
        let layouts = justify_layout_with_target(&aspects, content_width, target);
        rows.extend(materialize_rows(group, &layouts));
    }
    rows
}

/// Refill frozen row *shapes* (cells per row) with a new cell list.
///
/// Used when assets are added or removed: rows keep their positions, only
/// content and recomputed heights change. Cells that overflow the frozen
/// shapes (the set grew) become greedily-fitted tail rows.
pub(super) fn refill_rows(
    cells: Vec<Cell>,
    counts: &[usize],
    content_width: f32,
    target: f32,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut iter = cells.into_iter();
    for count in counts {
        let chunk: Vec<Cell> = iter.by_ref().take(*count).collect();
        if chunk.is_empty() {
            break;
        }
        let aspects: Vec<f32> = chunk.iter().map(|c| c.aspect()).collect();
        let layout = fit_row(&aspects, content_width, target);
        rows.push(Row {
            height: layout.height,
            widths: layout.item_widths,
            cells: chunk,
            header: None,
        });
    }
    let rest: Vec<Cell> = iter.collect();
    if !rest.is_empty() {
        let aspects: Vec<f32> = rest.iter().map(|c| c.aspect()).collect();
        let layouts = justify_layout_with_target(&aspects, content_width, target);
        let mut rest = rest.into_iter();
        for layout in layouts {
            rows.push(Row {
                height: layout.height,
                widths: layout.item_widths.clone(),
                cells: (&mut rest).take(layout.item_widths.len()).collect(),
                header: None,
            });
        }
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
