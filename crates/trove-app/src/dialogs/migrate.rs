//! Migrate from another asset manager: point at an Eagle `.library` folder or
//! a Billfish library, see what a scan would bring over, then run it as one
//! job. The scan is read-only; nothing lands in a library until the user
//! presses the button.
//!
//! Two run targets: from the main window the migration lands in the open
//! library (with a fresh/held preview); from the library-manager window there
//! is no open library, so a fresh one is created from the source folder's
//! name and the preview shows counts only.

use std::path::{Path, PathBuf};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::Button;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Sizable, WindowExt as _};
use gpui_kit::*;

use trove_core::services::migrate::{self, MigrationPlan};

use crate::components::controls::muted_label;
use crate::library::LibraryController;

pub struct MigrateDialog;

impl MigrateDialog {
    /// `controller` is the open main window's library — present when the
    /// dialog runs there, absent in the library-manager window.
    pub fn open(window: &mut Window, cx: &mut App, controller: Option<Entity<LibraryController>>) {
        window.open_dialog(cx, move |dialog, window, cx| {
            let draft = cx.new(|_| MigrateDraft {
                controller: controller.clone(),
                source: None,
                plan: None,
                fresh: 0,
                held: 0,
                scanning: false,
                unrecognized: false,
            });
            let draft_ok = draft.clone();
            dialog
                .title(rust_i18n::t!("migrate.title").to_string())
                .width(px(560.))
                .child(super::with_close_x(
                    "migrate-close-x",
                    v_flex().gap_2().p_1().child(content(&draft, window, cx)),
                ))
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(rust_i18n::t!("migrate.start").to_string())
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    let (controller, source, total) = {
                        let d = draft_ok.read(cx);
                        match (d.controller.clone(), d.source.clone(), d.plan.as_ref()) {
                            (Some(controller), Some(source), Some(plan)) => {
                                (Some(controller), source.clone(), plan.items.len())
                            }
                            (None, Some(source), Some(plan)) => {
                                (None, source.clone(), plan.items.len())
                            }
                            _ => {
                                window.push_notification(
                                    Notification::warning(
                                        rust_i18n::t!("migrate.unrecognized").to_string(),
                                    ),
                                    cx,
                                );
                                return false;
                            }
                        }
                    };
                    match controller {
                        Some(controller) => {
                            crate::library::jobs::start_migration_job_app(
                                &controller,
                                source,
                                total,
                                window,
                                cx,
                            );
                        }
                        None => {
                            let name = derive_library_name(&source);
                            crate::library::jobs::start_migration_standalone(
                                source, &name, total, window, cx,
                            );
                        }
                    }
                    true
                })
        });
    }
}

/// Dialog draft: the chosen folder and whatever the background scan made of
/// it. Held in an entity so the content closure re-reads it every frame.
struct MigrateDraft {
    /// The open main window's library, when the dialog runs there.
    controller: Option<Entity<LibraryController>>,
    source: Option<PathBuf>,
    plan: Option<MigrationPlan>,
    /// How many of the plan's files the library does not hold yet — the
    /// `(file name, size)` loose match the importer itself dedups by.
    /// Meaningful only with a controller.
    fresh: usize,
    held: usize,
    scanning: bool,
    unrecognized: bool,
}

/// The dialog body: the folder picker and whatever state the scan is in.
fn content(draft: &Entity<MigrateDraft>, window: &mut Window, cx: &mut App) -> Div {
    let (source, plan, scanning, unrecognized, fresh, held, has_controller) = {
        let d = draft.read(cx);
        (
            d.source.clone(),
            d.plan.clone(),
            d.scanning,
            d.unrecognized,
            d.fresh,
            d.held,
            d.controller.is_some(),
        )
    };
    // Folder picker.
    let mut source_row = h_flex()
        .gap_2()
        .items_center()
        .child(muted_label(rust_i18n::t!("migrate.choose").to_string(), cx));
    {
        let d = draft.clone();
        let handle = window.window_handle();
        source_row = source_row.child(
            Button::new("migrate-source")
                .xsmall()
                .outline()
                .label(source.as_ref().map_or_else(
                    || rust_i18n::t!("migrate.choose").to_string(),
                    |p| p.display().to_string(),
                ))
                .on_click(move |_, _, cx| {
                    let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                        files: false,
                        directories: true,
                        multiple: false,
                        prompt: Some(rust_i18n::t!("migrate.choose_prompt").into_owned().into()),
                    });
                    let d = d.clone();
                    cx.spawn(async move |cx| {
                        if let Ok(Ok(Some(paths))) = rx.await
                            && let Some(dir) = paths.first()
                        {
                            // The scan is background work; the fresh/held
                            // split needs the library, so it happens back on
                            // the main thread once the scan lands.
                            let scan_dir = dir.clone();
                            let scanned = cx
                                .background_executor()
                                .spawn(async move { migrate::scan(&scan_dir) })
                                .await;
                            let _ = handle.update(cx, |_, _, cx| {
                                d.update(cx, |d, cx| {
                                    d.plan = None;
                                    d.unrecognized = false;
                                    d.scanning = false;
                                    match scanned {
                                        Ok(plan) => {
                                            d.source = Some(plan.root.clone());
                                            // The fresh/held split reads the
                                            // library's own loose dedup key —
                                            // on the background thread, over a
                                            // read-only connection. The
                                            // manager-window variant (no
                                            // library open) shows counts only.
                                            if let Some(db) = d.controller.as_ref().map(|c| {
                                                c.read(cx).library.root().join("library.db")
                                            }) {
                                                let (fresh, held) =
                                                    migrate::preview_split(&db, &plan);
                                                d.fresh = fresh;
                                                d.held = held;
                                            }
                                            d.plan = Some(plan);
                                        }
                                        Err(_) => {
                                            d.unrecognized = true;
                                            d.source = Some(dir.clone());
                                        }
                                    }
                                    cx.notify();
                                });
                            });
                        }
                    })
                    .detach();
                }),
        );
    }

    let hint_key = if has_controller {
        "migrate.hint"
    } else {
        "migrate.hint_new"
    };
    let mut body = v_flex()
        .gap_2()
        .child(source_row)
        .child(muted_label(rust_i18n::t!(hint_key).to_string(), cx));

    if scanning {
        body = body.child(muted_label(
            rust_i18n::t!("migrate.scanning").to_string(),
            cx,
        ));
    } else if unrecognized {
        body = body.child(muted_label(
            rust_i18n::t!("migrate.unrecognized").to_string(),
            cx,
        ));
    } else if let Some(plan) = &plan {
        let summary = if has_controller {
            rust_i18n::t!(
                "migrate.plan",
                count = plan.items.len(),
                fresh = fresh,
                held = held,
                tags = plan.tag_names().len(),
                collections = plan.folders.len(),
                skipped = plan.skipped.len()
            )
            .to_string()
        } else {
            rust_i18n::t!(
                "migrate.plan_new",
                count = plan.items.len(),
                tags = plan.tag_names().len(),
                collections = plan.folders.len(),
                skipped = plan.skipped.len()
            )
            .to_string()
        };
        body = body.child(muted_label(summary, cx));
        let kinds = match plan.kind {
            migrate::SourceKind::Eagle => rust_i18n::t!("migrate.kind_eagle").to_string(),
            migrate::SourceKind::Billfish => rust_i18n::t!("migrate.kind_billfish").to_string(),
        };
        body = body.child(muted_label(kinds, cx));
    }
    body
}

/// A fresh library's name: the source folder's own, minus Eagle's
/// `.library` suffix. An empty result is fine — registration numbers it.
fn derive_library_name(source: &Path) -> String {
    source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .map(|n| n.strip_suffix(".library").unwrap_or(&n).trim().to_string())
        .unwrap_or_default()
}
