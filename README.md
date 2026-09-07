# Trove

> A local, private asset library — your photos, documents, audio and video in one searchable, taggable place.

Trove is a desktop asset manager built in Rust. It uses **[gpui-kit]** for the interface and **[rusqlite]** (SQLite) for single-file persistence. Assets are stored content-addressed on disk, so a file is stored exactly once no matter how it is organized.

[中文文档 (Chinese README)](./README.zh.md)

---

## Quick start

```sh
git clone https://github.com/panzhifu/trove.git && cd trove
cargo run -p trove-app
```

The first launch creates a library under the platform's config directory. Open **Settings** in the title bar to change the library location.

---

## Features

### Asset import
- Drag-and-drop files onto the window, or pick files from the system dialog.
- Content-addressed blob storage: identical files are deduplicated.
- Type probing and metadata mining on import — dimensions, duration, dominant color and more.
- **Smart collections automatically capture matching assets** — no manual sorting needed.

### Collections & tree navigation
- Nested collections form a tree with many-to-many asset membership.
- Cycle detection prevents accidental loops when moving folders.
- Cascading delete removes child folders.

### Asset types
- Images, videos, audio, documents, archives, **fonts** (ttf/otf/ttc/woff), and more — with per-kind icons in the grid.
- Import-time mining: EXIF, audio tags & duration, font family/style/weight, MP4 dimensions & duration; video posters via the system `ffmpeg` when available.

### Bulk selection
- Multi-select with Ctrl/Cmd+click or Shift range click; a floating toolbar offers favorite / add-to-collection / trash / clear.

### Tags & favorites & ratings
- Case-insensitive tags attach to any asset.
- One-click favorites and a 1–5 rating scale.

### Full-text search (FTS)
- Ranked full-text search over titles and descriptions, combined with any filter.
- Special characters are handled safely and literally.
- A maintenance command can rebuild the index after a migration.

### Visual search (v0.2)
- **Search by image**: find visually similar images using perceptual hash + color histogram.
- **Search by color**: find images matching a specific hex color (e.g. `#ff8000`).
- Visual signatures computed in background after import — no slowdown.

### Semantic search (CLIP, optional)
- Text-to-image search merged into the same search box: keyword (FTS) hits first, CLIP matches appended and deduplicated.
- Image-to-image search against CLIP embeddings in the visual-search panel.
- Embeddings are computed automatically after import; "Embed all" backfills existing images (Settings ▸ Search).
- Requirements (manual download, paths shown in Settings ▸ Search): the ONNX Runtime shared library, the CLIP ViT-B/32 ONNX model (`model.onnx`) and its BPE vocab (`bpe_simple_vocab_16e6.txt`) in the model directory.
- The model is English-caption trained — English queries match noticeably better than other languages.

### Smart collections
- Rule-based virtual folders defined as JSON query trees.
- Match on rating, kind, text, tag, favorite, color; combine with `and` / `or`.
- Trees are validated at compile time, and results support pagination.

### Trash & cleanup
- Deleted assets go to the trash, with restore at any time.
- Emptying the trash frees the underlying blobs and thumbnails.
- Orphan cleanup removes blobs no longer referenced by any asset.

### Drag & drop
- Drag files from the file manager onto the window to import.
- Drag assets onto a collection or the trash in the explorer.
- Drag assets onto a tag to tag them in bulk.
- Multi-select with Ctrl/Cmd+click; drag moves the whole selection.

### Context menus
- Asset: favorite, add to collection, move to trash / restore / delete forever.
- Collection: new sub-collection, rename, delete.
- Tag: filter by tag, delete.
- Smart collection: delete.
- Inline editing: add/rename collections via Enter-to-confirm editors.

### Inspector
- Thumbnail preview with dynamic height based on image aspect ratio.
- Tags and mined color palette.
- Inline editing: title, description, source URL, kind, and a 1–5 star rating — committed on blur/Enter or click.
- Properties: MIME type, size, dimensions, added date, SHA-256.
- Add or remove tags directly.

### Interface language
- English and 简体中文, switchable live in Settings ▸ Language; follows the system language by default.

### Configuration
- Persisted JSON config in the platform config directory.
- The library location can be changed from the Settings dialog.

---

## UI layout

![Trove main window](docs/screenshots/main-window.png)

The desktop app uses a dock layout with a custom title bar:

| Dock | Panel | Purpose |
|------|-------|---------|
| Top | Title bar | File / Settings buttons, window controls |
| Left | Explorer | Collection tree, smart collections, trash |
| Center | Workspace | Justified thumbnail grid + search |
| Right | Tags + Inspector | Tag filter and per-asset details |

- The **File** menu imports files; **Settings** opens the library-path dialog.
- The whole window is a drop surface — drop any files to import them.
- The workspace grid is a justified (Google-Photos-style) layout that fills the panel edge-to-edge at any width.
- A popover search input sits in the workspace title bar, next to the item count of the browsed view.

---

## Project structure

```
crates/
├── trove-core/          # Domain, persistence & services (no UI)
│   ├── src/
│   │   ├── model.rs     # Plain data types (Asset, Collection, Tag, …)
│   │   ├── library.rs   # High-level facade over store + media dir
│   │   ├── layout.rs    # Justified grid layout (dynamic programming)
│   │   ├── store/       # SQLite layer: schema, CRUD, FTS, smart queries
│   │   ├── media/       # Import, probing, thumbnails, color, visual + CLIP search
│   │   ├── maintenance.rs # Rebuild thumbs/index, orphan cleanup
│   │   ├── events.rs    # Cross-layer events
│   │   ├── config.rs    # App config persistence (JSON)
│   │   └── error.rs     # Error types
│   └── src/             # inline #[cfg(test)] modules
└── trove-app/           # gpui-kit desktop UI
    ├── src/
    │   ├── main.rs       # GPUI bootstrap
    │   ├── app.rs        # Root view: dock + title bar + drop surface
    │   ├── state.rs      # LibraryController (selection, browse, import)
    │   ├── title_bar.rs  # Custom title bar (File / Settings)
    │   ├── settings.rs   # Settings dialog
    │   ├── jobs.rs       # Background import with progress
    │   └── panels/       # Explorer, Workspace, Tags, Inspector
    └── Cargo.toml
```

Key storage tables:

```
assets             # asset records
collections        # nested folders
asset_collection   # many-to-many membership
tags               # case-insensitive tags
asset_tag          # asset–tag links
smart_collections  # rule-based virtual folders
asset_fts          # full-text search index
```

A library on disk:

```
<root>/
├── library.db      # single-file database
└── media/…         # content-addressed blobs
```

---

## Build & test

Requires the Rust toolchain. The workspace has no external system dependencies for the core crate.

```sh
cargo build
cargo test -p trove-core
cargo run -p trove-app
```

Test status: `trove-core` compiles and all **75** tests pass. `trove-app` compiles cleanly.

---

## Status

- **trove-core** — feature-complete for the above list; tested (83 tests).
- **trove-app** — compiles and runs: dock layout, custom title bar, justified thumbnail grid, drag & drop, multi-select, context menus, settings dialog, inspector, visual + semantic search, and import with progress are all wired.

---

## License

MIT

[gpui-kit]: https://github.com/panzhifu/gpui-kit
[rusqlite]: https://github.com/rusqlite/rusqlite
