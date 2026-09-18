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
        ScreenshotFull,
        ScreenshotRegion,
        ExportLibrary,
        ImportLibrary,
        ExportMediaPackage,
        ExportXmp,
        FindDuplicates,
        OpenSettings,
        // -- Edit menu / grid shortcuts ------------------------------------
        PasteImport,
        CopyImage,
        BatchRename,
        BatchConvert,
        BatchEdit,
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
        // -- Grid navigation (WorkspacePanel only) --------------------------
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        OpenPreview,
        // -- Inline editor (ExplorerPanel only) ------------------------------
        CancelEditor,
        // -- Fullscreen video stage -----------------------------------------
        EnterVideoFullscreen,
        ExitVideoFullscreen,
        // -- Region screenshot overlay ---------------------------------------
        CancelScreenshotRegion,
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
