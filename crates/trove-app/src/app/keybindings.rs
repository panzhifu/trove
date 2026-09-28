//! Keyboard binding management, in one place.
//!
//! Three sources meet here, in priority order:
//!
//! 1. **Defaults** — [`default_keybindings`](trove_core::keybindings::default_keybindings),
//!    the single source of truth for which actions are user-configurable and
//!    what key each starts with.
//! 2. **User overrides** — `AppConfig.keybindings` (config.json), the map
//!    Settings ▸ Shortcuts writes into. Looked up first; a default is used
//!    only when no override names the action.
//! 3. **Plugin commands** — declared by registered plugins
//!    ([`trove_core::plugins::all`]), each with its own default key and the
//!    user's override on top.
//!
//! Fixed furniture that is not configurable at all — the backspace alias, the
//! paste chord, Escape — is bound literally via `bind_fixed!`. Escape binds
//! one global [`Cancel`](crate::app::actions::Cancel) action: whichever view
//! on the focus path has a cancellation to perform handles it, and views
//! guard their own handlers so a stray Cancel is a no-op.

use gpui_kit::{App, KeyBinding};

use crate::app::actions::*;

/// Keyboard map for the asset grid. The `Workspace` key context is active
/// only while the grid (or one of its cells) holds focus, so typing in the
/// search input or elsewhere never triggers grid navigation.
pub(crate) const WORKSPACE_CONTEXT: &str = "Workspace";

/// Key context of the asset grid itself — the scrolling tiles, not the toolbar
/// above them. It exists for the one binding a wider context could not carry:
/// the space bar for quick look. `Workspace` covers the search input too, and a
/// binding for a bare character there is that character, gone from typing.
pub(crate) const GRID_CONTEXT: &str = "AssetGrid";

/// Key context of the video preview (a video is open in the main area). It
/// exists only while a video is previewed, so a bare character bound here — the
/// fullscreen letter, the play/pause space — never shadows typing in the search
/// box, which sits in the `Workspace` context. `Workspace` rides along on the
/// same node while a preview is open, so the grid's own bindings keep working
/// over the preview.
pub(crate) const VIDEO_PREVIEW_CONTEXT: &str = "VideoPreview";

/// Key context of the fullscreen video window. Its only binding is Escape:
/// leaving hands playback back to the main window. The context lives only
/// on that window's root, so the main window's Escape handlers are
/// untouched.
pub(crate) const VIDEO_FULLSCREEN_CONTEXT: &str = "VideoFullscreen";

/// Key context of the screenshot picker overlay. Its only binding is
/// Escape: it cancels the pick. The context lives only on the overlay's
/// root, so nothing else sees it.
///
/// Gated with the picker itself: the overlay is Linux-only, so on the other
/// two platforms this name would have no reader at all.
#[cfg(target_os = "linux")]
pub(crate) const CAPTURE_PICK_CONTEXT: &str = "CapturePick";

pub(crate) fn register(cx: &mut App, config: &trove_core::config::AppConfig) {
    use trove_core::keybindings::default_keybindings;

    // The defaults, indexed once: every binding below asks what its action's
    // default key is, and thirty-odd linear scans over the table cost more
    // than the one map that answers them all.
    let defaults: std::collections::HashMap<&str, &str> = default_keybindings()
        .iter()
        .map(|d| (d.action, d.key))
        .collect();

    // Resolve effective key for an action (custom override > default).
    let key_for = |action: &str, fallback: &str| -> String {
        config
            .keybindings
            .get(action)
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    };

    let mut bindings = vec![];
    let default_key = |action: &str| defaults.get(action).copied();

    // `bind!` is for actions with a default key the user may override in
    // Settings ▸ Shortcuts; one argument defaults the context to the
    // workspace, a second names it — `None` for a binding that works wherever
    // focus is. The action's name is its identifier spelled out, so
    // `stringify!` is the one place the pairing lives. `bind_fixed!` is for
    // keys that are part of the furniture and take no configuration: an
    // alias, the paste chord, Escape.
    macro_rules! bind {
        ($action:ident) => {
            bind!($action, Some(WORKSPACE_CONTEXT));
        };
        ($action:ident, $context:expr) => {
            let name = stringify!($action);
            if let Some(k) = default_key(name) {
                let k = key_for(name, k);
                if !k.is_empty() {
                    bindings.push(KeyBinding::new(&k, $action, $context));
                }
            }
        };
    }

    macro_rules! bind_fixed {
        ($action:ident, $key:literal, $context:expr) => {
            bindings.push(KeyBinding::new($key, $action, $context));
        };
    }

    bind!(MoveLeft);
    bind!(MoveRight);
    bind!(MoveUp);
    bind!(MoveDown);
    bind!(OpenPreview);
    bind!(QuickLook, Some(GRID_CONTEXT));
    bind!(TrashSelected);
    // Backspace stays a fixed alias for the same action.
    bind_fixed!(TrashSelected, "backspace", Some(WORKSPACE_CONTEXT));
    bind!(SelectAll);
    bind!(ClearSelection);
    bind!(Undo);
    bind!(Redo);
    // Menu-only until the user binds a key in Settings ▸ Shortcuts.
    bind!(BatchRename);
    bind!(BatchConvert);
    bind!(AutoTag);
    bind!(CopyImage);
    // Paste import is global (works wherever focus is).
    bind_fixed!(PasteImport, "ctrl-shift-v", None);
    // Escape is the one cancel, everywhere: an inline editor closes, the
    // fullscreen video stage steps down, the capture picker dismisses. The
    // views own their handlers; this is only the key.
    bind_fixed!(Cancel, "escape", None);
    // `f` leaves the fullscreen stage too, mirroring the enter key: the stage
    // replaces the preview, so the enter binding is out of the dispatch path
    // there and the toggle needs its own binding.
    bind!(ExitVideoFullscreen, Some(VIDEO_FULLSCREEN_CONTEXT));
    // `f` puts the video preview into the fullscreen stage. Its context is
    // the preview's, not `Workspace`, so the letter is only live while a
    // video is on screen — the search box shares the `Workspace` context and
    // would lose the letter otherwise.
    bind!(EnterVideoFullscreen, Some(VIDEO_PREVIEW_CONTEXT));
    // Space holds and resumes the video that replaced the grid. Its context is
    // the preview's, not the grid's, so the two space bars never both match:
    // `AssetGrid` is not on the focus path while a preview is up, and
    // `VideoPreview` is not on it while the grid is.
    bind!(TogglePlayback, Some(VIDEO_PREVIEW_CONTEXT));

    // Plugin commands: declared by registered plugins, bound with each
    // command's default key or the user's override (an empty effective key
    // means "not on the keyboard yet" — the command still dispatches from
    // wherever a menu shows it). Rebinding goes through Settings ▸ Shortcuts,
    // where these rows sit beside the built-in ones.
    for plugin in trove_core::plugins::all() {
        for command in plugin.commands() {
            let key = key_for(command.action, command.key);
            if key.is_empty() {
                continue;
            }
            bindings.push(KeyBinding::new(
                &key,
                RunPluginCommand {
                    command: command.action.into(),
                },
                (!command.global).then_some(WORKSPACE_CONTEXT),
            ));
        }
    }

    // Screenshots are global (like paste import), not grid-scoped.
    bind!(Screenshot, None);

    cx.bind_keys(bindings);
}
