//! Color-label widget: the single home of the color-label palette UI.
//!
//! Three surfaces share this module so the palette never drifts apart:
//! - [`picker`] — the standalone swatch row (Inspector): left-click applies
//!   the color to the primary asset, right-click opens the function menu.
//! - [`menu_entries`] — the workspace asset context-menu entries.
//! - [`label_name`] — localized palette names for the toolbar's
//!   filter-by-color menu.

use gpui_kit::base::h_flex;
use gpui_kit::component::menu::{ContextMenu, ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::*;
use uuid::Uuid;

use trove_core::model::AssetPatch;

use crate::library::LibraryController;

use super::common::{COLOR_LABEL_SWATCHES, color_swatch};

/// Localized display name of a palette color (`workspace.label_*` keys).
pub(crate) fn label_name(name: &str) -> String {
    rust_i18n::t!(format!("workspace.label_{name}")).to_string()
}

/// Which assets a label action hits: the whole selection around `Some(id)`
/// (the grid context menu), or the current primary asset (`None`, the
/// picker).
#[derive(Debug, Clone, Copy)]
pub(crate) enum LabelTarget {
    Selection(Uuid),
    Primary,
}

/// Set `label` (`None` = clear) on the assets `target` resolves to.
pub(crate) fn apply(
    controller: &Entity<LibraryController>,
    target: LabelTarget,
    label: Option<String>,
    cx: &mut App,
) {
    let error_key = match target {
        LabelTarget::Selection(_) => "workspace.trash_failed",
        LabelTarget::Primary => "inspector.update_failed",
    };
    controller.update(cx, |ctl, cx| {
        let ids: Vec<Uuid> = match target {
            LabelTarget::Selection(id) => ctl.action_targets(id),
            LabelTarget::Primary => ctl.primary().into_iter().collect(),
        };
        for id in ids {
            let patch = AssetPatch {
                color_label: Some(label.clone()),
                ..Default::default()
            };
            if let Err(e) = ctl.library.patch_asset(id, &patch) {
                ctl.notice = Some(rust_i18n::t!(error_key, error = e.to_string()).to_string());
            }
        }
        ctl.generation += 1;
        cx.notify();
    });
}

/// Right-click menu entries: one checked item per palette color plus a
/// clear action. Shared by the workspace context menu and the picker's
/// right-click menu.
pub(crate) fn menu_entries(
    mut menu: PopupMenu,
    controller: &Entity<LibraryController>,
    target: LabelTarget,
    current: Option<&str>,
) -> PopupMenu {
    for (name, _hex) in COLOR_LABEL_SWATCHES {
        let controller = controller.clone();
        let selected = current == Some(*name);
        let label = label_name(name);
        let name = name.to_string();
        menu = menu.item(
            PopupMenuItem::new(label)
                .checked(selected)
                .on_click(move |_, _, cx| {
                    apply(&controller, target, Some(name.clone()), cx);
                }),
        );
    }
    let controller = controller.clone();
    menu.separator().item(
        PopupMenuItem::new(rust_i18n::t!("workspace.clear_color_label").to_string()).on_click(
            move |_, _, cx| {
                apply(&controller, target, None, cx);
            },
        ),
    )
}

/// The standalone picker: one chip per palette color. Left-click applies
/// the color to the primary asset (clicking the active chip clears it);
/// right-click opens the same function menu as the grid context menu.
pub(crate) fn picker(
    controller: &Entity<LibraryController>,
    current: Option<&str>,
    cx: &App,
) -> ContextMenu<Stateful<Div>> {
    let menu_controller = controller.clone();
    let menu_current = current.map(str::to_string);
    let row = h_flex()
        .gap_1p5()
        .children(COLOR_LABEL_SWATCHES.iter().map(|(name, hex)| {
            let controller = controller.clone();
            let selected = current == Some(*name);
            let name = name.to_string();
            color_swatch(
                cx,
                format!("label-{name}"),
                hex,
                selected,
                move |_, _, cx| {
                    apply(
                        &controller,
                        LabelTarget::Primary,
                        if selected { None } else { Some(name.clone()) },
                        cx,
                    );
                },
            )
        }));
    div()
        .id("color-label-picker")
        .context_menu(move |menu, _, _| {
            menu_entries(
                menu,
                &menu_controller,
                LabelTarget::Primary,
                menu_current.as_deref(),
            )
        })
        .child(row)
}
