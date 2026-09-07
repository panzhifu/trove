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
        ImportFiles,
        ExportLibrary,
        ImportLibrary,
        ExportMediaPackage,
        FindDuplicates,
        OpenSettings,
        // -- Edit menu / grid shortcuts ------------------------------------
        PasteImport,
        BatchRename,
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
        // -- Grid navigation (WorkspacePanel only) --------------------------
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        OpenPreview,
        // -- Inline editor (ExplorerPanel only) ------------------------------
        CancelEditor,
    ]
);
