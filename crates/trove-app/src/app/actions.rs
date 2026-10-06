//! App-wide actions shared by the native menu bar (`set_menus` +
//! `AppMenuBar`) and the keyboard shortcuts (`bind_keys`).
//!
//! Keyboard-only actions that need the asset grid's row geometry
//! ([`MoveLeft`](self::MoveLeft) …) are handled by the `WorkspacePanel`;
//! the rest are handled by [`AppView`](crate::app::AppView) so they work
//! wherever the focus is.

gpui_kit::actions!(
    trove,
    [
        // -- File menu ----------------------------------------------------
        ManageLibraries,
        ImportFiles,
        ImportUrl,
        ImportFromApp,
        Screenshot,
        ExportXmp,
        ExportRepository,
        ImportRepository,
        FindDuplicates,
        OpenSettings,
        // -- Edit menu / grid shortcuts ------------------------------------
        PasteImport,
        CopyImage,
        BatchRename,
        BatchConvert,
        BatchEdit,
        AutoTag,
        SelectAll,
        ClearSelection,
        TrashSelected,
        Undo,
        Redo,
        // -- View menu -----------------------------------------------------
        ShowAllAssets,
        ShowTrash,
        RefreshLibrary,
        // -- Help menu -----------------------------------------------------
        About,
        CheckUpdates,
        // -- macOS app-menu conventions --------------------------------------
        // The actions exist on every platform (they are plain gpui ones);
        // only their *placement* is macOS-specific: About / Settings / Hide /
        // Quit belong to the app menu and Minimize / Zoom to a Window menu
        // (see `build_menus`). The cmd-q / cmd-h / cmd-m chords ride with
        // them, bound in `keybindings::register`.
        Quit,
        HideApp,
        MinimizeWindow,
        ZoomWindow,
        // -- Grid navigation (WorkspacePanel only) --------------------------
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        OpenPreview,
        QuickLook,
        // -- Search ----------------------------------------------------------
        // Ctrl+K wherever the focus is: the search pill opens with the caret
        // in it and the committed query selected. Handled on `AppView` (the
        // window root), so it reaches the search box from any panel.
        FocusSearch,
        // -- Fullscreen video stage -----------------------------------------
        EnterVideoFullscreen,
        ExitVideoFullscreen,
        // -- Live playback ---------------------------------------------------
        // Space on a previewed video or audio file: hold the soundtrack, or
        // pick it back up where it stopped.
        TogglePlayback,
        // `,` and `.` step one frame back / forward on the previewed clip —
        // video or animated image — and leave it holding that frame.
        StepFrameBack,
        StepFrameForward,
        // -- Cancel ----------------------------------------------------------
        // Escape, globally: an inline editor closes, a fullscreen stage
        // steps down, an overlay dismisses. Handled by whichever view on the
        // focus path has a cancellation to perform; nothing where none does.
        Cancel,
    ]
);

/// Run a plugin command by id (`"plugin-id/command-id"`), dispatched from a
/// keybinding the user bound in Settings ▸ Shortcuts.
///
/// One static action carries every plugin's commands — gpui actions are
/// static types, so a plugin's commands cannot each be their own type. The
/// payload routes to the plugin that declared the command; the handler on
/// each window's root forwards to `plugins::run_command`.
#[derive(Clone, PartialEq, gpui_kit::Action)]
#[action(no_json, namespace = trove)]
pub struct RunPluginCommand {
    pub command: std::sync::Arc<str>,
}
