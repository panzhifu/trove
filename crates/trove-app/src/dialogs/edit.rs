//! Batch pixel edits: rotate, flip and crop the selected images, replacing each
//! asset's media with the re-encoded result. Identity and organization survive
//! the edit — id, title, tags, collections and captured-at stay, while hash,
//! size, dimensions, thumbnail and visual fingerprint are recomputed by the
//! backend ([`trove_core::library::Library::batch_edit_images`]).
//!
//! Only assets the library owns can be edited: a *linked* file belongs to its
//! owner and the backend refuses it, so the dialog says so before the user
//! picks options rather than failing one file at a time afterwards.
//!
//! Crop is expressed in percent of the frame, measured *after* rotation, and
//! resolved to pixels per asset — a batch holds files of different sizes, so
//! percent is the only unit that means the same thing for all of them.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::Button;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, Sizable, WindowExt as _};
use gpui_kit::*;

use trove_core::media::edit::ImageEdit;
use trove_core::model::{Asset, AssetKind, Origin};
use trove_core::store::assets;

use crate::library::LibraryController;

/// Marker for the keyed edit progress toast (pushing with the same id replaces
/// the previous toast instead of stacking a new one).
pub struct EditNotice;

/// Rotation a batch applies. The backend's edits are applied in the order
/// given, and the dialog always composes rotation → flip → crop, which is what
/// the note under the form states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rotation {
    None,
    Cw,
    Half,
    Ccw,
}

impl Rotation {
    fn edit(self) -> Option<ImageEdit> {
        match self {
            Rotation::None => None,
            Rotation::Cw => Some(ImageEdit::Rotate90),
            Rotation::Half => Some(ImageEdit::Rotate180),
            Rotation::Ccw => Some(ImageEdit::Rotate270),
        }
    }

    /// Whether width and height swap, i.e. whether a crop percentage is
    /// measured against the rotated frame.
    fn swaps_axes(self) -> bool {
        matches!(self, Rotation::Cw | Rotation::Ccw)
    }

    fn label_key(self) -> &'static str {
        match self {
            Rotation::None => "edit.rot_none",
            Rotation::Cw => "edit.rot_cw",
            Rotation::Half => "edit.rot_180",
            Rotation::Ccw => "edit.rot_ccw",
        }
    }
}

/// Crop as percentages of the frame, `None` when the user left it alone.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CropPercent {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
}

/// What the dialog's form adds up to, independent of any one asset.
#[derive(Debug, Clone, Copy, PartialEq)]
struct EditSpec {
    rotation: Rotation,
    flip_h: bool,
    flip_v: bool,
    crop: Option<CropPercent>,
    quality: u8,
}

impl EditSpec {
    /// Whether this spec would change anything at all.
    ///
    /// A crop covering the whole frame does not count: [`crop_rect`] drops it
    /// for the same reason, and a batch that "applied" it would hand the
    /// backend an empty edit list — an error, not a no-op.
    fn is_empty(&self) -> bool {
        let no_crop = self
            .crop
            .is_none_or(|c| c.left <= 0.0 && c.top <= 0.0 && c.width >= 100.0 && c.height >= 100.0);
        self.rotation == Rotation::None && !self.flip_h && !self.flip_v && no_crop
    }
}

/// The edits for one asset: everything but the crop is shared, the crop
/// rectangle is computed from that asset's own dimensions.
///
/// `None` when the asset cannot take the requested edit — no recorded
/// dimensions to measure a crop against — so the caller counts it as skipped
/// instead of applying half the operation silently.
fn edits_for(asset: &Asset, spec: &EditSpec) -> Option<Vec<ImageEdit>> {
    edits_for_size(asset.width, asset.height, spec)
}

/// [`edits_for`] against bare dimensions, which is what the crop needs and all
/// a test has to supply.
fn edits_for_size(
    width: Option<u32>,
    height: Option<u32>,
    spec: &EditSpec,
) -> Option<Vec<ImageEdit>> {
    let mut edits = Vec::new();
    if let Some(rotate) = spec.rotation.edit() {
        edits.push(rotate);
    }
    if spec.flip_h {
        edits.push(ImageEdit::FlipHorizontal);
    }
    if spec.flip_v {
        edits.push(ImageEdit::FlipVertical);
    }
    if spec.crop.is_some() {
        let (Some(width), Some(height)) = (width, height) else {
            return None;
        };
        let rect = crop_rect(width, height, spec)?;
        edits.push(rect);
    }
    Some(edits)
}

/// The crop rectangle in pixels of the frame the crop is measured in.
///
/// Percentages are clamped so the rectangle always sits inside the frame (the
/// backend validates too, but a form that silently asks for an impossible
/// rectangle would fail every asset in the batch). A crop that covers the whole
/// frame is dropped: it is not an edit.
fn crop_rect(width: u32, height: u32, spec: &EditSpec) -> Option<ImageEdit> {
    let crop = spec.crop?;
    let (frame_w, frame_h) = if spec.rotation.swaps_axes() {
        (height, width)
    } else {
        (width, height)
    };
    if frame_w == 0 || frame_h == 0 {
        return None;
    }

    let pct = |v: f32| v.clamp(0.0, 100.0) / 100.0;
    let left = pct(crop.left);
    let top = pct(crop.top);
    let want_w = pct(crop.width);
    let want_h = pct(crop.height);

    let x = (left * frame_w as f32).floor() as u32;
    let y = (top * frame_h as f32).floor() as u32;
    let w = ((want_w * frame_w as f32).floor() as u32).min(frame_w - x);
    let h = ((want_h * frame_h as f32).floor() as u32).min(frame_h - y);
    if x == 0 && y == 0 && w == frame_w && h == frame_h {
        return None; // the whole frame: nothing to do
    }
    Some(ImageEdit::Crop { x, y, w, h })
}

pub struct EditDialog;

impl EditDialog {
    /// Open for the current selection. Refuses (with a toast) when the
    /// selection holds nothing this can act on.
    pub fn open(window: &mut Window, cx: &mut App, controller: Entity<LibraryController>) {
        let selection = controller.read(cx).selected_assets.clone();
        let conn = controller.read(cx).library.store().conn();
        let images: Vec<Asset> = assets::by_ids(conn, &selection)
            .map(|list| {
                list.into_iter()
                    .filter(|a| a.kind == AssetKind::Image && a.trashed_at.is_none())
                    .collect()
            })
            .unwrap_or_default();
        let editable: Vec<Asset> = images
            .iter()
            .filter(|a| a.origin != Origin::Linked)
            .cloned()
            .collect();
        if images.is_empty() {
            window.push_notification(
                Notification::warning(rust_i18n::t!("edit.empty_selection").to_string()),
                cx,
            );
            return;
        }
        if editable.is_empty() {
            // Worth saying out loud rather than failing per file: in a
            // link-only library this is every asset there is.
            window.push_notification(
                Notification::warning(rust_i18n::t!("edit.all_linked").to_string()),
                cx,
            );
            return;
        }

        let linked_skipped = images.len() - editable.len();
        let controller = controller.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            let crop_left = cx.new(|cx| InputState::new(window, cx).placeholder("0"));
            let crop_top = cx.new(|cx| InputState::new(window, cx).placeholder("0"));
            let crop_width = cx.new(|cx| InputState::new(window, cx).placeholder("100"));
            let crop_height = cx.new(|cx| InputState::new(window, cx).placeholder("100"));
            let quality = cx.new(|cx| InputState::new(window, cx).placeholder("90"));
            let draft = cx.new(|_| EditDraft {
                controller: controller.clone(),
                images: editable.clone(),
                linked_skipped,
                rotation: Rotation::None,
                flip_h: false,
                flip_v: false,
                crop_left: crop_left.clone(),
                crop_top: crop_top.clone(),
                crop_width: crop_width.clone(),
                crop_height: crop_height.clone(),
                quality: quality.clone(),
            });
            let draft_ok = draft.clone();
            dialog
                .title(rust_i18n::t!("edit.title", count = editable.len()).to_string())
                .width(px(520.))
                .close_button(false)
                .child(super::with_close_x(
                    "edit-close-x",
                    v_flex().gap_2().p_1().child(content(&draft, cx)),
                ))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(rust_i18n::t!("edit.apply").to_string())
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let spec = draft_ok.update(cx, |d, cx| d.plan(cx));
                    if spec.is_empty() {
                        window.push_notification(
                            Notification::warning(rust_i18n::t!("edit.no_edits").to_string()),
                            cx,
                        );
                        return false;
                    }
                    start_edit(draft_ok.clone(), spec, window, cx);
                    true
                })
        });
    }
}

/// Dialog draft: the frozen selection plus the chosen operations. Held in an
/// entity so the content closure re-reads it every frame.
struct EditDraft {
    controller: Entity<LibraryController>,
    images: Vec<Asset>,
    /// Linked assets in the selection, reported in the dialog's note.
    linked_skipped: usize,
    rotation: Rotation,
    flip_h: bool,
    flip_v: bool,
    crop_left: Entity<InputState>,
    crop_top: Entity<InputState>,
    crop_width: Entity<InputState>,
    crop_height: Entity<InputState>,
    quality: Entity<InputState>,
}

impl EditDraft {
    /// Read the form into a spec. Crop is only part of it when every field
    /// carries a number — a half-filled crop row is not a crop.
    fn plan(&self, cx: &App) -> EditSpec {
        let read = |state: &Entity<InputState>| -> Option<f32> {
            state.read(cx).value().trim().parse::<f32>().ok()
        };
        let crop = match (
            read(&self.crop_left),
            read(&self.crop_top),
            read(&self.crop_width),
            read(&self.crop_height),
        ) {
            (Some(left), Some(top), Some(width), Some(height)) => Some(CropPercent {
                left,
                top,
                width,
                height,
            }),
            _ => None,
        };
        let quality = self
            .quality
            .read(cx)
            .value()
            .trim()
            .parse()
            .unwrap_or(90)
            .clamp(1, 100);
        EditSpec {
            rotation: self.rotation,
            flip_h: self.flip_h,
            flip_v: self.flip_v,
            crop,
            quality,
        }
    }
}

/// The dialog body: rotation, flips, the crop row, quality and the notes.
fn content(draft: &Entity<EditDraft>, cx: &mut App) -> Div {
    let (rotation, flip_h, flip_v, linked_skipped) = {
        let d = draft.read(cx);
        (d.rotation, d.flip_h, d.flip_v, d.linked_skipped)
    };
    let label = |cx: &App, key: &'static str| {
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(rust_i18n::t!(key).to_string())
    };

    let mut rotate_row = h_flex()
        .gap_2()
        .items_center()
        .child(label(cx, "edit.rotate"));
    {
        let d = draft.clone();
        rotate_row = rotate_row.child(
            Button::new("edit-rotate")
                .xsmall()
                .outline()
                .label(rust_i18n::t!(rotation.label_key()).to_string())
                .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, _| {
                    let mut menu = menu.min_w(px(160.));
                    for picked in [Rotation::None, Rotation::Cw, Rotation::Half, Rotation::Ccw] {
                        let d = d.clone();
                        menu = menu.item(
                            PopupMenuItem::new(rust_i18n::t!(picked.label_key()).to_string())
                                .checked(picked == rotation)
                                .on_click(move |_, _, cx| {
                                    d.update(cx, |d, cx| {
                                        d.rotation = picked;
                                        cx.notify();
                                    })
                                }),
                        );
                    }
                    menu
                }),
        );
    }

    let mut flip_row = h_flex()
        .gap_2()
        .items_center()
        .child(label(cx, "edit.flip"));
    for (id, key, current, is_horizontal) in [
        ("edit-flip-h", "edit.flip_h", flip_h, true),
        ("edit-flip-v", "edit.flip_v", flip_v, false),
    ] {
        let d = draft.clone();
        flip_row = flip_row
            .child(label(cx, key))
            .child(
                Switch::new(id)
                    .checked(current)
                    .on_click(move |next: &bool, _, cx| {
                        d.update(cx, |d, cx| {
                            if is_horizontal {
                                d.flip_h = *next;
                            } else {
                                d.flip_v = *next;
                            }
                            cx.notify();
                        });
                    }),
            );
    }

    let crop_row = h_flex()
        .gap_2()
        .items_center()
        .child(label(cx, "edit.crop"))
        .child(
            Input::new(&draft.read(cx).crop_left)
                .small()
                .appearance(true)
                .w(px(58.)),
        )
        .child(
            Input::new(&draft.read(cx).crop_top)
                .small()
                .appearance(true)
                .w(px(58.)),
        )
        .child(
            Input::new(&draft.read(cx).crop_width)
                .small()
                .appearance(true)
                .w(px(58.)),
        )
        .child(
            Input::new(&draft.read(cx).crop_height)
                .small()
                .appearance(true)
                .w(px(58.)),
        );

    let quality_row = h_flex()
        .gap_2()
        .items_center()
        .child(label(cx, "edit.quality"))
        .child(
            Input::new(&draft.read(cx).quality)
                .small()
                .appearance(true)
                .w(px(70.)),
        );

    let mut body = v_flex()
        .gap_2()
        .child(rotate_row)
        .child(flip_row)
        .child(crop_row)
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("edit.crop_hint").to_string()),
        )
        .child(quality_row)
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("edit.note").to_string()),
        );
    if linked_skipped > 0 {
        body = body.child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("edit.linked_note", count = linked_skipped).to_string()),
        );
    }
    body
}

/// Apply the spec asset by asset on the UI thread, yielding between files so
/// the grid keeps painting and the progress toast advances.
///
/// The backend call owns the store connection, which is thread-confined, so it
/// cannot be moved to a background executor; one file per turn keeps the
/// longest stall to a single image's decode + re-encode.
fn start_edit(draft: Entity<EditDraft>, spec: EditSpec, window: &mut Window, cx: &mut App) {
    let (controller, images) = {
        let d = draft.read(cx);
        (d.controller.clone(), d.images.clone())
    };
    let handle = window.window_handle();

    // Assets the spec cannot act on are counted, not silently half-edited:
    // a crop with no recorded size, or a spec that comes down to nothing for
    // this particular asset (a full-frame crop, say).
    let mut planned: Vec<(uuid::Uuid, Vec<ImageEdit>)> = Vec::new();
    let mut skipped = 0u64;
    for asset in &images {
        match edits_for(asset, &spec) {
            Some(edits) if !edits.is_empty() => planned.push((asset.id, edits)),
            _ => skipped += 1,
        }
    }
    if planned.is_empty() {
        window.push_notification(
            Notification::warning(rust_i18n::t!("edit.nothing_planned").to_string()),
            cx,
        );
        return;
    }

    let total = planned.len();
    window.push_notification(
        Notification::info(rust_i18n::t!("edit.started", count = total).to_string())
            .id1::<EditNotice>("edit-progress"),
        cx,
    );

    cx.spawn(async move |cx| {
        let mut edited = 0u64;
        let mut failed = 0u64;
        for (done, (id, edits)) in planned.iter().enumerate() {
            let outcome = controller.update(cx, |ctl, cx| {
                let result = ctl.library.batch_edit_images(&[*id], edits, spec.quality);
                if matches!(result, Ok(ref report) if report.edited > 0) {
                    ctl.generation += 1;
                    cx.notify();
                }
                result
            });
            match outcome {
                Ok(report) => {
                    edited += report.edited;
                    skipped += report.skipped;
                    failed += report.failures.len() as u64;
                }
                Err(_) => failed += 1,
            }
            let _ = handle.update(cx, |_view, window, cx| {
                window.push_notification(
                    Notification::info(
                        rust_i18n::t!("edit.running", done = done + 1, total = total).to_string(),
                    )
                    .id1::<EditNotice>("edit-progress"),
                    cx,
                );
            });
            // Yield so the grid keeps painting between files: a polled future
            // runs to completion, and without this a large batch would decode
            // and re-encode inside one frame — the freeze the loop comment
            // promises does not exist unless the loop actually suspends.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1))
                .await;
        }

        let note = if failed == 0 && skipped == 0 {
            Notification::success(rust_i18n::t!("edit.done", count = edited).to_string())
        } else {
            Notification::warning(
                rust_i18n::t!(
                    "edit.done_partial",
                    edited = edited,
                    skipped = skipped,
                    failed = failed
                )
                .to_string(),
            )
        };
        let _ = handle.update(cx, |_view, window, cx| {
            window.push_notification(note, cx);
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    // Imported one by one rather than `use super::*`: pulling the gpui element
    // types into a test body is what pushes the bin target's macro recursion
    // over its limit (see `recursion_limit` in `main.rs`).
    use super::{CropPercent, EditSpec, Rotation, crop_rect, edits_for_size};
    use trove_core::media::edit::ImageEdit;

    fn spec(rotation: Rotation, crop: Option<CropPercent>) -> EditSpec {
        EditSpec {
            rotation,
            flip_h: false,
            flip_v: false,
            crop,
            quality: 90,
        }
    }

    #[test]
    fn rotation_and_flip_are_composed_before_the_crop() {
        let spec = EditSpec {
            rotation: Rotation::Cw,
            flip_h: true,
            flip_v: true,
            crop: None,
            quality: 90,
        };
        let edits = edits_for_size(Some(100), Some(50), &spec).unwrap();
        assert_eq!(
            edits,
            vec![
                ImageEdit::Rotate90,
                ImageEdit::FlipHorizontal,
                ImageEdit::FlipVertical
            ]
        );
    }

    #[test]
    fn a_crop_is_measured_against_the_rotated_frame() {
        // 100x50 rotated a quarter turn is 50x100, so 10%..60% of the *short*
        // side is what the user sees on screen.
        let crop = CropPercent {
            left: 10.0,
            top: 20.0,
            width: 50.0,
            height: 30.0,
        };
        let straight = crop_rect(100, 50, &spec(Rotation::None, Some(crop))).unwrap();
        assert_eq!(
            straight,
            ImageEdit::Crop {
                x: 10,
                y: 10,
                w: 50,
                h: 15
            }
        );

        let turned = crop_rect(100, 50, &spec(Rotation::Cw, Some(crop))).unwrap();
        assert_eq!(
            turned,
            ImageEdit::Crop {
                x: 5,
                y: 20,
                w: 25,
                h: 30
            }
        );
    }

    #[test]
    fn a_full_frame_crop_is_not_an_edit() {
        let crop = CropPercent {
            left: 0.0,
            top: 0.0,
            width: 100.0,
            height: 100.0,
        };
        assert_eq!(crop_rect(640, 480, &spec(Rotation::None, Some(crop))), None);
        // With nothing else selected, that leaves nothing to do at all.
        assert!(spec(Rotation::None, Some(crop)).is_empty());
        assert!(!spec(Rotation::Cw, Some(crop)).is_empty());
    }

    #[test]
    fn an_impossible_crop_is_clamped_inside_the_frame() {
        let crop = CropPercent {
            left: 90.0,
            top: 90.0,
            width: 50.0,
            height: 50.0,
        };
        assert_eq!(
            crop_rect(100, 100, &spec(Rotation::None, Some(crop))),
            Some(ImageEdit::Crop {
                x: 90,
                y: 90,
                w: 10,
                h: 10
            })
        );

        // Nonsense numbers clamp to the frame rather than escaping it — and a
        // clamp that lands on the whole frame is not an edit either.
        let wild = CropPercent {
            left: -50.0,
            top: 0.0,
            width: 500.0,
            height: 100.0,
        };
        assert_eq!(crop_rect(100, 40, &spec(Rotation::None, Some(wild))), None);

        // A rectangle that hangs off one edge clamps to what is left.
        let over = CropPercent {
            left: 0.0,
            top: 50.0,
            width: 100.0,
            height: 100.0,
        };
        assert_eq!(
            crop_rect(100, 40, &spec(Rotation::None, Some(over))),
            Some(ImageEdit::Crop {
                x: 0,
                y: 20,
                w: 100,
                h: 20
            })
        );
    }

    #[test]
    fn a_crop_without_recorded_dimensions_skips_the_asset() {
        let crop = CropPercent {
            left: 10.0,
            top: 10.0,
            width: 50.0,
            height: 50.0,
        };
        // Rotate-only still works without dimensions…
        assert!(edits_for_size(None, None, &spec(Rotation::Cw, None)).is_some());
        // …but a crop has nothing to measure against, so the asset is skipped
        // rather than rotated without the crop the user asked for.
        assert!(edits_for_size(None, None, &spec(Rotation::Cw, Some(crop))).is_none());
    }
}
