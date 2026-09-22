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

> Needs the [Rust toolchain](https://www.rust-lang.org/tools/install). The first launch opens a **welcome window**: the libraries that already exist on the left, a name field on the right to start one. Libraries live where the platform expects them — there is no folder to pick.

This README summarizes what ships. The **[Chinese README](./README.md)** is the canonical, most up-to-date document.

[docs/](docs/README.md)

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
- **Import mode** — link only, never copy: files stay where you keep them and Trove records the path plus a content hash. A moved original can be re-linked after a SHA-256 check.
- **RAW / HEIC / SVG / PSD** — camera RAW through the rawler pipeline; HEIC via system `heif-dec`; SVG rasterized, PSD composites its embedded preview.
- **Design-format thumbnails** — mines EXIF / audio tags / font family·style·weight / MP4 dimensions; video poster when ffmpeg is present.

### Browse & inspect

- **Three views** — grid (justified layout) / list / timeline, with density slider, multi-select and a floating toolbar.
- **Shape & aspect filters** — the toolbar shape filter (landscape / portrait / square) carries media aspect presets — WeChat cover 2.35:1, video 16:9, vertical video 9:16, photo 4:3 / 3:4, square 1:1 — matched with 3% tolerance so rounded dimensions still hit.
- **Inspector** — thumbnail + tags + color palette + inline editing (title / description / source URL / rating); properties page shows MIME / size / dimensions / SHA-256; one-click reveal of the underlying file.
- **Animated images** — GIF / animated WebP / APNG play frame-by-frame in the preview dialog and Inspector; grid thumbnails stay static for performance.
- **Font live previews** — specimen-card thumbnails rasterized in the font itself at import, with a family-name caption; sample text supports `{name}` / `{family}` placeholders and is customizable in Settings; per-font system install / uninstall from the Inspector.
- **Recently viewed** — sidebar of the last 200 assets; trashed assets drop out until restored.

### 3D model preview

- **Formats** — OBJ / STL / PLY / glTF / GLB imported as a first-class asset kind; `.blend` previews through a headless Blender conversion to GLB.
- **GPU viewport** — wgpu-powered, orbit / zoom / pan, two-sided Lambert + Blinn-Phong shading; falls back to a CPU software rasterizer when no GPU is available.
- **Quality** — eye-dome lighting (EDL) + gap fill + back-face culling on closed meshes.
- **Large files** — streaming resident budget + thinning so 20 GB never OOMs; coverage-preserving sampling; smooth zoom and pan; offline spatial index optional (`.trovecloud`, 9 B/point, 60% of source).
- **Mesh culling** — large meshes (≥8192 tris) are clustered into meshlets with per-cluster GPU frustum culling.

### Video & screenshots

- **Audio-capable preview** — frame-by-frame ffmpeg decode plus an audio pipeline (rodio plays 44.1 kHz stereo PCM); play / pause / seek / timeline / volume / mute / speed (0.5×–2× with pitch holding); audio-clock-driven A/V sync.
- **Screenshot capture** — one menu entry: a picker opens, drag to select a region (or click a window where the session offers a window list) and the PNG lands in the library. In-process first (KWin D-Bus → xcap), external tools as the fallback (grim+slurp / scrot / macOS screencapture).
- **Batch pixel editor** — rotate / flip / crop (percent coordinates, resolved per asset from its own dimensions), JPEG quality; replaces the media file in place while preserving asset identity and organization membership.
- **Batch conversion** — re-encode images to JPEG / PNG / WebP / BMP / TIFF, optional longest-edge cap, optional re-import.
- **XMP metadata export** — write a standard XMP sidecar beside each asset's file (title / description / tags / rating), atomic write, fully escaped, never touches the original file.

### Maintenance & safety

- **Trash** — delete → trash → restore / delete forever; emptying frees blobs and thumbnails.
- **Orphan cleanup** — removes blobs no longer referenced by any asset.
- **Integrity check** — re-hashes every stored file and compares with the record; one-click move-to-trash for bad ones.
- **Auto backup** — SQLite `VACUUM INTO` snapshot into `backups/` (at most once a day, rolling 10).
- **Duplicate finder** — clusters visually identical images by pHash; "keep newest, trash the rest" per group.
- **Storage breakdown** — what Trove itself has written to disk, one line per directory (settings and themes / database / backups / thumbnails and index / logs / inbox), with the parts that can be deleted and rebuilt called out.
- **Multiple libraries** — create, switch and delete named libraries; each keeps its own database, watched folders and thumbnail cache, with live statistics (counts, size, tags, collections).

### Extensions

- **Local collect server** — `http://127.0.0.1:23916`, `POST /add` (raw bytes) and `POST /fetch` (server-side fetch); browser extension connects directly.
- **Browser extension** — MV3 addon under `extension/`; right-click any image to send it to your running Trove.
- **Multilingual UI** — 9 languages, switch live in Settings: English, 简体中文, 日本語, 한국어, Español, Français, Deutsch, Português, Русский; follows the system language by default.

---

## Command line

`trove-cli` is the headless sibling of the desktop app: same `trove-core`, same database, same rules — and it runs **while the app has the library open**, in which case the library is opened read-only, queries behave as usual, writes go to the database, and index updates are left to whichever process owns them.

```sh
cargo build -p trove-cli            # produces target/debug/trove

trove libraries                     # every library on this machine
trove info                          # counts, sizes, index state
trove list --kind image -n 20       # list assets (all filters are in --help)
trove search 猫 --aspect wechat-cover
trove get <uuid>                    # full record, with the file's absolute path
trove import ~/Pictures --into Reference
trove tag <uuid> --add animal
trove trash <uuid>                  # reversible; `purge --yes` is not
trove collection create Reference
trove doctor                        # library, index and ffmpeg self-check
```

Three conventions:

- **stdout is always one JSON document**; `--human` swaps in tables.
- **The exit status decides whether to parse it**: 0 ok, 1 failed, 2 misuse, 3 library unusable (absent, wrong schema version, or a write that needs an index another process holds).
- Diagnostics go to stderr; `--quiet` keeps errors only.

`--help` is the whole contract. `--library <slug>` picks a library, and `TROVE_DATA_DIR` / `TROVE_CONFIG_DIR` / `TROVE_CACHE_DIR` relocate the entire environment.

---

## Layout

| Dock | Panel | Purpose |
|---|---|---|
| Top | Title bar | File / Settings buttons, window controls |
| Left | Explorer | Collection tree, smart collections, recently viewed, trash |
| Center | Workspace | Justified thumbnail grid + search |
| Right | Tags + Inspector | Tag filter and per-asset details |

**Settings** opens a window with six pages — About (version, release check, language) · Appearance (light/dark, themes, custom themes) · Files (storage, libraries, watched folders, thumbnails, backups, cleanup) · Model (point-cloud look, axes, preview zoom) · Search (full-text index, visual fingerprints) · Shortcuts. With no library yet, launch opens the **welcome window** instead of the main one.

- **System tray** — closing the window minimizes to the tray (KDE/freedesktop StatusNotifierItem / Windows notification icon / macOS NSStatusItem); the tray menu restores the window or quits for good.

---

## Build & test

```sh
cargo build
cargo test -p trove-core
cargo run -p trove-app
```

**Baseline: `trove-core` 519 + `trove-app` 48 all pass; `cargo fmt --check` clean; clippy 0 warnings workspace-wide.** Two real-GPU smoke tests live in `trove-app` (EDL, meshlet culling) and skip automatically on headless machines.

Per-module implementation notes are indexed in [docs/README.md](docs/README.md).

---

## License

[MIT](./LICENSE)

[gpui-kit]: https://github.com/panzhifu/gpui-kit
[rusqlite]: https://github.com/rusqlite/rusqlite
