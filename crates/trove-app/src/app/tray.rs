//! System tray: an icon in the platform's tray area with a small menu.
//!
//! The tray is what keeps the app reachable after the window is closed: the
//! close button only minimizes the window (see [`AppView`](crate::app::AppView)),
//! and the menu here is the way back — or out, for good.
//!
//! The menu and the icon are built by `tray-icon`, which speaks each
//! platform's own protocol: on Linux that is the KDE/freedesktop
//! StatusNotifierItem over D-Bus (the `ksni` backend, so no GTK is needed),
//! on Windows the shell's notify icon, on macOS `NSStatusItem`.
//!
//! Clicks arrive on the tray's own thread, so they are put on a channel here
//! and drained by the app view on the main thread: everything a command
//! touches — windows, actions, the library — belongs there.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

/// How often the app view drains the tray's channel. Menu clicks are rare,
/// and a missed frame costs nothing, so this is deliberately slow.
pub(crate) const POLL: Duration = Duration::from_millis(120);

/// The icon, at a size a HiDPI tray can scale down from rather than up.
/// Embedded because the tray has to exist before anything is read from disk,
/// and because an installed app has no source tree to read from.
const ICON_64: &[u8] = include_bytes!("../../../../design/icon/trove-64.png");

// Menu ids: what comes back from a click, so they are the contract with
// [`TrayCommand`] below.
const ID_SHOW: &str = "trove.tray.show";
const ID_IMPORT: &str = "trove.tray.import";
const ID_SETTINGS: &str = "trove.tray.settings";
const ID_QUIT: &str = "trove.tray.quit";

/// What the tray asked the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayCommand {
    /// Bring the window back. It is only minimized, never gone.
    ShowWindow,
    /// Open the file picker: the same action the File menu dispatches.
    ImportFiles,
    /// Open the settings dialog.
    OpenSettings,
    /// Leave for good, taking the tray icon along.
    Quit,
}

/// The tray icon and the channel its menu writes to.
///
/// Dropping this removes the icon, which is how quitting cleans up: the app
/// must not vanish from the shell while it is still shutting down, nor stay
/// behind after it has.
pub(crate) struct Tray {
    /// Held for its `Drop` — the icon lives as long as this struct does.
    _icon: TrayIcon,
    /// The menu has to outlive the icon that shows it, and the ids have to
    /// stay registered for clicks to resolve.
    _menu: Menu,
    commands: Receiver<TrayCommand>,
}

impl Tray {
    /// Put the icon in the tray and wire its menu up.
    ///
    /// Failing is not fatal: a desktop without a tray (or one that refuses
    /// the icon) simply gets no tray, and the app carries on. The window's
    /// close button follows suit — see the caller.
    pub(crate) fn install() -> Option<Self> {
        let icon = decode_icon()?;
        let (tx, commands) = mpsc::channel();
        let menu = build_menu();
        // One handler per process, installed once: both are process-wide
        // singletons in `tray-icon`.
        MenuEvent::set_event_handler(Some(menu_handler(tx.clone())));
        // macOS pops the menu on click by itself and would double up with a
        // show-window command; everywhere else a click on the icon means
        // "bring it back".
        #[cfg(not(target_os = "macos"))]
        tray_icon::TrayIconEvent::set_event_handler(Some(icon_handler(tx.clone())));

        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip("Trove")
            .with_icon(icon)
            // macOS only, and ignored elsewhere: draw the icon as a template
            // so the shell tints it for light and dark menu bars.
            .with_icon_as_template(true)
            .build()
            .map_err(|e| tracing::warn!("tray icon unavailable: {e}"))
            .ok()?;

        Some(Self {
            _icon: tray_icon,
            _menu: menu,
            commands,
        })
    }

    /// The next command the menu has sent, if any. Called from the main
    /// thread; never blocks.
    pub(crate) fn poll(&self) -> Option<TrayCommand> {
        self.commands.try_recv().ok()
    }
}

/// Turn the embedded PNG into the RGBA the tray wants.
fn decode_icon() -> Option<Icon> {
    let image = image::load_from_memory(ICON_64)
        .map_err(|e| tracing::warn!("tray icon undecodable: {e}"))
        .ok()?
        .to_rgba8();
    let (width, height) = (image.width(), image.height());
    Icon::from_rgba(image.into_raw(), width, height)
        .map_err(|e| tracing::warn!("tray icon rejected: {e}"))
        .ok()
}

/// The menu: the four things worth doing from the tray.
fn build_menu() -> Menu {
    use rust_i18n::t;

    let menu = Menu::new();
    let show = MenuItem::with_id(ID_SHOW, t!("tray.show_window"), true, None);
    let import = MenuItem::with_id(ID_IMPORT, t!("tray.import_files"), true, None);
    let settings = MenuItem::with_id(ID_SETTINGS, t!("tray.open_settings"), true, None);
    // Not `PredefinedMenuItem::quit`: that one exits the process on the spot,
    // skipping the cleanup the app still owes its windows and the tray itself.
    let quit = MenuItem::with_id(ID_QUIT, t!("tray.quit"), true, None);

    let _ = menu.append(&show);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&import);
    let _ = menu.append(&settings);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit);
    menu
}

/// Menu clicks, on the tray's thread: only forwarded.
fn menu_handler(tx: Sender<TrayCommand>) -> impl Fn(MenuEvent) {
    move |event: MenuEvent| {
        let command = match event.id().0.as_str() {
            ID_SHOW => TrayCommand::ShowWindow,
            ID_IMPORT => TrayCommand::ImportFiles,
            ID_SETTINGS => TrayCommand::OpenSettings,
            ID_QUIT => TrayCommand::Quit,
            other => {
                tracing::debug!("unknown tray menu id: {other}");
                return;
            }
        };
        // A closed channel means the app is going away; there is nothing to
        // tell it any more.
        let _ = tx.send(command);
    }
}

/// Clicks on the icon itself: a left click brings the window back.
#[cfg(not(target_os = "macos"))]
fn icon_handler(tx: Sender<TrayCommand>) -> impl Fn(tray_icon::TrayIconEvent) {
    use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

    move |event: TrayIconEvent| {
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            let _ = tx.send(TrayCommand::ShowWindow);
        }
    }
}
