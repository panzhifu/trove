//! The asset manager: pick a library, make one, rename or delete the
//! ones that exist.
//!
//! Shown *instead of* the main window while no library exists — there is no
//! way past it, a library is where every asset record, every collection and
//! every per-library preference lives — and reachable at any time from the
//! File menu's 「素材库」 while a session is running.
//!
//! Two panes, in the order the decision reads: the library names on the
//! left, and creating plus managing on the right. A click selects; a double
//! click (or the Open button) enters.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme, Disableable as _, IconName, Root, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::AppView;
use trove_core::config::{AppConfig, LibraryEntry};

/// The asset manager's root view.
pub struct LibraryManagerView {
    focus_handle: FocusHandle,
    /// The name being typed for the new library.
    name: Entity<InputState>,
    /// The library picked in the left pane, by slug.
    selected: Option<String>,
    /// The new name being typed for the selected library.
    rename: Entity<InputState>,
    /// The delete button's two-step confirmation: the first click arms it,
    /// the second one deletes.
    confirm_delete: bool,
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
        let rename = cx.new(|cx| InputState::new(window, cx));
        cx.subscribe_in(&name, window, |this, _, event, window, cx| {
            // Enter in the name field is the same act as the button.
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.create(window, cx);
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
            rename,
            confirm_delete: false,
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

    /// Pick a library in the left pane: it becomes the manage pane's
    /// subject, and its name is staged for renaming.
    fn select(&mut self, slug: String, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(slug.as_str()) {
            return;
        }
        self.selected = Some(slug);
        self.confirm_delete = false;
        self.rename
            .update(cx, |input, cx| input.set_value(name, window, cx));
        cx.notify();
    }

    /// Rename the selected library to whatever the rename field holds. An
    /// empty field is a no-op — there is nothing to rename it to.
    fn rename_selected(&mut self, cx: &mut Context<Self>) {
        let Some(slug) = self.selected.clone() else {
            return;
        };
        let new_name = self.rename.read(cx).value().trim().to_string();
        if new_name.is_empty() {
            return;
        }
        let mut config = AppConfig::load();
        if let Err(error) = config.rename_library(&slug, &new_name) {
            tracing::error!(%error, "could not rename the library");
        }
        cx.notify();
    }

    /// Delete the selected library — the second click of a two-step
    /// confirmation. The registry entry goes, and with it the library's
    /// database and cache; the media files themselves were never inside.
    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if !self.confirm_delete {
            // First click: arm it. The button turns to "confirm" and the
            // next selection change disarms.
            self.confirm_delete = true;
            cx.notify();
            return;
        }
        let Some(slug) = self.selected.clone() else {
            return;
        };
        let config = AppConfig::load();
        let Some(entry) = config.libraries.iter().find(|l| l.slug == slug) else {
            return;
        };
        let entry = entry.clone();
        let mut config = AppConfig::load();
        let _ = config.forget_library(&slug);
        let _ = std::fs::remove_dir_all(entry.dir());
        let _ = std::fs::remove_dir_all(entry.cache_dir());
        self.selected = None;
        self.confirm_delete = false;
        cx.notify();
    }
}

impl Render for LibraryManagerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let config = AppConfig::load();
        let libraries = config.libraries.clone();
        let active_slug = config.active_slug().to_string();
        let view = cx.entity();

        let mut list = v_flex()
            .w(px(320.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .p_4()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("library_manager.libraries").to_string()),
            );
        if libraries.is_empty() {
            list = list.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("library_manager.no_libraries").to_string()),
            );
        }
        for entry in &libraries {
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
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .track_focus(&self.focus_handle)
            .child(
                TitleBar::new().child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::BOLD)
                        .child(rust_i18n::t!("library_manager.title").to_string()),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .items_stretch()
                    .child(list)
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .overflow_y_scrollbar()
                            .gap_5()
                            .px_8()
                            .py_8()
                            .child(self.create_section(cx))
                            .child(div().w_full().border_t_1().border_color(cx.theme().border))
                            .child(self.manage_section(cx)),
                    ),
            )
    }
}

impl LibraryManagerView {
    /// The create pane: what a library is, the name field, and the button.
    fn create_section(&mut self, cx: &mut Context<Self>) -> Div {
        v_flex()
            .gap_3()
            .child(
                h_flex().gap_2().items_center().child(IconName::Plus).child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::MEDIUM)
                        .child(rust_i18n::t!("library_manager.create").to_string()),
                ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("library_manager.create_desc").to_string()),
            )
            .child(
                v_flex()
                    .gap_1()
                    .w_full()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("library_manager.name").to_string()),
                    )
                    .child(Input::new(&self.name)),
            )
            .child(
                h_flex().w_full().justify_end().child(
                    Button::new("manager-create")
                        .primary()
                        .label(rust_i18n::t!("library_manager.create_button").to_string())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.create(window, cx);
                        })),
                ),
            )
    }

    /// The manage pane: the picked library, rename, open, delete.
    fn manage_section(&mut self, cx: &mut Context<Self>) -> Div {
        let heading = h_flex()
            .gap_2()
            .items_center()
            .child(IconName::Settings)
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::MEDIUM)
                    .child(rust_i18n::t!("library_manager.manage").to_string()),
            );

        let config = AppConfig::load();
        let active_slug = config.active_slug().to_string();
        let Some(entry) = self
            .selected
            .as_ref()
            .and_then(|slug| config.libraries.iter().find(|l| &l.slug == slug))
            .cloned()
        else {
            return v_flex().gap_3().child(heading).child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("library_manager.manage_none").to_string()),
            );
        };
        let in_use = entry.slug == active_slug;

        let mut heading = heading;
        if in_use {
            heading = heading.child(
                div()
                    .rounded_full()
                    .px_2()
                    .py_0p5()
                    .text_xs()
                    .bg(cx.theme().info.opacity(0.15))
                    .text_color(cx.theme().info)
                    .child(rust_i18n::t!("library_manager.in_use").to_string()),
            );
        }

        v_flex()
            .gap_3()
            .child(heading)
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::MEDIUM)
                    .child(entry.name.clone()),
            )
            // Rename: the field arrives pre-filled with the current name, so
            // the edit is a change-and-confirm, not a retyping exercise.
            .child(
                v_flex()
                    .gap_1()
                    .w_full()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("library_manager.rename_to").to_string()),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(div().flex_1().min_w_0().child(Input::new(&self.rename))),
                    ),
            )
            .child(
                h_flex().w_full().justify_end().child(
                    Button::new("manager-rename")
                        .outline()
                        .label(rust_i18n::t!("library_manager.rename").to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.rename_selected(cx);
                        })),
                ),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("manager-open")
                            .primary()
                            .disabled(in_use)
                            .label(rust_i18n::t!("library_manager.open").to_string())
                            .on_click({
                                let entry = entry.clone();
                                cx.listener(move |this, _, window, cx| {
                                    this.enter(entry.clone(), window, cx);
                                })
                            }),
                    )
                    .child({
                        let armed = self.confirm_delete;
                        let mut button = Button::new("manager-delete");
                        if armed {
                            button = button
                                .danger()
                                .label(rust_i18n::t!("library_manager.delete_confirm").to_string());
                        } else {
                            button = button
                                .danger()
                                .outline()
                                .label(rust_i18n::t!("library_manager.delete").to_string());
                        }
                        button
                            .disabled(in_use)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.delete_selected(cx);
                            }))
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("library_manager.delete_hint").to_string()),
            )
    }
}

/// One library in the left pane: its name, where it lives underneath, a
/// "in use" badge when it is the open one. Click selects it for the manage
/// pane; a double click enters straight away.
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
        .p_2()
        .rounded_md()
        .cursor_pointer()
        .when(selected, |row| row.bg(cx.theme().selection))
        .when(!selected, |row| {
            row.hover(|hovered| hovered.bg(cx.theme().muted))
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
                        this.select(entry.slug.clone(), entry.name.clone(), window, cx);
                    }
                });
            }
        })
        .child(
            v_flex()
                .min_w_0()
                .child(
                    h_flex()
                        .min_w_0()
                        .items_baseline()
                        .gap_2()
                        .child(div().text_sm().truncate().child(entry.name.clone()))
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
        .into_any_element()
}
