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

use std::sync::{Arc, OnceLock};

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::{ActiveTheme, IconName, Root, Sizable as _, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::AppView;
use crate::app::actions::RunPluginCommand;
use trove_core::config::{AppConfig, LibraryEntry};
use trove_core::paths;

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
    fn begin_rename(
        &mut self,
        entry: LibraryEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    /// The full-backup archive: the software configuration and every
    /// library's data in one zip. The heavy work runs on the background
    /// executor; the toast reports the outcome either way.
    fn export_backup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let suggested = trove_core::services::archive::backup_file_name();
        let rx = cx.prompt_for_new_path(&paths::data_dir(), Some(suggested.as_str()));
        let handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                let outcome = cx
                    .background_executor()
                    .spawn(async move {
                        trove_core::services::archive::create_full_backup(&path)
                    })
                    .await;
                let _ = handle.update(cx, |_, window, cx| {
                    let note = match outcome {
                        Ok(report) => Notification::success(
                            rust_i18n::t!(
                                "app.backup_done",
                                path = report.path.display().to_string()
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let config = AppConfig::load();
        let libraries = config.libraries.clone();
        let active_slug = config.active_slug().to_string();
        let view = cx.entity();
        // Toasts (backup results) and the delete dialog are layers the
        // window's root view has to draw, same as the app root does.
        let dialog_layer = gpui_kit::component::Root::render_dialog_layer(window, cx);
        let notification_layer = gpui_kit::component::Root::render_notification_layer(window, cx);

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
            .on_action(cx.listener(
                |this, _: &crate::app::actions::CancelEditor, window, cx| {
                    this.cancel_rename(window, cx);
                },
            ))
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
            .children(dialog_layer)
            .children(notification_layer)
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
        let mut list = v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
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
        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .overflow_y_scrollbar()
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
                                rust_i18n::t!(
                                    "app.version",
                                    version = env!("CARGO_PKG_VERSION")
                                )
                                .to_string(),
                            ),
                    )
                    .child(self.action_card(cx).mt_8()),
            )
    }

    /// The card: the create row, with the language picker as the footer row.
    /// There is no "open" row: entering a library is the sidebar's double
    /// click, and the backup export lives in the sidebar rows' kebab menus.
    fn action_card(&mut self, cx: &mut Context<Self>) -> Div {
        v_flex()
            .w_full()
            .bg(cx.theme().group_box)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius_lg)
            .child(self.create_row(cx))
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
                            PopupMenuItem::new(
                                rust_i18n::t!("settings.follow_system").to_string(),
                            )
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
                                        let _ =
                                            crate::app::i18n::set_language(Some(code.clone()));
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
/// writes the full-backup archive — the whole install, not just this row's
/// library. Delete confirms through a dialog naming what goes, so an
/// irreversible act never happens from a menu slip.
fn row_menu(
    view: &Entity<LibraryManagerView>,
    entry: LibraryEntry,
    in_use: bool,
) -> impl IntoElement {
    let menu_entry = entry.clone();
    let menu_view = view.clone();
    let export_view = view.clone();
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
        let export_view = export_view.clone();
        menu.min_w(px(140.))
            .item(
                PopupMenuItem::new(rust_i18n::t!("library_manager.backup").to_string()).on_click(
                    move |_, window, cx| {
                        export_view.update(cx, |this, cx| this.export_backup(window, cx));
                    },
                ),
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
            .on_ok(move |_, _, cx| {
                let mut config = AppConfig::load();
                let _ = config.forget_library(&commit_entry.slug);
                let _ = std::fs::remove_dir_all(commit_entry.dir());
                let _ = std::fs::remove_dir_all(commit_entry.cache_dir());
                cx.refresh_windows();
                true
            })
    });
}
