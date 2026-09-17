//! The welcome window: pick a library, or make one.
//!
//! Shown *instead of* the main window while no library exists. There is no
//! way past it: a library is where every asset record, every collection and
//! every per-library preference lives, so the application has nothing to open
//! until one exists.
//!
//! Two panes, in the order the decision reads: the libraries that already
//! exist on the left, and the one-step act of making a new one on the right.

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, IconName, Root, TitleBar};
use gpui_kit::*;

use super::AppView;
use trove_core::config::{AppConfig, LibraryEntry};

/// The welcome window root view.
pub struct WelcomeView {
    focus_handle: FocusHandle,
    /// The name being typed for the new library.
    name: Entity<InputState>,
}

/// Handle of the open welcome window, so `open` can focus instead of
/// stacking windows.
#[derive(Default)]
struct WelcomeWindowState(Option<AnyWindowHandle>);

impl gpui_kit::Global for WelcomeWindowState {}

/// Open the library manager over a running session — the File menu's
/// 「素材库」 — or focus it when it is already up.
pub fn open(cx: &mut App) {
    if let Some(state) = cx.try_global::<WelcomeWindowState>()
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
        cx.set_global(WelcomeWindowState(Some(window.window_handle())));
        let view = cx.new(|cx| WelcomeView::new(window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    });
    if let Err(e) = handle {
        panic!("open library manager window: {e}");
    }
}

impl WelcomeView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("welcome.name_placeholder").to_string())
        });
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
        Self { focus_handle, name }
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
}

impl Render for WelcomeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let libraries = AppConfig::load().libraries;
        // An empty registry is a normal first run, not an error, so the left
        // pane says what to do rather than showing nothing.
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
                    .child(rust_i18n::t!("welcome.libraries").to_string()),
            );
        if libraries.is_empty() {
            list = list.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("welcome.no_libraries").to_string()),
            );
        }
        for entry in libraries {
            list = list.child(library_row(&view, entry, cx));
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
                        .child(rust_i18n::t!("welcome.title").to_string()),
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
                        // Making one.
                        v_flex()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .justify_center()
                            .gap_3()
                            .px_8()
                            .child(
                                h_flex().gap_2().items_center().child(IconName::Plus).child(
                                    div()
                                        .text_base()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(rust_i18n::t!("welcome.create").to_string()),
                                ),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(rust_i18n::t!("welcome.create_desc").to_string()),
                            )
                            .child(
                                v_flex()
                                    .gap_1()
                                    .w_full()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(rust_i18n::t!("welcome.name").to_string()),
                                    )
                                    .child(Input::new(&self.name)),
                            )
                            .child(
                                h_flex().w_full().justify_end().child(
                                    Button::new("welcome-create")
                                        .primary()
                                        .label(rust_i18n::t!("welcome.create_button").to_string())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.create(window, cx);
                                        })),
                                ),
                            ),
                    ),
            )
    }
}

/// One library the user can enter: its name, and where it lives underneath.
fn library_row(
    view: &Entity<WelcomeView>,
    entry: LibraryEntry,
    cx: &mut Context<WelcomeView>,
) -> AnyElement {
    let dir = entry.dir().display().to_string();
    h_flex()
        .id(SharedString::from(format!("welcome-{}", entry.slug)))
        .w_full()
        .items_center()
        .justify_between()
        .gap_2()
        .p_2()
        .rounded_md()
        .cursor_pointer()
        .hover(|row| row.bg(cx.theme().muted))
        .on_click({
            let view = view.clone();
            let entry = entry.clone();
            move |_, window, cx| {
                view.update(cx, |this, cx| this.enter(entry.clone(), window, cx));
            }
        })
        .child(
            v_flex()
                .min_w_0()
                .child(div().text_sm().truncate().child(entry.name.clone()))
                .child(
                    div()
                        .text_xs()
                        .truncate()
                        .text_color(cx.theme().muted_foreground)
                        .child(dir),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().info)
                .child(rust_i18n::t!("welcome.open").to_string()),
        )
        .into_any_element()
}
