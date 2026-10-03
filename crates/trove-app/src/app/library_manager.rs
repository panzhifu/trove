//! The asset manager: pick a library, make one, rename or delete the
//! ones that exist.
//!
//! Shown *instead of* the main window while no library exists — there is no
//! way past it, a library is where every asset record, every collection and
//! every per-library preference lives — and reachable at any time from the
//! File menu's 「素材库」 while a session is running.
//!
//! Launcher layout: the libraries stack in a sidebar on the left (click
//! selects, double click enters, and rename turns the row itself into an
//! editor the way the collections panel does), each row's kebab carrying
//! the row's commands — full-backup export, rename, delete; the right side
//! is a centered hero — logo, name, version — over one card with the
//! commands: create, and the interface language.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use gpui::PathPromptOptions;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme, IconName, Root, Sizable as _, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::root::RepositoryNotice;

use super::AppView;
use super::settings_write;
use crate::app::actions::RunPluginCommand;
use crate::components::scrollbar;
use trove_core::config::{AppConfig, LibraryEntry};

/// The app logo, decoded once per process. gpui's `RenderImage` wants BGRA
/// bytes, so the PNG's channels are swapped the same way the screenshot
/// picker swaps its frames.
fn app_logo() -> &'static Arc<RenderImage> {
    static LOGO: OnceLock<Arc<RenderImage>> = OnceLock::new();
    LOGO.get_or_init(|| {
        let rgba = image::load_from_memory(include_bytes!("../../../../design/icon/trove-256.png"))
            .expect("embedded app icon is a valid PNG")
            .into_rgba8();
        let mut buffer = rgba.as_raw().clone();
        for pixel in buffer.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        let bgra = image::RgbaImage::from_raw(rgba.width(), rgba.height(), buffer)
            .expect("the buffer came from an image of the same size");
        Arc::new(RenderImage::new(vec![image::Frame::new(bgra)]))
    })
}

/// The asset manager's root view.
pub struct LibraryManagerView {
    focus_handle: FocusHandle,
    /// The name being typed for the new library.
    name: Entity<InputState>,
    /// The library picked in the sidebar, by slug: the open row's subject.
    selected: Option<String>,
    /// The reused inline rename editor (the collections panel's pattern) and
    /// the slug whose row it currently replaces. `None` when no rename is
    /// under way.
    editor: Entity<InputState>,
    renaming: Option<String>,
}

/// Handle of the open library manager, so `open` can focus instead of
/// stacking windows.
#[derive(Default)]
struct LibraryManagerWindowState(Option<AnyWindowHandle>);

impl gpui_kit::Global for LibraryManagerWindowState {}

/// Open the library manager over a running session — the File menu's
/// 「素材库」 — or focus it when it is already up.
pub fn open(cx: &mut App) {
    // Finish the leftovers of past deletes on the way in -- this is the window
    // where the user deletes libraries, so it is where reclaimed disk belongs.
    sweep_staged_removals();
    if let Some(state) = cx.try_global::<LibraryManagerWindowState>()
        && let Some(handle) = state.0
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1024.), px(720.)), cx)),
        ..crate::app::title_bar::window_options()
    };
    let handle = cx.open_window(options, |window, cx| {
        cx.set_global(LibraryManagerWindowState(Some(window.window_handle())));
        let view = cx.new(|cx| LibraryManagerView::new(window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    });
    if let Err(e) = handle {
        panic!("open library manager window: {e}");
    }
}

impl LibraryManagerView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("library_manager.name_placeholder").to_string())
        });
        cx.subscribe_in(&name, window, |this, _, event, window, cx| {
            // Enter in the name field is the same act as the button.
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.create(window, cx);
            }
            cx.notify();
        })
        .detach();

        let editor = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe_in(&editor, window, |this, _, event, window, cx| {
            // Enter in the inline editor commits the rename, exactly like the
            // collections panel's editor.
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit_rename(window, cx);
            }
            cx.notify();
        })
        .detach();

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            focus_handle,
            name,
            selected: None,
            editor,
            renaming: None,
        }
    }

    /// Record `entry` as the open library and hand over to the main window.
    /// This window only goes away once the new one is up, so a failure to
    /// open leaves the user somewhere they can still act.
    ///
    /// Two ways in, decided by whether a main window is already running: if
    /// it is, the switch hot-swaps the library inside that window — tray,
    /// watch service and settings all stay put — and this window closes. On
    /// first launch there is nothing to swap, so a main window is opened and
    /// this window hands over to it.
    fn enter(&mut self, entry: LibraryEntry, window: &mut Window, cx: &mut Context<Self>) {
        // A running session: swap the library inside its main window. The
        // active-library record only moves when the swap actually did, so a
        // refused switch (an import mid-flight) leaves everything consistent.
        if let Some(state) = cx.try_global::<crate::app::root::SessionState>()
            && let Some(controller) = state.0.as_ref().and_then(|weak| weak.upgrade())
        {
            let swapped = controller.update(cx, |ctl, cx| {
                let swapped = ctl
                    .swap_library(entry.dir(), entry.cache_dir())
                    .and_then(|()| AppConfig::load().set_active_library(&entry.slug))
                    .is_ok();
                if swapped {
                    // The old watch task scanned for the previous library;
                    // restart the resident watch on the new one.
                    if let Some(handle) = ctl.watch_handle {
                        let entity = cx.entity();
                        crate::library::jobs::start_watch_service(&entity, handle, cx);
                    }
                }
                swapped
            });
            // A refused swap keeps this window up, so the pick can be
            // retried or abandoned.
            if swapped {
                window.remove_window();
            }
            return;
        }

        if AppConfig::load().set_active_library(&entry.slug).is_err() {
            return;
        }
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1024.), px(720.)), cx)),
            ..crate::app::title_bar::window_options()
        };
        let opened = cx
            .open_window(options, |window, cx| {
                let view = cx.new(|cx| AppView::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .is_ok();
        if opened {
            window.remove_window();
        }
    }

    /// Register a library under the typed name, then enter it. An empty name
    /// is not an error — `add_library` numbers it.
    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_string();
        match AppConfig::load().add_library(&name) {
            Ok(entry) => self.enter(entry, window, cx),
            Err(error) => {
                // The window has nowhere to put a notice, and this only fails
                // on a filesystem that cannot create the directory at all.
                tracing::error!(%error, "could not create the library");
            }
        }
    }

    /// Pick a library in the sidebar: it becomes the open row's subject.
    fn select(&mut self, slug: String, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(slug.as_str()) {
            return;
        }
        self.selected = Some(slug);
        cx.notify();
    }

    /// Right-click → Rename: the row itself becomes a prefilled, focused
    /// editor — the same act as the collections panel's rename.
    fn begin_rename(&mut self, entry: LibraryEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |state, cx| {
            state.set_value(entry.name.clone(), window, cx);
        });
        self.renaming = Some(entry.slug);
        self.editor.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// Enter in the inline editor: commit the rename. An empty field is not a
    /// rename — the editor just closes on the old name.
    fn submit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slug) = self.renaming.clone() else {
            return;
        };
        let name = self.editor.read(cx).value().trim().to_string();
        self.renaming = None;
        self.editor
            .update(cx, |state, cx| state.set_value("", window, cx));
        if !name.is_empty() {
            let mut config = AppConfig::load();
            if let Err(error) = config.rename_library(&slug, &name) {
                tracing::error!(%error, "could not rename the library");
            }
        }
        // Other windows may carry the name too (the main window's title, the
        // tray), so refresh beyond this one.
        cx.refresh_windows();
    }

    /// Esc in the inline editor: drop it without committing. Fires only while
    /// the editor holds focus — the input's own Escape handler propagates the
    /// key, same as the collections panel.
    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.is_none() {
            return;
        }
        self.renaming = None;
        self.editor
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }

    /// Import a `.trove` repository package as a new library: pick the file,
    /// register a library named after the package, unpack in the background,
    /// then offer the hand-over — the success toast carries an "enter"
    /// button, since the manager window has no library of its own to swap.
    fn import_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity();
        let handle = window.window_handle();
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(
                rust_i18n::t!("app.import_repository_prompt")
                    .into_owned()
                    .into(),
            ),
        });
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(archive) = paths.first()
            {
                let read = {
                    let archive = archive.clone();
                    cx.background_executor()
                        .spawn(async move {
                            trove_core::services::repo_package::read_manifest(&archive)
                        })
                        .await
                };
                let manifest = match read {
                    Ok(manifest) => manifest,
                    Err(_) => {
                        let _ = handle.update(cx, |_, window, cx| {
                            window.push_notification(
                                Notification::warning(
                                    rust_i18n::t!("app.import_repository_failed").to_string(),
                                ),
                                cx,
                            );
                        });
                        return;
                    }
                };
                let entry = {
                    let name = manifest.library.name.clone();
                    let mut config = AppConfig::load();
                    config.add_library(&name).ok()
                };
                let Some(entry) = entry else {
                    return;
                };
                let _ = handle.update(cx, |_, window, cx| {
                    window.push_notification(
                        Notification::info(
                            rust_i18n::t!("app.import_repository_started").to_string(),
                        )
                        .id1::<RepositoryNotice>("repository-package"),
                        cx,
                    );
                });
                let archive = archive.clone();
                let dest = entry.dir();
                let outcome = cx
                    .background_executor()
                    .spawn(async move {
                        trove_core::services::repo_package::install_library_package(&archive, &dest)
                    })
                    .await;
                match outcome {
                    Ok(report) => {
                        // The action callback is an `Fn` (the toast may draw
                        // the button more than once), so the captured view
                        // and entry ride in `Rc`s and clone on click.
                        let view = std::rc::Rc::new(view.clone());
                        let entry = std::rc::Rc::new(entry);
                        let _ = handle.update(cx, |_, window, cx| {
                            window.push_notification(
                                Notification::success(
                                    rust_i18n::t!(
                                        "library_manager.import_repository_done",
                                        name = entry.name,
                                        assets = report.assets_total
                                    )
                                    .to_string(),
                                )
                                .action({
                                    let view = std::rc::Rc::clone(&view);
                                    let entry = std::rc::Rc::clone(&entry);
                                    move |_notification, _window, _cx| {
                                        let view = std::rc::Rc::clone(&view);
                                        let entry = std::rc::Rc::clone(&entry);
                                        Button::new("enter-imported-library")
                                            .primary()
                                            .label(rust_i18n::t!("migrate.enter").to_string())
                                            .on_click(move |_, window, cx| {
                                                view.update(cx, |this, cx| {
                                                    this.enter((*entry).clone(), window, cx);
                                                });
                                            })
                                    }
                                }),
                                cx,
                            );
                            cx.refresh_windows();
                        });
                    }
                    Err(_) => {
                        let mut config = AppConfig::load();
                        let _ = config.forget_library(&entry.slug);
                        let _ = std::fs::remove_dir_all(entry.dir());
                        let _ = handle.update(cx, |_, window, cx| {
                            window.push_notification(
                                Notification::warning(
                                    rust_i18n::t!("app.import_repository_failed").to_string(),
                                )
                                .id1::<RepositoryNotice>("repository-package"),
                                cx,
                            );
                        });
                    }
                }
            }
        })
        .detach();
    }

    /// The repository package for one row's library: records, its own media,
    /// and copies of the linked files, in one `.trove` file. The heavy work
    /// runs on the background executor — the writer snapshots the database
    /// itself, so an open library is safe — and the toast reports the
    /// outcome either way.
    fn export_repository(
        &mut self,
        entry: LibraryEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let suggested = trove_core::services::repo_package::package_file_name(&entry.name);
        let rx = cx.prompt_for_new_path(&entry.dir(), Some(suggested.as_str()));
        let handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                // The save dialog has no extension filter; the format's
                // extension is the app's to enforce.
                let path = match path.extension().and_then(|e| e.to_str()) {
                    Some(e) if e.eq_ignore_ascii_case("trove") => path,
                    _ => path.with_extension("trove"),
                };
                let dir = entry.dir();
                let name = entry.name.clone();
                let outcome = cx
                    .background_executor()
                    .spawn(async move {
                        trove_core::services::repo_package::export_library_package(
                            &dir, &name, &path,
                        )
                    })
                    .await;
                let _ = handle.update(cx, |_, window, cx| {
                    let note = match outcome {
                        Ok(report) if report.linked_missing == 0 => Notification::success(
                            rust_i18n::t!(
                                "app.export_repository_done",
                                path = report.path.display().to_string(),
                                files = report.files
                            )
                            .to_string(),
                        ),
                        Ok(report) => Notification::warning(
                            rust_i18n::t!(
                                "app.export_repository_missing",
                                path = report.path.display().to_string(),
                                count = report.linked_missing
                            )
                            .to_string(),
                        ),
                        Err(e) => Notification::warning(
                            rust_i18n::t!("app.export_failed", error = e.to_string()).to_string(),
                        ),
                    };
                    window.push_notification(note, cx);
                });
            }
        })
        .detach();
    }
}

impl Render for LibraryManagerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let config = AppConfig::load();
        let libraries = config.libraries.clone();
        let active_slug = config.active_slug().to_string();
        let view = cx.entity();

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .track_focus(&self.focus_handle)
            // The inline rename editor's Escape. The input's own Escape
            // handler propagates the key, so this fires only while the
            // editor input holds focus — same arrangement as the collections
            // panel.
            .key_context("LibraryManager")
            .on_action(
                cx.listener(|this, _: &crate::app::actions::Cancel, window, cx| {
                    this.cancel_rename(window, cx);
                }),
            )
            // Plugin commands are global chords: this window answers them
            // too, even though it never opens the asset grid's context.
            .on_action(|action: &RunPluginCommand, window, cx| {
                crate::plugins::run_command(&action.command, window, cx);
            })
            .child(TitleBar::new())
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.sidebar(&libraries, &active_slug, view, cx))
                    .child(self.main_pane(cx)),
            )
    }
}

impl LibraryManagerView {
    /// The sidebar: the library rows, each with its own kebab menu; a row
    /// being renamed is replaced by its inline editor.
    fn sidebar(
        &self,
        libraries: &[LibraryEntry],
        active_slug: &str,
        view: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut list = scrollbar::vertical(v_flex().flex_1().min_h_0())
            .px_2()
            .pt_3()
            .pb_3()
            .gap_0p5();
        if libraries.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("library_manager.no_libraries").to_string()),
            );
        }
        for entry in libraries {
            if self.renaming.as_deref() == Some(entry.slug.as_str()) {
                // The renamed row itself is replaced by the inline editor.
                list = list.child(inline_editor(&self.editor));
                continue;
            }
            let selected = self.selected.as_deref() == Some(entry.slug.as_str());
            list = list.child(library_row(
                &view,
                entry.clone(),
                selected,
                entry.slug == active_slug,
                cx,
            ));
        }

        v_flex()
            .w(px(264.))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(list)
    }

    /// The right pane: the hero (logo, name, version) above the action card.
    fn main_pane(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        scrollbar::vertical(v_flex().flex_1().h_full().min_w_0())
            .items_center()
            .px_8()
            .pt_12()
            .pb_8()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(560.))
                    .items_center()
                    .child(img(ImageSource::Render(app_logo().clone())).size_24())
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .mt_4()
                            .child("Trove"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .mt_1()
                            .child(
                                rust_i18n::t!("app.version", version = env!("CARGO_PKG_VERSION"))
                                    .to_string(),
                            ),
                    )
                    .child(self.action_card(cx).mt_8()),
            )
    }

    /// The card: the create row, two import rows, and the language picker as
    /// the footer row. There is no "open" row: entering a library is the
    /// sidebar's double click, and the backup export lives in the sidebar
    /// rows' kebab menus.
    fn action_card(&mut self, cx: &mut Context<Self>) -> Div {
        v_flex()
            .w_full()
            .bg(cx.theme().group_box)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius_lg)
            .child(self.create_row(cx))
            .child(self.import_repository_row(cx))
            .child(self.migrate_row(cx))
            .child(self.language_footer(cx))
    }

    /// One card row: the command name on the left, the controls on the
    /// right.
    fn card_row(
        &self,
        text: String,
        as_title: bool,
        control: AnyElement,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut text_el = div().text_sm().flex_1().min_w_0();
        text_el = if as_title {
            text_el.font_weight(FontWeight::MEDIUM)
        } else {
            text_el.text_color(cx.theme().muted_foreground)
        };
        h_flex()
            .w_full()
            .items_center()
            .gap_4()
            .px_4()
            .py_4()
            .child(text_el.child(text))
            .child(control)
    }

    /// Import a `.trove` repository package as a new library.
    fn import_repository_row(&mut self, cx: &mut Context<Self>) -> Div {
        self.card_row(
            rust_i18n::t!("library_manager.import_repository").to_string(),
            true,
            Button::new("manager-import-repository")
                .outline()
                .label(rust_i18n::t!("library_manager.import_repository_button").to_string())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.import_repository(window, cx);
                }))
                .into_any_element(),
            cx,
        )
    }

    /// Migrate from Eagle / Billfish: a fresh library is created from the
    /// source folder and the job runs on its own.
    fn migrate_row(&mut self, cx: &mut Context<Self>) -> Div {
        self.card_row(
            rust_i18n::t!("library_manager.import_migrate").to_string(),
            true,
            Button::new("manager-import-migrate")
                .outline()
                .label(rust_i18n::t!("library_manager.import_migrate_button").to_string())
                .on_click(cx.listener(|_this, _, window, cx| {
                    crate::dialogs::migrate::MigrateDialog::open(window, cx, None);
                }))
                .into_any_element(),
            cx,
        )
    }

    /// Create: the name field (optional — an empty name is auto-numbered)
    /// and the primary commit.
    fn create_row(&mut self, cx: &mut Context<Self>) -> Div {
        self.card_row(
            rust_i18n::t!("library_manager.new_library").to_string(),
            true,
            h_flex()
                .items_center()
                .flex_shrink_0()
                .gap_2()
                .child(Input::new(&self.name).w_40())
                .child(
                    Button::new("manager-create")
                        .primary()
                        .label(rust_i18n::t!("library_manager.create_button").to_string())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.create(window, cx);
                        })),
                )
                .into_any_element(),
            cx,
        )
    }

    /// The card's footer: the interface language, switchable live — the same
    /// picker the settings dialog carries, because a first launch has no
    /// library yet and therefore no way into those settings.
    fn language_footer(&mut self, cx: &mut Context<Self>) -> Div {
        let language = AppConfig::load().language;
        let current = match language.as_deref() {
            Some(code) => crate::app::i18n::SUPPORTED
                .iter()
                .find(|(c, _)| *c == code)
                .map(|(_, name)| SharedString::from(*name))
                .unwrap_or_else(|| SharedString::from(code)),
            None => rust_i18n::t!("settings.follow_system").to_owned().into(),
        };

        h_flex()
            .w_full()
            .items_center()
            .justify_center()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("manager-language")
                    .outline()
                    .dropdown_caret(true)
                    .w_64()
                    .label(current.to_string())
                    .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, _| {
                        let mut picker = menu.min_w(px(180.)).item(
                            PopupMenuItem::new(rust_i18n::t!("settings.follow_system").to_string())
                                .checked(language.is_none())
                                .on_click(|_, _, cx| {
                                    let _ = crate::app::i18n::set_language(None);
                                    cx.refresh_windows();
                                    crate::app::title_bar::apply_menus(cx);
                                }),
                        );
                        for (code, name) in crate::app::i18n::SUPPORTED {
                            let code = code.to_string();
                            picker = picker.item(
                                PopupMenuItem::new(*name)
                                    .checked(language.as_deref() == Some(code.as_str()))
                                    .on_click(move |_, _, cx| {
                                        let _ = crate::app::i18n::set_language(Some(code.clone()));
                                        cx.refresh_windows();
                                        crate::app::title_bar::apply_menus(cx);
                                    }),
                            );
                        }
                        picker
                    }),
            )
    }
}

/// One library in the sidebar: its name, where it lives underneath, a
/// "in use" badge when it is the open one, and a kebab with the row's
/// commands (export, rename, delete). Click selects it for the card; a
/// double click enters straight away.
fn library_row(
    view: &Entity<LibraryManagerView>,
    entry: LibraryEntry,
    selected: bool,
    in_use: bool,
    cx: &mut Context<LibraryManagerView>,
) -> AnyElement {
    let dir = entry.dir().display().to_string();
    h_flex()
        .id(SharedString::from(format!("manager-{}", entry.slug)))
        .w_full()
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .when(selected, |row| row.bg(cx.theme().sidebar_accent))
        .when(!selected, |row| {
            row.hover(|hovered| hovered.bg(cx.theme().sidebar_accent.opacity(0.5)))
        })
        .on_click({
            let view = view.clone();
            let entry = entry.clone();
            move |event: &ClickEvent, window, cx| {
                // A double click enters straight away; a single click only
                // selects. Keyboard "clicks" never enter.
                let double_click = match event {
                    ClickEvent::Mouse(click) => click.up.click_count >= 2,
                    ClickEvent::Keyboard(_) | ClickEvent::Touch(_) => false,
                };
                view.update(cx, |this, cx| {
                    if double_click {
                        this.enter(entry.clone(), window, cx);
                    } else {
                        this.select(entry.slug.clone(), cx);
                    }
                });
            }
        })
        .child(
            v_flex()
                .min_w_0()
                .flex_1()
                .child(
                    h_flex()
                        .min_w_0()
                        .items_baseline()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().sidebar_foreground)
                                .truncate()
                                .child(entry.name.clone()),
                        )
                        .when(in_use, |line| {
                            line.child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(cx.theme().info)
                                    .child(rust_i18n::t!("library_manager.in_use").to_string()),
                            )
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .truncate()
                        .text_color(cx.theme().muted_foreground)
                        .child(dir),
                ),
        )
        .child(
            // The kebab owns its clicks: without stopping the propagation the
            // row would also take them — selecting at best, entering on a
            // double click at worst.
            div()
                .flex_shrink_0()
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .child(row_menu(view, entry.clone(), in_use)),
        )
        .into_any_element()
}

/// One row's kebab: the commands that act on this library. The export item
/// writes this row's library as one `.trove` repository package — records,
/// its own media, and copies of the linked files. Delete confirms through a
/// dialog naming what goes, so an irreversible act never happens from a menu
/// slip.
fn row_menu(
    view: &Entity<LibraryManagerView>,
    entry: LibraryEntry,
    in_use: bool,
) -> impl IntoElement {
    let menu_entry = entry.clone();
    let menu_view = view.clone();
    let repo_view = view.clone();
    let repo_entry = entry.clone();
    Button::new(SharedString::from(format!(
        "manager-row-menu-{}",
        entry.slug
    )))
    .ghost()
    .xsmall()
    .icon(IconName::EllipsisVertical)
    .tooltip(rust_i18n::t!("library_manager.library_actions").to_string())
    .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, _| {
        let rename_entry = menu_entry.clone();
        let rename_view = menu_view.clone();
        let delete_entry = menu_entry.clone();
        let repo_entry = repo_entry.clone();
        let repo_view = repo_view.clone();
        menu.min_w(px(140.))
            .item(
                PopupMenuItem::new(rust_i18n::t!("library_manager.export_repository").to_string())
                    .on_click(move |_, window, cx| {
                        repo_view.update(cx, |this, cx| {
                            this.export_repository(repo_entry.clone(), window, cx)
                        });
                    }),
            )
            .item(
                PopupMenuItem::new(rust_i18n::t!("library_manager.rename").to_string()).on_click(
                    move |_, window, cx| {
                        rename_view.update(cx, |this, cx| {
                            this.begin_rename(rename_entry.clone(), window, cx)
                        });
                    },
                ),
            )
            .item(
                PopupMenuItem::new(rust_i18n::t!("library_manager.delete").to_string())
                    .disabled(in_use)
                    .on_click(move |_, window, cx| {
                        open_delete_confirm(delete_entry.clone(), window, cx)
                    }),
            )
    })
}

/// The tail every staged directory carries, and how a later run knows what it
/// is looking at: `<parent>/<name>.deleting-<stamp>`.
const STAGED_MARK: &str = ".deleting-";

/// A library directory the user asked deleted, moved aside under a name that
/// says so, instead of being unlinked where it stands.
///
/// The reason is the failure mode of `remove_dir_all`: it deletes as it walks, so
/// a refusal halfway through -- a file held open, a permission change, a full disk
/// reported late -- leaves a directory that is neither the library it was nor gone.
/// Nothing can put that back. A rename within one directory can fail too, but it
/// fails *whole*: either the folder is still the library, or it is this folder,
/// staged and findable by name. So the destructive step happens last, on a path
/// that no longer answers to the library list, and a leftover from it is a
/// directory a later run can spot and finish off (see
/// [`sweep_staged_removals`]).
///
/// `Ok(None)` means there was nothing to move: a library created but never opened
/// has no cache directory at all, and that is a delete that already succeeded, not
/// a failure.
fn stage_for_removal(dir: &Path, stamp: &str) -> std::io::Result<Option<PathBuf>> {
    if !dir.exists() {
        return Ok(None);
    }
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| std::io::Error::other("a library directory with no name"))?;
    let staged = dir.with_file_name(format!("{name}{STAGED_MARK}{stamp}"));
    std::fs::rename(dir, &staged)?;
    Ok(Some(staged))
}

/// Put staged directories back where the library list expects them.
///
/// Only used when a step that comes *after* the staging fails, which is the one
/// point where the whole delete can still be undone: the entries still exist, the
/// data was never touched, so the honest outcome is "nothing happened" rather than
/// a library listed against a folder it can no longer find.
fn put_back(staged: &[(PathBuf, PathBuf)]) {
    for (original, to) in staged.iter().rev() {
        if let Err(error) = std::fs::rename(to, original) {
            // The window where this was recoverable is closing: the folder is
            // staged and the entry still points at the original name. Say which
            // is which, because that is what a person needs in front of a shell.
            tracing::error!(
                from = %to.display(),
                to = %original.display(),
                %error,
                "a staged library directory could not be put back"
            );
        }
    }
}

/// Finish the delete: remove staged directories that the entry no longer refers
/// to. Failures are left staged (and reported) rather than retried here, because
/// the next [`sweep_staged_removals`] is a better place to try -- the reason a
/// folder refuses now (a running process holding it open, say) is often gone by
/// then.
fn clear_staged(staged: &[(PathBuf, PathBuf)]) -> Vec<(PathBuf, String)> {
    staged
        .iter()
        .filter_map(|(_, to)| {
            std::fs::remove_dir_all(to)
                .err()
                .map(|error| (to.clone(), error.to_string()))
        })
        .collect()
}

/// Every directory under `roots` that is staged and not yet gone.
///
/// Sorted by name so a repeated sweep works through them in a stable order, and
/// the "is this a staged folder" test is the marker alone: a stale directory from
/// some other cause is nobody's to delete.
fn find_staged(roots: impl IntoIterator<Item = impl AsRef<std::path::Path>>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root.as_ref()) else {
            continue; // no such root yet -- nothing staged under it
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains(STAGED_MARK) {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    found
}

/// Delete the leftovers of past library deletions, called when the library
/// manager opens.
///
/// A delete that staged its folders and then died before removing them leaves
/// exactly one kind of trace -- a `*.deleting-<stamp>` directory beside the
/// libraries -- and it belongs to a library the user already chose to delete. So
/// the manager, which is where that choice lives, finishes the job on the way in.
/// The count is logged rather than shown: the folders are not in the library list
/// and cannot reappear there, so a dialog about reclaimed disk would be talking
/// about something the user has no way to have noticed.
pub(crate) fn sweep_staged_removals() {
    let staged = find_staged([trove_core::paths::libraries_dir(), cache_roots()]);
    if staged.is_empty() {
        return;
    }
    let mut cleared = 0_usize;
    for dir in &staged {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => cleared += 1,
            Err(error) => {
                tracing::warn!(path = %dir.display(), %error, "a staged library directory is still on disk");
            }
        }
    }
    tracing::info!(
        cleared,
        left = staged.len() - cleared,
        "finished past library deletions on opening the library manager"
    );
}

/// Where per-library caches live -- the sibling of
/// [`trove_core::paths::library_cache_dir`], whose parent this sweeps.
fn cache_roots() -> PathBuf {
    trove_core::paths::cache_dir().join("libraries")
}

/// The inline editor that replaces a row while it is renamed — the
/// collections panel's editor row, transplanted.
fn inline_editor(editor: &Entity<InputState>) -> AnyElement {
    h_flex()
        .w_full()
        .px_1()
        .py_0p5()
        .child(Input::new(editor).small().appearance(true))
        .into_any_element()
}

/// Delete `entry`: a confirmation dialog that names the library and what goes
/// with it, with the destructive red as the commit button. The library's
/// database and cache go; the media files were never inside.
fn open_delete_confirm(entry: LibraryEntry, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, _| {
        let commit_entry = entry.clone();
        dialog
            .title(
                rust_i18n::t!(
                    "library_manager.delete_dialog_title",
                    name = entry.name.clone()
                )
                .to_string(),
            )
            .width(px(360.))
            .close_button(false)
            .child(
                div()
                    .text_sm()
                    .p_1()
                    .child(rust_i18n::t!("library_manager.delete_hint").to_string()),
            )
            .button_props(
                DialogButtonProps::default()
                    .ok_text(rust_i18n::t!("library_manager.delete_confirm").to_string())
                    .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                    .cancel_text(rust_i18n::t!("library_manager.cancel").to_string())
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                // Deleting a library runs in three steps -- stage the folders
                // aside, write the entry away, then remove what is staged -- so
                // that the one thing that cannot be taken back, an unlink that
                // dies halfway, happens last and only to a path the library list
                // no longer refers to. See [`stage_for_removal`].
                let data = commit_entry.dir();
                let cache = commit_entry.cache_dir();
                let stamp = trove_core::model::now().format("%Y%m%d-%H%M%S").to_string();
                let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
                for dir in [&data, &cache] {
                    match stage_for_removal(dir, &stamp) {
                        Ok(Some(to)) => staged.push((dir.clone(), to)),
                        // Nothing there: a library that never opened has no cache
                        // directory, and that half is already done.
                        Ok(None) => {}
                        Err(error) => {
                            // The delete never started. Every folder that did move
                            // moves back, the entry stays, and the row is still
                            // openable -- so the message can honestly say the
                            // library is where it was.
                            put_back(&staged);
                            tracing::warn!(
                                path = %dir.display(),
                                %error,
                                "a library delete could not begin"
                            );
                            window.push_notification(
                                Notification::warning(
                                    rust_i18n::t!(
                                        "library_manager.delete_failed",
                                        name = commit_entry.name.clone(),
                                        path = dir.display().to_string(),
                                        error = error.to_string(),
                                    )
                                    .to_string(),
                                ),
                                cx,
                            );
                            cx.refresh_windows();
                            return true;
                        }
                    }
                }

                let mut config = AppConfig::load();
                let outcome = config.forget_library(&commit_entry.slug);
                settings_write::note(
                    match &outcome {
                        Ok(()) => Ok(()),
                        Err(error) => Err(error),
                    },
                    "library forgotten",
                );
                if let Err(error) = &outcome {
                    put_back(&staged);
                    tracing::warn!(
                        %error,
                        "a library delete was rolled back: its entry could not be removed"
                    );
                    window.push_notification(
                        Notification::warning(
                            rust_i18n::t!(
                                "library_manager.delete_rolled_back",
                                name = commit_entry.name.clone(),
                                error = error.to_string(),
                            )
                            .to_string(),
                        ),
                        cx,
                    );
                    cx.refresh_windows();
                    return true;
                }

                // Past this point the library is gone from the list, and the only
                // thing left is bytes. A folder that refuses now stays staged --
                // named `*.deleting-<stamp>`, finished off the next time the
                // library manager opens -- rather than being retried into the same
                // refusal; whatever held it open is often gone by then.
                let leftovers = clear_staged(&staged);
                for (path, error) in &leftovers {
                    tracing::warn!(
                        path = %path.display(),
                        error,
                        "a deleted library's files are still staged on disk"
                    );
                }
                if !leftovers.is_empty() {
                    window.push_notification(
                        Notification::warning(
                            rust_i18n::t!(
                                "library_manager.delete_staged",
                                name = commit_entry.name.clone(),
                                count = leftovers.len() as i64,
                            )
                            .to_string(),
                        ),
                        cx,
                    );
                }
                cx.refresh_windows();
                true
            })
    });
}

#[cfg(test)]
mod tests {
    use super::{STAGED_MARK, clear_staged, find_staged, put_back, stage_for_removal};
    use std::path::{Path, PathBuf};

    fn unique_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trove-libmgr-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn library_at(root: &Path, slug: &str) -> PathBuf {
        let dir = root.join(slug);
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        std::fs::write(dir.join("library.db"), b"catalog").unwrap();
        dir
    }

    /// Staging moves the whole folder and leaves nothing behind at the old name.
    ///
    /// The point of the marker is that a *rename* cannot half-happen: either the
    /// folder is still the library, or it is this folder.
    #[test]
    fn a_staged_library_moves_whole_and_keeps_its_bytes() {
        let root = unique_root("stage");
        let data = library_at(&root, "work");
        let before = data.join("library.db");
        assert!(before.is_file());

        let staged = stage_for_removal(&data, "20260928-120000")
            .expect("a plain directory stages")
            .expect("it was there");
        assert!(!data.exists(), "nothing is left at the library's own name");
        assert!(
            staged
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains(STAGED_MARK),
            "and the new name says what it is: {staged:?}"
        );
        assert_eq!(
            staged.parent(),
            data.parent(),
            "beside the libraries, not away"
        );
        assert_eq!(
            std::fs::read(staged.join("library.db")).unwrap(),
            b"catalog",
            "the folder moved with its database inside"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Absent counts as staged-nothing: a library that was created but never
    /// opened has no cache directory, and reporting that as a failure would strand
    /// the entry of a library that really was deleted.
    #[test]
    fn an_absent_directory_is_not_a_failure() {
        let root = unique_root("absent");
        let missing = root.join("never-opened-cache");
        assert!(
            stage_for_removal(&missing, "20260928-120000")
                .expect("nothing to move is not an error")
                .is_none(),
            "and it says so rather than inventing a folder"
        );
        assert!(!missing.exists(), "it must not create one either");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A rollback puts every staged folder back under the name the entry uses --
    /// the one moment a delete is still fully reversible.
    #[test]
    fn putting_back_restores_the_names_the_entry_points_at() {
        let root = unique_root("rollback");
        let data = library_at(&root, "work");
        let cache = root.join("work-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let staged: Vec<(PathBuf, PathBuf)> = [data.as_path(), cache.as_path()]
            .into_iter()
            .map(|dir| {
                let to = stage_for_removal(dir, "20260928-120001").unwrap().unwrap();
                (dir.to_path_buf(), to)
            })
            .collect();
        assert!(!data.exists() && !cache.exists());

        put_back(&staged);
        assert!(
            data.join("library.db").is_file(),
            "the library is where it was"
        );
        assert!(cache.exists(), "and so is its cache");
        assert!(
            find_staged([&root]).is_empty(),
            "nothing staged is left behind to sweep"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The sweep's whole safety property: it recognises a folder by the marker and
    /// by nothing else, so a live library beside it is untouched.
    #[test]
    fn only_a_staged_folder_is_offered_to_the_sweep() {
        let root = unique_root("sweep");
        let live = library_at(&root, "keepme");
        let staged = root.join(format!("gone{STAGED_MARK}20260928-120002"));
        std::fs::create_dir_all(&staged).unwrap();
        let other = root.join("not-a-library");
        std::fs::create_dir_all(&other).unwrap();

        let found = find_staged([&root]);
        assert_eq!(found, vec![staged.clone()], "the marker is the only test");
        assert!(
            live.join("library.db").is_file(),
            "a library is not a leftover"
        );

        let leftovers = clear_staged(&[(live.clone(), staged.clone())]);
        assert!(
            leftovers.is_empty(),
            "the staged folder went: {leftovers:?}"
        );
        assert!(!staged.exists());
        assert!(
            live.exists(),
            "and the pair's other half was never the thing to delete"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// A folder that refuses removal is reported, not swallowed -- it stays staged
    /// and named so the next sweep can finish what this one could not.
    ///
    /// A regular file is the deterministic refusal: `remove_dir_all` will not take
    /// one apart, so the test does not depend on permissions, a busy disk, or who
    /// runs it.
    #[test]
    fn a_refusal_is_handed_back_with_its_path() {
        let root = unique_root("refuse");
        let not_a_dir = root.join(format!("work{STAGED_MARK}20260928-120003"));
        std::fs::write(&not_a_dir, b"x").unwrap();

        let leftovers = clear_staged(&[(root.join("work"), not_a_dir.clone())]);
        assert_eq!(leftovers.len(), 1, "the refusal was reported");
        assert_eq!(leftovers[0].0, not_a_dir, "and named");
        assert!(
            !leftovers[0].1.is_empty(),
            "with the reason the log line needs"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
