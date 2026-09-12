//! Shortcuts page: keybinding editor rows with per-action capture
//! prompts and a reset-to-defaults button.

use super::*;

// ============================ shortcuts page ================================

/// Shortcuts ▸ Keyboard: view and customize keybindings.
pub(super) fn shortcuts_page() -> SettingPage {
    let page = SettingPage::new(rust_i18n::t!("settings.shortcuts").to_string())
        .icon(IconName::Settings)
        .resettable(false);

    let items = keybinding_items();
    let mut group = SettingGroup::new();
    for item in &items {
        let action = item.action;
        let default_key = item.key;
        let ctx = item.context;
        let label = action_label(action);
        let ctx_label = ctx
            .map(context_label)
            .unwrap_or_else(|| context_label("global"));

        let action_row = action.to_string();
        let key_row = default_key.to_string();
        let ctx_row = ctx_label.clone();
        group = group.item(SettingItem::new(
            label.clone(),
            SettingField::render(move |_, _, cx| {
                keybinding_row(&action_row, &key_row, &ctx_row, cx)
            }),
        ));
    }

    // Add reset button at the bottom.
    group = group.item(
        SettingItem::new(
            rust_i18n::t!("shortcuts.shortcut_reset").to_string(),
            SettingField::render(move |_, _, _cx| {
                h_flex().w_full().justify_end().child(
                    Button::new("reset-keybindings")
                        .outline()
                        .small()
                        .label(rust_i18n::t!("shortcuts.shortcut_reset").to_string())
                        .on_click(move |_, _, cx| {
                            reset_keybindings(cx);
                        }),
                )
            }),
        )
        .description(rust_i18n::t!("shortcuts.shortcut_reset_done").to_string()),
    );

    page.group(group)
}

/// Render a single keybinding row: description + clickable key + context.
fn keybinding_row(action: &str, default_key: &str, context_label: &str, cx: &mut App) -> Div {
    let config = AppConfig::load();
    let display_key = config
        .keybindings
        .get(action)
        .cloned()
        .unwrap_or_else(|| default_key.to_string());
    let label = action_label(action);
    let action_owned = action.to_string();
    let default_owned = default_key.to_string();

    h_flex()
        .w_full()
        .justify_between()
        .gap_2()
        .child(
            div()
                .flex_1()
                .text_sm()
                .text_color(cx.theme().foreground)
                .child(label),
        )
        .child(
            Button::new(format!("key-{action}"))
                .ghost()
                .xsmall()
                .label(display_key.to_uppercase())
                .on_click(move |_, window, cx| {
                    prompt_keybinding_change(&action_owned, &default_owned, window, cx);
                }),
        )
        .child(
            div()
                .text_xs()
                .w(px(60.))
                .text_right()
                .text_color(cx.theme().muted_foreground)
                .child(context_label.to_string()),
        )
}

/// Localized label for an action id.
fn action_label(action: &str) -> String {
    match action {
        "MoveLeft" => rust_i18n::t!("shortcuts.actions.MoveLeft").to_string(),
        "MoveRight" => rust_i18n::t!("shortcuts.actions.MoveRight").to_string(),
        "MoveUp" => rust_i18n::t!("shortcuts.actions.MoveUp").to_string(),
        "MoveDown" => rust_i18n::t!("shortcuts.actions.MoveDown").to_string(),
        "OpenPreview" => rust_i18n::t!("shortcuts.actions.OpenPreview").to_string(),
        "TrashSelected" => rust_i18n::t!("shortcuts.actions.TrashSelected").to_string(),
        "SelectAll" => rust_i18n::t!("shortcuts.actions.SelectAll").to_string(),
        "ClearSelection" => rust_i18n::t!("shortcuts.actions.ClearSelection").to_string(),
        "Undo" => rust_i18n::t!("shortcuts.actions.Undo").to_string(),
        "Redo" => rust_i18n::t!("shortcuts.actions.Redo").to_string(),
        "ImportFiles" => rust_i18n::t!("shortcuts.actions.ImportFiles").to_string(),
        "OpenSettings" => rust_i18n::t!("shortcuts.actions.OpenSettings").to_string(),
        "ScreenshotFull" => rust_i18n::t!("shortcuts.actions.ScreenshotFull").to_string(),
        "ScreenshotRegion" => rust_i18n::t!("shortcuts.actions.ScreenshotRegion").to_string(),
        "RefreshLibrary" => rust_i18n::t!("shortcuts.actions.RefreshLibrary").to_string(),
        "BatchRename" => rust_i18n::t!("shortcuts.actions.BatchRename").to_string(),
        "BatchConvert" => rust_i18n::t!("shortcuts.actions.BatchConvert").to_string(),
        "CopyImage" => rust_i18n::t!("shortcuts.actions.CopyImage").to_string(),
        other => other.to_string(),
    }
}

/// Localized context label.
fn context_label(context: &str) -> String {
    match context {
        "Workspace" => rust_i18n::t!("shortcuts.context.Workspace").to_string(),
        _ => rust_i18n::t!("shortcuts.context.global").to_string(),
    }
}

/// Prompt the user for a new keybinding via keyboard capture dialog.
fn prompt_keybinding_change(action_id: &str, default_key: &str, window: &mut Window, cx: &mut App) {
    use gpui_kit::component::dialog::DialogButtonProps;
    use gpui_kit::component::input::{Input, InputState};

    let action_id = action_id.to_string();
    let default = default_key.to_string();
    let action_label_disp = action_label(&action_id);
    window.open_dialog(cx, move |dialog, window, cx| {
        let input_state = cx.new(|cx| InputState::new(window, cx).placeholder(default.clone()));
        let input_clone = input_state.clone();
        let action_ok = action_id.clone();
        dialog
            .title(rust_i18n::t!("shortcuts.shortcut_prompt").to_string())
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div().text_sm().text_color(cx.theme().foreground).child(
                            rust_i18n::t!(
                                "shortcuts.shortcut_prompt_hint",
                                action = action_label_disp.clone(),
                                default = default.clone()
                            )
                            .to_string(),
                        ),
                    )
                    .child(Input::new(&input_clone).small())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("shortcuts.shortcut_prompt_note").to_string()),
                    ),
            )
            .button_props(
                DialogButtonProps::default()
                    .ok_text(rust_i18n::t!("shortcuts.change").to_string())
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                let value: String = input_clone.read(cx).value().to_string();
                let trimmed = value.trim().to_lowercase();
                if !trimmed.is_empty() {
                    let mut config = AppConfig::load();
                    config.keybindings.insert(action_ok.clone(), trimmed);
                    let _ = config.save();
                    cx.refresh_windows();
                }
                true
            })
    });
}

/// Reset all keybindings to defaults.
fn reset_keybindings(cx: &mut App) {
    let mut config = AppConfig::load();
    config.keybindings.clear();
    let _ = config.save();
    cx.refresh_windows();
}

/// Get all configurable keybindings.
fn keybinding_items() -> Vec<KeyBindingConfig> {
    keybindings::default_keybindings()
}
