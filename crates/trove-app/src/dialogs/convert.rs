//! Batch format conversion: re-encode the selected image assets to a plain
//! raster format (JPEG/PNG/WebP/BMP/TIFF) into a user-chosen folder, with an
//! optional longest-edge cap and an optional re-import of the results.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::Button;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme, Sizable, WindowExt as _};
use gpui_kit::*;

use trove_core::media::convert::{self, CONVERT_FORMATS, ConvertFormat, ConvertItem};
use trove_core::model::{Asset, AssetKind, Origin};
use trove_core::store::assets;

use crate::library::LibraryController;

/// Marker for the keyed conversion progress toast (pushing with the same id
/// replaces the previous toast instead of stacking a new one).
pub struct ConvertNotice;

pub struct ConvertDialog;

impl ConvertDialog {
    /// Open for the current selection. Only image assets are convertible;
    /// refuses (with a toast) when the selection has none.
    pub fn open(window: &mut Window, cx: &mut App, controller: Entity<LibraryController>) {
        let selection = controller.read(cx).selected_assets.clone();
        let conn = controller.read(cx).library.store().conn();
        let images: Vec<Asset> = assets::by_ids(conn, &selection)
            .map(|list| {
                list.into_iter()
                    .filter(|a| a.kind == AssetKind::Image)
                    .collect()
            })
            .unwrap_or_default();
        if images.is_empty() {
            window.push_notification(
                Notification::warning(rust_i18n::t!("convert.empty_selection").to_string()),
                cx,
            );
            return;
        }

        let controller = controller.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            let quality = cx.new(|cx| InputState::new(window, cx).placeholder("85".to_string()));
            let max_dim = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(rust_i18n::t!("convert.max_dim_hint").to_string())
            });
            let draft = cx.new(|_| ConvertDraft {
                controller: controller.clone(),
                images: images.clone(),
                dest: None,
                format: ConvertFormat::Jpeg,
                quality: quality.clone(),
                max_dim: max_dim.clone(),
                import_after: false,
            });
            let draft_ok = draft.clone();
            dialog
                .title(rust_i18n::t!("convert.title", count = images.len()).to_string())
                .width(px(480.))
                .close_button(false)
                .child(super::with_close_x(
                    "convert-close-x",
                    v_flex().gap_2().p_1().child(content(&draft, window, cx)),
                ))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(rust_i18n::t!("convert.apply").to_string())
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let (dest, opts, items, import_after) = draft_ok.update(cx, |d, cx| d.plan(cx));
                    let Some(dest) = dest else {
                        window.push_notification(
                            Notification::warning(rust_i18n::t!("convert.no_dest").to_string()),
                            cx,
                        );
                        return false;
                    };
                    start_conversion(
                        draft_ok.clone(),
                        dest,
                        items,
                        opts,
                        import_after,
                        window,
                        cx,
                    );
                    true
                })
        });
    }
}

/// Dialog draft: the frozen image selection plus the chosen options. Held in
/// an entity so the content closure re-reads it every frame.
struct ConvertDraft {
    controller: Entity<LibraryController>,
    images: Vec<Asset>,
    dest: Option<PathBuf>,
    format: ConvertFormat,
    quality: Entity<InputState>,
    max_dim: Entity<InputState>,
    import_after: bool,
}

impl ConvertDraft {
    /// Collect the background-executable plan: destination, options, items
    /// and the re-import flag.
    fn plan(
        &self,
        cx: &App,
    ) -> (
        Option<PathBuf>,
        convert::ConvertOptions,
        Vec<ConvertItem>,
        bool,
    ) {
        let root = self.controller.read(cx).library.root().to_path_buf();
        let items = self
            .images
            .iter()
            .filter_map(|asset| {
                let source = if asset.origin == Origin::Linked {
                    asset
                        .extra
                        .get("source_path")
                        .and_then(|v| v.as_str())
                        .map(PathBuf::from)
                } else {
                    asset.rel_path.as_ref().map(|rel| root.join(rel))
                }?;
                Some(ConvertItem {
                    asset_id: asset.id,
                    source,
                    title: asset.title.clone().unwrap_or_else(|| {
                        std::path::Path::new(&asset.file_name)
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or(&asset.file_name)
                            .to_string()
                    }),
                })
            })
            .collect();
        let quality: u8 = self
            .quality
            .read(cx)
            .value()
            .trim()
            .parse()
            .unwrap_or(85)
            .clamp(1, 100);
        let max_dimension: Option<u32> = self
            .max_dim
            .read(cx)
            .value()
            .trim()
            .parse()
            .ok()
            .filter(|&d| d > 0);
        let opts = convert::ConvertOptions {
            format: self.format,
            quality,
            max_dimension,
        };
        (self.dest.clone(), opts, items, self.import_after)
    }
}

/// The dialog body: format dropdown, quality, size cap, destination picker
/// and the re-import switch.
fn content(draft: &Entity<ConvertDraft>, window: &mut Window, cx: &mut App) -> Div {
    let (format, dest, import_after) = {
        let d = draft.read(cx);
        (d.format, d.dest.clone(), d.import_after)
    };
    let quality_input = draft.read(cx).quality.clone();
    let max_dim_input = draft.read(cx).max_dim.clone();

    let label = |cx: &App, key: &'static str| {
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(rust_i18n::t!(key).to_string())
    };

    // Format picker (quality applies to JPEG only).
    let mut format_row = h_flex()
        .gap_2()
        .items_center()
        .child(label(cx, "convert.format"));
    {
        let d = draft.clone();
        let current = format;
        format_row = format_row.child(
            Button::new("convert-format")
                .xsmall()
                .outline()
                .label(format_label(current))
                .dropdown_menu_with_anchor(Anchor::TopLeft, move |menu, _, _| {
                    let mut menu = menu.min_w(px(140.));
                    for picked in CONVERT_FORMATS {
                        let d = d.clone();
                        menu = menu.item(
                            PopupMenuItem::new(format_label(picked))
                                .checked(picked == current)
                                .on_click(move |_, _, cx| {
                                    d.update(cx, |d, cx| {
                                        d.format = picked;
                                        cx.notify();
                                    })
                                }),
                        );
                    }
                    menu
                }),
        );
    }
    if format.supports_quality() {
        format_row = format_row.child(label(cx, "convert.quality")).child(
            Input::new(&quality_input)
                .small()
                .appearance(true)
                .w(px(70.)),
        );
    }
    format_row = format_row.child(label(cx, "convert.max_dim")).child(
        Input::new(&max_dim_input)
            .small()
            .appearance(true)
            .w(px(90.)),
    );

    // Destination picker.
    let mut dest_row = h_flex()
        .gap_2()
        .items_center()
        .child(label(cx, "convert.dest"));
    {
        let d = draft.clone();
        let handle = window.window_handle();
        dest_row = dest_row.child(
            Button::new("convert-dest")
                .xsmall()
                .outline()
                .label(dest.as_ref().map_or_else(
                    || rust_i18n::t!("convert.choose_dest").to_string(),
                    |p| p.display().to_string(),
                ))
                .on_click(move |_, _, cx| {
                    let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                        files: false,
                        directories: true,
                        multiple: false,
                        prompt: Some(rust_i18n::t!("convert.dest_prompt").into_owned().into()),
                    });
                    let d = d.clone();
                    cx.spawn(async move |cx| {
                        if let Ok(Ok(Some(paths))) = rx.await
                            && let Some(dir) = paths.first()
                        {
                            let _ = handle.update(cx, |_, _, cx| {
                                d.update(cx, |d, cx| {
                                    d.dest = Some(dir.to_path_buf());
                                    cx.notify();
                                })
                            });
                        }
                    })
                    .detach();
                }),
        );
    }

    // Re-import switch.
    let mut import_row = h_flex()
        .gap_2()
        .items_center()
        .child(label(cx, "convert.import_after"));
    {
        let d = draft.clone();
        import_row = import_row.child(
            Switch::new("convert-import")
                .checked(import_after)
                .on_click(move |next: &bool, _, cx| {
                    d.update(cx, |d, cx| {
                        d.import_after = *next;
                        cx.notify();
                    });
                }),
        );
    }

    let note_key = if format == ConvertFormat::WebP {
        "convert.note_webp"
    } else {
        "convert.note"
    };

    v_flex()
        .gap_2()
        .child(format_row)
        .child(dest_row)
        .child(import_row)
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!(note_key).to_string()),
        )
}

/// Localized label for a format, e.g. "JPEG".
fn format_label(format: ConvertFormat) -> String {
    let key = match format {
        ConvertFormat::Jpeg => "convert.f_jpeg",
        ConvertFormat::Png => "convert.f_png",
        ConvertFormat::WebP => "convert.f_webp",
        ConvertFormat::Bmp => "convert.f_bmp",
        ConvertFormat::Tiff => "convert.f_tiff",
    };
    rust_i18n::t!(key).to_string()
}

/// Convert every item on the background executor (one per turn) with a keyed
/// progress toast; optionally import the results into the browsed collection.
fn start_conversion(
    draft: Entity<ConvertDraft>,
    dest: PathBuf,
    items: Vec<ConvertItem>,
    opts: convert::ConvertOptions,
    import_after: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let total = items.len();
    let controller = draft.read(cx).controller.clone();
    let handle = window.window_handle();

    window.push_notification(
        Notification::info(rust_i18n::t!("convert.started", count = total).to_string())
            .id1::<ConvertNotice>("convert-progress"),
        cx,
    );

    cx.spawn(async move |cx| {
        // Shared across the per-item background tasks so names stay unique
        // within the batch (and against the destination folder).
        let used = Arc::new(Mutex::new(HashSet::new()));
        let mut converted: Vec<PathBuf> = Vec::new();
        let mut failed = 0usize;
        for (done, item) in items.into_iter().enumerate() {
            let dest = dest.clone();
            let opts = opts.clone();
            let used = used.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut used = used.lock().expect("convert name registry poisoned");
                    convert::convert_item(&dest, &item, &opts, &mut used)
                })
                .await;
            match result {
                Ok(path) => converted.push(path),
                Err(_) => failed += 1,
            }
            let _ = handle.update(cx, |_view, window, cx| {
                window.push_notification(
                    Notification::info(
                        rust_i18n::t!("convert.running", done = done + 1, total = total)
                            .to_string(),
                    )
                    .id1::<ConvertNotice>("convert-progress"),
                    cx,
                );
            });
        }

        let note = if failed == 0 {
            Notification::success(
                rust_i18n::t!("convert.done", count = converted.len()).to_string(),
            )
        } else {
            Notification::warning(
                rust_i18n::t!(
                    "convert.done_failed",
                    converted = converted.len(),
                    failed = failed
                )
                .to_string(),
            )
        };
        let _ = handle.update(cx, |_view, window, cx| {
            window.push_notification(note, cx);
        });

        if import_after && !converted.is_empty() {
            let _ = handle.update(cx, |_view, window, cx| {
                crate::library::jobs::import_paths_app(&controller, converted, window, cx);
            });
        }
    })
    .detach();
}
