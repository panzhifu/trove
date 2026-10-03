//! The export dialog: hand the selected assets' files to a user-chosen
//! folder. Images take a raster target (or are copied as-is); videos take a
//! ffmpeg target — transcode presets, a quick remux, or a verbatim copy;
//! every other kind is copied unchanged. `on_ok` freezes the plan and
//! starts the export job.

use std::path::PathBuf;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Sizable, WindowExt as _};
use gpui_kit::*;

use trove_core::media::convert::{CONVERT_FORMATS, ConvertFormat};
use trove_core::media::export::{
    self, VIDEO_EXPORT_FORMATS, VIDEO_MAX_HEIGHTS, VideoExportFormat, VideoQuality,
};
use trove_core::model::{Asset, AssetKind};
use trove_core::tasks::export::ExportItem;

use crate::components::controls::{self, muted_label};
use crate::library::LibraryController;
use crate::library::jobs::start_export_job_app;

pub struct ExportDialog;

impl ExportDialog {
    /// Open for `ids` (the context menu hands over `action_targets` — the
    /// whole selection when it contains the clicked asset, the asset alone
    /// otherwise). Refuses nothing else: a selection of only fonts and
    /// documents still exports, as plain copies.
    pub fn open(
        window: &mut Window,
        cx: &mut App,
        controller: Entity<LibraryController>,
        ids: Vec<uuid::Uuid>,
    ) {
        if ids.is_empty() {
            window.push_notification(
                Notification::warning(rust_i18n::t!("export.empty_selection").to_string()),
                cx,
            );
            return;
        }
        let assets: Vec<Asset> = controller
            .read(cx)
            .library
            .assets_by_ids(&ids)
            .unwrap_or_default();
        let (images, videos, others) = partition(&assets);
        if images.is_empty() && videos.is_empty() && others.is_empty() {
            window.push_notification(
                Notification::warning(rust_i18n::t!("export.empty_selection").to_string()),
                cx,
            );
            return;
        }

        let ffmpeg_missing = !trove_core::media::video::ffmpeg_available();
        let show_ffmpeg_warning = !videos.is_empty() && ffmpeg_missing;

        let controller2 = controller.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            let quality = cx.new(|cx| InputState::new(window, cx).placeholder("85".to_string()));
            let draft = cx.new(|_| ExportDraft {
                controller: controller2.clone(),
                images: images.clone(),
                videos: videos.clone(),
                others: others.len(),
                dest: None,
                image_format: None,
                jpeg_quality: quality.clone(),
                video_format: export::VideoExportFormat::Original,
                video_quality: VideoQuality::Balanced,
                video_max_height: None,
            });
            let draft_ok = draft.clone();
            dialog
                .title(rust_i18n::t!("export.title", count = assets.len()).to_string())
                .width(px(520.))
                .close_button(false)
                .child(super::with_close_x(
                    "export-close-x",
                    v_flex()
                        .gap_2()
                        .p_1()
                        .child(content(&draft, show_ffmpeg_warning, window, cx)),
                ))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(rust_i18n::t!("export.apply").to_string())
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let (dest, items, options) = draft_ok.update(cx, |d, cx| d.plan(cx));
                    if dest.is_none() {
                        window.push_notification(
                            Notification::warning(rust_i18n::t!("export.no_dest").to_string()),
                            cx,
                        );
                        return false;
                    }
                    if items.is_empty() {
                        window.push_notification(
                            Notification::warning(
                                rust_i18n::t!("export.nothing_resolvable").to_string(),
                            ),
                            cx,
                        );
                        return false;
                    }
                    start_export_job_app(
                        &draft_ok.read(cx).controller.clone(),
                        items,
                        options,
                        window,
                        cx,
                    );
                    true
                })
        });
    }
}

/// Dialog draft: the frozen selection plus the chosen targets. Held in an
/// entity so the content closure re-reads it every frame.
struct ExportDraft {
    controller: Entity<LibraryController>,
    images: Vec<Asset>,
    videos: Vec<Asset>,
    others: usize,
    dest: Option<PathBuf>,
    /// `None` keeps the originals' bytes.
    image_format: Option<ConvertFormat>,
    jpeg_quality: Entity<InputState>,
    video_format: VideoExportFormat,
    video_quality: VideoQuality,
    video_max_height: Option<u32>,
}

/// Split the selection into the three groups the dialog speaks about.
fn partition(assets: &[Asset]) -> (Vec<Asset>, Vec<Asset>, Vec<Asset>) {
    let mut images = Vec::new();
    let mut videos = Vec::new();
    let mut others = Vec::new();
    for asset in assets {
        match asset.kind {
            AssetKind::Image => images.push(asset.clone()),
            AssetKind::Video => videos.push(asset.clone()),
            _ => others.push(asset.clone()),
        }
    }
    (images, videos, others)
}

impl ExportDraft {
    /// Collect the background-executable plan: destination, items with
    /// resolved sources (fileless assets drop out here) and the options the
    /// job runs under.
    fn plan(
        &self,
        cx: &App,
    ) -> (
        Option<PathBuf>,
        Vec<ExportItem>,
        trove_core::tasks::export::ExportOptions,
    ) {
        let library = &self.controller.read(cx).library;
        let items: Vec<ExportItem> = self
            .images
            .iter()
            .chain(self.videos.iter())
            .filter_map(|asset| {
                let source = library.asset_file(asset.id)?;
                Some(ExportItem {
                    asset_id: asset.id,
                    source,
                    title: asset.title.clone().unwrap_or_else(|| asset.file_stem()),
                    kind: asset.kind,
                })
            })
            .collect();
        let jpeg_quality: u8 = self
            .jpeg_quality
            .read(cx)
            .value()
            .trim()
            .parse()
            .unwrap_or(85)
            .clamp(1, 100);
        let options = trove_core::tasks::export::ExportOptions {
            dest: self.dest.clone().unwrap_or_default(),
            items: Vec::new(),
            image_format: self.image_format,
            jpeg_quality,
            video_format: self.video_format,
            video_quality: self.video_quality,
            video_max_height: self.video_max_height,
        };
        (self.dest.clone(), items, options)
    }
}

/// The dialog body: per-kind targets, the no-ffmpeg warning when one is
/// needed, the destination picker and the others-copied note.
fn content(
    draft: &Entity<ExportDraft>,
    show_ffmpeg_warning: bool,
    window: &mut Window,
    cx: &mut App,
) -> Div {
    let (image_format, video_format, video_quality, video_max_height, dest, others) = {
        let d = draft.read(cx);
        (
            d.image_format,
            d.video_format,
            d.video_quality,
            d.video_max_height,
            d.dest.clone(),
            d.others,
        )
    };
    let quality_input = draft.read(cx).jpeg_quality.clone();

    // Counts line: what the frozen selection contains.
    let (images, videos) = {
        let d = draft.read(cx);
        (d.images.len(), d.videos.len())
    };
    let mut body = v_flex().gap_2().child(muted_label(
        rust_i18n::t!(
            "export.counts",
            images = images,
            videos = videos,
            others = others
        )
        .to_string(),
        cx,
    ));

    // Image target row.
    let mut image_row = h_flex().gap_2().items_center().child(muted_label(
        rust_i18n::t!("export.image_format").to_string(),
        cx,
    ));
    {
        let d = draft.clone();
        let mut options: Vec<(Option<ConvertFormat>, String)> =
            vec![(None, rust_i18n::t!("export.f_original").to_string())];
        options.extend(
            CONVERT_FORMATS
                .into_iter()
                .map(|picked| (Some(picked), image_label(picked))),
        );
        image_row = image_row.child(controls::dropdown_button(
            "export-image-format",
            image_format.map_or_else(
                || rust_i18n::t!("export.f_original").to_string(),
                image_label,
            ),
            options,
            image_format,
            move |picked: Option<ConvertFormat>, cx: &mut App| {
                d.update(cx, |d, cx| {
                    d.image_format = picked;
                    cx.notify();
                })
            },
            160.0,
            Anchor::TopLeft,
        ));
    }
    if image_format == Some(ConvertFormat::Jpeg) {
        image_row = image_row
            .child(muted_label(
                rust_i18n::t!("export.jpeg_quality").to_string(),
                cx,
            ))
            .child(
                Input::new(&quality_input)
                    .small()
                    .appearance(true)
                    .w(px(70.)),
            );
    }

    // Video target row, with quality and height cap when the target
    // re-encodes.
    let mut video_row = h_flex().gap_2().items_center().child(muted_label(
        rust_i18n::t!("export.video_format").to_string(),
        cx,
    ));
    {
        let d = draft.clone();
        let options: Vec<(VideoExportFormat, String)> = VIDEO_EXPORT_FORMATS
            .into_iter()
            .map(|picked| (picked, video_label(picked)))
            .collect();
        video_row = video_row.child(controls::dropdown_button(
            "export-video-format",
            video_label(video_format),
            options,
            video_format,
            move |picked: VideoExportFormat, cx: &mut App| {
                d.update(cx, |d, cx| {
                    d.video_format = picked;
                    cx.notify();
                })
            },
            200.0,
            Anchor::TopLeft,
        ));
    }
    if video_format.reencodes() {
        {
            let d = draft.clone();
            let options: Vec<(VideoQuality, String)> = export::VIDEO_QUALITIES
                .into_iter()
                .map(|picked| (picked, quality_label(picked)))
                .collect();
            video_row = video_row
                .child(muted_label(
                    rust_i18n::t!("export.video_quality").to_string(),
                    cx,
                ))
                .child(controls::dropdown_button(
                    "export-video-quality",
                    quality_label(video_quality),
                    options,
                    video_quality,
                    move |picked: VideoQuality, cx: &mut App| {
                        d.update(cx, |d, cx| {
                            d.video_quality = picked;
                            cx.notify();
                        })
                    },
                    120.0,
                    Anchor::TopLeft,
                ));
        }
        {
            let d = draft.clone();
            let options: Vec<(Option<u32>, String)> = VIDEO_MAX_HEIGHTS
                .into_iter()
                .map(|picked| (picked, height_label(picked)))
                .collect();
            video_row = video_row
                .child(muted_label(
                    rust_i18n::t!("export.resolution").to_string(),
                    cx,
                ))
                .child(controls::dropdown_button(
                    "export-video-height",
                    height_label(video_max_height),
                    options,
                    video_max_height,
                    move |picked: Option<u32>, cx: &mut App| {
                        d.update(cx, |d, cx| {
                            d.video_max_height = picked;
                            cx.notify();
                        })
                    },
                    120.0,
                    Anchor::TopLeft,
                ));
        }
    }

    // Destination picker — the same shape the convert dialog uses.
    let mut dest_row = h_flex()
        .gap_2()
        .items_center()
        .child(muted_label(rust_i18n::t!("export.dest").to_string(), cx));
    {
        let d = draft.clone();
        let handle = window.window_handle();
        dest_row = dest_row.child(
            Button::new("export-dest")
                .xsmall()
                .outline()
                .label(dest.as_ref().map_or_else(
                    || rust_i18n::t!("export.choose_dest").to_string(),
                    |p| p.display().to_string(),
                ))
                .on_click(move |_, _, cx| {
                    let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                        files: false,
                        directories: true,
                        multiple: false,
                        prompt: Some(rust_i18n::t!("export.dest_prompt").into_owned().into()),
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

    body = body
        .child(image_row)
        .child(video_row)
        .child(dest_row)
        .child(muted_label(
            rust_i18n::t!("export.note_others").to_string(),
            cx,
        ));
    if show_ffmpeg_warning {
        body = body.child(
            div()
                .text_xs()
                .text_color(cx.theme().warning)
                .child(rust_i18n::t!("export.note_no_ffmpeg").to_string()),
        );
    }
    body
}

/// Localized label for an image target ("Original" or the format's name).
fn image_label(format: ConvertFormat) -> String {
    let key = match format {
        ConvertFormat::Jpeg => "export.f_jpeg",
        ConvertFormat::Png => "export.f_png",
        ConvertFormat::WebP => "export.f_webp",
        ConvertFormat::Bmp => "export.f_bmp",
        ConvertFormat::Tiff => "export.f_tiff",
    };
    rust_i18n::t!(key).to_string()
}

/// Localized label for a video target.
fn video_label(format: VideoExportFormat) -> String {
    let key = match format {
        VideoExportFormat::Original => "export.f_original",
        VideoExportFormat::RemuxMp4 => "export.vf_remux",
        VideoExportFormat::Mp4H264 => "export.vf_mp4_h264",
        VideoExportFormat::Mp4H265 => "export.vf_mp4_h265",
        VideoExportFormat::MkvH264 => "export.vf_mkv_h264",
        VideoExportFormat::WebmVp9 => "export.vf_webm_vp9",
    };
    rust_i18n::t!(key).to_string()
}

/// Localized label for a quality level.
fn quality_label(quality: VideoQuality) -> String {
    let key = match quality {
        VideoQuality::High => "export.quality_high",
        VideoQuality::Balanced => "export.quality_balanced",
        VideoQuality::Compact => "export.quality_compact",
    };
    rust_i18n::t!(key).to_string()
}

/// Localized label for a height cap.
fn height_label(max_height: Option<u32>) -> String {
    match max_height {
        None => rust_i18n::t!("export.res_original").to_string(),
        Some(2160) => rust_i18n::t!("export.res_2160").to_string(),
        Some(1080) => rust_i18n::t!("export.res_1080").to_string(),
        Some(720) => rust_i18n::t!("export.res_720").to_string(),
        Some(other) => format!("{other}p"),
    }
}
