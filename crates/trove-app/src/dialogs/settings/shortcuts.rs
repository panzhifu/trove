//! Shortcuts page: every rebindable action, filterable, and captured live —
//! click a row, press the combination, done. No typing `ctrl-shift-z` into a
//! text field and hoping the format matches.

use std::collections::HashMap;

use gpui::Keystroke;
use gpui_kit::component::kbd::Kbd;

use super::*;

// ============================ shortcuts page ================================

/// Which slice of the action list the page shows. One of the pills on top.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ShortcutFilter {
    /// Two live bindings in one context answer to the same key.
    Conflicts,
    #[default]
    All,
    /// Has a key right now.
    Assigned,
    /// The user has overridden or cleared the default.
    Customized,
    /// No key right now.
    Unassigned,
}

/// One action as the list shows it.
#[derive(Clone)]
struct ShortcutRow {
    action: &'static str,
    label: String,
    context: Option<&'static str>,
    /// The key the action answers to right now; empty means unbound.
    key: String,
    /// The user has written an override (or a clear) into the config.
    customized: bool,
    /// Its key collides with another live binding in the same context.
    conflict: bool,
}

/// Shortcuts ▸ Keyboard: the filter pills, then one row per action, then the
/// reset.
///
/// `state` is the window's own view, borrowed for the render — reading the
/// capture state through `view.read(cx)` here would panic, since the view is
/// mid-update. `view` exists for the rows' callbacks, which run later.
pub(super) fn shortcuts_page(state: &SettingsView, view: &Entity<SettingsView>) -> SettingPage {
    let capturing = state.capturing;
    let filter = state.shortcut_filter;
    let rows = shortcut_rows();

    SettingPage::new(rust_i18n::t!("settings.shortcuts").to_string())
        .icon(IconName::Settings)
        .resettable(false)
        .group(filter_group(view, &rows, filter))
        .group(list_group(view, &rows, capturing, filter))
        .group(reset_group())
}

// ============================ shortcut rows =================================

/// The list the page renders from: every rebindable action with its
/// effective key (custom override > default), sorted by action id, conflicts
/// marked. Rebuilt per render — the config is one small file, the list is
/// short, and the page has to follow edits the moment they land.
fn shortcut_rows() -> Vec<ShortcutRow> {
    let config = AppConfig::load();
    let mut rows: Vec<ShortcutRow> = keybindings::default_keybindings()
        .into_iter()
        .map(|entry| {
            let customized = config.keybindings.contains_key(entry.action);
            let key = config
                .keybindings
                .get(entry.action)
                .cloned()
                .unwrap_or_else(|| entry.key.to_string());
            ShortcutRow {
                action: entry.action,
                label: action_label(entry.action),
                context: entry.context,
                key,
                customized,
                conflict: false,
            }
        })
        .collect();
    // Plugin commands ride the same list, store and capture flow as the
    // built-in actions — a plugin that declares a command has declared a
    // rebindable shortcut, wherever the label comes from.
    for plugin in trove_core::plugins::all() {
        for command in plugin.commands() {
            let customized = config.keybindings.contains_key(command.action);
            let key = config
                .keybindings
                .get(command.action)
                .cloned()
                .unwrap_or_else(|| command.key.to_string());
            rows.push(ShortcutRow {
                action: command.action,
                label: action_label(command.action),
                context: (!command.global).then_some("Workspace"),
                key,
                customized,
                conflict: false,
            });
        }
    }
    rows.sort_by(|a, b| a.action.cmp(b.action));

    // A conflict is two live bindings answering to one key inside one
    // context. A scoped key and the global one may share a keystroke — the
    // scoped one shadows — so context is part of the collision's address.
    let mut first_seen: HashMap<(Option<&'static str>, String), usize> = HashMap::new();
    for ix in 0..rows.len() {
        if rows[ix].key.is_empty() {
            continue;
        }
        let address = (rows[ix].context, rows[ix].key.clone());
        match first_seen.get(&address) {
            Some(first) => {
                let first = *first;
                rows[first].conflict = true;
                rows[ix].conflict = true;
            }
            None => {
                first_seen.insert(address, ix);
            }
        }
    }
    rows
}

/// One action line: the name (danger when its key fights another action),
/// its scope in small print, the pill that shows — or takes — the key, and
/// the round "+" that starts a capture.
fn shortcut_row(view: &Entity<SettingsView>, row: &ShortcutRow, capturing: bool, cx: &App) -> Div {
    let label = div()
        .min_w_0()
        .truncate()
        .text_sm()
        .text_color(if row.conflict {
            cx.theme().danger
        } else {
            cx.theme().foreground
        })
        .child(row.label.clone());

    let name = if capturing {
        // The line is listening: say so under the name, where the eye
        // already is.
        v_flex().flex_1().min_w_0().child(label).child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("shortcuts.capture_hint").to_string()),
        )
    } else {
        let mut line = h_flex().min_w_0().items_baseline().gap_2().child(label);
        if let Some(context) = row.context {
            line = line.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(context_label(context)),
            );
        }
        v_flex().flex_1().min_w_0().child(line)
    };

    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(name)
        .child(key_pill(view, row, capturing, cx))
        .child(capture_button(view, row, capturing))
}

/// The pill that shows — or takes — this action's key. Clicking it starts a
/// capture, same as the "+" beside it; it is the wider target of the two.
fn key_pill(
    view: &Entity<SettingsView>,
    row: &ShortcutRow,
    capturing: bool,
    cx: &App,
) -> Stateful<Div> {
    let view = view.clone();
    let action = row.action;

    div()
        .id(SharedString::from(format!("key-{}", row.action)))
        .cursor_pointer()
        .rounded_full()
        .px_3()
        .py_1()
        .border_1()
        .flex_shrink_0()
        .when(capturing, |pill| {
            pill.bg(cx.theme().muted).border_color(cx.theme().muted)
        })
        .when(!capturing && row.conflict, |pill| {
            pill.border_color(cx.theme().danger)
        })
        .when(!capturing && !row.conflict, |pill| {
            pill.border_color(cx.theme().border)
        })
        .child(if capturing {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("shortcuts.press_keys").to_string())
                .into_any_element()
        } else if row.key.is_empty() {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("shortcuts.unset").to_string())
                .into_any_element()
        } else {
            match Keystroke::parse(&row.key) {
                Ok(stroke) => Kbd::new(stroke).into_any_element(),
                Err(_) => div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(row.key.clone())
                    .into_any_element(),
            }
        })
        .on_click(move |_, _, cx| {
            view.update(cx, |this, cx| this.start_capture(action, cx));
        })
}

/// The round "+": start a capture for this action. While this row is the one
/// capturing, the plus stands down — the pill is already listening.
fn capture_button(view: &Entity<SettingsView>, row: &ShortcutRow, capturing: bool) -> Button {
    let view = view.clone();
    let action = row.action;
    let mut button = Button::new(SharedString::from(format!("capture-{}", row.action)))
        .ghost()
        .small()
        .icon(IconName::Plus);
    if capturing {
        button = button.disabled(true);
    }
    button.on_click(move |_, _, cx| {
        view.update(cx, |this, cx| this.start_capture(action, cx));
    })
}

// ============================== filter pills ================================

/// The pills: conflicts first with their count, then the whole list and its
/// slices — the shape of the thing at a glance, before any scrolling.
fn filter_group(
    view: &Entity<SettingsView>,
    rows: &[ShortcutRow],
    active: ShortcutFilter,
) -> SettingGroup {
    let conflicts = rows.iter().filter(|r| r.conflict).count();
    let pills = [
        (
            "filter-conflicts",
            rust_i18n::t!("shortcuts.filter_conflicts", count = conflicts).to_string(),
            ShortcutFilter::Conflicts,
            true,
        ),
        (
            "filter-all",
            rust_i18n::t!("shortcuts.filter_all").to_string(),
            ShortcutFilter::All,
            false,
        ),
        (
            "filter-assigned",
            rust_i18n::t!("shortcuts.filter_assigned").to_string(),
            ShortcutFilter::Assigned,
            false,
        ),
        (
            "filter-customized",
            rust_i18n::t!("shortcuts.filter_customized").to_string(),
            ShortcutFilter::Customized,
            false,
        ),
        (
            "filter-unassigned",
            rust_i18n::t!("shortcuts.filter_unassigned").to_string(),
            ShortcutFilter::Unassigned,
            false,
        ),
    ];

    SettingGroup::new().item(SettingItem::render({
        let view = view.clone();
        move |_, _, cx| {
            let mut bar = h_flex().w_full().flex_wrap().gap_2();
            for (id, label, filter, danger) in pills.iter() {
                bar = bar.child(filter_pill(
                    id,
                    label.clone(),
                    active == *filter,
                    *danger,
                    *filter,
                    &view,
                    cx,
                ));
            }
            bar
        }
    }))
}

/// One pill: rounded, quiet when idle, filled when the filter it names is
/// the one showing. The conflict pill reads in the danger tone either way.
fn filter_pill(
    id: &'static str,
    label: String,
    selected: bool,
    danger: bool,
    filter: ShortcutFilter,
    view: &Entity<SettingsView>,
    cx: &App,
) -> Stateful<Div> {
    let view = view.clone();
    div()
        .id(SharedString::from(id))
        .cursor_pointer()
        .rounded_full()
        .px_3()
        .py_1()
        .text_sm()
        .border_1()
        .when(selected, |pill| {
            pill.bg(if danger {
                cx.theme().danger
            } else {
                cx.theme().foreground
            })
            .border_color(if danger {
                cx.theme().danger
            } else {
                cx.theme().foreground
            })
            .text_color(if danger {
                cx.theme().danger_foreground
            } else {
                cx.theme().background
            })
        })
        .when(!selected, |pill| {
            pill.bg(cx.theme().background)
                .border_color(cx.theme().border)
                .text_color(if danger {
                    cx.theme().danger
                } else {
                    cx.theme().muted_foreground
                })
        })
        .child(label)
        .on_click(move |_, _, cx| {
            view.update(cx, |this, cx| {
                this.shortcut_filter = filter;
                cx.notify();
            });
        })
}

/// The action list under the active filter.
fn list_group(
    view: &Entity<SettingsView>,
    rows: &[ShortcutRow],
    capturing: Option<&'static str>,
    filter: ShortcutFilter,
) -> SettingGroup {
    let shown: Vec<ShortcutRow> = rows
        .iter()
        .filter(|r| match filter {
            ShortcutFilter::Conflicts => r.conflict,
            ShortcutFilter::All => true,
            ShortcutFilter::Assigned => !r.key.is_empty(),
            ShortcutFilter::Customized => r.customized,
            ShortcutFilter::Unassigned => r.key.is_empty(),
        })
        .cloned()
        .collect();

    if shown.is_empty() {
        return SettingGroup::new().item(SettingItem::render(|_, _, cx| {
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(rust_i18n::t!("shortcuts.filter_empty").to_string())
        }));
    }

    let mut group = SettingGroup::new();
    for row in shown {
        let is_capturing = capturing == Some(row.action);
        group = group.item(SettingItem::render({
            let view = view.clone();
            move |_, _, cx| shortcut_row(&view, &row, is_capturing, cx)
        }));
    }
    group
}

/// The reset row: defaults for everything, one click.
fn reset_group() -> SettingGroup {
    SettingGroup::new().item(
        SettingItem::new(
            rust_i18n::t!("shortcuts.shortcut_reset").to_string(),
            SettingField::render(|_, _, _| {
                h_flex().w_full().justify_end().child(
                    Button::new("reset-keybindings")
                        .outline()
                        .small()
                        .label(rust_i18n::t!("shortcuts.shortcut_reset").to_string())
                        .on_click(|_, _, cx| reset_keybindings(cx)),
                )
            }),
        )
        .description(rust_i18n::t!("shortcuts.shortcut_reset_done").to_string()),
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
        "EnterVideoFullscreen" => {
            rust_i18n::t!("shortcuts.actions.EnterVideoFullscreen").to_string()
        }
        "ExitVideoFullscreen" => rust_i18n::t!("shortcuts.actions.ExitVideoFullscreen").to_string(),
        other => {
            // Plugin commands use a per-plugin catalog key:
            // `commands.<id with "/" and "-" folded to "_">`. The owning
            // plugin's own language files win (they travel with the plugin);
            // the app catalog is the fallback, and a plugin that ships no
            // entry at all falls back to the raw id.
            let key = format!("commands.{}", other.replace(['/', '-'], "_"));
            if let Some(text) = crate::plugins::i18n::translate(&key, &[], &[]) {
                return text;
            }
            let text = rust_i18n::t!(key.as_str()).to_string();
            if text == key { other.to_string() } else { text }
        }
    }
}

/// Localized context label.
fn context_label(context: &str) -> String {
    match context {
        "Workspace" => rust_i18n::t!("shortcuts.context.Workspace").to_string(),
        "VideoPreview" => rust_i18n::t!("shortcuts.context.VideoPreview").to_string(),
        _ => rust_i18n::t!("shortcuts.context.global").to_string(),
    }
}

/// Reset all keybindings to defaults.
fn reset_keybindings(cx: &mut App) {
    let mut config = AppConfig::load();
    config.keybindings.clear();
    let _ = config.save();
    crate::register_keys(cx);
    cx.refresh_windows();
}
