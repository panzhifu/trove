# 资产仓库设计（核心）

> 资产仓库是整个软件的心脏：身份模型、磁盘布局、元数据 schema、导入/缩略图管线、并发模型。
> 其余一切（UI、插件）都是它的皮肤与扩展点。

## 1. 设计原则

1. **库 = 自包含文件夹**：一个库是一个可整体移动、备份、分享的目录。
2. **身份与内容分离**：`id`（UUID，身份，稳定）与 `sha256`（内容哈希，完整性 + 去重 + 变更检测）是两回事。
3. **缩略图是缓存**：永远可重算，不是事实源。
4. **插件是纯函数**：只做「字节 → 数据」，不碰库、不碰文件系统；所有索引/落库在宿主 core 完成。
5. **并发后台化**：所有 DB 访问、哈希、解码、缩略图都在后台执行器跑，UI 永不阻塞。

## 2. 磁盘布局

```
my-library/
├── library.json          # 库清单：id、name、schema_version
├── index.db              # libSQL 本地格式（可读标准 SQLite 文件），元数据 + 索引
├── blobs/{aa}/{sha256}.{ext}   # 原始文件，内容寻址（按 sha256 前两位分片）
├── thumbs/{aa}/{sha256}-{size}.webp  # 缩略图缓存，按 sha256+尺寸，可跨 asset 共享
└── trash/                # 待清除的 blob 文件（软删除后等待 purge）
```

## 3. 身份与内容模型

- **`asset.id`**：UUID v4（身份）。重命名、打标签、挪文件夹都不变。
- **`asset.sha256`**：内容哈希。换文件内容才变；同时用于去重与「原件被外部修改」检测。

两者分离带来一个关键设计：**blob（内容）与 asset（元数据）分表**，支持去重——同一张图被导入两次，物理文件只存一份，两条 asset 行共享同一个 blob。

```
blob   = 物理文件（sha256 寻址，ref_count 计数）
asset  = 用户视角的一条素材（id、name、tags、folder、rating…，指向一个 blob）
```

## 4. libSQL 元数据 schema

> libSQL 的 FTS5、递归 CTE、并发行为需在 M0 spike 里验证；下述 schema 为设计目标，不绑死具体 API。
>
> **唯一事实源**是 `crates/trove-core/src/store/schema.rs` 里的 `MIGRATIONS`，下表是它的说明，改动时同步。

| 表 | 关键字段 | 说明 |
|---|---|---|
| `blobs` | sha256(PK), ext, rel_path, size_bytes, ref_count | 内容寻址的物理文件，ref_count 用于回收 |
| `assets` | id(PK), sha256(FK→blobs), name, ext, folder_id, size_bytes, width, height, mime, rating, annotation, source_url, is_trashed, imported_at, modified_at | 素材元数据 |
| `folders` | id(PK), name, parent_id, sort_order, color, created_at, updated_at | 文件夹树，parent_id 邻接表 |
| `tag_groups` | id(PK), name, color | 标签分组（如「项目」「风格」） |
| `tags` | id(PK), group_id(FK), name, color, UNIQUE(group_id, name) | 标签 |
| `asset_tags` | (asset_id, tag_id) 复合主键 | 素材↔标签多对多 |
| `smart_folders` | id(PK), name, query | 智能文件夹（v2，保存的搜索条件） |

**没有 `library` 表**：库的 id / name / schema_version 存在 `library.json` 清单里（对应
`trove_core::Library`），不进库文件。理由：schema 迁移器需要先读 `schema_version`
才知道该应用哪些迁移，而这个版本必须在打开数据库**之前**就能拿到。

索引设计（v1 最少集）：

- `assets(sha256)` —— 去重查询、变更检测。
- `assets(folder_id)`、`assets(ext)`、`assets(rating)` —— 基础过滤。
- `assets(imported_at)`、`assets(modified_at)` —— 时间排序。
- `folders(parent_id)` —— 树展开（递归 CTE 走 `id`/`parent_id`）。
- `tags(group_id)`、`asset_tags(tag_id)` —— 标签分组展开与反向查找。

## 5. 导入与缩略图管线

导入是一条**异步流水线**，每步都在后台执行器，任一步失败不破坏库（事务保证）：

```
1. 复制/移动文件进 staging
2. 计算 sha256（宿主原生）
3. 查 blobs：已存在 → ref_count+1，跳过复制；否则写入 blobs/
4. 识别格式（扩展名 + magic bytes）→ 查插件注册表
5. 调 FormatPlugin.extract_meta / decode（WASM 沙箱，或内置原生解码）
6. 宿主把 RGBA 编码为 WebP 缩略图（256 / 512 两档）→ thumbs/
7. 写 assets 行 + 回填 size/width/height → 提交事务
8. 通过 channel 通知 UI 刷新
```

分工边界（重要）：

- **格式插件只做**「字节 → RGBA / 元数据」。
- **宿主 core 统一做**：哈希、去重、缩略图编码（WebP）、落库。插件不知道缩略图策略与存储细节。

## 6. 并发模型（GPUI）

- 仓库以 GPUI 实体（`Entity<Library>`）持有，内部是 `Arc<LibraryStore>`。
- **写串行、读可并发**：一个写连接（后台任务串行消费），读用连接池；v1 用 `Mutex<Connection>` 起步。
- 后台执行器（`cx.background_executor()` / `cx.spawn`）跑：哈希、解码、缩略图、DB 写。
- 结果通过 channel 回投成 `LibraryEvent`，UI 侧 `cx.notify()` 刷新；导入进度用事件流驱动进度条。

## 7. 回收站与删除

- 删除素材 = 把 `assets.is_trashed` 置 1（软删除），非立即物理删除。
- 对应 blob 的 `ref_count` 减 1；降到 0 时 blob 移入 `trash/`，由「清空回收站」统一 purge。
- 这样误删可恢复，且去重共享的 blob 不会被提前误删。

## 8. 多库与迁移

- 支持多库；同一时刻打开一个（Eagle 同款心智）。
- 库是自包含目录，**整体拷贝 = 备份/迁移**，无需额外导出逻辑。
- `library.json` 记录 `schema_version`，启动时按版本做 schema 迁移。

## 9. 搜索（v1 预留，不实现）

v1 不做搜索，但 schema 已把文本字段（name / annotation / tags）放好，未来按需加：

1. **基础过滤**：SQL 即可（名称/扩展名/文件夹/标签/评分/尺寸/日期范围）。
2. **全文检索**：FTS5 虚拟表（libSQL 支持度待 M0 验证；若不支持，退回 `LIKE` 或外置 tantivy）。
3. **颜色筛选**：导入时由插件/原生提取主色存入 `assets` 或独立表。
4. **相似图搜索**：需 embedding，考虑 lance / tantivy / usearch，明确 v2+。

设计约束：以上任何增强都**不改变身份模型与磁盘布局**，只增表/增索引。
