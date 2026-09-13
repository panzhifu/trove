<h1 align="center">Trove</h1>

<p align="center"><b>Local · Private · Your own asset library</b></p>

<p align="center">
  <a href="./README.md">中文（主要文档）</a> ·
  <a href="./LICENSE">MIT</a>
</p>

<br>

<p align="center">
  <img src="docs/screenshots/main-window.png" alt="Trove main window" width="900"/>
</p>

<p align="center"><i>Justified grid · dock layout · 3D model viewport · visual search — all local, data never leaves your machine</i></p>

---

## Quick start

```sh
git clone https://github.com/panzhifu/trove.git && cd trove
cargo run -p trove-app
```

> Needs the [Rust toolchain](https://www.rust-lang.org/tools/install). First launch creates a library under your platform's config directory; open **Settings** to relocate it.

This README summarizes what ships. The **[Chinese README](./README.zh.md)** is the canonical, most up-to-date document.

[docs/](docs/README.md) · [Feature gaps](docs/FEATURE-GAPS.md)

---

## What's inside

### Organize & find

- **Full-text search** — Tantivy-backed, over file name / title / description / tag name, ranked. Word, substring and pinyin matching (`sunse` → `Sunset`, `mao` → 花园里的猫), composable with any filter.
- **Visual search** — search by image + search by color. Perceptual hash (pHash) + color histogram, computed at import, zero inference required.
- **Smart collections** — rule-driven virtual folders as JSON query trees. Match on rating / kind / text / tag / favorite / color / date / aspect ratio / orientation; `and` / `or`. Validated at compile time.
- **Tags** — hierarchical, case-insensitive, color labels; filters, counts and smart collections include subtrees.
- **Ratings & favorites** — 1–5 stars, one-click favorite.
- **Collection tree** — nested folders, many-to-many membership, drag to reparent, cycle detection.

### Import & collect

- **Drag-and-drop** — drop files from the file manager onto the window (the whole window is a drop surface).
- **Paste to import** — `Ctrl+Shift+V` sends a clipboard image straight into the library.
- **Import from URL** — downloads in the background, imports, records the source URL.
- **Watched folders** — add a directory in Settings; new files are imported automatically.
- **Import mode** — copy into library (default) / link to original. Linked assets stay where they are and can be re-linked by SHA-256 after moving.
- **RAW / HEIC / SVG / PSD** — camera RAW through the rawler pipeline; HEIC via system `heif-dec`; SVG rasterized, PSD composites its embedded preview.
- **Design-format thumbnails** — mines EXIF / audio tags / font family·style·weight / MP4 dimensions; video poster when ffmpeg is present.

### Browse & inspect

- **Three views** — grid (justified layout) / list / timeline, with density slider, multi-select and a floating toolbar.
- **Inspector** — thumbnail + tags + color palette + inline editing (title / description / source URL / rating); properties page shows MIME / size / dimensions / SHA-256; one-click reveal of the underlying file.
- **Animated images** — GIF / animated WebP / APNG play frame-by-frame in the preview dialog and Inspector; grid thumbnails stay static for performance.
- **Font live previews** — specimen-card thumbnails rasterized in the font itself at import; sample text customizable in Settings; a Fonts system view and per-font system install / uninstall from the Inspector.
- **Recently viewed** — sidebar of the last 200 assets; trashed assets drop out until restored.

### 3D model preview

- **Formats** — OBJ / STL / PLY imported as a first-class asset kind.
- **GPU viewport** — wgpu-powered, orbit / zoom / pan, two-sided Lambert + Blinn-Phong shading; falls back to a CPU software rasterizer when no GPU is available.
- **Quality** — eye-dome lighting (EDL) + gap fill + back-face culling on closed meshes.
- **Large files** — streaming resident budget + thinning so 20 GB never OOMs; coverage-preserving sampling; smooth zoom and pan; offline spatial index optional (`.trovecloud`, 9 B/point, 60% of source).
- **Mesh culling** — large meshes (≥8192 tris) are clustered into meshlets with per-cluster GPU frustum culling.

### Video & screenshots

- **Silent preview** — frame-by-frame ffmpeg decode with play / pause / seek / timeline; no audio pipeline.
- **Screenshot capture** — full screen or interactive region, imported as PNG. Platform-native backends (gnome-screenshot / scrot / macOS screencapture / Windows snippingtool), user-overridable.
- **Batch conversion** — re-encode images to JPEG / PNG / WebP / BMP / TIFF, optional longest-edge cap, optional re-import.

### Maintenance & safety

- **Trash** — delete → trash → restore / delete forever; emptying frees blobs and thumbnails.
- **Orphan cleanup** — removes blobs no longer referenced by any asset.
- **Integrity check** — re-hashes every stored file and compares with the record; one-click move-to-trash for bad ones.
- **Auto backup** — SQLite `VACUUM INTO` snapshot into `backups/` (at most once a day, rolling 10).
- **Duplicate finder** — clusters visually identical images by pHash; "keep newest, trash the rest" per group.
- **Library hot-switch** — recent libraries; live statistics (counts, size, tags, collections).

### Extensions

- **Local collect server** — `http://127.0.0.1:23916`, `POST /add` (raw bytes) and `POST /fetch` (server-side fetch); browser extension connects directly.
- **Browser extension** — MV3 addon under `extension/`; right-click any image to send it to your running Trove.
- **Bilingual** — English / 简体中文, switch live in Settings; follows the system language by default.

---

## Layout

| Dock | Panel | Purpose |
|---|---|---|
| Top | Title bar | File / Settings buttons, window controls |
| Left | Explorer | Collection tree, smart collections, recently viewed, trash |
| Center | Workspace | Justified thumbnail grid + search |
| Right | Tags + Inspector | Tag filter and per-asset details |

---

## Build & test

```sh
cargo build
cargo test -p trove-core
cargo run -p trove-app
```

**Baseline: `trove-core` 310 + `trove-app` 22 all pass; `cargo fmt --check` clean; clippy 0 warnings workspace-wide.** Two real-GPU smoke tests live in `trove-app` (EDL, meshlet culling) and skip automatically on headless machines.

What is still missing vs. Eagle / Billfish / digiKam / Adobe Bridge is mapped in [docs/FEATURE-GAPS.md](docs/FEATURE-GAPS.md).

---

## License

[MIT](./LICENSE)

[gpui-kit]: https://github.com/panzhifu/gpui-kit
[rusqlite]: https://github.com/rusqlite/rusqlite
