# Trove

> A local, private asset library — your photos, documents, audio and video in one searchable, taggable place.
> 一个本地、私有的素材库 —— 把图片、文档、音视频统一收纳，可搜索、可打标签、可智能筛选。数据完全留在本机。

Trove is a desktop asset manager built in Rust. It uses **[gpui-kit]** for the interface and **[rusqlite]** (SQLite) for single-file persistence. Assets are stored content-addressed on disk, so a file is stored exactly once no matter how it is organized.

[Trove 是一个用 Rust 构建的桌面端素材管理工具。界面基于 **[gpui-kit]**，数据持久化使用 **[rusqlite]**（SQLite）。文件以内容寻址方式存储，无论被归入多少个文件夹或标签，同一文件在磁盘上只保存一份。]

---

## Quick start / 快速开始

```sh
# Clone / 克隆
git clone https://github.com/panzhifu/trove.git && cd trove

# Run the desktop app / 运行桌面应用
cargo run -p trove-app

# Run the core tests / 运行核心库测试
cargo test -p trove-core
```

The first launch creates a library under the platform's config directory. Open **Settings** in the title bar to change the library location.

首次启动会在平台标准配置目录下创建素材库。打开标题栏的 **Settings** 可修改素材库位置。

---

## Features / 功能特性

### Asset import / 资产导入
- Drag-and-drop files onto the window, or pick files from the system dialog.
- 支持把文件拖入窗口，或通过系统对话框选择文件导入。
- Content-addressed blob storage: identical files are deduplicated.
- 内容寻址存储：相同文件自动去重。
- Type probing and metadata mining on import — dimensions, duration, dominant color and more.
- 导入时自动识别类型并提取元数据：尺寸、时长、主色调等。

### Collections & tree navigation / 收藏夹与树形导航
- Nested collections form a tree with many-to-many asset membership.
- 支持嵌套的收藏夹树，资产与收藏夹是多对多关系。
- Cycle detection prevents accidental loops when moving folders.
- 移动文件夹时会检测环，避免形成循环依赖。
- Cascading delete removes child folders.
- 删除父文件夹会级联删除子文件夹。

### Asset types / 资产类型
- Images, videos, audio, documents, archives, **fonts** (ttf/otf/ttc/woff), and more — with per-kind icons in the grid.
- 图片、视频、音频、文档、压缩包、**字体**（ttf/otf/ttc/woff）等类型，网格中按类型显示图标。
- Import-time mining: EXIF, audio tags & duration, font family/style/weight, MP4 dimensions & duration; video posters via the system `ffmpeg` when available.
- 导入时挖掘元数据：EXIF、音频标签与时长、字体族/样式/字重、MP4 尺寸与时长；系统装有 `ffmpeg` 时自动生成视频封面。

### Bulk selection / 批量操作
- Multi-select with Ctrl/Cmd+click or Shift range click; a floating toolbar offers favorite / add-to-collection / trash / clear.
- Ctrl/Cmd+点击或 Shift 范围选择；浮动工具栏提供收藏 / 加入收藏夹 / 移入回收站 / 清除。

### Tags & favorites & ratings / 标签、收藏与评分
- Case-insensitive tags attach to any asset.
- 标签不区分大小写，可附加到任意资产。
- One-click favorites and a 1–5 rating scale.
- 一键收藏，以及 1–5 星评分。

### Full-text search (FTS) / 全文搜索
- Ranked full-text search over titles and descriptions, combined with any filter.
- 基于标题和描述的全文搜索（带相关性排序），并可与任意筛选条件组合。
- Special characters are handled safely and literally.
- 特殊字符会被安全地按字面处理，不会导致崩溃。
- A maintenance command can rebuild the index after a migration.
- 提供重建索引的维护命令，迁移后可按需重建。

### Smart collections / 智能收藏夹
- Rule-based virtual folders defined as JSON query trees.
- 基于规则的虚拟文件夹，用 JSON 查询树描述。
- Match on rating, kind, text, tag, favorite, color; combine with `and` / `or`.
- 支持按评分、类型、文本、标签、收藏、颜色匹配，可用 `and` / `or` 组合。
- Trees are validated at compile time, and results support pagination.
- 查询树在编译期即校验，结果支持分页。

### Trash & cleanup / 回收站与清理
- Deleted assets go to the trash, with restore at any time.
- 删除的资产进入回收站，可随时恢复。
- Emptying the trash frees the underlying blobs and thumbnails.
- 清空回收站会释放底层 blob 文件与缩略图。
- Orphan cleanup removes blobs no longer referenced by any asset.
- 孤儿清理会移除不再被任何资产引用的 blob 文件。

### Drag & drop / 拖放
- Drag files from the file manager onto the window to import.
- 从文件管理器拖文件到窗口即可导入。
- Drag assets onto a collection or the trash in the explorer.
- 在左侧导航中把资产拖到收藏夹或回收站。
- Drag assets onto a tag to tag them in bulk.
- 把资产拖到标签上可批量打标。
- Multi-select with Ctrl/Cmd+click; drag moves the whole selection.
- Ctrl/Cmd+点击多选；拖拽移动整个选区。

### Context menus / 右键菜单
- Asset: favorite, add to collection, move to trash / restore / delete forever.
- 资产：收藏、加入收藏夹、移入回收站 / 恢复 / 永久删除。
- Collection: new sub-collection, rename, delete.
- 收藏夹：新建子收藏夹、重命名、删除。
- Tag: filter by tag, delete.
- 标签：按标签筛选、删除。
- Smart collection: delete.
- 智能收藏夹：删除。
- Inline editing: add/rename collections via Enter-to-confirm editors.
- 行内编辑：通过回车确认的编辑器添加/重命名收藏夹。

### Inspector / 检查器
- Thumbnail preview, tags, and mined color palette.
- 缩略图预览、标签、提取的色板。
- Inline editing: title, description, source URL, kind, and a 1–5 star rating — committed on blur/Enter or click.
- 行内编辑：标题、描述、来源链接、类型与 1–5 星评分——失焦/回车或点击即保存。
- Properties: MIME type, size, dimensions, added date, SHA-256.
- 属性：MIME 类型、大小、尺寸、添加日期、SHA-256。
- Add or remove tags directly.
- 可直接添加或移除标签。

### Interface language / 界面语言
- English and 简体中文, switchable live in Settings ▸ Language; follows the system language by default.
- 支持英文与简体中文，设置 ▸ 语言实时切换；默认跟随系统语言。

### Configuration / 配置
- Persisted JSON config in the platform config directory.
- 配置以 JSON 保存在平台标准配置目录。
- The library location can be changed from the Settings dialog.
- 可在设置对话框中修改素材库所在位置。

---

## UI layout / 界面布局

The desktop app uses a dock layout with a custom title bar:

桌面应用采用停靠栏（dock）布局，配自定义标题栏：

| Dock | Panel | Purpose / 用途 |
|------|-------|----------------|
| Top / 顶 | Title bar | File / Settings buttons, window controls / File / Settings 按钮、窗口控制 |
| Left / 左 | Explorer | Collection tree, smart collections, trash / 收藏夹树、智能收藏夹、回收站 |
| Center / 中 | Workspace | Justified thumbnail grid + search / 对齐缩略图网格 + 搜索 |
| Right / 右 | Tags + Inspector | Tag filter and per-asset details / 标签筛选与资产详情 |

- The **File** menu imports files; **Settings** opens the library-path dialog.
- **File** 菜单导入文件；**Settings** 打开素材库路径设置。
- The whole window is a drop surface — drop any files to import them.
- 整窗都可以作为拖放目标，直接把文件拖入即可导入。
- The workspace grid is a justified (Google-Photos-style) layout that fills the panel edge-to-edge at any width.
- 工作区网格采用对齐布局（Google 相册风格），任意宽度下都撑满面板、不留空白。
- A popover search input sits in the workspace title bar, next to the item count of the browsed view; an "Empty all" button appears in trash mode.
- 工作区标题栏内嵌弹出式搜索框，旁边显示当前视图的资产数量；回收站模式下显示"清空全部"按钮。

---

## Project structure / 项目结构

```
crates/
├── trove-core/          # Domain, persistence & services (no UI)
│   ├── src/
│   │   ├── model.rs     # Plain data types (Asset, Collection, Tag, …)
│   │   ├── library.rs   # High-level facade over store + media dir
│   │   ├── layout.rs    # Justified grid layout (dynamic programming)
│   │   ├── store/       # SQLite (rusqlite) layer: schema, CRUD, FTS, smart queries
│   │   │   ├── schema.rs# Versioned migrations
│   │   │   ├── smart.rs # Smart collection compilation & evaluation
│   │   │   └── batch.rs # Atomic batch mutations
│   │   ├── media/       # Import, probing, thumbnails, color, metadata
│   │   ├── maintenance.rs # Rebuild thumbs/index, orphan cleanup
│   │   ├── events.rs    # Cross-layer events
│   │   ├── config.rs    # App config persistence (JSON)
│   │   └── error.rs     # Error types
│   └── src/             # inline #[cfg(test)] modules / 内联测试模块
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

Key storage tables / 核心数据表：

```
assets             # asset records
collections        # nested folders / 嵌套文件夹
asset_collection   # many-to-many membership / 资产-收藏夹多对多
tags               # case-insensitive tags / 标签
asset_tag          # asset–tag links / 资产-标签关联
smart_collections  # rule-based virtual folders / 智能收藏夹
asset_fts          # full-text search index / 全文搜索索引
```

A library on disk looks like / 磁盘上的素材库结构：

```
<root>/
├── library.db      # single-file database / 单文件数据库
└── media/…         # content-addressed blobs / 内容寻址的文件
```

---

## Build & test / 构建与测试

Requires the Rust toolchain. The workspace has no external system dependencies for the core crate.

需要 Rust 工具链。core 库本身没有外部系统依赖。

```sh
# Build the whole workspace / 构建整个工作区
cargo build

# Run the core tests (the persistence + service layer)
# 运行核心库测试（持久化与服务层）
cargo test -p trove-core

# Run the desktop app / 运行桌面应用
cargo run -p trove-app
```

Test status / 测试状态：`trove-core` compiles and all **46** tests pass. `trove-app` compiles cleanly.

`trove-core` 编译通过，**46** 个测试全部通过。`trove-app` 编译通过。

---

## Status / 开发状态

- **trove-core** — feature-complete for the above list; tested (46 tests). / 上述核心功能已齐并有测试覆盖（46 个测试）。
- **trove-app** — compiles and runs: dock layout, custom title bar, justified thumbnail grid, drag & drop, multi-select, context menus, settings dialog, inspector, and import with progress are all wired. / 已编译可运行：停靠布局、自定义标题栏、对齐缩略图网格、拖放、多选、右键菜单、设置对话框、检查器、带进度导入均已接通。

---

[gpui-kit]: https://github.com/panzhifu/gpui-kit
[rusqlite]: https://github.com/rusqlite/rusqlite