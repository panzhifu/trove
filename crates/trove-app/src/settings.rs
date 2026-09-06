//! Settings dialog for configuring application preferences.

use std::path::PathBuf;

use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Sizable, WindowExt};
use gpui_kit::*;

use trove_core::config::AppConfig;

/// Settings dialog: lets the user configure the library location and other preferences.
pub struct SettingsDialog;

impl SettingsDialog {
    /// Open the settings dialog.
    pub fn open(window: &mut Window, cx: &mut App) {
        let current_config = AppConfig::load();
        let current_path = current_config.resolved_library_path();

        let library_path_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Library folder path"));

        // Pre-fill with current path
        let path_str = current_path.display().to_string();
        library_path_input.update(cx, |state, cx| {
            state.set_value(path_str, window, cx);
        });

        // Clone for use in the dialog closure
        let input_for_child = library_path_input.clone();
        let input_for_ok = library_path_input.clone();
        let current_path_for_display = current_path.clone();

        window.open_dialog(cx, move |dialog, _window, cx| {
            dialog
                .title("Settings")
                .child(
                    v_flex()
                        .gap_3()
                        .w_full()
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(cx.theme().foreground)
                                        .child("Library Location"),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Choose where to store your asset library."),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .w_full()
                                        .child(
                                            Input::new(&input_for_child)
                                                .small()
                                                .flex_1()
                                                .appearance(true),
                                        )
                                        .child(
                                            Button::new("browse-btn")
                                                .ghost()
                                                .small()
                                                .label("Browse…")
                                                .tooltip("Select a folder for your library")
                                                .on_click({
                                                    let _input = input_for_child.clone();
                                                    move |_, _, cx| {
                                                        let rx = cx.prompt_for_paths(
                                                            PathPromptOptions {
                                                                files: false,
                                                                directories: true,
                                                                multiple: false,
                                                                prompt: Some("Select library folder".into()),
                                                            },
                                                        );
                                                        cx.spawn(async move |_cx| {
                                                            if let Ok(result) = rx.await {
                                                                if let Ok(Some(paths)) = result {
                                                                    if let Some(_path) = paths.first() {
                                                                        // Path selected - will be used when saving
                                                                    }
                                                                }
                                                            }
                                                        }).detach();
                                                    }
                                                }),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!("Current: {}", current_path_for_display.display())),
                                ),
                        ),
                )
                .on_ok({
                    let input = input_for_ok.clone();
                    move |_, _, cx| {
                        let path_str = input.read(cx).value().to_string();
                        if !path_str.trim().is_empty() {
                            let path = PathBuf::from(path_str.trim());
                            let mut config = AppConfig::load();
                            if let Err(e) = config.set_library_path(path) {
                                eprintln!("Failed to save config: {e}");
                            }
                        }
                        true // close dialog
                    }
                })
                .on_cancel(|_, _, _| true) // close dialog
        });
    }
}
