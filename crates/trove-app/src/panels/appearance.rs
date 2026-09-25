//! A container's own look — the glyph and the accent a folder is given — as one
//! drawing point and one picker.
//!
//! [`trove_core::model::Appearance`] holds why the library stores *names* (an
//! accent, and an icon key) rather than values. This is the other half: what
//! those names look like in the theme the user is in, and the control that
//! chooses them.
//!
//! There is one draw function ([`glyph`]) and one picker ([`Picker`]) because the
//! same folder appears in the collection tree, in the rule editor and in the
//! chooser's own preview; a second implementation of either is how surfaces start
//! to disagree about what a folder looks like.
//!
//! The chooser is a panel rather than a dialog, and it writes as it is clicked:
//! the folder takes each look on the spot, which is what lets a user try three
//! accents and settle on one without a round trip through a form. That is only
//! safe because every offer is reversible where it was made — re-choosing what
//! is already on puts it back, and the footer's clear resets the whole thing.
//!
//! The accent is painted as *foreground* everywhere — on the glyph, and on the
//! row's label where the theme's text colour would otherwise be — and never as a
//! fill behind text. That keeps the contrast question out of scope: a colour that
//! only has to be legible over the panel background is one whose lightness can be
//! corrected in a single place ([`accent_color`]), instead of every surface
//! having to derive a readable foreground of its own.

use gpui_kit::assets::IconName;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::{ActiveTheme, Colorize as _, Icon};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trove_core::model::{Accent, Appearance, Glyph};
use uuid::Uuid;

use crate::library::LibraryController;
use crate::panels::common::color_swatch;

// ---------------------------------------------------------------------------
// The catalogues a folder may be given
// ---------------------------------------------------------------------------

/// One page of the chooser: a label over a run of offers.
struct Group {
    label_key: &'static str,
    cells: &'static [Cell],
}

/// An offer on a glyph page — drawn, compared with what is stored, and stored.
#[derive(Clone, Copy, PartialEq)]
enum Cell {
    Emoji(&'static str),
    Icon(IconName),
}

impl Cell {
    /// The glyph this offer would store.
    fn glyph(self) -> Glyph {
        match self {
            Self::Emoji(text) => Glyph::Emoji(text.to_string()),
            Self::Icon(name) => Glyph::Icon(icon_key(name)),
        }
    }

    /// Part of the cell's element id: two cells sharing an id would share their
    /// hover state.
    fn key(self) -> String {
        match self {
            Self::Emoji(text) => text.to_string(),
            Self::Icon(name) => icon_key(name),
        }
    }

    fn view(self, picked: bool, cx: &App) -> AnyElement {
        match self {
            Self::Emoji(text) => div()
                .text_base()
                .child(SharedString::from(text))
                .into_any_element(),
            Self::Icon(name) => {
                let color = match picked {
                    true => cx.theme().accent_foreground,
                    false => cx.theme().foreground,
                };
                Icon::new(name)
                    .size_4()
                    .text_color(color)
                    .into_any_element()
            }
        }
    }
}

/// Vector icons on offer, grouped.
///
/// Curated rather than the ~1800 bundled Lucide names: a folder is being given a
/// mark, and a wall of near-identical glyphs buries the few that say anything.
/// This is a list of what may be *chosen*, not of what may be drawn —
/// [`resolve`] accepts any real bundled icon, which is what lets the catalogue
/// change without stranding libraries that used an entry since dropped from it.
const ICON_GROUPS: &[Group] = &[
    Group {
        label_key: "appearance.group_media",
        cells: &[
            Cell::Icon(IconName::Image),
            Cell::Icon(IconName::Images),
            Cell::Icon(IconName::Camera),
            Cell::Icon(IconName::Video),
            Cell::Icon(IconName::Film),
            Cell::Icon(IconName::Music),
            Cell::Icon(IconName::Mic),
            Cell::Icon(IconName::Type),
            Cell::Icon(IconName::FileText),
            Cell::Icon(IconName::FileArchive),
            Cell::Icon(IconName::FileBox),
            Cell::Icon(IconName::Shapes),
            Cell::Icon(IconName::Palette),
            Cell::Icon(IconName::Brush),
            Cell::Icon(IconName::Pencil),
        ],
    },
    Group {
        label_key: "appearance.group_folders",
        cells: &[
            Cell::Icon(IconName::Folder),
            Cell::Icon(IconName::Folders),
            Cell::Icon(IconName::FolderOpen),
            Cell::Icon(IconName::FolderHeart),
            Cell::Icon(IconName::FolderKey),
            Cell::Icon(IconName::FolderLock),
            Cell::Icon(IconName::Library),
            Cell::Icon(IconName::GalleryThumbnails),
            Cell::Icon(IconName::LayoutGrid),
            Cell::Icon(IconName::Layers),
            Cell::Icon(IconName::Archive),
            Cell::Icon(IconName::Box),
            Cell::Icon(IconName::Package),
            Cell::Icon(IconName::Briefcase),
            Cell::Icon(IconName::ShoppingBag),
        ],
    },
    Group {
        label_key: "appearance.group_marks",
        cells: &[
            Cell::Icon(IconName::Star),
            Cell::Icon(IconName::Heart),
            Cell::Icon(IconName::Bookmark),
            Cell::Icon(IconName::Tag),
            Cell::Icon(IconName::Tags),
            Cell::Icon(IconName::Award),
            Cell::Icon(IconName::Sparkles),
            Cell::Icon(IconName::Flame),
            Cell::Icon(IconName::Zap),
            Cell::Icon(IconName::Eye),
            Cell::Icon(IconName::Search),
            Cell::Icon(IconName::Check),
            Cell::Icon(IconName::Circle),
            Cell::Icon(IconName::Diamond),
            Cell::Icon(IconName::X),
        ],
    },
    Group {
        label_key: "appearance.group_life",
        cells: &[
            Cell::Icon(IconName::House),
            Cell::Icon(IconName::MapPin),
            Cell::Icon(IconName::Globe),
            Cell::Icon(IconName::Plane),
            Cell::Icon(IconName::Car),
            Cell::Icon(IconName::Ship),
            Cell::Icon(IconName::Mountain),
            Cell::Icon(IconName::TreePine),
            Cell::Icon(IconName::Flower),
            Cell::Icon(IconName::Leaf),
            Cell::Icon(IconName::Cloud),
            Cell::Icon(IconName::Sun),
            Cell::Icon(IconName::Moon),
            Cell::Icon(IconName::Cake),
            Cell::Icon(IconName::Gift),
        ],
    },
];

/// Emoji on offer, grouped.
///
/// An accent cannot tint these: the text engine paints an emoji from the
/// system's emoji font, and that paint takes a size and no colour. The accent
/// still lands on the row's label, which is why the page says so in
/// [`EMOJI_NOTE`] rather than leaving it to be discovered.
const EMOJI_GROUPS: &[Group] = &[
    Group {
        label_key: "appearance.group_media",
        cells: &[
            Cell::Emoji("🖼️"),
            Cell::Emoji("📷"),
            Cell::Emoji("🎬"),
            Cell::Emoji("🎞️"),
            Cell::Emoji("🎨"),
            Cell::Emoji("🖌️"),
            Cell::Emoji("✏️"),
            Cell::Emoji("🎼"),
            Cell::Emoji("🎧"),
            Cell::Emoji("🎤"),
            Cell::Emoji("📄"),
            Cell::Emoji("📚"),
            Cell::Emoji("💾"),
            Cell::Emoji("🖥️"),
            Cell::Emoji("📱"),
            Cell::Emoji("🕹️"),
        ],
    },
    Group {
        label_key: "appearance.group_life",
        cells: &[
            Cell::Emoji("🏠"),
            Cell::Emoji("✈️"),
            Cell::Emoji("🚗"),
            Cell::Emoji("⛰️"),
            Cell::Emoji("🌊"),
            Cell::Emoji("🌙"),
            Cell::Emoji("☀️"),
            Cell::Emoji("🌸"),
            Cell::Emoji("🍀"),
            Cell::Emoji("🎁"),
            Cell::Emoji("🎂"),
            Cell::Emoji("☕"),
            Cell::Emoji("🍕"),
            Cell::Emoji("🐶"),
            Cell::Emoji("🐱"),
            Cell::Emoji("👶"),
        ],
    },
    Group {
        label_key: "appearance.group_marks",
        cells: &[
            Cell::Emoji("⭐"),
            Cell::Emoji("❤️"),
            Cell::Emoji("🔥"),
            Cell::Emoji("✨"),
            Cell::Emoji("🏆"),
            Cell::Emoji("📌"),
            Cell::Emoji("📎"),
            Cell::Emoji("🔒"),
            Cell::Emoji("🗝️"),
            Cell::Emoji("💡"),
            Cell::Emoji("🚀"),
            Cell::Emoji("⚡"),
            Cell::Emoji("🎯"),
            Cell::Emoji("✅"),
            Cell::Emoji("❗"),
            Cell::Emoji("➡️"),
        ],
    },
];

/// The note under the emoji page, where an accent has no effect on the glyph.
const EMOJI_NOTE: &str = "appearance.emoji_hint";

/// Every icon the appearance surfaces name: what the picker offers, plus the
/// fallbacks the tree draws for a row that has no glyph of its own. The asset
/// source asserts this list is registered
/// (`crate::assets::every_appearance_icon_is_bundled`), because an icon outside
/// its bundle loads as empty and paints invisible rather than missing. Nothing
/// at runtime asks for the list, so it exists only for that assertion.
#[cfg(test)]
pub(crate) fn catalog_icons() -> impl Iterator<Item = IconName> {
    ICON_GROUPS
        .iter()
        .flat_map(|group| group.cells)
        .filter_map(|cell| match cell {
            Cell::Icon(name) => Some(*name),
            Cell::Emoji(_) => None,
        })
        .chain([
            IconName::Folder,
            IconName::Search,
            IconName::Library,
            IconName::Clock,
            IconName::Trash,
        ])
}

/// The catalogue key of a bundled icon: its file stem, which is what
/// [`Glyph::Icon`] stores.
fn icon_key(name: IconName) -> String {
    name.path()
        .trim_start_matches("icons/")
        .trim_end_matches(".svg")
        .to_string()
}

/// A resolved glyph: a stored name turned into something this build can draw.
enum Resolved {
    Emoji(SharedString),
    Icon(IconName),
}

/// The stored glyph as a drawable thing, or `None` for the default look.
///
/// A name this build cannot resolve draws as the folder's usual icon rather than
/// as nothing: a catalogue entry that moved costs the user their picture, not
/// their folder.
fn resolve(appearance: &Appearance) -> Option<Resolved> {
    match appearance.glyph.as_ref()? {
        Glyph::Emoji(text) => Some(Resolved::Emoji(text.as_str().into())),
        Glyph::Icon(name) => {
            let target = format!("icons/{name}.svg");
            IconName::ALL
                .iter()
                .copied()
                .find(|icon| icon.path() == target)
                .map(Resolved::Icon)
        }
    }
}

/// The accent as this theme paints it: the stored name's reference colour, with
/// its lightness moved into the band that stays legible as text over the panel
/// the folder tree is drawn on. Saturation is left alone — the hue is the part
/// the user chose.
pub(crate) fn accent_color(accent: Accent, cx: &App) -> Hsla {
    let Hsla { h, s, l, a } = Hsla::parse_hex(accent.reference()).unwrap_or(cx.theme().primary);
    let l = match cx.theme().is_dark() {
        true => l.max(0.66),
        false => l.min(0.46),
    };
    hsla(h, s, l, a)
}

/// The one place a container's glyph is drawn. `fallback` is what the surface
/// would have shown anyway, so a folder that asks for nothing — and a row that
/// is not a folder at all, which passes `None` — looks exactly as it did before
/// this existed.
pub(crate) fn glyph(
    appearance: Option<&Appearance>,
    fallback: impl Into<IconName>,
    cx: &App,
) -> AnyElement {
    let plain = Appearance::default();
    let appearance = appearance.unwrap_or(&plain);
    let icon = match resolve(appearance) {
        Some(Resolved::Emoji(text)) => {
            // Painted in the emoji font's own colours: no accent reaches it.
            return div()
                .flex_none()
                .size_4()
                .flex()
                .items_center()
                .justify_center()
                .overflow_hidden()
                .text_sm()
                .child(text)
                .into_any_element();
        }
        Some(Resolved::Icon(name)) => name,
        None => fallback.into(),
    };
    let icon = Icon::new(icon).size_4();
    match appearance.accent {
        Some(accent) => icon.text_color(accent_color(accent, cx)).into_any_element(),
        None => icon.into_any_element(),
    }
}

/// The text colour a folder's row should use: its accent where it has one, the
/// theme's foreground where it does not.
pub(crate) fn label_color(appearance: &Appearance, cx: &App) -> Hsla {
    appearance
        .accent
        .map(|accent| accent_color(accent, cx))
        .unwrap_or(cx.theme().foreground)
}

// ---------------------------------------------------------------------------
// The picker
// ---------------------------------------------------------------------------

/// How what is chosen here reaches the library.
enum Commit {
    /// The container exists, so every click is written there and then: the tree
    /// row changes under the pointer while the look is being tried out.
    Live {
        controller: Entity<LibraryController>,
        target: Target,
    },
    /// A draft. The rule editor holds the chooser and persists its look with the
    /// row it is building, so nothing is written until that row saves.
    Draft,
}

/// The chooser: the accents in a row, both glyph catalogues behind one scroll,
/// and a preview of the row as the tree will draw it.
pub(crate) struct Picker {
    appearance: Appearance,
    /// What the preview row calls the folder.
    name: SharedString,
    commit: Commit,
}

impl Picker {
    /// The chooser for a container that exists, prefilled with what it already
    /// asks for.
    pub(crate) fn live(
        controller: Entity<LibraryController>,
        target: Target,
        name: String,
        appearance: Appearance,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_| Self {
            appearance,
            name: preview_label(name),
            commit: Commit::Live { controller, target },
        })
    }

    /// The chooser for a container that does not exist yet.
    pub(crate) fn draft(appearance: Appearance, name: String, cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self {
            appearance,
            name: preview_label(name),
            commit: Commit::Draft,
        })
    }

    pub(crate) fn appearance(&self) -> &Appearance {
        &self.appearance
    }

    /// The one place a choice lands.
    fn apply(&mut self, next: Appearance, cx: &mut Context<Self>) {
        self.appearance = next;
        if let Commit::Live { controller, target } = &self.commit {
            let controller = controller.clone();
            let target = *target;
            let appearance = self.appearance.clone();
            controller.update(cx, move |ctl, cx| {
                // A refused write has to be seen: a folder that quietly kept
                // its old look reads as a bug, not as a failure. With no dialog
                // to close, the controller's notice is the only place it can go.
                if let Err(error) = target.write(ctl, &appearance) {
                    ctl.notice = Some(error);
                }
                ctl.generation += 1;
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Choosing the glyph already on puts it back, which is how a glyph clears
    /// without a second control per cell.
    fn toggle_glyph(&mut self, glyph: Glyph, cx: &mut Context<Self>) {
        let mut next = self.appearance.clone();
        next.glyph = (next.glyph.as_ref() != Some(&glyph)).then_some(glyph);
        self.apply(next, cx);
    }

    fn toggle_accent(&mut self, accent: Option<Accent>, cx: &mut Context<Self>) {
        let mut next = self.appearance.clone();
        next.accent = if next.accent == accent { None } else { accent };
        self.apply(next, cx);
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        if self.appearance.is_plain() {
            return;
        }
        self.apply(Appearance::default(), cx);
    }
}

/// The name the preview shows: the folder's own where it has one.
fn preview_label(name: String) -> SharedString {
    match name.trim().is_empty() {
        true => rust_i18n::t!("appearance.preview_name").to_string(),
        false => name,
    }
    .into()
}

/// Keep the menu this panel is drawn in from closing.
///
/// `PopupMenu` dismisses its whole chain when any of its items is clicked, and a
/// chooser that shuts after one cell cannot be tried out — so a click here is a
/// choice, and stops where it was handled.
fn hold_menu(cx: &mut App) {
    cx.stop_propagation();
}

impl Render for Picker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let picker = cx.entity();
        let appearance = self.appearance.clone();
        let name = self.name.clone();

        v_flex()
            .id("appearance-picker")
            .aria_label(rust_i18n::t!("appearance.title").to_string())
            .w_full()
            .gap_2()
            .child(accent_row(&picker, &appearance, cx))
            .child(
                v_flex()
                    .id("appearance-catalogue")
                    .gap_2()
                    .h(px(224.))
                    .overflow_y_scrollbar()
                    .child(glyph_run(
                        &picker,
                        &appearance,
                        "appearance.emoji",
                        EMOJI_GROUPS,
                        Some(EMOJI_NOTE),
                        cx,
                    ))
                    .child(glyph_run(
                        &picker,
                        &appearance,
                        "appearance.icon",
                        ICON_GROUPS,
                        None,
                        cx,
                    )),
            )
            .child(footer(&picker, &appearance, &name, cx))
    }
}

/// The row the folder will keep, drawn the way the tree draws it: the preview and
/// the real rows go through the same functions, so they cannot drift apart. The
/// one control that puts everything back sits at its end, because with no dialog
/// to cancel there is nothing else to undo a look from.
fn footer(
    picker: &Entity<Picker>,
    current: &Appearance,
    name: &SharedString,
    cx: &App,
) -> AnyElement {
    let plain = current.is_plain();
    let picker = picker.clone();
    h_flex()
        .gap_2()
        .items_center()
        .px_2()
        .py_1p5()
        .rounded(cx.theme().radius)
        .bg(cx.theme().secondary)
        .child(glyph(Some(current), IconName::Folder, cx))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .truncate()
                .text_color(label_color(current, cx))
                .child(name.clone()),
        )
        .child(
            div()
                .id("appearance-clear")
                .flex_none()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .when(!plain, |this| {
                    this.cursor_pointer()
                        .hover(|this| this.text_color(cx.theme().foreground))
                })
                .child(rust_i18n::t!("appearance.clear").to_string())
                .on_click(move |_, _, cx| {
                    hold_menu(cx);
                    picker.update(cx, |this, cx| this.clear(cx));
                }),
        )
        .into_any_element()
}

/// The accents, in a row above everything else: the one question a folder's look
/// answers first, and the only one whose answers are a fixed, small set.
fn accent_row(picker: &Entity<Picker>, current: &Appearance, cx: &App) -> AnyElement {
    // "No accent" leads, and wears the panel's own muted colour: that is what
    // clearing leaves behind.
    let chips = std::iter::once(None)
        .chain(Accent::ALL.iter().copied().map(Some))
        .map(|accent| {
            let picker = picker.clone();
            let color = match accent {
                Some(accent) => accent_color(accent, cx),
                None => cx.theme().muted,
            };
            color_swatch(
                cx,
                format!(
                    "appearance-accent-{}",
                    accent.map_or("none", |accent| accent.as_str())
                ),
                color.to_hex().trim_start_matches('#'),
                current.accent == accent,
                move |_, _, cx| {
                    hold_menu(cx);
                    picker.update(cx, |this, cx| this.toggle_accent(accent, cx));
                },
            )
        });
    v_flex()
        .gap_1p5()
        .child(h_flex().flex_wrap().gap_1().children(chips))
        .child(hint("appearance.accent_hint", cx))
        .into_any_element()
}

/// One catalogue run — the emoji or the icons — under its heading.
///
/// Both runs share the panel's single scroll, since neither fits a menu on its
/// own and a chooser that closes to scroll is a chooser that closes.
fn glyph_run(
    picker: &Entity<Picker>,
    current: &Appearance,
    heading: &'static str,
    groups: &[Group],
    note: Option<&'static str>,
    cx: &App,
) -> AnyElement {
    let mut run = v_flex().gap_2().child(section(heading));
    for (last, group) in groups
        .iter()
        .enumerate()
        .map(|(ix, g)| (ix + 1 == groups.len(), g))
    {
        let cells = group
            .cells
            .iter()
            .copied()
            .enumerate()
            .map(|(ix, cell)| cell_view(picker, cell, ix, current, cx));
        run = run
            .child(hint(group.label_key, cx))
            .child(h_flex().flex_wrap().gap_1().children(cells))
            .when(!last, |this| this.child(Separator::horizontal()));
    }
    run.children(note.map(|key| hint(key, cx)))
        .into_any_element()
}

/// One offer on a glyph run: the cell, and what clicking it does.
fn cell_view(
    picker: &Entity<Picker>,
    cell: Cell,
    ix: usize,
    current: &Appearance,
    cx: &App,
) -> AnyElement {
    let picker = picker.clone();
    let picked = current.glyph == Some(cell.glyph());
    div()
        .id(format!("appearance-{}-{ix}", cell.key()))
        .size(px(22.4))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .rounded(cx.theme().radius)
        .when(picked, |this| this.bg(cx.theme().accent))
        .hover(|this| this.bg(cx.theme().accent.opacity(0.55)))
        .child(cell.view(picked, cx))
        .on_click(move |_, _, cx| {
            hold_menu(cx);
            let glyph = cell.glyph();
            picker.update(cx, |this, cx| this.toggle_glyph(glyph, cx));
        })
        .into_any_element()
}

/// A run heading, heavier than the group labels under it.
fn section(key: &'static str) -> AnyElement {
    div()
        .text_sm()
        .font_weight(FontWeight::BOLD)
        .child(rust_i18n::t!(key).to_string())
        .into_any_element()
}

fn hint(key: &str, cx: &App) -> AnyElement {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(rust_i18n::t!(key).to_string())
        .into_any_element()
}

// ---------------------------------------------------------------------------
// The two forms that host the picker
// ---------------------------------------------------------------------------

/// Where a chosen look is written. Both container kinds share everything up to
/// this: only the table differs.
#[derive(Clone, Copy)]
pub(crate) enum Target {
    Collection(Uuid),
    Smart(Uuid),
}

impl Target {
    fn stored(&self, ctl: &LibraryController) -> Option<Appearance> {
        let conn = ctl.library.store().conn();
        match self {
            Self::Collection(id) => trove_core::store::collections::get(conn, *id)
                .ok()
                .flatten()
                .map(|found| found.appearance),
            Self::Smart(id) => trove_core::store::smart_collections::get(conn, *id)
                .ok()
                .flatten()
                .map(|found| found.appearance),
        }
    }

    pub(crate) fn write(
        &self,
        ctl: &LibraryController,
        appearance: &Appearance,
    ) -> Result<(), String> {
        let conn = ctl.library.store().conn();
        let wrote = match self {
            Self::Collection(id) => {
                trove_core::store::collections::set_appearance(conn, *id, appearance)
            }
            Self::Smart(id) => {
                trove_core::store::smart_collections::set_appearance(conn, *id, appearance)
            }
        };
        wrote.map_err(|error| error.to_string())
    }
}

/// The「外观」entry of a container's right-click menu: the chooser *is* the
/// submenu, so the look is picked where the folder was right-clicked and each
/// click lands in the library as it is made.
///
/// The panel is wrapped rather than dropped in bare: a menu item paints the
/// accent behind whatever is hovered, and the chooser is hovered as a whole, so
/// it carries its own ground over that — and over the item's inner padding,
/// which is what the negative margin gives back.
pub(crate) fn submenu_item(
    window: &mut Window,
    cx: &mut App,
    controller: Entity<LibraryController>,
    target: Target,
    name: String,
) -> PopupMenuItem {
    let current = target.stored(controller.read(cx)).unwrap_or_default();
    let chooser = Picker::live(controller, target, name, current, cx);
    let menu = PopupMenu::build(window, cx, move |menu, _, _| {
        menu.min_w(px(160.))
            .item(PopupMenuItem::element(move |_, cx| {
                div()
                    .mx_neg_2()
                    .px_2()
                    .bg(cx.theme().popover)
                    .child(chooser.clone())
            }))
    });

    PopupMenuItem::submenu(rust_i18n::t!("appearance.title").to_string(), menu)
}

/// The chooser as the rule editor's right column, so a smart collection can be
/// given its look while it is still a draft. The host owns the picker and
/// persists its appearance with the new row.
pub(crate) fn column(chooser: &Entity<Picker>, cx: &App) -> AnyElement {
    v_flex()
        .w(px(160.))
        .flex_shrink_0()
        .gap_2()
        .border_l_1()
        .border_color(cx.theme().border)
        .pl_4()
        .child(chooser.clone())
        .into_any_element()
}
